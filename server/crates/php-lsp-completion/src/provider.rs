//! Completion item providers.
//!
//! Given a completion context and the workspace index, provides relevant
//! completion items.

use crate::context::CompletionContext;
use lsp_types::CompletionItem;
use lsp_types::CompletionItemKind;
use php_lsp_index::workspace::WorkspaceIndex;
use php_lsp_types::{FileSymbols, PhpSymbolKind};

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
        } => provide_member_completions(object_expr, class_fqn.as_deref(), index, file_symbols),
        CompletionContext::StaticAccess {
            class_fqn,
            class_expr,
        } => provide_static_completions(class_fqn, class_expr, index),
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

            items.push(symbol_to_completion_item(&member, false));
        }
    }

    items
}

/// Provide static access completions (`::`).
fn provide_static_completions(
    class_fqn: &str,
    class_expr: &str,
    index: &WorkspaceIndex,
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
        items.push(symbol_to_completion_item(&member, true));
    }

    // Also add class constants and enum cases
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
                        ..Default::default()
                    });
                    seen.insert(var_name);
                }
            }
        }
    }

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
                ..Default::default()
            });
        }
    }

    // Limit results
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
            items.push(CompletionItem {
                label: keyword.to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                ..Default::default()
            });
        }
    }

    // Add matching types
    let results = index.search(prefix);
    for sym in results.iter().take(50) {
        items.push(CompletionItem {
            label: sym.name.clone(),
            kind: Some(symbol_kind_to_completion_kind(sym.kind)),
            detail: Some(sym.fqn.clone()),
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
                ..Default::default()
            });
        }
    }

    // Limit
    items.truncate(100);
    items
}

/// Convert a SymbolInfo to a CompletionItem.
fn symbol_to_completion_item(sym: &php_lsp_types::SymbolInfo, _is_static: bool) -> CompletionItem {
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

    let label = if sym.kind == PhpSymbolKind::Property && !sym.name.starts_with('$') {
        format!("${}", sym.name)
    } else {
        sym.name.clone()
    };

    let mut tags = Vec::new();
    if sym.modifiers.is_deprecated {
        tags.push(lsp_types::CompletionItemTag::DEPRECATED);
    }

    CompletionItem {
        label,
        kind: Some(kind),
        detail,
        tags: if tags.is_empty() { None } else { Some(tags) },
        // Store FQN in data for resolve
        data: Some(serde_json::Value::String(sym.fqn.clone())),
        ..Default::default()
    }
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
                },
            ],
        };

        let index = WorkspaceIndex::new();
        index.update_file("file:///test.php", file_symbols.clone());

        let ctx = CompletionContext::MemberAccess {
            object_expr: "$baz2".to_string(),
            class_fqn: Some("App\\Test\\Baz".to_string()),
        };
        let items = provide_completions(&ctx, &index, &file_symbols);

        assert!(
            items.iter().any(|i| i.label == "test"),
            "Should include members of inferred class"
        );
    }
}
