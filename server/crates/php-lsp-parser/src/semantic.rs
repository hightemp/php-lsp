//! Semantic diagnostics for PHP files.
//!
//! Walks the CST and checks class/function/use references
//! against a resolver function (typically backed by the workspace index).

use php_lsp_types::{FileSymbols, SymbolInfo, UseKind};
use std::sync::Arc;
use tree_sitter::Tree;

/// A semantic diagnostic found in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticDiagnostic {
    /// Line/column range: (start_line, start_col, end_line, end_col).
    pub range: (u32, u32, u32, u32),
    /// Diagnostic message.
    pub message: String,
    /// Severity kind.
    pub kind: SemanticDiagnosticKind,
}

/// Kind of semantic diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticDiagnosticKind {
    /// Class/interface/trait/enum not found in index.
    UnknownClass,
    /// Function not found in index.
    UnknownFunction,
    /// Use statement references a symbol not found in index.
    UnresolvedUse,
    /// Wrong number of arguments in a call.
    ArgumentCountMismatch,
}

/// Names that should not be reported as unknown (PHP built-in types, special names).
const BUILTIN_TYPE_NAMES: &[&str] = &[
    "self", "static", "parent", "$this", "int", "float", "string", "bool", "array", "object",
    "null", "void", "never", "mixed", "callable", "iterable", "true", "false", "resource",
];

/// Extract semantic diagnostics from a file.
///
/// `resolver` is called with a FQN to look up a symbol in the index.
/// Returns `Some(SymbolInfo)` if the symbol is known, `None` if unknown.
pub fn extract_semantic_diagnostics<F>(
    tree: &Tree,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: F,
) -> Vec<SemanticDiagnostic>
where
    F: Fn(&str) -> Option<Arc<SymbolInfo>>,
{
    let mut diagnostics = Vec::new();
    let root = tree.root_node();

    // Check use statements
    check_use_statements(file_symbols, &resolver, &mut diagnostics);

    // Walk CST for class and function references
    walk_node_for_diagnostics(root, source, file_symbols, &resolver, &mut diagnostics);

    diagnostics
}

/// Check if use statements can be resolved.
fn check_use_statements<F>(
    file_symbols: &FileSymbols,
    resolver: &F,
    diagnostics: &mut Vec<SemanticDiagnostic>,
) where
    F: Fn(&str) -> Option<Arc<SymbolInfo>>,
{
    for use_stmt in &file_symbols.use_statements {
        // Only check class-type use statements
        if use_stmt.kind != UseKind::Class {
            continue;
        }

        let fqn = &use_stmt.fqn;

        // Skip PHP built-in names
        if BUILTIN_TYPE_NAMES.contains(&fqn.as_str()) {
            continue;
        }

        // Skip single-segment names (could be PHP built-in extensions)
        if !fqn.contains('\\') {
            continue;
        }

        if resolver(fqn).is_none() {
            diagnostics.push(SemanticDiagnostic {
                range: use_stmt.range,
                message: format!("Unresolved use statement: {}", fqn),
                kind: SemanticDiagnosticKind::UnresolvedUse,
            });
        }
    }
}

/// Recursively walk CST nodes to find class/function references.
fn walk_node_for_diagnostics<F>(
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: &F,
    diagnostics: &mut Vec<SemanticDiagnostic>,
) where
    F: Fn(&str) -> Option<Arc<SymbolInfo>>,
{
    let kind = node.kind();

    match kind {
        // new ClassName()
        "object_creation_expression" => {
            check_class_in_new(node, source, file_symbols, resolver, diagnostics);
        }
        // Type hints in function parameters, return types, property types
        "named_type" | "optional_type" => {
            check_type_reference(node, source, file_symbols, resolver, diagnostics);
        }
        // extends / implements clauses
        "base_clause" | "class_interface_clause" => {
            check_inheritance_clause(node, source, file_symbols, resolver, diagnostics);
        }
        // function_call_expression (free function calls)
        "function_call_expression" => {
            check_function_call(node, source, file_symbols, resolver, diagnostics);
        }
        _ => {}
    }

    // Recurse into children
    let child_count = node.child_count();
    for i in 0..child_count {
        if let Some(child) = node.child(i) {
            walk_node_for_diagnostics(child, source, file_symbols, resolver, diagnostics);
        }
    }
}

/// Check a class name in `new ClassName(...)`.
fn check_class_in_new<F>(
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: &F,
    diagnostics: &mut Vec<SemanticDiagnostic>,
) where
    F: Fn(&str) -> Option<Arc<SymbolInfo>>,
{
    // Find the class name child
    let mut class_fqn: Option<String> = None;
    let mut class_name_node: Option<tree_sitter::Node> = None;

    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            let ck = child.kind();
            if ck == "name" || ck == "qualified_name" {
                let name = &source[child.byte_range()];
                let fqn = resolve_class_name(name, file_symbols);

                if should_check_class(&fqn) && resolver(&fqn).is_none() {
                    diagnostics.push(SemanticDiagnostic {
                        range: node_range(&child),
                        message: format!("Unknown class: {}", fqn),
                        kind: SemanticDiagnosticKind::UnknownClass,
                    });
                }

                class_fqn = Some(fqn);
                class_name_node = Some(child);
                break;
            }
        }
    }

    // Check constructor argument count
    if let (Some(fqn), Some(_name_node)) = (class_fqn, class_name_node) {
        let ctor_fqn = format!("{}::__construct", fqn);
        if let Some(ctor_sym) = resolver(&ctor_fqn) {
            if let Some(ref sig) = ctor_sym.signature {
                let required = sig
                    .params
                    .iter()
                    .filter(|p| p.default_value.is_none() && !p.is_variadic)
                    .count();
                let max = if sig.params.iter().any(|p| p.is_variadic) {
                    usize::MAX
                } else {
                    sig.params.len()
                };

                // Count actual arguments
                let actual = count_arguments(node);

                if actual < required {
                    // Find the arguments node for better range
                    let args_node = node.child_by_field_name("arguments").unwrap_or(node);
                    diagnostics.push(SemanticDiagnostic {
                        range: node_range(&args_node),
                        message: format!(
                            "Too few arguments to {}::__construct(): expected at least {}, got {}",
                            fqn, required, actual
                        ),
                        kind: SemanticDiagnosticKind::ArgumentCountMismatch,
                    });
                } else if actual > max {
                    let args_node = node.child_by_field_name("arguments").unwrap_or(node);
                    diagnostics.push(SemanticDiagnostic {
                        range: node_range(&args_node),
                        message: format!(
                            "Too many arguments to {}::__construct(): expected at most {}, got {}",
                            fqn, max, actual
                        ),
                        kind: SemanticDiagnosticKind::ArgumentCountMismatch,
                    });
                }
            }
        }
    }
}

/// Check type references in type hints.
fn check_type_reference<F>(
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: &F,
    diagnostics: &mut Vec<SemanticDiagnostic>,
) where
    F: Fn(&str) -> Option<Arc<SymbolInfo>>,
{
    // For optional_type (?Type), drill into the child
    let target = if node.kind() == "optional_type" {
        node.named_child(0)
    } else {
        Some(node)
    };

    if let Some(target) = target {
        if target.kind() == "named_type" {
            // Get the name/qualified_name child
            for i in 0..target.named_child_count() {
                if let Some(child) = target.named_child(i) {
                    let ck = child.kind();
                    if ck == "name" || ck == "qualified_name" {
                        let name = &source[child.byte_range()];
                        let fqn = resolve_class_name(name, file_symbols);

                        if should_check_class(&fqn) && resolver(&fqn).is_none() {
                            diagnostics.push(SemanticDiagnostic {
                                range: node_range(&child),
                                message: format!("Unknown class: {}", fqn),
                                kind: SemanticDiagnosticKind::UnknownClass,
                            });
                        }
                        break;
                    }
                }
            }
        }
    }
}

/// Check class names in extends/implements clauses.
fn check_inheritance_clause<F>(
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: &F,
    diagnostics: &mut Vec<SemanticDiagnostic>,
) where
    F: Fn(&str) -> Option<Arc<SymbolInfo>>,
{
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            let ck = child.kind();
            if ck == "name" || ck == "qualified_name" {
                let name = &source[child.byte_range()];
                let fqn = resolve_class_name(name, file_symbols);

                if should_check_class(&fqn) && resolver(&fqn).is_none() {
                    diagnostics.push(SemanticDiagnostic {
                        range: node_range(&child),
                        message: format!("Unknown class: {}", fqn),
                        kind: SemanticDiagnosticKind::UnknownClass,
                    });
                }
            }
        }
    }
}

/// Check a free function call.
fn check_function_call<F>(
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: &F,
    diagnostics: &mut Vec<SemanticDiagnostic>,
) where
    F: Fn(&str) -> Option<Arc<SymbolInfo>>,
{
    // The function name is the first named child (name or qualified_name)
    if let Some(name_node) = node.named_child(0) {
        let nk = name_node.kind();
        if nk == "name" || nk == "qualified_name" || nk == "namespace_name" {
            let name = &source[name_node.byte_range()];

            // Skip PHP built-in functions by checking if name is simple and common
            // We only check namespaced function calls or functions that aren't in the index
            let fqn = resolve_function_name(name, file_symbols);

            // Resolve function symbol for argument count checks.
            // For simple names, try namespace-qualified fallback as well.
            let mut resolved: Option<(String, Arc<SymbolInfo>)> =
                resolver(&fqn).map(|sym| (fqn.clone(), sym));

            if resolved.is_none() && !fqn.contains('\\') {
                if let Some(ref ns) = file_symbols.namespace {
                    let ns_fqn = format!("{}\\{}", ns, fqn);
                    resolved = resolver(&ns_fqn).map(|sym| (ns_fqn, sym));
                }
            }

            if let Some((resolved_fqn, func_sym)) = resolved {
                if let Some(ref sig) = func_sym.signature {
                    let required = sig
                        .params
                        .iter()
                        .filter(|p| p.default_value.is_none() && !p.is_variadic)
                        .count();
                    let max = if sig.params.iter().any(|p| p.is_variadic) {
                        usize::MAX
                    } else {
                        sig.params.len()
                    };
                    let actual = count_arguments(node);

                    if actual < required {
                        let args_node = node.child_by_field_name("arguments").unwrap_or(node);
                        diagnostics.push(SemanticDiagnostic {
                            range: node_range(&args_node),
                            message: format!(
                                "Too few arguments to {}(): expected at least {}, got {}",
                                resolved_fqn, required, actual
                            ),
                            kind: SemanticDiagnosticKind::ArgumentCountMismatch,
                        });
                    } else if actual > max {
                        let args_node = node.child_by_field_name("arguments").unwrap_or(node);
                        diagnostics.push(SemanticDiagnostic {
                            range: node_range(&args_node),
                            message: format!(
                                "Too many arguments to {}(): expected at most {}, got {}",
                                resolved_fqn, max, actual
                            ),
                            kind: SemanticDiagnosticKind::ArgumentCountMismatch,
                        });
                    }
                }
            }

            // Don't flag simple function names — too many PHP built-ins
            // Only flag namespaced function calls that can't be resolved
            if fqn.contains('\\') && resolver(&fqn).is_none() {
                diagnostics.push(SemanticDiagnostic {
                    range: node_range(&name_node),
                    message: format!("Unknown function: {}", fqn),
                    kind: SemanticDiagnosticKind::UnknownFunction,
                });
            }
        }
    }
}

/// Whether we should check a class name against the index.
fn should_check_class(fqn: &str) -> bool {
    // Skip built-in type names
    let lower = fqn.to_lowercase();
    if BUILTIN_TYPE_NAMES.contains(&lower.as_str()) {
        return false;
    }

    // Skip single-word names that look like PHP built-in types
    if !fqn.contains('\\') {
        // Common PHP built-in classes we skip (too many false positives)
        return false;
    }

    true
}

/// Resolve a class name to FQN using use statements and namespace.
fn resolve_class_name(name: &str, file_symbols: &FileSymbols) -> String {
    // Already fully qualified
    if name.starts_with('\\') {
        return name.trim_start_matches('\\').to_string();
    }

    // Special names
    if BUILTIN_TYPE_NAMES.contains(&name) {
        return name.to_string();
    }

    // Try to resolve via use statements
    let parts: Vec<&str> = name.split('\\').collect();
    let first_part = parts[0];

    for use_stmt in &file_symbols.use_statements {
        if use_stmt.kind != UseKind::Class {
            continue;
        }

        let alias = use_stmt
            .alias
            .as_deref()
            .unwrap_or_else(|| use_stmt.fqn.rsplit('\\').next().unwrap_or(&use_stmt.fqn));

        if alias == first_part {
            if parts.len() == 1 {
                return use_stmt.fqn.clone();
            } else {
                let rest = parts[1..].join("\\");
                return format!("{}\\{}", use_stmt.fqn, rest);
            }
        }
    }

    // Prepend current namespace
    if let Some(ref ns) = file_symbols.namespace {
        format!("{}\\{}", ns, name)
    } else {
        name.to_string()
    }
}

/// Resolve a function name to FQN.
fn resolve_function_name(name: &str, file_symbols: &FileSymbols) -> String {
    // Fully qualified
    if name.starts_with('\\') {
        return name.trim_start_matches('\\').to_string();
    }

    // Try use statements for functions
    let parts: Vec<&str> = name.split('\\').collect();
    let first_part = parts[0];

    for use_stmt in &file_symbols.use_statements {
        if use_stmt.kind != UseKind::Function {
            continue;
        }

        let alias = use_stmt
            .alias
            .as_deref()
            .unwrap_or_else(|| use_stmt.fqn.rsplit('\\').next().unwrap_or(&use_stmt.fqn));

        if alias == first_part {
            if parts.len() == 1 {
                return use_stmt.fqn.clone();
            } else {
                let rest = parts[1..].join("\\");
                return format!("{}\\{}", use_stmt.fqn, rest);
            }
        }
    }

    // If name is qualified (has \), prepend namespace
    if name.contains('\\') {
        if let Some(ref ns) = file_symbols.namespace {
            return format!("{}\\{}", ns, name);
        }
    }

    // Simple name — could be a global PHP function, don't resolve
    name.to_string()
}

/// Count the number of actual arguments in an `object_creation_expression` or similar call node.
fn count_arguments(node: tree_sitter::Node) -> usize {
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            if child.kind() == "arguments" {
                // Count direct named children that are "argument"
                let mut count = 0;
                for j in 0..child.named_child_count() {
                    if let Some(arg) = child.named_child(j) {
                        if arg.kind() == "argument" {
                            count += 1;
                        }
                    }
                }
                return count;
            }
        }
    }
    0
}

/// Get range tuple from a node.
fn node_range(node: &tree_sitter::Node) -> (u32, u32, u32, u32) {
    let sp = node.start_position();
    let ep = node.end_position();
    (
        sp.row as u32,
        sp.column as u32,
        ep.row as u32,
        ep.column as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::FileParser;
    use crate::symbols::extract_file_symbols;
    use php_lsp_types::{ParamInfo, PhpSymbolKind, Signature};

    fn dummy_symbol() -> Arc<SymbolInfo> {
        Arc::new(SymbolInfo {
            name: String::new(),
            kind: PhpSymbolKind::Class,
            fqn: String::new(),
            range: (0, 0, 0, 0),
            selection_range: (0, 0, 0, 0),
            uri: String::new(),
            visibility: php_lsp_types::Visibility::Public,
            modifiers: Default::default(),
            doc_comment: None,
            signature: None,
            parent_fqn: None,
        })
    }

    fn function_symbol(fqn: &str, params: Vec<ParamInfo>) -> Arc<SymbolInfo> {
        Arc::new(SymbolInfo {
            name: fqn.rsplit('\\').next().unwrap_or(fqn).to_string(),
            kind: PhpSymbolKind::Function,
            fqn: fqn.to_string(),
            range: (0, 0, 0, 0),
            selection_range: (0, 0, 0, 0),
            uri: String::new(),
            visibility: php_lsp_types::Visibility::Public,
            modifiers: Default::default(),
            doc_comment: None,
            signature: Some(Signature {
                params,
                return_type: None,
            }),
            parent_fqn: None,
        })
    }

    fn parse_and_check(
        code: &str,
        resolver: impl Fn(&str) -> Option<Arc<SymbolInfo>>,
    ) -> Vec<SemanticDiagnostic> {
        let mut parser = FileParser::new();
        parser.parse_full(code);
        let tree = parser.tree().unwrap();
        let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
        extract_semantic_diagnostics(tree, code, &file_symbols, resolver)
    }

    #[test]
    fn test_unknown_class_in_new() {
        let code = r#"<?php
namespace App;

use App\Service\UserService;

$x = new UserService();
$y = new UnknownClass();
"#;
        // UserService is known, UnknownClass is unknown
        let diags = parse_and_check(code, |fqn| {
            if fqn == "App\\Service\\UserService" {
                Some(dummy_symbol())
            } else {
                None
            }
        });

        let unknown: Vec<_> = diags
            .iter()
            .filter(|d| d.kind == SemanticDiagnosticKind::UnknownClass)
            .collect();

        assert!(
            unknown.iter().any(|d| d.message.contains("UnknownClass")),
            "Expected unknown class diagnostic for UnknownClass, got: {:?}",
            unknown
        );

        // UserService should not be flagged
        assert!(
            !unknown.iter().any(|d| d.message.contains("UserService")),
            "UserService should be resolved, got: {:?}",
            unknown
        );
    }

    #[test]
    fn test_unresolved_use() {
        let code = r#"<?php
namespace App;

use App\Service\UserService;
use App\Missing\SomeClass;
"#;
        let diags = parse_and_check(code, |fqn| {
            if fqn == "App\\Service\\UserService" {
                Some(dummy_symbol())
            } else {
                None
            }
        });

        let unresolved: Vec<_> = diags
            .iter()
            .filter(|d| d.kind == SemanticDiagnosticKind::UnresolvedUse)
            .collect();

        assert_eq!(
            unresolved.len(),
            1,
            "Expected 1 unresolved use, got: {:?}",
            unresolved
        );
        assert!(unresolved[0].message.contains("App\\Missing\\SomeClass"));
    }

    #[test]
    fn test_unknown_namespaced_function() {
        let code = r#"<?php
namespace App;

App\Utils\helper();
"#;
        let diags = parse_and_check(code, |_fqn| None);

        let unknown_funcs: Vec<_> = diags
            .iter()
            .filter(|d| d.kind == SemanticDiagnosticKind::UnknownFunction)
            .collect();

        // Should flag App\Utils\helper as unknown since it's namespaced
        assert!(
            !unknown_funcs.is_empty(),
            "Expected unknown function diagnostic for namespaced call"
        );
    }

    #[test]
    fn test_no_false_positives_for_builtins() {
        let code = r#"<?php
$x = new \stdClass();
strlen("hello");
array_map(fn($x) => $x, []);
"#;
        // All symbols are known (built-in)
        let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

        assert!(
            diags.is_empty(),
            "Should have no diagnostics for built-in usage, got: {:?}",
            diags
        );
    }

    #[test]
    fn test_function_argument_count_mismatch_too_few() {
        let code = r#"<?php
namespace App;

function helper(string $a, string $b): void {}
helper();
"#;
        let diags = parse_and_check(code, |fqn| {
            if fqn == "App\\helper" {
                Some(function_symbol(
                    fqn,
                    vec![
                        ParamInfo {
                            name: "a".to_string(),
                            type_info: None,
                            default_value: None,
                            is_variadic: false,
                            is_by_ref: false,
                            is_promoted: false,
                        },
                        ParamInfo {
                            name: "b".to_string(),
                            type_info: None,
                            default_value: None,
                            is_variadic: false,
                            is_by_ref: false,
                            is_promoted: false,
                        },
                    ],
                ))
            } else {
                None
            }
        });

        let arg_diags: Vec<_> = diags
            .iter()
            .filter(|d| d.kind == SemanticDiagnosticKind::ArgumentCountMismatch)
            .collect();

        assert!(
            arg_diags
                .iter()
                .any(|d| d.message.contains("Too few arguments to App\\helper()")),
            "Expected too-few-arguments diagnostic, got: {:?}",
            arg_diags
        );
    }

    #[test]
    fn test_function_argument_count_mismatch_too_many() {
        let code = r#"<?php
namespace App;

function helper(string $a): void {}
helper("x", "y");
"#;
        let diags = parse_and_check(code, |fqn| {
            if fqn == "App\\helper" {
                Some(function_symbol(
                    fqn,
                    vec![ParamInfo {
                        name: "a".to_string(),
                        type_info: None,
                        default_value: None,
                        is_variadic: false,
                        is_by_ref: false,
                        is_promoted: false,
                    }],
                ))
            } else {
                None
            }
        });

        let arg_diags: Vec<_> = diags
            .iter()
            .filter(|d| d.kind == SemanticDiagnosticKind::ArgumentCountMismatch)
            .collect();

        assert!(
            arg_diags
                .iter()
                .any(|d| d.message.contains("Too many arguments to App\\helper()")),
            "Expected too-many-arguments diagnostic, got: {:?}",
            arg_diags
        );
    }
}
