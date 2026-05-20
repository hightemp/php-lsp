//! Completion item providers.
//!
//! Given a completion context and the workspace index, provides relevant
//! completion items.

use crate::context::CompletionContext;
use lsp_types::{CompletionItem, CompletionItemKind, InsertTextFormat};
use php_lsp_index::workspace::WorkspaceIndex;
use php_lsp_types::{FileSymbols, PhpSymbolKind, SymbolInfo, Visibility};

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
    match context {
        CompletionContext::MemberAccess {
            object_expr,
            class_fqn,
            member_prefix,
        } => provide_member_completions(
            object_expr,
            member_prefix,
            class_fqn.as_deref(),
            index,
            file_symbols,
        ),
        CompletionContext::StaticAccess {
            class_fqn,
            class_expr,
            member_prefix,
        } => provide_static_completions(class_fqn, class_expr, member_prefix, index, file_symbols),
        CompletionContext::Variable { prefix } => {
            provide_variable_completions(prefix, file_symbols)
        }
        CompletionContext::Namespace { prefix } => provide_namespace_completions(prefix, index),
        CompletionContext::UseStatement { prefix } => provide_namespace_completions(prefix, index),
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
    file_symbols: &FileSymbols,
) -> Vec<CompletionItem> {
    let mut items = Vec::new();

    // Try to resolve the type of the object expression
    // For `$this`, look up the current class
    let class_fqn = if let Some(fqn) = inferred_class_fqn {
        Some(fqn.to_string())
    } else if object_expr == "$this" {
        // Find the class we're inside
        find_current_class_fqn(file_symbols)
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
            if !member_is_visible(&member, object_expr == "$this", file_symbols) {
                continue;
            }

            items.push(symbol_to_completion_item(
                &member,
                false,
                Some(member_prefix),
            ));
        }
    }

    sort_completion_items(&mut items);
    items
}

/// Provide static access completions (`::`).
fn provide_static_completions(
    class_fqn: &str,
    class_expr: &str,
    member_prefix: &str,
    index: &WorkspaceIndex,
    file_symbols: &FileSymbols,
) -> Vec<CompletionItem> {
    let mut items = Vec::new();

    // Resolve the FQN
    let fqn = if class_expr == "self" || class_expr == "static" || class_expr == "parent" {
        // For self/static/parent, we'd need the current class context
        // For now, use the fqn as-is
        class_fqn.to_string()
    } else {
        class_fqn.to_string()
    };

    let members = index.get_members(&fqn);
    for member in members {
        if !member.modifiers.is_static
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
            file_symbols,
        ) {
            continue;
        }
        items.push(symbol_to_completion_item(
            &member,
            true,
            Some(member_prefix),
        ));
    }

    // Also add class constants and enum cases
    sort_completion_items(&mut items);
    items
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
    let mut items = Vec::new();

    // Search for types matching the prefix
    let prefix_lower = prefix.to_lowercase();

    for entry in index.types.iter() {
        let sym = entry.value();
        if sym.fqn.to_lowercase().contains(&prefix_lower)
            || sym.name.to_lowercase().starts_with(&prefix_lower)
        {
            items.push(CompletionItem {
                label: sym.name.clone(),
                kind: Some(symbol_kind_to_completion_kind(sym.kind)),
                detail: Some(sym.fqn.clone()),
                sort_text: Some(format!("0300_{}", sym.name.to_ascii_lowercase())),
                filter_text: Some(format!("{} {}", sym.name, sym.fqn)),
                data: Some(serde_json::Value::String(sym.fqn.clone())),
                ..Default::default()
            });
        }
    }

    // Limit results
    sort_completion_items(&mut items);
    items.truncate(100);
    items
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
    for sym in results.iter().take(50) {
        items.push(CompletionItem {
            label: sym.name.clone(),
            kind: Some(symbol_kind_to_completion_kind(sym.kind)),
            detail: Some(sym.fqn.clone()),
            sort_text: Some(format!("0300_{}", sym.name.to_ascii_lowercase())),
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
    file_symbols: &FileSymbols,
) -> bool {
    match member.visibility {
        Visibility::Public => true,
        Visibility::Protected => accessing_from_self,
        Visibility::Private => {
            accessing_from_self
                && member.parent_fqn.as_deref() == find_current_class_fqn(file_symbols).as_deref()
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
        }
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
                doc_comment: None,
                signature: None,
                parent_fqn: None,
                extends: vec![],
                implements: vec![],
                traits: vec![],
            }],
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
            }],
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
                    doc_comment: None,
                    signature: None,
                    parent_fqn: None,
                    extends: vec![],
                    implements: vec![],
                    traits: vec![],
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
                    doc_comment: None,
                    signature: Some(Signature {
                        params: vec![],
                        return_type: None,
                    }),
                    parent_fqn: Some("App\\Test\\Baz".to_string()),
                    extends: vec![],
                    implements: vec![],
                    traits: vec![],
                },
            ],
        };

        let index = WorkspaceIndex::new();
        index.update_file("file:///test.php", file_symbols.clone());

        let ctx = CompletionContext::MemberAccess {
            object_expr: "$baz2".to_string(),
            member_prefix: String::new(),
            class_fqn: Some("App\\Test\\Baz".to_string()),
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
        };
        let index = WorkspaceIndex::new();
        index.update_file("file:///test.php", file_symbols.clone());

        let ctx = CompletionContext::MemberAccess {
            object_expr: "$service".to_string(),
            member_prefix: String::new(),
            class_fqn: Some("App\\Service".to_string()),
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
        };
        let index = WorkspaceIndex::new();
        index.update_file("file:///test.php", file_symbols.clone());

        let ctx = CompletionContext::MemberAccess {
            object_expr: "$client".to_string(),
            member_prefix: "reques".to_string(),
            class_fqn: Some("App\\Client".to_string()),
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
}
