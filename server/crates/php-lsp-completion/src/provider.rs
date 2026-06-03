//! Completion item providers.
//!
//! Given a completion context and the workspace index, provides relevant
//! completion items.

use crate::context::{CompletionContext, MemberAccessMode};
use lsp_types::{CompletionItem, CompletionItemKind, InsertTextFormat};
use php_lsp_index::workspace::WorkspaceIndex;
use php_lsp_parser::phpdoc::parse_phpdoc;
use php_lsp_types::{
    FileSymbols, PhpDocMethod, PhpDocProperty, PhpDocPropertyAccess, PhpSymbolKind, SymbolInfo,
    Visibility,
};
use serde_json::json;
use std::collections::HashSet;

/// PHP keywords for free context.
const PHP_KEYWORDS: &[&str] = &[
    "abstract",
    "array",
    "as",
    "break",
    "callable",
    "case",
    "catch",
    "class",
    "clone",
    "const",
    "continue",
    "declare",
    "default",
    "do",
    "echo",
    "else",
    "elseif",
    "enum",
    "extends",
    "final",
    "finally",
    "fn",
    "for",
    "foreach",
    "function",
    "global",
    "if",
    "implements",
    "include",
    "include_once",
    "instanceof",
    "interface",
    "list",
    "match",
    "namespace",
    "new",
    "print",
    "private",
    "protected",
    "public",
    "readonly",
    "require",
    "require_once",
    "return",
    "static",
    "switch",
    "throw",
    "trait",
    "try",
    "use",
    "var",
    "while",
    "yield",
];

struct SnippetTemplate {
    label: &'static str,
    insert_text: &'static str,
    detail: &'static str,
}

const PHP_SNIPPETS: &[SnippetTemplate] = &[
    SnippetTemplate {
        label: "class",
        insert_text: "class ${1:Name}\n{\n    $0\n}",
        detail: "class declaration",
    },
    SnippetTemplate {
        label: "interface",
        insert_text: "interface ${1:Name}\n{\n    $0\n}",
        detail: "interface declaration",
    },
    SnippetTemplate {
        label: "trait",
        insert_text: "trait ${1:Name}\n{\n    $0\n}",
        detail: "trait declaration",
    },
    SnippetTemplate {
        label: "enum",
        insert_text: "enum ${1:Name}\n{\n    $0\n}",
        detail: "enum declaration",
    },
    SnippetTemplate {
        label: "function",
        insert_text: "function ${1:name}(${2}): ${3:void}\n{\n    $0\n}",
        detail: "function declaration",
    },
    SnippetTemplate {
        label: "if",
        insert_text: "if (${1:condition}) {\n    $0\n}",
        detail: "if statement",
    },
    SnippetTemplate {
        label: "foreach",
        insert_text: "foreach (\\$${1:items} as \\$${2:item}) {\n    $0\n}",
        detail: "foreach statement",
    },
    SnippetTemplate {
        label: "try",
        insert_text: "try {\n    $1\n} catch (${2:\\Throwable} \\$${3:e}) {\n    $0\n}",
        detail: "try/catch statement",
    },
];

/// Provide completion items based on context.
pub fn provide_completions(
    context: &CompletionContext,
    index: &WorkspaceIndex,
    file_symbols: &FileSymbols,
) -> Vec<CompletionItem> {
    let current_class_fqn = find_current_class_fqn(file_symbols);
    provide_completions_with_current_class(
        context,
        index,
        file_symbols,
        current_class_fqn.as_deref(),
    )
}

/// Provide completion items at a byte-column cursor range.
pub fn provide_completions_at_range(
    context: &CompletionContext,
    index: &WorkspaceIndex,
    file_symbols: &FileSymbols,
    cursor_range: (u32, u32, u32, u32),
) -> Vec<CompletionItem> {
    let current_class_fqn = find_current_class_fqn_at_range(file_symbols, cursor_range);
    provide_completions_with_current_class(
        context,
        index,
        file_symbols,
        current_class_fqn.as_deref(),
    )
}

fn provide_completions_with_current_class(
    context: &CompletionContext,
    index: &WorkspaceIndex,
    file_symbols: &FileSymbols,
    current_class_fqn: Option<&str>,
) -> Vec<CompletionItem> {
    match context {
        CompletionContext::MemberAccess {
            object_expr,
            class_fqn,
            member_prefix,
            access_mode,
        } => provide_member_completions(
            object_expr,
            member_prefix,
            class_fqn.as_deref(),
            index,
            current_class_fqn,
            *access_mode,
        ),
        CompletionContext::StaticAccess {
            class_fqn,
            class_expr,
            member_prefix,
        } => provide_static_completions(
            class_fqn,
            class_expr,
            member_prefix,
            index,
            current_class_fqn,
        ),
        CompletionContext::ArrayKey { .. } => vec![],
        CompletionContext::Variable { prefix } => {
            provide_variable_completions(prefix, file_symbols)
        }
        CompletionContext::Namespace { prefix } => provide_namespace_completions(prefix, index),
        CompletionContext::UseStatement { prefix } => {
            provide_use_statement_completions(prefix, index)
        }
        CompletionContext::Free { prefix } => provide_free_completions(prefix, index),
        CompletionContext::None => vec![],
    }
}

/// Provide member access completions (`->`).
fn provide_member_completions(
    object_expr: &str,
    member_prefix: &str,
    inferred_class_fqn: Option<&str>,
    index: &WorkspaceIndex,
    current_class_fqn: Option<&str>,
    access_mode: MemberAccessMode,
) -> Vec<CompletionItem> {
    let mut items = Vec::new();

    // Try to resolve the type of the object expression
    // For `$this`, look up the current class
    let class_fqn = if let Some(fqn) = inferred_class_fqn {
        Some(fqn.to_string())
    } else if object_expr == "$this" {
        current_class_fqn.map(str::to_string)
    } else {
        // Best-effort: look for variable type hints or assignments
        // For now, just try to find any type annotation
        None
    };

    if let Some(fqn) = class_fqn {
        let members = index.get_members(&fqn);
        for member in members {
            // Skip static members for instance access
            if member.modifiers.is_static {
                continue;
            }
            if !member_is_visible(&member, object_expr == "$this", current_class_fqn) {
                continue;
            }

            items.push(symbol_to_completion_item(
                &member,
                false,
                Some(member_prefix),
            ));
        }
        let mut seen_labels: HashSet<String> =
            items.iter().map(|item| item.label.clone()).collect();
        add_phpdoc_virtual_member_completions(
            &fqn,
            member_prefix,
            index,
            &mut items,
            &mut seen_labels,
            access_mode,
        );
    }

    sort_completion_items(&mut items);
    items
}

fn add_phpdoc_virtual_member_completions(
    class_fqn: &str,
    member_prefix: &str,
    index: &WorkspaceIndex,
    items: &mut Vec<CompletionItem>,
    seen_labels: &mut HashSet<String>,
    access_mode: MemberAccessMode,
) {
    for owner in index.get_type_hierarchy_symbols(class_fqn) {
        let Some(ref doc_comment) = owner.doc_comment else {
            continue;
        };
        let phpdoc = parse_phpdoc(doc_comment);

        for method in &phpdoc.methods {
            if method.is_static || !seen_labels.insert(method.name.clone()) {
                continue;
            }
            items.push(phpdoc_method_completion_item(
                &owner.fqn,
                method,
                member_prefix,
            ));
        }

        for property in &phpdoc.properties {
            if !phpdoc_property_matches_access(property.access, access_mode) {
                continue;
            }
            if !seen_labels.insert(property.name.clone()) {
                continue;
            }
            items.push(phpdoc_property_completion_item(
                &owner.fqn,
                property,
                member_prefix,
            ));
        }
    }
}

fn add_phpdoc_static_virtual_method_completions(
    class_fqn: &str,
    member_prefix: &str,
    index: &WorkspaceIndex,
    items: &mut Vec<CompletionItem>,
    seen_labels: &mut HashSet<String>,
) {
    for owner in index.get_type_hierarchy_symbols(class_fqn) {
        let Some(ref doc_comment) = owner.doc_comment else {
            continue;
        };
        let phpdoc = parse_phpdoc(doc_comment);

        for method in &phpdoc.methods {
            if !method.is_static || !seen_labels.insert(method.name.clone()) {
                continue;
            }
            items.push(phpdoc_method_completion_item(
                &owner.fqn,
                method,
                member_prefix,
            ));
        }
    }
}

fn phpdoc_property_matches_access(
    property_access: PhpDocPropertyAccess,
    completion_access: MemberAccessMode,
) -> bool {
    match completion_access {
        MemberAccessMode::Read => property_access.is_readable(),
        MemberAccessMode::Write => property_access.is_writable(),
    }
}

fn phpdoc_property_completion_item(
    owner_fqn: &str,
    property: &PhpDocProperty,
    member_prefix: &str,
) -> CompletionItem {
    let label = property.name.clone();
    let access = phpdoc_property_tag(property.access);
    CompletionItem {
        label: label.clone(),
        kind: Some(CompletionItemKind::PROPERTY),
        detail: Some(match &property.type_info {
            Some(type_info) => format!("{} {}", access, type_info),
            None => access.to_string(),
        }),
        sort_text: Some(format!(
            "{}_{}_{}",
            symbol_sort_rank(PhpSymbolKind::Property),
            completion_prefix_rank(&label, Some(member_prefix)),
            label.to_ascii_lowercase()
        )),
        filter_text: Some(format!("{} {}::${}", label, owner_fqn, property.name)),
        data: Some(json!({
            "kind": "phpdoc-virtual-member",
            "ownerFqn": owner_fqn,
            "memberKind": "property",
            "memberName": property.name,
        })),
        commit_characters: Some(vec![";".to_string(), ",".to_string()]),
        ..Default::default()
    }
}

fn phpdoc_method_completion_item(
    owner_fqn: &str,
    method: &PhpDocMethod,
    member_prefix: &str,
) -> CompletionItem {
    let label = method.name.clone();
    CompletionItem {
        label: label.clone(),
        kind: Some(CompletionItemKind::METHOD),
        detail: Some(phpdoc_method_detail(method)),
        sort_text: Some(format!(
            "{}_{}_{}",
            symbol_sort_rank(PhpSymbolKind::Method),
            completion_prefix_rank(&label, Some(member_prefix)),
            label.to_ascii_lowercase()
        )),
        filter_text: Some(format!("{} {}::{}", label, owner_fqn, method.name)),
        data: Some(json!({
            "kind": "phpdoc-virtual-member",
            "ownerFqn": owner_fqn,
            "memberKind": "method",
            "memberName": method.name,
        })),
        commit_characters: Some(vec!["(".to_string()]),
        ..Default::default()
    }
}

fn phpdoc_property_tag(access: PhpDocPropertyAccess) -> &'static str {
    match access {
        PhpDocPropertyAccess::ReadWrite => "@property",
        PhpDocPropertyAccess::ReadOnly => "@property-read",
        PhpDocPropertyAccess::WriteOnly => "@property-write",
    }
}

fn phpdoc_method_detail(method: &PhpDocMethod) -> String {
    let params: Vec<String> = method
        .params
        .iter()
        .map(|param| {
            let mut value = String::new();
            if let Some(ref type_info) = param.type_info {
                value.push_str(&type_info.to_string());
                value.push(' ');
            }
            if param.is_by_ref {
                value.push('&');
            }
            if param.is_variadic {
                value.push_str("...");
            }
            value.push('$');
            value.push_str(&param.name);
            if let Some(ref default) = param.default_value {
                value.push_str(" = ");
                value.push_str(default);
            }
            value
        })
        .collect();
    let mut detail = format!("({})", params.join(", "));
    if let Some(ref return_type) = method.return_type {
        detail.push_str(": ");
        detail.push_str(&return_type.to_string());
    }
    detail
}

/// Provide static access completions (`::`).
fn provide_static_completions(
    class_fqn: &str,
    class_expr: &str,
    member_prefix: &str,
    index: &WorkspaceIndex,
    current_class_fqn: Option<&str>,
) -> Vec<CompletionItem> {
    let mut items = Vec::new();

    items.push(class_pseudo_constant_completion_item(
        class_fqn,
        member_prefix,
    ));

    let fqn = class_fqn.to_string();

    let members = index.get_members(&fqn);
    for member in members {
        let is_parent_instance_method =
            class_expr == "parent" && member.kind == PhpSymbolKind::Method;
        if !member.modifiers.is_static
            && !is_parent_instance_method
            && !matches!(
                member.kind,
                PhpSymbolKind::ClassConstant | PhpSymbolKind::EnumCase
            )
        {
            continue;
        }
        if !member_is_visible(
            &member,
            matches!(class_expr, "self" | "static" | "parent"),
            current_class_fqn,
        ) {
            continue;
        }
        items.push(symbol_to_completion_item(
            &member,
            true,
            Some(member_prefix),
        ));
    }
    let mut seen_labels: HashSet<String> = items.iter().map(|item| item.label.clone()).collect();
    add_phpdoc_static_virtual_method_completions(
        &fqn,
        member_prefix,
        index,
        &mut items,
        &mut seen_labels,
    );

    sort_completion_items(&mut items);
    items
}

fn class_pseudo_constant_completion_item(class_fqn: &str, member_prefix: &str) -> CompletionItem {
    let detail = if class_fqn.is_empty() {
        "class-string".to_string()
    } else {
        format!("class-string<{}>", class_fqn)
    };
    let mut item = CompletionItem {
        label: "class".to_string(),
        kind: Some(CompletionItemKind::CONSTANT),
        detail: Some(detail),
        insert_text: Some("class".to_string()),
        filter_text: Some(format!("class {}::class", class_fqn)),
        ..Default::default()
    };
    item.sort_text = Some(format!(
        "{}_{}_class",
        symbol_sort_rank(PhpSymbolKind::ClassConstant),
        completion_prefix_rank(&item.label, Some(member_prefix))
    ));
    item
}

/// Provide variable completions.
fn provide_variable_completions(prefix: &str, file_symbols: &FileSymbols) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // Collect variables from file symbols
    // In PHP, common variables are $this, parameters, local vars
    let prefix_lower = prefix.to_lowercase();

    // Add $this
    if "this".starts_with(&prefix_lower) {
        items.push(CompletionItem {
            label: "$this".to_string(),
            kind: Some(CompletionItemKind::VARIABLE),
            sort_text: Some("0100_$this".to_string()),
            filter_text: Some("$this this".to_string()),
            ..Default::default()
        });
        seen.insert("$this".to_string());
    }

    // Extract parameters from method/function symbols
    for sym in &file_symbols.symbols {
        if let Some(ref sig) = sym.signature {
            for param in &sig.params {
                let var_name = format!("${}", param.name);
                if !seen.contains(&var_name) && param.name.to_lowercase().starts_with(&prefix_lower)
                {
                    let detail = param.type_info.as_ref().map(|t| t.to_string());
                    items.push(CompletionItem {
                        label: var_name.clone(),
                        kind: Some(CompletionItemKind::VARIABLE),
                        detail,
                        sort_text: Some(format!("0101_{}", param.name.to_ascii_lowercase())),
                        filter_text: Some(format!("{} {}", var_name, param.name)),
                        ..Default::default()
                    });
                    seen.insert(var_name);
                }
            }
        }
    }

    sort_completion_items(&mut items);
    items
}

/// Provide namespace/class completions.
fn provide_namespace_completions(prefix: &str, index: &WorkspaceIndex) -> Vec<CompletionItem> {
    provide_namespace_completions_with_options(prefix, index, false)
}

fn provide_use_statement_completions(prefix: &str, index: &WorkspaceIndex) -> Vec<CompletionItem> {
    provide_namespace_completions_with_options(prefix, index, true)
}

fn provide_namespace_completions_with_options(
    prefix: &str,
    index: &WorkspaceIndex,
    insert_fqn: bool,
) -> Vec<CompletionItem> {
    let mut items = Vec::new();

    for entry in index.types.iter() {
        let sym = entry.value();
        if let Some(match_rank) = namespace_completion_match_rank(&sym.name, &sym.fqn, prefix) {
            let mut item = CompletionItem {
                label: sym.name.clone(),
                kind: Some(symbol_kind_to_completion_kind(sym.kind)),
                detail: Some(sym.fqn.clone()),
                sort_text: Some(format!(
                    "0300_{}_{}_{}",
                    match_rank,
                    sym.name.to_ascii_lowercase(),
                    sym.fqn.to_ascii_lowercase()
                )),
                filter_text: Some(format!("{} {}", sym.name, sym.fqn)),
                data: Some(serde_json::Value::String(sym.fqn.clone())),
                ..Default::default()
            };
            if insert_fqn {
                item.insert_text = Some(sym.fqn.clone());
            }
            items.push(item);
        }
    }

    // Limit results
    sort_completion_items(&mut items);
    items.truncate(100);
    items
}

fn namespace_completion_match_rank(name: &str, fqn: &str, prefix: &str) -> Option<&'static str> {
    let prefix = prefix.trim().trim_start_matches('\\');
    if prefix.is_empty() {
        return Some("1000");
    }

    let name_lower = name.to_lowercase();
    let fqn_lower = fqn.to_lowercase();
    let prefix_lower = prefix.to_lowercase();

    if name == prefix || fqn == prefix {
        Some("0000")
    } else if name.starts_with(prefix) || fqn.starts_with(prefix) {
        Some("0100")
    } else if name_lower == prefix_lower || fqn_lower == prefix_lower {
        Some("0200")
    } else if name_lower.starts_with(&prefix_lower) || fqn_lower.starts_with(&prefix_lower) {
        Some("0300")
    } else if name_lower.contains(&prefix_lower) || fqn_lower.contains(&prefix_lower) {
        Some("0900")
    } else {
        None
    }
}

/// Provide free context completions (classes, functions, keywords).
fn provide_free_completions(prefix: &str, index: &WorkspaceIndex) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    let prefix_lower = prefix.to_lowercase();

    // Add matching keywords
    for keyword in PHP_KEYWORDS {
        if keyword.starts_with(&prefix_lower) {
            items.push(keyword_completion_item(keyword));
        }
    }

    // Add matching types
    let results = index.search(prefix);
    for sym in results {
        items.push(CompletionItem {
            label: sym.name.clone(),
            kind: Some(symbol_kind_to_completion_kind(sym.kind)),
            detail: Some(sym.fqn.clone()),
            sort_text: Some(format!(
                "0300_{}_{}",
                completion_prefix_rank(&sym.name, Some(prefix)),
                sym.name.to_ascii_lowercase()
            )),
            filter_text: Some(format!("{} {}", sym.name, sym.fqn)),
            data: Some(serde_json::Value::String(sym.fqn.clone())),
            ..Default::default()
        });
    }

    // Add matching functions
    for entry in index.functions.iter() {
        let sym = entry.value();
        if sym.name.to_lowercase().starts_with(&prefix_lower) {
            items.push(CompletionItem {
                label: sym.name.clone(),
                kind: Some(CompletionItemKind::FUNCTION),
                detail: Some(sym.fqn.clone()),
                sort_text: Some(format!("0200_{}", sym.name.to_ascii_lowercase())),
                filter_text: Some(format!("{} {}", sym.name, sym.fqn)),
                commit_characters: Some(vec!["(".to_string()]),
                data: Some(serde_json::Value::String(sym.fqn.clone())),
                ..Default::default()
            });
        }
    }

    // Limit
    sort_completion_items(&mut items);
    items.truncate(100);
    items
}

/// Convert a SymbolInfo to a CompletionItem.
fn symbol_to_completion_item(
    sym: &SymbolInfo,
    is_static_access: bool,
    member_prefix: Option<&str>,
) -> CompletionItem {
    let kind = symbol_kind_to_completion_kind(sym.kind);

    let detail = sym.signature.as_ref().map(|sig| {
        let params_str: Vec<String> = sig
            .params
            .iter()
            .map(|p| {
                let mut s = String::new();
                if let Some(ref t) = p.type_info {
                    s.push_str(&t.to_string());
                    s.push(' ');
                }
                s.push('$');
                s.push_str(&p.name);
                s
            })
            .collect();
        let mut detail = format!("({})", params_str.join(", "));
        if let Some(ref ret) = sig.return_type {
            detail.push_str(&format!(": {}", ret));
        }
        detail
    });

    let label =
        if sym.kind == PhpSymbolKind::Property && is_static_access && !sym.name.starts_with('$') {
            format!("${}", sym.name)
        } else {
            sym.name.clone()
        };

    let mut tags = Vec::new();
    if sym.modifiers.is_deprecated {
        tags.push(lsp_types::CompletionItemTag::DEPRECATED);
    }

    let mut item = CompletionItem {
        label,
        kind: Some(kind),
        detail,
        tags: if tags.is_empty() { None } else { Some(tags) },
        // Store FQN in data for resolve
        data: Some(serde_json::Value::String(sym.fqn.clone())),
        ..Default::default()
    };
    item.sort_text = Some(format!(
        "{}_{}_{}",
        symbol_sort_rank(sym.kind),
        completion_prefix_rank(&item.label, member_prefix),
        item.label.to_ascii_lowercase()
    ));
    item.filter_text = Some(format!("{} {}", item.label, sym.fqn));
    item.commit_characters = match sym.kind {
        PhpSymbolKind::Method | PhpSymbolKind::Function => Some(vec!["(".to_string()]),
        PhpSymbolKind::Class
        | PhpSymbolKind::Interface
        | PhpSymbolKind::Trait
        | PhpSymbolKind::Enum => Some(vec!["\\".to_string(), ":".to_string()]),
        PhpSymbolKind::Property => Some(vec![";".to_string(), ",".to_string()]),
        _ => None,
    };
    item
}

fn completion_prefix_rank(label: &str, member_prefix: Option<&str>) -> &'static str {
    let Some(prefix) = member_prefix
        .map(str::trim)
        .filter(|prefix| !prefix.is_empty())
    else {
        return "1000";
    };

    let normalized_label = label.trim_start_matches('$').to_ascii_lowercase();
    let normalized_prefix = prefix.trim_start_matches('$').to_ascii_lowercase();

    if normalized_label.starts_with(&normalized_prefix) {
        "0000"
    } else if normalized_label.contains(&normalized_prefix) {
        "0100"
    } else {
        "1000"
    }
}

fn keyword_completion_item(keyword: &str) -> CompletionItem {
    if let Some(snippet) = PHP_SNIPPETS.iter().find(|snippet| snippet.label == keyword) {
        CompletionItem {
            label: snippet.label.to_string(),
            kind: Some(CompletionItemKind::SNIPPET),
            detail: Some(snippet.detail.to_string()),
            insert_text: Some(snippet.insert_text.to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            sort_text: Some(format!("0000_{}", snippet.label)),
            filter_text: Some(snippet.label.to_string()),
            ..Default::default()
        }
    } else {
        CompletionItem {
            label: keyword.to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            sort_text: Some(format!("0001_{}", keyword)),
            filter_text: Some(keyword.to_string()),
            ..Default::default()
        }
    }
}

fn symbol_sort_rank(kind: PhpSymbolKind) -> &'static str {
    match kind {
        PhpSymbolKind::Method => "0100",
        PhpSymbolKind::Property => "0101",
        PhpSymbolKind::Function => "0200",
        PhpSymbolKind::Class
        | PhpSymbolKind::Interface
        | PhpSymbolKind::Trait
        | PhpSymbolKind::Enum => "0300",
        PhpSymbolKind::ClassConstant | PhpSymbolKind::GlobalConstant | PhpSymbolKind::EnumCase => {
            "0400"
        }
        PhpSymbolKind::Namespace => "0500",
    }
}

fn member_is_visible(
    member: &SymbolInfo,
    accessing_from_self: bool,
    current_class_fqn: Option<&str>,
) -> bool {
    match member.visibility {
        Visibility::Public => true,
        Visibility::Protected => accessing_from_self && current_class_fqn.is_some(),
        Visibility::Private => {
            accessing_from_self
                && member
                    .parent_fqn
                    .as_deref()
                    .zip(current_class_fqn)
                    .is_some_and(|(declaring_class, current_class)| {
                        fqn_matches(declaring_class, current_class)
                    })
        }
    }
}

fn sort_completion_items(items: &mut [CompletionItem]) {
    items.sort_by(|a, b| {
        a.sort_text
            .as_deref()
            .unwrap_or(&a.label)
            .cmp(b.sort_text.as_deref().unwrap_or(&b.label))
            .then_with(|| a.label.cmp(&b.label))
    });
}

/// Convert PhpSymbolKind to LSP CompletionItemKind.
fn symbol_kind_to_completion_kind(kind: PhpSymbolKind) -> CompletionItemKind {
    match kind {
        PhpSymbolKind::Class => CompletionItemKind::CLASS,
        PhpSymbolKind::Interface => CompletionItemKind::INTERFACE,
        PhpSymbolKind::Trait => CompletionItemKind::INTERFACE,
        PhpSymbolKind::Enum => CompletionItemKind::ENUM,
        PhpSymbolKind::Function => CompletionItemKind::FUNCTION,
        PhpSymbolKind::Method => CompletionItemKind::METHOD,
        PhpSymbolKind::Property => CompletionItemKind::PROPERTY,
        PhpSymbolKind::ClassConstant => CompletionItemKind::CONSTANT,
        PhpSymbolKind::GlobalConstant => CompletionItemKind::CONSTANT,
        PhpSymbolKind::EnumCase => CompletionItemKind::ENUM_MEMBER,
        PhpSymbolKind::Namespace => CompletionItemKind::MODULE,
    }
}

/// Find the FQN of the class/interface/trait/enum we're currently inside.
fn find_current_class_fqn(file_symbols: &FileSymbols) -> Option<String> {
    for sym in &file_symbols.symbols {
        match sym.kind {
            PhpSymbolKind::Class
            | PhpSymbolKind::Interface
            | PhpSymbolKind::Trait
            | PhpSymbolKind::Enum => {
                return Some(sym.fqn.clone());
            }
            _ => {}
        }
    }
    None
}

fn find_current_class_fqn_at_range(
    file_symbols: &FileSymbols,
    range: (u32, u32, u32, u32),
) -> Option<String> {
    current_class_symbol_at_range(file_symbols, range).map(|sym| sym.fqn.clone())
}

fn current_class_symbol_at_range(
    file_symbols: &FileSymbols,
    range: (u32, u32, u32, u32),
) -> Option<&SymbolInfo> {
    let mut current: Option<&SymbolInfo> = None;
    for sym in file_symbols.symbols.iter().filter(|sym| {
        matches!(
            sym.kind,
            PhpSymbolKind::Class
                | PhpSymbolKind::Interface
                | PhpSymbolKind::Trait
                | PhpSymbolKind::Enum
        ) && byte_range_contains(sym.range, range)
    }) {
        if current.is_none_or(|candidate| byte_range_contains(candidate.range, sym.range)) {
            current = Some(sym);
        }
    }
    current
}

fn byte_range_contains(outer: (u32, u32, u32, u32), inner: (u32, u32, u32, u32)) -> bool {
    (inner.0 > outer.0 || (inner.0 == outer.0 && inner.1 >= outer.1))
        && (inner.2 < outer.2 || (inner.2 == outer.2 && inner.3 <= outer.3))
}

fn fqn_matches(left: &str, right: &str) -> bool {
    left.trim_start_matches('\\') == right.trim_start_matches('\\')
}

#[cfg(test)]
mod tests {
    use super::*;
    use php_lsp_types::*;

    fn make_symbol(
        name: &str,
        fqn: &str,
        kind: PhpSymbolKind,
        parent_fqn: Option<&str>,
        visibility: Visibility,
        is_static: bool,
    ) -> SymbolInfo {
        SymbolInfo {
            name: name.to_string(),
            fqn: fqn.to_string(),
            kind,
            uri: "file:///test.php".to_string(),
            range: (0, 0, 0, 0),
            selection_range: (0, 0, 0, name.len() as u32),
            visibility,
            modifiers: SymbolModifiers {
                is_static,
                ..Default::default()
            },
            attributes: vec![],
            doc_comment: None,
            signature: if matches!(kind, PhpSymbolKind::Method | PhpSymbolKind::Function) {
                Some(Signature {
                    params: vec![],
                    return_type: None,
                })
            } else {
                None
            },
            parent_fqn: parent_fqn.map(str::to_string),
            extends: vec![],
            implements: vec![],
            traits: vec![],
            templates: vec![],
            template_bindings: vec![],
        }
    }

    fn with_range(mut symbol: SymbolInfo, range: (u32, u32, u32, u32)) -> SymbolInfo {
        symbol.range = range;
        symbol
    }

    #[test]
    fn test_keyword_completion() {
        let index = WorkspaceIndex::new();
        let file_symbols = FileSymbols::default();
        let ctx = CompletionContext::Free {
            prefix: "cla".to_string(),
        };
        let items = provide_completions(&ctx, &index, &file_symbols);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"class"), "Should contain 'class' keyword");
        let class_item = items
            .iter()
            .find(|item| item.label == "class")
            .expect("class keyword completion");
        assert_eq!(class_item.kind, Some(CompletionItemKind::SNIPPET));
        assert_eq!(
            class_item.insert_text_format,
            Some(InsertTextFormat::SNIPPET)
        );
        assert!(
            class_item
                .insert_text
                .as_deref()
                .is_some_and(|text| text.contains("${1:Name}")),
            "class completion should use snippet placeholders"
        );
    }

    #[test]
    fn test_class_completion() {
        let index = WorkspaceIndex::new();
        let file_symbols = FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![SymbolInfo {
                name: "UserService".to_string(),
                fqn: "App\\UserService".to_string(),
                kind: PhpSymbolKind::Class,
                uri: "file:///test.php".to_string(),
                range: (0, 0, 10, 0),
                selection_range: (0, 6, 0, 17),
                visibility: Visibility::Public,
                modifiers: SymbolModifiers::default(),
                attributes: vec![],
                doc_comment: None,
                signature: None,
                parent_fqn: None,
                extends: vec![],
                implements: vec![],
                traits: vec![],
                templates: vec![],
                template_bindings: vec![],
            }],
            ..Default::default()
        };
        index.update_file("file:///test.php", file_symbols.clone());

        let ctx = CompletionContext::Free {
            prefix: "User".to_string(),
        };
        let items = provide_completions(&ctx, &index, &file_symbols);
        assert!(
            items.iter().any(|i| i.label == "UserService"),
            "Should find UserService"
        );
    }

    #[test]
    fn test_use_statement_completion_inserts_full_fqn() {
        let mut class = make_symbol(
            "ClassName",
            "Vendor\\Package\\ClassName",
            PhpSymbolKind::Class,
            None,
            Visibility::Public,
            false,
        );
        class.uri = "file:///vendor/ClassName.php".to_string();
        let symbols = FileSymbols {
            symbols: vec![class],
            ..Default::default()
        };
        let index = WorkspaceIndex::new();
        index.update_file("file:///vendor/ClassName.php", symbols);

        let ctx = CompletionContext::UseStatement {
            prefix: "Ven".to_string(),
        };
        let items = provide_completions(&ctx, &index, &FileSymbols::default());
        let item = items
            .iter()
            .find(|item| item.label == "ClassName")
            .expect("use completion should keep short class label");

        assert_eq!(
            item.insert_text.as_deref(),
            Some("Vendor\\Package\\ClassName")
        );
        assert_eq!(item.detail.as_deref(), Some("Vendor\\Package\\ClassName"));
    }

    #[test]
    fn test_namespace_completion_prioritizes_fqn_prefix_over_contains_matches() {
        let file_symbols = FileSymbols {
            symbols: vec![
                make_symbol(
                    "AlphaNoise",
                    "Vendor\\App\\AlphaNoise",
                    PhpSymbolKind::Class,
                    None,
                    Visibility::Public,
                    false,
                ),
                make_symbol(
                    "ZedService",
                    "App\\ZedService",
                    PhpSymbolKind::Class,
                    None,
                    Visibility::Public,
                    false,
                ),
            ],
            ..Default::default()
        };
        let index = WorkspaceIndex::new();
        index.update_file("file:///test.php", file_symbols.clone());

        let ctx = CompletionContext::Namespace {
            prefix: "App\\".to_string(),
        };
        let items = provide_completions(&ctx, &index, &file_symbols);
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();

        assert_eq!(labels.first(), Some(&"ZedService"));
        assert_eq!(labels, vec!["ZedService", "AlphaNoise"]);
    }

    #[test]
    fn test_namespace_completion_keeps_prefix_matches_before_truncating_contains_noise() {
        let mut symbols = Vec::new();
        for idx in 0..120 {
            symbols.push(make_symbol(
                &format!("AlphaTyNoise{idx:03}"),
                &format!("App\\AlphaTyNoise{idx:03}"),
                PhpSymbolKind::Class,
                None,
                Visibility::Public,
                false,
            ));
        }
        symbols.push(make_symbol(
            "TypeGuess",
            "App\\TypeGuess",
            PhpSymbolKind::Class,
            None,
            Visibility::Public,
            false,
        ));
        let file_symbols = FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols,
            ..Default::default()
        };
        let index = WorkspaceIndex::new();
        index.update_file("file:///test.php", file_symbols.clone());

        let ctx = CompletionContext::Namespace {
            prefix: "Ty".to_string(),
        };
        let items = provide_completions(&ctx, &index, &file_symbols);

        assert_eq!(
            items.first().map(|item| item.label.as_str()),
            Some("TypeGuess")
        );
        assert!(
            items.iter().any(|item| item.label == "TypeGuess"),
            "prefix match should survive namespace completion truncation"
        );
    }

    #[test]
    fn test_free_completion_ranks_prefix_matches_before_contains_matches() {
        let mut symbols = Vec::new();
        for idx in 0..120 {
            symbols.push(make_symbol(
                &format!("OtherTyNoise{idx:03}"),
                &format!("App\\OtherTyNoise{idx:03}"),
                PhpSymbolKind::Class,
                None,
                Visibility::Public,
                false,
            ));
        }
        symbols.push(make_symbol(
            "TypeGuess",
            "App\\TypeGuess",
            PhpSymbolKind::Class,
            None,
            Visibility::Public,
            false,
        ));
        let file_symbols = FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols,
            ..Default::default()
        };
        let index = WorkspaceIndex::new();
        index.update_file("file:///test.php", file_symbols.clone());

        let ctx = CompletionContext::Free {
            prefix: "Ty".to_string(),
        };
        let items = provide_completions(&ctx, &index, &file_symbols);

        assert_eq!(
            items.first().map(|item| item.label.as_str()),
            Some("TypeGuess")
        );
        assert!(
            items.iter().any(|item| item.label == "TypeGuess"),
            "prefix match should survive truncation"
        );
    }

    #[test]
    fn test_variable_completion() {
        let file_symbols = FileSymbols {
            namespace: None,
            use_statements: vec![],
            symbols: vec![SymbolInfo {
                name: "test".to_string(),
                fqn: "test".to_string(),
                kind: PhpSymbolKind::Function,
                uri: "file:///test.php".to_string(),
                range: (0, 0, 5, 0),
                selection_range: (0, 9, 0, 13),
                visibility: Visibility::Public,
                modifiers: SymbolModifiers::default(),
                attributes: vec![],
                doc_comment: None,
                signature: Some(Signature {
                    params: vec![ParamInfo {
                        name: "username".to_string(),
                        type_info: Some(TypeInfo::Simple("string".to_string())),
                        default_value: None,
                        is_variadic: false,
                        is_by_ref: false,
                        is_promoted: false,
                    }],
                    return_type: None,
                }),
                parent_fqn: None,
                extends: vec![],
                implements: vec![],
                traits: vec![],
                templates: vec![],
                template_bindings: vec![],
            }],
            ..Default::default()
        };
        let index = WorkspaceIndex::new();

        let ctx = CompletionContext::Variable {
            prefix: "user".to_string(),
        };
        let items = provide_completions(&ctx, &index, &file_symbols);
        assert!(
            items.iter().any(|i| i.label == "$username"),
            "Should find $username"
        );
    }

    #[test]
    fn test_member_completion_uses_inferred_class_fqn() {
        let file_symbols = FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![
                SymbolInfo {
                    name: "Baz".to_string(),
                    fqn: "App\\Test\\Baz".to_string(),
                    kind: PhpSymbolKind::Class,
                    uri: "file:///test.php".to_string(),
                    range: (0, 0, 10, 0),
                    selection_range: (0, 6, 0, 9),
                    visibility: Visibility::Public,
                    modifiers: SymbolModifiers::default(),
                    attributes: vec![],
                    doc_comment: None,
                    signature: None,
                    parent_fqn: None,
                    extends: vec![],
                    implements: vec![],
                    traits: vec![],
                    templates: vec![],
                    template_bindings: vec![],
                },
                SymbolInfo {
                    name: "test".to_string(),
                    fqn: "App\\Test\\Baz::test".to_string(),
                    kind: PhpSymbolKind::Method,
                    uri: "file:///test.php".to_string(),
                    range: (2, 4, 2, 20),
                    selection_range: (2, 13, 2, 17),
                    visibility: Visibility::Public,
                    modifiers: SymbolModifiers::default(),
                    attributes: vec![],
                    doc_comment: None,
                    signature: Some(Signature {
                        params: vec![],
                        return_type: None,
                    }),
                    parent_fqn: Some("App\\Test\\Baz".to_string()),
                    extends: vec![],
                    implements: vec![],
                    traits: vec![],
                    templates: vec![],
                    template_bindings: vec![],
                },
            ],
            ..Default::default()
        };

        let index = WorkspaceIndex::new();
        index.update_file("file:///test.php", file_symbols.clone());

        let ctx = CompletionContext::MemberAccess {
            object_expr: "$baz2".to_string(),
            member_prefix: String::new(),
            class_fqn: Some("App\\Test\\Baz".to_string()),
            access_mode: MemberAccessMode::Read,
        };
        let items = provide_completions(&ctx, &index, &file_symbols);

        assert!(
            items.iter().any(|i| i.label == "test"),
            "Should include members of inferred class"
        );
    }

    #[test]
    fn test_member_completion_filters_static_and_visibility() {
        let file_symbols = FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![
                make_symbol(
                    "Service",
                    "App\\Service",
                    PhpSymbolKind::Class,
                    None,
                    Visibility::Public,
                    false,
                ),
                make_symbol(
                    "name",
                    "App\\Service::$name",
                    PhpSymbolKind::Property,
                    Some("App\\Service"),
                    Visibility::Public,
                    false,
                ),
                make_symbol(
                    "secret",
                    "App\\Service::$secret",
                    PhpSymbolKind::Property,
                    Some("App\\Service"),
                    Visibility::Private,
                    false,
                ),
                make_symbol(
                    "create",
                    "App\\Service::create",
                    PhpSymbolKind::Method,
                    Some("App\\Service"),
                    Visibility::Public,
                    true,
                ),
            ],
            ..Default::default()
        };
        let index = WorkspaceIndex::new();
        index.update_file("file:///test.php", file_symbols.clone());

        let ctx = CompletionContext::MemberAccess {
            object_expr: "$service".to_string(),
            member_prefix: String::new(),
            class_fqn: Some("App\\Service".to_string()),
            access_mode: MemberAccessMode::Read,
        };
        let items = provide_completions(&ctx, &index, &file_symbols);
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();

        assert!(labels.contains(&"name"));
        assert!(
            !labels.contains(&"$name"),
            "instance property should omit `$`"
        );
        assert!(
            !labels.contains(&"secret"),
            "external private member should be hidden"
        );
        assert!(
            !labels.contains(&"create"),
            "static method should be hidden on `->`"
        );
    }

    #[test]
    fn test_member_completion_sorts_methods_before_properties() {
        let file_symbols = FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![
                make_symbol(
                    "Client",
                    "App\\Client",
                    PhpSymbolKind::Class,
                    None,
                    Visibility::Public,
                    false,
                ),
                make_symbol(
                    "requestHeaders",
                    "App\\Client::$requestHeaders",
                    PhpSymbolKind::Property,
                    Some("App\\Client"),
                    Visibility::Public,
                    false,
                ),
                make_symbol(
                    "getRequest",
                    "App\\Client::getRequest",
                    PhpSymbolKind::Method,
                    Some("App\\Client"),
                    Visibility::Public,
                    false,
                ),
                make_symbol(
                    "request",
                    "App\\Client::request",
                    PhpSymbolKind::Method,
                    Some("App\\Client"),
                    Visibility::Public,
                    false,
                ),
            ],
            ..Default::default()
        };
        let index = WorkspaceIndex::new();
        index.update_file("file:///test.php", file_symbols.clone());

        let ctx = CompletionContext::MemberAccess {
            object_expr: "$client".to_string(),
            member_prefix: "reques".to_string(),
            class_fqn: Some("App\\Client".to_string()),
            access_mode: MemberAccessMode::Read,
        };
        let items = provide_completions(&ctx, &index, &file_symbols);
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();

        assert_eq!(labels.first().copied(), Some("request"));
        assert!(
            labels.iter().position(|label| *label == "request").unwrap()
                < labels
                    .iter()
                    .position(|label| *label == "requestHeaders")
                    .unwrap(),
            "methods should sort before properties in member completion"
        );
        assert!(
            labels.iter().position(|label| *label == "request").unwrap()
                < labels
                    .iter()
                    .position(|label| *label == "getRequest")
                    .unwrap(),
            "members starting with typed prefix should sort before substring matches"
        );
    }

    #[test]
    fn test_member_completion_includes_phpdoc_virtual_members() {
        let mut service = make_symbol(
            "Service",
            "App\\Service",
            PhpSymbolKind::Class,
            None,
            Visibility::Public,
            false,
        );
        service.doc_comment = Some(
            "/**\n * @property-read string $slug Service slug\n * @method User owner()\n */"
                .to_string(),
        );
        let file_symbols = FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![service],
            ..Default::default()
        };
        let index = WorkspaceIndex::new();
        index.update_file("file:///test.php", file_symbols.clone());

        let ctx = CompletionContext::MemberAccess {
            object_expr: "$service".to_string(),
            member_prefix: String::new(),
            class_fqn: Some("App\\Service".to_string()),
            access_mode: MemberAccessMode::Read,
        };
        let items = provide_completions(&ctx, &index, &file_symbols);

        let slug = items
            .iter()
            .find(|item| item.label == "slug")
            .expect("virtual property completion");
        assert_eq!(slug.kind, Some(CompletionItemKind::PROPERTY));
        assert_eq!(slug.detail.as_deref(), Some("@property-read string"));
        assert!(
            slug.data
                .as_ref()
                .and_then(|data| data.get("kind"))
                .and_then(|kind| kind.as_str())
                == Some("phpdoc-virtual-member")
        );

        let owner = items
            .iter()
            .find(|item| item.label == "owner")
            .expect("virtual method completion");
        assert_eq!(owner.kind, Some(CompletionItemKind::METHOD));
        assert_eq!(owner.detail.as_deref(), Some("(): User"));
    }

    #[test]
    fn test_member_completion_filters_phpdoc_properties_by_access_mode() {
        let mut service = make_symbol(
            "Service",
            "App\\Service",
            PhpSymbolKind::Class,
            None,
            Visibility::Public,
            false,
        );
        service.doc_comment = Some(
            "/**\n * @property-read int $version\n * @property-write bool $dirty\n * @property string $label\n */"
                .to_string(),
        );
        let file_symbols = FileSymbols {
            symbols: vec![service],
            ..Default::default()
        };
        let index = WorkspaceIndex::new();
        index.update_file("file:///test.php", file_symbols.clone());

        let read_ctx = CompletionContext::MemberAccess {
            object_expr: "$service".to_string(),
            member_prefix: String::new(),
            class_fqn: Some("App\\Service".to_string()),
            access_mode: MemberAccessMode::Read,
        };
        let read_items = provide_completions(&read_ctx, &index, &file_symbols);
        let read_labels: Vec<&str> = read_items.iter().map(|item| item.label.as_str()).collect();
        assert!(read_labels.contains(&"version"));
        assert!(read_labels.contains(&"label"));
        assert!(
            !read_labels.contains(&"dirty"),
            "write-only virtual properties should be hidden in read completion"
        );

        let write_ctx = CompletionContext::MemberAccess {
            object_expr: "$service".to_string(),
            member_prefix: String::new(),
            class_fqn: Some("App\\Service".to_string()),
            access_mode: MemberAccessMode::Write,
        };
        let write_items = provide_completions(&write_ctx, &index, &file_symbols);
        let write_labels: Vec<&str> = write_items.iter().map(|item| item.label.as_str()).collect();
        assert!(write_labels.contains(&"dirty"));
        assert!(write_labels.contains(&"label"));
        assert!(
            !write_labels.contains(&"version"),
            "read-only virtual properties should be hidden in write completion"
        );
    }

    #[test]
    fn test_member_completion_inherits_phpdoc_virtual_members() {
        let mut base = make_symbol(
            "BaseService",
            "App\\BaseService",
            PhpSymbolKind::Class,
            None,
            Visibility::Public,
            false,
        );
        base.doc_comment = Some("/**\n * @property int $id\n */".to_string());
        let mut service = make_symbol(
            "Service",
            "App\\Service",
            PhpSymbolKind::Class,
            None,
            Visibility::Public,
            false,
        );
        service.extends = vec!["App\\BaseService".to_string()];
        let file_symbols = FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![base, service],
            ..Default::default()
        };
        let index = WorkspaceIndex::new();
        index.update_file("file:///test.php", file_symbols.clone());

        let ctx = CompletionContext::MemberAccess {
            object_expr: "$service".to_string(),
            member_prefix: String::new(),
            class_fqn: Some("App\\Service".to_string()),
            access_mode: MemberAccessMode::Read,
        };
        let items = provide_completions(&ctx, &index, &file_symbols);

        assert!(items.iter().any(|item| item.label == "id"));
    }

    #[test]
    fn test_static_completion_filters_instance_members() {
        let file_symbols = FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![
                make_symbol(
                    "Service",
                    "App\\Service",
                    PhpSymbolKind::Class,
                    None,
                    Visibility::Public,
                    false,
                ),
                make_symbol(
                    "run",
                    "App\\Service::run",
                    PhpSymbolKind::Method,
                    Some("App\\Service"),
                    Visibility::Public,
                    false,
                ),
                make_symbol(
                    "create",
                    "App\\Service::create",
                    PhpSymbolKind::Method,
                    Some("App\\Service"),
                    Visibility::Public,
                    true,
                ),
                make_symbol(
                    "counter",
                    "App\\Service::$counter",
                    PhpSymbolKind::Property,
                    Some("App\\Service"),
                    Visibility::Public,
                    true,
                ),
            ],
            ..Default::default()
        };
        let index = WorkspaceIndex::new();
        index.update_file("file:///test.php", file_symbols.clone());

        let ctx = CompletionContext::StaticAccess {
            class_expr: "Service".to_string(),
            member_prefix: String::new(),
            class_fqn: "App\\Service".to_string(),
        };
        let items = provide_completions(&ctx, &index, &file_symbols);
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();

        assert!(labels.contains(&"create"));
        assert!(labels.contains(&"$counter"));
        assert!(
            !labels.contains(&"run"),
            "instance method should be hidden on `::`"
        );
    }

    #[test]
    fn test_static_completion_includes_static_phpdoc_virtual_methods() {
        let mut service = make_symbol(
            "Service",
            "App\\Service",
            PhpSymbolKind::Class,
            None,
            Visibility::Public,
            false,
        );
        service.doc_comment =
            Some("/**\n * @method User owner()\n * @method static self make()\n */".to_string());
        let file_symbols = FileSymbols {
            symbols: vec![service],
            ..Default::default()
        };
        let index = WorkspaceIndex::new();
        index.update_file("file:///test.php", file_symbols.clone());

        let ctx = CompletionContext::StaticAccess {
            class_expr: "Service".to_string(),
            member_prefix: String::new(),
            class_fqn: "App\\Service".to_string(),
        };
        let items = provide_completions(&ctx, &index, &file_symbols);
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();

        assert!(labels.contains(&"make"));
        assert!(
            !labels.contains(&"owner"),
            "instance @method should not appear in static completion"
        );
    }

    #[test]
    fn test_static_completion_includes_class_pseudo_constant() {
        let file_symbols = FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![make_symbol(
                "Service",
                "App\\Service",
                PhpSymbolKind::Class,
                None,
                Visibility::Public,
                false,
            )],
            ..Default::default()
        };
        let index = WorkspaceIndex::new();
        index.update_file("file:///test.php", file_symbols.clone());

        let ctx = CompletionContext::StaticAccess {
            class_expr: "Service".to_string(),
            member_prefix: String::new(),
            class_fqn: "App\\Service".to_string(),
        };
        let items = provide_completions(&ctx, &index, &file_symbols);
        let class_item = items
            .iter()
            .find(|item| item.label == "class")
            .expect("static completion should include ::class");

        assert_eq!(class_item.kind, Some(CompletionItemKind::CONSTANT));
    }

    #[test]
    fn test_parent_static_completion_includes_instance_methods() {
        let file_symbols = FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![
                make_symbol(
                    "Base",
                    "App\\Base",
                    PhpSymbolKind::Class,
                    None,
                    Visibility::Public,
                    false,
                ),
                make_symbol(
                    "setUp",
                    "App\\Base::setUp",
                    PhpSymbolKind::Method,
                    Some("App\\Base"),
                    Visibility::Protected,
                    false,
                ),
                make_symbol(
                    "create",
                    "App\\Base::create",
                    PhpSymbolKind::Method,
                    Some("App\\Base"),
                    Visibility::Public,
                    true,
                ),
            ],
            ..Default::default()
        };
        let index = WorkspaceIndex::new();
        index.update_file("file:///test.php", file_symbols.clone());

        let ctx = CompletionContext::StaticAccess {
            class_expr: "parent".to_string(),
            member_prefix: String::new(),
            class_fqn: "App\\Base".to_string(),
        };
        let items = provide_completions(&ctx, &index, &file_symbols);
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();

        assert!(
            labels.contains(&"setUp"),
            "`parent::` should complete inherited instance methods"
        );
        assert!(labels.contains(&"create"));
    }

    #[test]
    fn test_member_completion_uses_cursor_class_for_two_classes() {
        let base = with_range(
            make_symbol(
                "Base",
                "App\\Base",
                PhpSymbolKind::Class,
                None,
                Visibility::Public,
                false,
            ),
            (0, 0, 6, 1),
        );
        let base_secret = make_symbol(
            "baseSecret",
            "App\\Base::baseSecret",
            PhpSymbolKind::Method,
            Some("App\\Base"),
            Visibility::Private,
            false,
        );
        let mut child = with_range(
            make_symbol(
                "Child",
                "App\\Child",
                PhpSymbolKind::Class,
                None,
                Visibility::Public,
                false,
            ),
            (8, 0, 14, 1),
        );
        child.extends = vec!["App\\Base".to_string()];
        let child_secret = make_symbol(
            "childSecret",
            "App\\Child::childSecret",
            PhpSymbolKind::Method,
            Some("App\\Child"),
            Visibility::Private,
            false,
        );
        let file_symbols = FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![base, base_secret, child, child_secret],
            ..Default::default()
        };
        let index = WorkspaceIndex::new();
        index.update_file("file:///test.php", file_symbols.clone());

        let ctx = CompletionContext::MemberAccess {
            object_expr: "$this".to_string(),
            member_prefix: String::new(),
            class_fqn: Some("App\\Child".to_string()),
            access_mode: MemberAccessMode::Read,
        };
        let items = provide_completions_at_range(&ctx, &index, &file_symbols, (10, 12, 10, 12));
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();

        assert!(
            labels.contains(&"childSecret"),
            "$this-> should include private members from the cursor class"
        );
        assert!(
            !labels.contains(&"baseSecret"),
            "$this-> should not expose private members from another class"
        );
    }

    #[test]
    fn test_member_completion_uses_cursor_trait_context() {
        let other = with_range(
            make_symbol(
                "Other",
                "App\\Other",
                PhpSymbolKind::Class,
                None,
                Visibility::Public,
                false,
            ),
            (0, 0, 4, 1),
        );
        let feature = with_range(
            make_symbol(
                "Feature",
                "App\\Feature",
                PhpSymbolKind::Trait,
                None,
                Visibility::Public,
                false,
            ),
            (6, 0, 12, 1),
        );
        let trait_secret = make_symbol(
            "traitSecret",
            "App\\Feature::traitSecret",
            PhpSymbolKind::Method,
            Some("App\\Feature"),
            Visibility::Private,
            false,
        );
        let file_symbols = FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![other, feature, trait_secret],
            ..Default::default()
        };
        let index = WorkspaceIndex::new();
        index.update_file("file:///test.php", file_symbols.clone());

        let ctx = CompletionContext::MemberAccess {
            object_expr: "$this".to_string(),
            member_prefix: String::new(),
            class_fqn: Some("App\\Feature".to_string()),
            access_mode: MemberAccessMode::Read,
        };
        let items = provide_completions_at_range(&ctx, &index, &file_symbols, (8, 12, 8, 12));
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();

        assert!(
            labels.contains(&"traitSecret"),
            "$this-> should use the trait at the cursor for private visibility"
        );
    }

    #[test]
    fn test_member_completion_uses_innermost_anonymous_class_context() {
        let outer = with_range(
            make_symbol(
                "Outer",
                "App\\Outer",
                PhpSymbolKind::Class,
                None,
                Visibility::Public,
                false,
            ),
            (0, 0, 24, 1),
        );
        let outer_secret = make_symbol(
            "outerSecret",
            "App\\Outer::outerSecret",
            PhpSymbolKind::Method,
            Some("App\\Outer"),
            Visibility::Private,
            false,
        );
        let anonymous_fqn = "App\\Outer@anonymous:8";
        let mut anonymous = with_range(
            make_symbol(
                "anonymous",
                anonymous_fqn,
                PhpSymbolKind::Class,
                None,
                Visibility::Public,
                false,
            ),
            (8, 8, 16, 9),
        );
        anonymous.extends = vec!["App\\Outer".to_string()];
        let anonymous_secret = make_symbol(
            "anonymousSecret",
            "App\\Outer@anonymous:8::anonymousSecret",
            PhpSymbolKind::Method,
            Some(anonymous_fqn),
            Visibility::Private,
            false,
        );
        let file_symbols = FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![outer, outer_secret, anonymous, anonymous_secret],
            ..Default::default()
        };
        let index = WorkspaceIndex::new();
        index.update_file("file:///test.php", file_symbols.clone());

        let ctx = CompletionContext::MemberAccess {
            object_expr: "$this".to_string(),
            member_prefix: String::new(),
            class_fqn: Some(anonymous_fqn.to_string()),
            access_mode: MemberAccessMode::Read,
        };
        let items = provide_completions_at_range(&ctx, &index, &file_symbols, (12, 16, 12, 16));
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();

        assert!(
            labels.contains(&"anonymousSecret"),
            "anonymous class private members should be visible inside that class"
        );
        assert!(
            !labels.contains(&"outerSecret"),
            "outer class private members should not leak into an anonymous class"
        );
    }

    #[test]
    fn test_static_completion_uses_cursor_class_for_self_static_and_parent() {
        let base = with_range(
            make_symbol(
                "Base",
                "App\\Base",
                PhpSymbolKind::Class,
                None,
                Visibility::Public,
                false,
            ),
            (0, 0, 6, 1),
        );
        let base_private = make_symbol(
            "basePrivate",
            "App\\Base::basePrivate",
            PhpSymbolKind::Method,
            Some("App\\Base"),
            Visibility::Private,
            true,
        );
        let base_protected = make_symbol(
            "baseProtected",
            "App\\Base::baseProtected",
            PhpSymbolKind::Method,
            Some("App\\Base"),
            Visibility::Protected,
            false,
        );
        let mut child = with_range(
            make_symbol(
                "Child",
                "App\\Child",
                PhpSymbolKind::Class,
                None,
                Visibility::Public,
                false,
            ),
            (8, 0, 14, 1),
        );
        child.extends = vec!["App\\Base".to_string()];
        let child_private = make_symbol(
            "childPrivate",
            "App\\Child::childPrivate",
            PhpSymbolKind::Method,
            Some("App\\Child"),
            Visibility::Private,
            true,
        );
        let file_symbols = FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![base, base_private, base_protected, child, child_private],
            ..Default::default()
        };
        let index = WorkspaceIndex::new();
        index.update_file("file:///test.php", file_symbols.clone());

        for class_expr in ["self", "static"] {
            let ctx = CompletionContext::StaticAccess {
                class_expr: class_expr.to_string(),
                member_prefix: String::new(),
                class_fqn: "App\\Child".to_string(),
            };
            let items = provide_completions_at_range(&ctx, &index, &file_symbols, (10, 12, 10, 12));
            let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();

            assert!(
                labels.contains(&"childPrivate"),
                "{class_expr}:: should include private members from the cursor class"
            );
            assert!(
                !labels.contains(&"basePrivate"),
                "{class_expr}:: should not expose private members from another class"
            );
        }

        let ctx = CompletionContext::StaticAccess {
            class_expr: "parent".to_string(),
            member_prefix: String::new(),
            class_fqn: "App\\Base".to_string(),
        };
        let items = provide_completions_at_range(&ctx, &index, &file_symbols, (10, 12, 10, 12));
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();

        assert!(
            labels.contains(&"baseProtected"),
            "parent:: should include protected parent instance methods"
        );
        assert!(
            !labels.contains(&"basePrivate"),
            "parent:: should not expose private parent members"
        );
    }
}
