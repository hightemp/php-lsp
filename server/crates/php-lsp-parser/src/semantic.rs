//! Semantic diagnostics for PHP files.
//!
//! Walks the CST and checks class/function/use references
//! against a resolver function (typically backed by the workspace index).

use crate::cst::{
    ancestor_field_contains, has_ancestor_before_scope, is_by_ref_output_argument_variable,
    is_foreach_header_declared_variable, node_contains,
};
use php_lsp_types::{
    global_constant_fqn_key, FileSymbols, PhpDoc, PhpSymbolKind, SymbolInfo, TypeInfo, UseKind,
};
use std::collections::{HashMap, HashSet};
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
    /// Variable is read before it is declared in the current scope.
    UndefinedVariable,
    /// Imported symbol is not used in the file.
    UnusedImport,
    /// Local variable is declared but not read.
    UnusedVariable,
    /// Function/method parameter is declared but not read.
    UnusedParameter,
    /// Symbol is declared more than once in the same file.
    DuplicateSymbol,
}

/// Names that should not be reported as unknown (PHP built-in types, special names).
const BUILTIN_TYPE_NAMES: &[&str] = &[
    "self", "static", "parent", "$this", "int", "float", "string", "bool", "array", "object",
    "null", "void", "never", "mixed", "callable", "iterable", "true", "false", "resource",
];

/// PHP language constructs may use call-like syntax but are not resolvable
/// functions and should not produce unknown-function diagnostics.
const LANGUAGE_CONSTRUCT_CALLS: &[&str] =
    &["die", "empty", "eval", "exit", "isset", "print", "unset"];

const CLASS_LIKE_SYMBOL_KINDS: &[PhpSymbolKind] = &[
    PhpSymbolKind::Class,
    PhpSymbolKind::Interface,
    PhpSymbolKind::Trait,
    PhpSymbolKind::Enum,
];
const FUNCTION_SYMBOL_KINDS: &[PhpSymbolKind] = &[PhpSymbolKind::Function];
const METHOD_SYMBOL_KINDS: &[PhpSymbolKind] = &[PhpSymbolKind::Method];

fn resolve_symbol_matching_kinds<F>(
    resolver: &F,
    fqn: &str,
    expected_kinds: &[PhpSymbolKind],
) -> Option<Arc<SymbolInfo>>
where
    F: Fn(&str, &[PhpSymbolKind]) -> Option<Arc<SymbolInfo>>,
{
    resolver(fqn, expected_kinds).filter(|symbol| expected_kinds.contains(&symbol.kind))
}

/// Extract semantic diagnostics from a file.
///
/// `resolver` is called with a FQN and the acceptable symbol kinds.
/// Returns `Some(SymbolInfo)` if the symbol is known, `None` if unknown.
pub fn extract_semantic_diagnostics<F>(
    tree: &Tree,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: F,
) -> Vec<SemanticDiagnostic>
where
    F: Fn(&str, &[PhpSymbolKind]) -> Option<Arc<SymbolInfo>>,
{
    let mut diagnostics = Vec::new();
    let root = tree.root_node();

    // Check use statements
    check_use_statements(file_symbols, &resolver, &mut diagnostics);

    // Walk CST for class and function references
    walk_node_for_diagnostics(root, source, file_symbols, &resolver, &mut diagnostics);
    check_unused_imports(root, source, file_symbols, &mut diagnostics);
    check_variable_diagnostics(root, source, file_symbols, &resolver, &mut diagnostics);
    check_duplicate_symbols_in_file(file_symbols, &mut diagnostics);

    diagnostics
}

/// Check if use statements can be resolved.
fn check_use_statements<F>(
    file_symbols: &FileSymbols,
    resolver: &F,
    diagnostics: &mut Vec<SemanticDiagnostic>,
) where
    F: Fn(&str, &[PhpSymbolKind]) -> Option<Arc<SymbolInfo>>,
{
    for use_stmt in &file_symbols.use_statements {
        // Only check class-type use statements
        if use_stmt.kind != UseKind::Class {
            continue;
        }

        let fqn = &use_stmt.fqn;

        // Skip PHP built-in names
        if is_builtin_type_name(fqn) {
            continue;
        }

        // Skip single-segment names (could be PHP built-in extensions)
        if !fqn.contains('\\') {
            continue;
        }

        if resolve_symbol_matching_kinds(resolver, fqn, CLASS_LIKE_SYMBOL_KINDS).is_none() {
            // Skip aliased use statements that don't resolve — they are often
            // namespace-prefix imports (e.g., `use Symfony\...\Constraints as Assert;`)
            // where the FQN refers to a namespace, not a class.
            if use_stmt.alias.is_some() {
                continue;
            }

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
    F: Fn(&str, &[PhpSymbolKind]) -> Option<Arc<SymbolInfo>>,
{
    let start = node.start_position();
    let scoped_file_symbols =
        file_symbols.scoped_at_byte_position(start.row as u32, start.column as u32);
    let file_symbols = scoped_file_symbols.as_ref();
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
        "namespace_definition" => {
            check_namespace_relative_function_call(
                node,
                source,
                file_symbols,
                resolver,
                diagnostics,
            );
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
    F: Fn(&str, &[PhpSymbolKind]) -> Option<Arc<SymbolInfo>>,
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

                if should_check_class(&fqn)
                    && resolve_symbol_matching_kinds(resolver, &fqn, CLASS_LIKE_SYMBOL_KINDS)
                        .is_none()
                {
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
        if let Some(ctor_sym) =
            resolve_symbol_matching_kinds(resolver, &ctor_fqn, METHOD_SYMBOL_KINDS)
        {
            if let Some(ref sig) = ctor_sym.signature {
                // Required = contiguous leading params without defaults.
                // Once a param has a default or is variadic, all subsequent are optional.
                let required = sig
                    .params
                    .iter()
                    .position(|p| p.default_value.is_some() || p.is_variadic)
                    .unwrap_or(sig.params.len());
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
    F: Fn(&str, &[PhpSymbolKind]) -> Option<Arc<SymbolInfo>>,
{
    // For optional_type (?Type), drill into the inner node.
    let target = if node.kind() == "optional_type" {
        node.named_child(0)
    } else {
        Some(node)
    };

    let Some(target) = target else {
        return;
    };

    let name_node = match target.kind() {
        "name" | "qualified_name" | "primitive_type" => Some(target),
        "named_type" => {
            let mut found = None;
            for i in 0..target.named_child_count() {
                if let Some(child) = target.named_child(i) {
                    let ck = child.kind();
                    if ck == "name" || ck == "qualified_name" || ck == "primitive_type" {
                        found = Some(child);
                        break;
                    }
                }
            }
            found
        }
        _ => None,
    };

    if let Some(name_node) = name_node {
        let name = &source[name_node.byte_range()];
        if is_builtin_type_name(name) {
            return;
        }

        let fqn = resolve_class_name(name, file_symbols);
        if should_check_class(&fqn)
            && resolve_symbol_matching_kinds(resolver, &fqn, CLASS_LIKE_SYMBOL_KINDS).is_none()
        {
            diagnostics.push(SemanticDiagnostic {
                range: node_range(&name_node),
                message: format!("Unknown class: {}", fqn),
                kind: SemanticDiagnosticKind::UnknownClass,
            });
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
    F: Fn(&str, &[PhpSymbolKind]) -> Option<Arc<SymbolInfo>>,
{
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            let ck = child.kind();
            if ck == "name" || ck == "qualified_name" {
                let name = &source[child.byte_range()];
                let fqn = resolve_class_name(name, file_symbols);

                if should_check_class(&fqn)
                    && resolve_symbol_matching_kinds(resolver, &fqn, CLASS_LIKE_SYMBOL_KINDS)
                        .is_none()
                {
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
    F: Fn(&str, &[PhpSymbolKind]) -> Option<Arc<SymbolInfo>>,
{
    // Prefer the explicit "function" field to preserve qualified names.
    let target_node = node
        .child_by_field_name("function")
        .or_else(|| node.named_child(0));

    if let Some(name_node) = target_node {
        let nk = name_node.kind();
        if nk == "name" || nk == "qualified_name" || nk == "namespace_name" {
            let name = &source[name_node.byte_range()];
            if is_language_construct_call(name) {
                return;
            }

            let fqn = resolve_function_name(name, file_symbols);

            let resolved = resolve_function_call_target(name, &fqn, file_symbols, resolver);

            if let Some((resolved_fqn, func_sym)) = resolved {
                if let Some(ref sig) = func_sym.signature {
                    // Required = contiguous leading params without defaults.
                    // Once a param has a default or is variadic, all subsequent are optional.
                    let required = sig
                        .params
                        .iter()
                        .position(|p| p.default_value.is_some() || p.is_variadic)
                        .unwrap_or(sig.params.len());
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

            if let Some(unknown_fqn) =
                unknown_function_diagnostic_fqn(name, &fqn, file_symbols, resolver)
            {
                diagnostics.push(SemanticDiagnostic {
                    range: node_range(&name_node),
                    message: format!("Unknown function: {}", unknown_fqn),
                    kind: SemanticDiagnosticKind::UnknownFunction,
                });
            }
        }
    }
}

fn check_namespace_relative_function_call<F>(
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: &F,
    diagnostics: &mut Vec<SemanticDiagnostic>,
) where
    F: Fn(&str, &[PhpSymbolKind]) -> Option<Arc<SymbolInfo>>,
{
    let Some((name, selection)) = crate::resolve::namespace_relative_function_call(node, source)
    else {
        return;
    };
    let resolved = resolve_function_name(&name, file_symbols);
    if resolve_function_symbol(resolver, &resolved).is_none() {
        diagnostics.push(SemanticDiagnostic {
            range: node_range(&selection),
            message: format!("Unknown function: {resolved}"),
            kind: SemanticDiagnosticKind::UnknownFunction,
        });
    }
}

fn resolve_function_call_target<F>(
    name: &str,
    resolved_name: &str,
    file_symbols: &FileSymbols,
    resolver: &F,
) -> Option<(String, Arc<SymbolInfo>)>
where
    F: Fn(&str, &[PhpSymbolKind]) -> Option<Arc<SymbolInfo>>,
{
    if !is_unqualified_name(name) {
        return resolve_function_symbol(resolver, resolved_name)
            .map(|sym| (resolved_name.to_string(), sym));
    }

    let has_function_import = file_symbols.use_statements.iter().any(|statement| {
        if statement.kind != UseKind::Function {
            return false;
        }
        let alias = statement
            .alias
            .as_deref()
            .unwrap_or_else(|| statement.fqn.rsplit('\\').next().unwrap_or(&statement.fqn));
        alias.eq_ignore_ascii_case(name)
    });
    if has_function_import {
        return resolve_function_symbol(resolver, resolved_name)
            .map(|sym| (resolved_name.to_string(), sym));
    }

    if let Some(sym) = resolve_function_symbol(resolver, resolved_name) {
        return Some((resolved_name.to_string(), sym));
    }

    if file_symbols.namespace.is_some() {
        return resolve_function_symbol(resolver, name).map(|sym| (name.to_string(), sym));
    }

    None
}

fn unknown_function_diagnostic_fqn<F>(
    name: &str,
    resolved_name: &str,
    file_symbols: &FileSymbols,
    resolver: &F,
) -> Option<String>
where
    F: Fn(&str, &[PhpSymbolKind]) -> Option<Arc<SymbolInfo>>,
{
    if resolve_function_call_target(name, resolved_name, file_symbols, resolver).is_some() {
        None
    } else {
        Some(resolved_name.to_string())
    }
}

fn resolve_function_symbol<F>(resolver: &F, fqn: &str) -> Option<Arc<SymbolInfo>>
where
    F: Fn(&str, &[PhpSymbolKind]) -> Option<Arc<SymbolInfo>>,
{
    resolve_symbol_matching_kinds(resolver, fqn, FUNCTION_SYMBOL_KINDS)
}

fn is_unqualified_name(name: &str) -> bool {
    !name.starts_with('\\') && !name.contains('\\')
}

fn is_language_construct_call(name: &str) -> bool {
    is_unqualified_name(name)
        && LANGUAGE_CONSTRUCT_CALLS
            .iter()
            .any(|construct| name.eq_ignore_ascii_case(construct))
}

/// Whether we should check a class name against the index.
fn should_check_class(fqn: &str) -> bool {
    // Skip built-in type names
    if is_builtin_type_name(fqn) {
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
    crate::resolve::resolve_class_name_pub(name, file_symbols)
}

fn is_builtin_type_name(name: &str) -> bool {
    let lower = name.trim_start_matches('\\').to_ascii_lowercase();
    BUILTIN_TYPE_NAMES.contains(&lower.as_str())
}

/// Resolve a function name to FQN.
fn resolve_function_name(name: &str, file_symbols: &FileSymbols) -> String {
    crate::resolve::resolve_function_name_pub(name, file_symbols)
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

fn check_unused_imports(
    root: tree_sitter::Node,
    source: &str,
    file_symbols: &FileSymbols,
    diagnostics: &mut Vec<SemanticDiagnostic>,
) {
    for use_stmt in &file_symbols.use_statements {
        let imported_name = use_stmt
            .alias
            .as_deref()
            .unwrap_or_else(|| use_stmt.fqn.rsplit('\\').next().unwrap_or(&use_stmt.fqn));
        if imported_name.is_empty() {
            continue;
        }

        let scope_range = file_symbols
            .namespace_scope_at_byte_position(use_stmt.range.0, use_stmt.range.1)
            .map(|scope| scope.range);
        let is_used_in_phpdoc = use_stmt.kind == UseKind::Class
            && import_name_is_used_in_phpdoc(source, imported_name, scope_range);

        if !import_name_is_used(
            root,
            source,
            imported_name,
            use_stmt.range,
            scope_range,
            use_stmt.kind,
        ) && !is_used_in_phpdoc
        {
            diagnostics.push(SemanticDiagnostic {
                range: use_stmt.range,
                message: format!("Unused import: {}", use_stmt.fqn),
                kind: SemanticDiagnosticKind::UnusedImport,
            });
        }
    }
}

fn import_name_is_used(
    node: tree_sitter::Node,
    source: &str,
    imported_name: &str,
    import_range: (u32, u32, u32, u32),
    scope_range: Option<ByteRange>,
    import_kind: UseKind,
) -> bool {
    let current_range = node_range(&node);
    if scope_range.is_some_and(|scope| !ranges_overlap(scope, current_range)) {
        return false;
    }
    if range_contains(import_range, current_range) {
        return false;
    }

    if matches!(node.kind(), "name" | "qualified_name" | "namespace_name") {
        let text = &source[node.byte_range()];
        if import_kind != UseKind::Class && text.trim_start_matches('\\').contains('\\') {
            // Function and constant imports alias one unqualified symbol only;
            // do not descend into a qualified name and accidentally count its
            // first child as use of that alias.
            return false;
        }
        let first = first_name_segment(text);
        if if matches!(import_kind, UseKind::Class | UseKind::Function) {
            first.eq_ignore_ascii_case(imported_name)
        } else {
            first == imported_name
        } {
            return true;
        }
    }

    let mut cursor = node.walk();
    let used = node.named_children(&mut cursor).any(|child| {
        import_name_is_used(
            child,
            source,
            imported_name,
            import_range,
            scope_range,
            import_kind,
        )
    });
    used
}

fn import_name_is_used_in_phpdoc(
    source: &str,
    imported_name: &str,
    scope_range: Option<ByteRange>,
) -> bool {
    let (scope_start, scope_end) = scope_range.map_or((0, source.len()), |range| {
        (
            source_byte_offset(source, range.0, range.1),
            source_byte_offset(source, range.2, range.3),
        )
    });
    let scoped_source = &source[scope_start.min(source.len())..scope_end.min(source.len())];

    let mut offset = 0usize;
    while let Some(relative_start) = scoped_source[offset..].find("/**") {
        let start = offset + relative_start;
        let Some(relative_end) = scoped_source[start..].find("*/") else {
            break;
        };
        let end = start + relative_end + 2;
        let phpdoc = crate::phpdoc::parse_phpdoc(&scoped_source[start..end]);
        if phpdoc_uses_imported_name(&phpdoc, imported_name) {
            return true;
        }
        offset = end;
    }
    false
}

fn phpdoc_uses_imported_name(phpdoc: &PhpDoc, imported_name: &str) -> bool {
    phpdoc
        .params
        .iter()
        .filter_map(|param| param.type_info.as_ref())
        .any(|type_info| type_info_uses_imported_name(type_info, imported_name))
        || phpdoc
            .return_type
            .as_ref()
            .is_some_and(|type_info| type_info_uses_imported_name(type_info, imported_name))
        || phpdoc
            .var_type
            .as_ref()
            .is_some_and(|type_info| type_info_uses_imported_name(type_info, imported_name))
        || phpdoc
            .throws
            .iter()
            .any(|type_info| type_info_uses_imported_name(type_info, imported_name))
        || phpdoc.properties.iter().any(|property| {
            property
                .type_info
                .as_ref()
                .is_some_and(|type_info| type_info_uses_imported_name(type_info, imported_name))
        })
        || phpdoc.methods.iter().any(|method| {
            method
                .return_type
                .as_ref()
                .is_some_and(|type_info| type_info_uses_imported_name(type_info, imported_name))
                || method
                    .params
                    .iter()
                    .filter_map(|param| param.type_info.as_ref())
                    .any(|type_info| type_info_uses_imported_name(type_info, imported_name))
        })
        || phpdoc.templates.iter().any(|template| {
            template
                .bound
                .as_ref()
                .is_some_and(|type_info| type_info_uses_imported_name(type_info, imported_name))
        })
        || phpdoc.template_bindings.iter().any(|binding| {
            type_name_uses_imported_name(&binding.target, imported_name)
                || binding
                    .args
                    .iter()
                    .any(|type_info| type_info_uses_imported_name(type_info, imported_name))
        })
        || phpdoc
            .type_aliases
            .iter()
            .any(|alias| type_info_uses_imported_name(&alias.type_info, imported_name))
        || phpdoc.type_alias_imports.iter().any(|alias_import| {
            type_name_uses_imported_name(&alias_import.source_type, imported_name)
        })
}

fn type_info_uses_imported_name(type_info: &TypeInfo, imported_name: &str) -> bool {
    match type_info {
        TypeInfo::Simple(name) => type_name_uses_imported_name(name, imported_name),
        TypeInfo::Generic { base, args } => {
            type_name_uses_imported_name(base, imported_name)
                || args
                    .iter()
                    .any(|type_info| type_info_uses_imported_name(type_info, imported_name))
        }
        TypeInfo::ArrayShape(items) | TypeInfo::ObjectShape(items) => items
            .iter()
            .any(|item| type_info_uses_imported_name(&item.value, imported_name)),
        TypeInfo::Callable {
            params,
            return_type,
        } => {
            params
                .iter()
                .any(|type_info| type_info_uses_imported_name(type_info, imported_name))
                || return_type
                    .as_deref()
                    .is_some_and(|type_info| type_info_uses_imported_name(type_info, imported_name))
        }
        TypeInfo::ClassString(inner) => inner
            .as_deref()
            .is_some_and(|type_info| type_info_uses_imported_name(type_info, imported_name)),
        TypeInfo::Conditional {
            target,
            if_type,
            else_type,
            ..
        } => {
            type_info_uses_imported_name(target, imported_name)
                || type_info_uses_imported_name(if_type, imported_name)
                || type_info_uses_imported_name(else_type, imported_name)
        }
        TypeInfo::Union(types) | TypeInfo::Intersection(types) => types
            .iter()
            .any(|type_info| type_info_uses_imported_name(type_info, imported_name)),
        TypeInfo::Nullable(inner) => type_info_uses_imported_name(inner, imported_name),
        TypeInfo::LiteralString(_)
        | TypeInfo::LiteralInt(_)
        | TypeInfo::LiteralFloat(_)
        | TypeInfo::LiteralBool(_)
        | TypeInfo::LiteralNull
        | TypeInfo::Void
        | TypeInfo::Never
        | TypeInfo::Mixed
        | TypeInfo::Self_
        | TypeInfo::Static_
        | TypeInfo::Parent_ => false,
    }
}

fn type_name_uses_imported_name(name: &str, imported_name: &str) -> bool {
    first_name_segment(name).eq_ignore_ascii_case(imported_name)
}

fn first_name_segment(name: &str) -> &str {
    name.trim_start_matches('\\')
        .split('\\')
        .next()
        .unwrap_or(name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VariableDeclarationKind {
    Parameter,
    Variable,
    ClosureUse,
    PromotedProperty,
}

#[derive(Debug, Clone)]
struct VariableOccurrence {
    name: String,
    range: (u32, u32, u32, u32),
    start_byte: usize,
    declaration_kind: Option<VariableDeclarationKind>,
    null_coalesce_probe: bool,
}

type ByteRange = (u32, u32, u32, u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum DuplicateSymbolCategory {
    ClassLike,
    Function,
    GlobalConstant,
    Method,
    Property,
    ClassConstant,
}

type SymbolKey = (DuplicateSymbolCategory, String);

fn check_variable_diagnostics<F>(
    root: tree_sitter::Node,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: &F,
    diagnostics: &mut Vec<SemanticDiagnostic>,
) where
    F: Fn(&str, &[PhpSymbolKind]) -> Option<Arc<SymbolInfo>>,
{
    check_variables_in_scope(root, source, file_symbols, resolver, diagnostics);
}

fn check_variables_in_scope<F>(
    scope: tree_sitter::Node,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: &F,
    diagnostics: &mut Vec<SemanticDiagnostic>,
) where
    F: Fn(&str, &[PhpSymbolKind]) -> Option<Arc<SymbolInfo>>,
{
    let mut occurrences = Vec::new();
    collect_variable_occurrences(
        scope,
        scope.id(),
        source,
        file_symbols,
        resolver,
        &mut occurrences,
    );
    report_variable_diagnostics(
        &occurrences,
        scope,
        source,
        file_symbols,
        resolver,
        should_report_unused_declarations(scope),
        diagnostics,
    );

    let mut cursor = scope.walk();
    for child in scope.named_children(&mut cursor) {
        walk_nested_scopes(child, source, file_symbols, resolver, diagnostics);
    }
}

fn walk_nested_scopes<F>(
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: &F,
    diagnostics: &mut Vec<SemanticDiagnostic>,
) where
    F: Fn(&str, &[PhpSymbolKind]) -> Option<Arc<SymbolInfo>>,
{
    if is_variable_scope(node) {
        check_variables_in_scope(node, source, file_symbols, resolver, diagnostics);
        return;
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_nested_scopes(child, source, file_symbols, resolver, diagnostics);
    }
}

fn collect_variable_occurrences(
    node: tree_sitter::Node,
    scope_id: usize,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: &impl Fn(&str, &[PhpSymbolKind]) -> Option<Arc<SymbolInfo>>,
    occurrences: &mut Vec<VariableOccurrence>,
) {
    if node.id() != scope_id && is_variable_scope(node) {
        collect_closure_use_reads(node, source, occurrences);
        return;
    }

    if is_builtin_compact_function_call(node, source, file_symbols, resolver) {
        collect_compact_variable_reads(node, source, occurrences);
    }

    if node.kind() == "variable_name" && !is_non_local_variable_context(node) {
        let name = normalize_var_name(&source[node.byte_range()]);
        if !is_ignorable_variable(&name) {
            occurrences.push(VariableOccurrence {
                name: name.clone(),
                range: node_range(&node),
                start_byte: node.start_byte(),
                declaration_kind: variable_declaration_kind(node, source, &name),
                null_coalesce_probe: is_null_coalesce_probe(node, source),
            });
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_variable_occurrences(child, scope_id, source, file_symbols, resolver, occurrences);
    }
}

fn collect_closure_use_reads(
    scope: tree_sitter::Node,
    source: &str,
    occurrences: &mut Vec<VariableOccurrence>,
) {
    if !matches!(
        scope.kind(),
        "anonymous_function" | "anonymous_function_creation_expression"
    ) {
        return;
    }

    let mut cursor = scope.walk();
    for child in scope.named_children(&mut cursor) {
        if child.kind() == "anonymous_function_use_clause" {
            collect_variable_reads_in_node(child, source, occurrences);
        }
    }
}

fn collect_variable_reads_in_node(
    node: tree_sitter::Node,
    source: &str,
    occurrences: &mut Vec<VariableOccurrence>,
) {
    if node.kind() == "variable_name" {
        let name = normalize_var_name(&source[node.byte_range()]);
        if !is_ignorable_variable(&name) {
            occurrences.push(VariableOccurrence {
                name,
                range: node_range(&node),
                start_byte: node.start_byte(),
                declaration_kind: None,
                null_coalesce_probe: false,
            });
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_variable_reads_in_node(child, source, occurrences);
    }
}

fn is_builtin_compact_function_call(
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: &impl Fn(&str, &[PhpSymbolKind]) -> Option<Arc<SymbolInfo>>,
) -> bool {
    if node.kind() != "function_call_expression" {
        return false;
    }

    let Some(function) = node
        .child_by_field_name("function")
        .or_else(|| node.named_child(0))
    else {
        return false;
    };
    if function.kind() == "member_access_expression" {
        return false;
    }

    let raw_name = source[function.byte_range()].trim();
    if let Some(global_name) = raw_name.strip_prefix('\\') {
        return !global_name.contains('\\') && global_name.eq_ignore_ascii_case("compact");
    }
    if !raw_name.eq_ignore_ascii_case("compact") {
        return false;
    }

    let start = node.start_position();
    let scoped_symbols =
        file_symbols.scoped_at_byte_position(start.row as u32, start.column as u32);

    if let Some(import) = scoped_symbols.use_statements.iter().find(|statement| {
        if statement.kind != UseKind::Function {
            return false;
        }
        let alias = statement
            .alias
            .as_deref()
            .unwrap_or_else(|| statement.fqn.rsplit('\\').next().unwrap_or(&statement.fqn));
        alias.eq_ignore_ascii_case(raw_name)
    }) {
        return import.fqn.eq_ignore_ascii_case("compact");
    }

    let namespaced = resolve_function_name(raw_name, &scoped_symbols);
    if scoped_symbols.namespace.is_some()
        && resolve_function_symbol(resolver, &namespaced).is_some()
    {
        return false;
    }

    true
}

fn collect_compact_variable_reads(
    call: tree_sitter::Node,
    source: &str,
    occurrences: &mut Vec<VariableOccurrence>,
) {
    let Some(arguments) = call_arguments_node(call) else {
        return;
    };

    let mut cursor = arguments.walk();
    for argument in arguments.named_children(&mut cursor) {
        let value = argument
            .child_by_field_name("value")
            .or_else(|| argument.named_child(0))
            .unwrap_or(argument);
        collect_compact_variable_reads_from_argument(value, source, occurrences);
    }
}

fn call_arguments_node(call: tree_sitter::Node) -> Option<tree_sitter::Node> {
    if let Some(arguments) = call.child_by_field_name("arguments") {
        return Some(arguments);
    }

    let mut cursor = call.walk();
    let arguments = call
        .named_children(&mut cursor)
        .find(|child| child.kind() == "arguments");
    arguments
}

fn collect_compact_variable_reads_from_argument(
    node: tree_sitter::Node,
    source: &str,
    occurrences: &mut Vec<VariableOccurrence>,
) {
    if let Some(name) = compact_variable_name_from_string_node(node, source) {
        if !is_ignorable_variable(&name) {
            occurrences.push(VariableOccurrence {
                name,
                range: node_range(&node),
                start_byte: node.start_byte(),
                declaration_kind: None,
                null_coalesce_probe: false,
            });
        }
        return;
    }

    if node.kind() == "array_element_initializer" {
        if let Some(value) = node
            .child_by_field_name("value")
            .or_else(|| node.named_child(0))
        {
            collect_compact_variable_reads_from_argument(value, source, occurrences);
        }
        return;
    }

    if matches!(
        node.kind(),
        "array_creation_expression" | "parenthesized_expression"
    ) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            collect_compact_variable_reads_from_argument(child, source, occurrences);
        }
    }
}

fn compact_variable_name_from_string_node(node: tree_sitter::Node, source: &str) -> Option<String> {
    if !matches!(node.kind(), "string" | "encapsed_string") {
        return None;
    }
    if node.kind() == "encapsed_string" && node_has_descendant_kind(node, "variable_name") {
        return None;
    }

    let value = static_php_string_literal_value(source[node.byte_range()].trim())?;
    is_valid_compact_variable_name(&value).then(|| normalize_var_name(&value))
}

fn static_php_string_literal_value(raw: &str) -> Option<String> {
    let mut chars = raw.char_indices();
    let (first_idx, first) = chars.next()?;
    let (quote_start, quote) = if matches!(first, 'b' | 'B') {
        let (idx, ch) = chars.next()?;
        (idx, ch)
    } else {
        (first_idx, first)
    };

    if !matches!(quote, '\'' | '"') || !raw.ends_with(quote) {
        return None;
    }

    let content_start = quote_start + quote.len_utf8();
    let content_end = raw.len().checked_sub(quote.len_utf8())?;
    if content_start > content_end {
        return None;
    }

    Some(unescape_static_php_string(
        &raw[content_start..content_end],
        quote,
    ))
}

fn unescape_static_php_string(content: &str, quote: char) -> String {
    let mut out = String::with_capacity(content.len());
    let mut chars = content.chars();

    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }

        let Some(next) = chars.next() else {
            out.push(ch);
            break;
        };

        if next == '\\' || next == quote {
            out.push(next);
        } else {
            out.push(ch);
            out.push(next);
        }
    }

    out
}

fn is_valid_compact_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };

    if first == '$' || !is_php_variable_name_start(first) {
        return false;
    }

    chars.all(is_php_variable_name_continue)
}

fn is_php_variable_name_start(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphabetic() || !ch.is_ascii()
}

fn is_php_variable_name_continue(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric() || !ch.is_ascii()
}

fn node_has_descendant_kind(node: tree_sitter::Node, kind: &str) -> bool {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == kind || node_has_descendant_kind(child, kind) {
            return true;
        }
    }
    false
}

fn is_non_local_variable_context(node: tree_sitter::Node) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "property_declaration" | "property_element" | "scoped_property_access_expression" => {
                return true
            }
            "method_declaration"
            | "function_definition"
            | "arrow_function"
            | "anonymous_function"
            | "anonymous_function_creation_expression"
            | "program" => return false,
            _ => current = parent.parent(),
        }
    }
    false
}

fn is_null_coalesce_probe(node: tree_sitter::Node, source: &str) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        if matches!(
            parent.kind(),
            "method_declaration"
                | "function_definition"
                | "arrow_function"
                | "anonymous_function"
                | "anonymous_function_creation_expression"
                | "program"
        ) {
            return false;
        }

        let text = &source[parent.byte_range()];
        if let Some(operator_offset) = text.find("??") {
            let node_offset = node.start_byte().saturating_sub(parent.start_byte());
            return node_offset < operator_offset;
        }

        current = parent.parent();
    }
    false
}

fn report_variable_diagnostics<F>(
    occurrences: &[VariableOccurrence],
    scope: tree_sitter::Node,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: &F,
    report_unused_declarations: bool,
    diagnostics: &mut Vec<SemanticDiagnostic>,
) where
    F: Fn(&str, &[PhpSymbolKind]) -> Option<Arc<SymbolInfo>>,
{
    let mut declared_by_name: HashMap<&str, Vec<&VariableOccurrence>> = HashMap::new();
    let mut used_by_name: HashMap<&str, Vec<&VariableOccurrence>> = HashMap::new();

    for occurrence in occurrences {
        if occurrence.declaration_kind.is_some() {
            declared_by_name
                .entry(&occurrence.name)
                .or_default()
                .push(occurrence);
        } else {
            used_by_name
                .entry(&occurrence.name)
                .or_default()
                .push(occurrence);
        }
    }

    let mut reported_undefined = HashSet::new();
    for occurrence in occurrences
        .iter()
        .filter(|occurrence| occurrence.declaration_kind.is_none())
    {
        if occurrence.name == "$this" {
            continue;
        }
        if occurrence.null_coalesce_probe {
            continue;
        }

        let declared_before = declared_by_name
            .get(occurrence.name.as_str())
            .map(|decls| {
                decls
                    .iter()
                    .any(|decl| decl.start_byte < occurrence.start_byte)
            })
            .unwrap_or(false);

        if !declared_before && reported_undefined.insert(occurrence.name.clone()) {
            diagnostics.push(SemanticDiagnostic {
                range: occurrence.range,
                message: format!("Undefined variable: {}", occurrence.name),
                kind: SemanticDiagnosticKind::UndefinedVariable,
            });
        }
    }

    if !report_unused_declarations {
        return;
    }

    for (name, declarations) in declared_by_name {
        if name == "$this" {
            continue;
        }
        let has_read = used_by_name.get(name).is_some_and(|uses| !uses.is_empty());
        if has_read {
            continue;
        }

        let Some(first_declaration) = declarations.first() else {
            continue;
        };
        match first_declaration.declaration_kind {
            Some(VariableDeclarationKind::Parameter) => {
                if should_suppress_unused_parameter(scope, source, file_symbols, resolver) {
                    continue;
                }
                diagnostics.push(SemanticDiagnostic {
                    range: first_declaration.range,
                    message: format!("Unused parameter: {}", first_declaration.name),
                    kind: SemanticDiagnosticKind::UnusedParameter,
                });
            }
            Some(VariableDeclarationKind::Variable) => diagnostics.push(SemanticDiagnostic {
                range: first_declaration.range,
                message: format!("Unused variable: {}", first_declaration.name),
                kind: SemanticDiagnosticKind::UnusedVariable,
            }),
            Some(
                VariableDeclarationKind::ClosureUse | VariableDeclarationKind::PromotedProperty,
            )
            | None => {}
        }
    }
}

fn is_variable_scope(node: tree_sitter::Node) -> bool {
    matches!(
        node.kind(),
        "method_declaration"
            | "function_definition"
            | "anonymous_function"
            | "anonymous_function_creation_expression"
    )
}

fn should_report_unused_declarations(scope: tree_sitter::Node) -> bool {
    is_variable_scope(scope) && !is_bodyless_method_scope(scope)
}

fn is_bodyless_method_scope(scope: tree_sitter::Node) -> bool {
    if scope.kind() != "method_declaration" {
        return false;
    }

    let mut cursor = scope.walk();
    let has_body = scope
        .named_children(&mut cursor)
        .any(|child| child.kind() == "compound_statement");
    !has_body
}

fn should_suppress_unused_parameter<F>(
    scope: tree_sitter::Node,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: &F,
) -> bool
where
    F: Fn(&str, &[PhpSymbolKind]) -> Option<Arc<SymbolInfo>>,
{
    if scope.kind() != "method_declaration" {
        return false;
    }

    if method_has_override_attribute(scope, source) {
        return true;
    }

    let Some(method_name) = method_name(scope, source) else {
        return false;
    };

    method_overrides_indexed_parent(scope, &method_name, file_symbols, resolver)
}

fn method_overrides_indexed_parent<F>(
    scope: tree_sitter::Node,
    method_name: &str,
    file_symbols: &FileSymbols,
    resolver: &F,
) -> bool
where
    F: Fn(&str, &[PhpSymbolKind]) -> Option<Arc<SymbolInfo>>,
{
    let scope_range = node_range(&scope);
    let Some(class_sym) = innermost_class_symbol_containing(file_symbols, scope_range) else {
        return false;
    };

    class_sym
        .extends
        .iter()
        .chain(class_sym.implements.iter())
        .any(|parent| {
            class_or_ancestor_has_method(parent, method_name, resolver, &mut HashSet::new())
        })
}

fn innermost_class_symbol_containing(
    file_symbols: &FileSymbols,
    range: (u32, u32, u32, u32),
) -> Option<&SymbolInfo> {
    file_symbols
        .symbols
        .iter()
        .filter(|sym| {
            matches!(
                sym.kind,
                php_lsp_types::PhpSymbolKind::Class
                    | php_lsp_types::PhpSymbolKind::Interface
                    | php_lsp_types::PhpSymbolKind::Trait
                    | php_lsp_types::PhpSymbolKind::Enum
            ) && range_contains(sym.range, range)
        })
        .min_by_key(|sym| {
            (
                sym.range.2.saturating_sub(sym.range.0),
                sym.range.3.saturating_sub(sym.range.1),
            )
        })
}

fn class_or_ancestor_has_method<F>(
    class_fqn: &str,
    method_name: &str,
    resolver: &F,
    visited: &mut HashSet<String>,
) -> bool
where
    F: Fn(&str, &[PhpSymbolKind]) -> Option<Arc<SymbolInfo>>,
{
    let class_fqn = class_fqn.trim_start_matches('\\');
    if !visited.insert(class_fqn.to_string()) {
        return false;
    }

    let method_fqn = format!("{}::{}", class_fqn, method_name);
    if resolve_symbol_matching_kinds(resolver, &method_fqn, METHOD_SYMBOL_KINDS).is_some() {
        return true;
    }

    let Some(class_sym) =
        resolve_symbol_matching_kinds(resolver, class_fqn, CLASS_LIKE_SYMBOL_KINDS)
    else {
        return false;
    };

    class_sym
        .extends
        .iter()
        .chain(class_sym.implements.iter())
        .any(|parent| class_or_ancestor_has_method(parent, method_name, resolver, visited))
}

fn method_has_override_attribute(scope: tree_sitter::Node, source: &str) -> bool {
    let text = &source[scope.byte_range()];
    text.contains("#[Override") || text.contains("#[\\Override")
}

fn method_name(scope: tree_sitter::Node, source: &str) -> Option<String> {
    let name_node = if let Some(name_node) = scope.child_by_field_name("name") {
        Some(name_node)
    } else {
        let mut cursor = scope.walk();
        let found = scope
            .named_children(&mut cursor)
            .find(|child| child.kind() == "name");
        found
    };

    name_node.map(|node| source[node.byte_range()].to_string())
}

fn variable_declaration_kind(
    node: tree_sitter::Node,
    source: &str,
    var_name: &str,
) -> Option<VariableDeclarationKind> {
    if is_foreach_header_declared_variable(node, source) {
        return Some(VariableDeclarationKind::Variable);
    }
    if is_assignment_left_hand_declared_variable(node) {
        return Some(VariableDeclarationKind::Variable);
    }
    if ancestor_field_contains(node, "catch_clause", &["name", "variable"]) {
        return Some(VariableDeclarationKind::Variable);
    }
    if is_by_ref_output_argument_variable(node, source) {
        return Some(VariableDeclarationKind::Variable);
    }
    if has_ancestor_before_scope(node, "anonymous_function_use_clause") {
        return Some(VariableDeclarationKind::ClosureUse);
    }

    let parent = node.parent()?;

    match parent.kind() {
        "simple_parameter" => parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id())
            .then_some(VariableDeclarationKind::Parameter),
        "property_promotion_parameter" => parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id())
            .then_some(VariableDeclarationKind::PromotedProperty),
        "assignment_expression" => parent
            .child_by_field_name("left")
            .is_some_and(|left| left.id() == node.id() || node_contains(left, node))
            .then_some(VariableDeclarationKind::Variable),
        "global_declaration" | "static_variable_declaration" => {
            Some(VariableDeclarationKind::Variable)
        }
        "anonymous_function_use_clause" => Some(VariableDeclarationKind::ClosureUse),
        _ if normalize_var_name(&source[parent.byte_range()]) == var_name
            && matches!(
                parent.kind(),
                "assignment_expression" | "by_ref_assignment_expression"
            ) =>
        {
            Some(VariableDeclarationKind::Variable)
        }
        _ => None,
    }
}

fn is_assignment_left_hand_declared_variable(node: tree_sitter::Node) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "assignment_expression" | "by_ref_assignment_expression" => {
                return parent
                    .child_by_field_name("left")
                    .is_some_and(|left| left.id() == node.id() || node_contains(left, node));
            }
            "method_declaration"
            | "function_definition"
            | "anonymous_function"
            | "anonymous_function_creation_expression"
            | "program" => return false,
            _ => current = parent.parent(),
        }
    }
    false
}

fn normalize_var_name(text: &str) -> String {
    if text.starts_with('$') {
        text.to_string()
    } else {
        format!("${}", text)
    }
}

fn is_ignorable_variable(name: &str) -> bool {
    name == "$this"
        || name == "$_"
        || name.starts_with("$_")
        || matches!(
            name,
            "$GLOBALS" | "$argc" | "$argv" | "$http_response_header" | "$HTTP_RAW_POST_DATA"
        )
}

fn check_duplicate_symbols_in_file(
    file_symbols: &FileSymbols,
    diagnostics: &mut Vec<SemanticDiagnostic>,
) {
    let mut seen: HashMap<SymbolKey, Vec<(&str, ByteRange)>> = HashMap::new();

    for symbol in &file_symbols.symbols {
        let Some(key) = duplicate_symbol_key(symbol) else {
            continue;
        };
        seen.entry(key)
            .or_default()
            .push((symbol.fqn.as_str(), symbol.selection_range));
    }

    let mut duplicates: Vec<(&str, ByteRange)> = seen
        .into_values()
        .filter(|entries| entries.len() > 1)
        .flatten()
        .collect();
    duplicates.sort_by_key(|(_, range)| *range);

    diagnostics.extend(
        duplicates
            .into_iter()
            .map(|(fqn, range)| SemanticDiagnostic {
                range,
                message: format!("Duplicate symbol: {fqn}"),
                kind: SemanticDiagnosticKind::DuplicateSymbol,
            }),
    );
}

fn duplicate_symbol_key(symbol: &SymbolInfo) -> Option<SymbolKey> {
    use php_lsp_types::PhpSymbolKind;

    let key = match symbol.kind {
        PhpSymbolKind::Class
        | PhpSymbolKind::Interface
        | PhpSymbolKind::Trait
        | PhpSymbolKind::Enum => (
            DuplicateSymbolCategory::ClassLike,
            symbol.fqn.to_ascii_lowercase(),
        ),
        PhpSymbolKind::Function => (
            DuplicateSymbolCategory::Function,
            symbol.fqn.to_ascii_lowercase(),
        ),
        PhpSymbolKind::GlobalConstant => (
            DuplicateSymbolCategory::GlobalConstant,
            global_constant_fqn_key(&symbol.fqn),
        ),
        PhpSymbolKind::Method => (
            DuplicateSymbolCategory::Method,
            member_duplicate_key(symbol, true)?,
        ),
        PhpSymbolKind::Property => (
            DuplicateSymbolCategory::Property,
            member_duplicate_key(symbol, false)?,
        ),
        PhpSymbolKind::ClassConstant | PhpSymbolKind::EnumCase => (
            DuplicateSymbolCategory::ClassConstant,
            member_duplicate_key(symbol, false)?,
        ),
        PhpSymbolKind::Namespace => return None,
    };
    Some(key)
}

fn member_duplicate_key(symbol: &SymbolInfo, lowercase_member: bool) -> Option<String> {
    let (owner, member) = symbol.fqn.rsplit_once("::")?;
    let member = if lowercase_member {
        member.to_ascii_lowercase()
    } else {
        member.to_string()
    };
    Some(format!("{}::{member}", owner.to_ascii_lowercase()))
}

fn ranges_overlap(left: ByteRange, right: ByteRange) -> bool {
    (left.0, left.1) < (right.2, right.3) && (right.0, right.1) < (left.2, left.3)
}

fn source_byte_offset(source: &str, line: u32, column: u32) -> usize {
    let mut offset = 0usize;
    for (row, text) in source.split_inclusive('\n').enumerate() {
        if row == line as usize {
            return offset + (column as usize).min(text.len());
        }
        offset += text.len();
    }
    source.len()
}

fn range_contains(outer: (u32, u32, u32, u32), inner: (u32, u32, u32, u32)) -> bool {
    (inner.0 > outer.0 || (inner.0 == outer.0 && inner.1 >= outer.1))
        && (inner.2 < outer.2 || (inner.2 == outer.2 && inner.3 <= outer.3))
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

/// Walk a tree-sitter tree and collect all class FQNs that arise from aliased
/// use statements.  For example, `use Symfony\...\Constraints as Assert;` +
/// code containing `new Assert\NotBlank(...)` produces FQN
/// `Symfony\...\Constraints\NotBlank`.
///
/// This is used by the server to pre-resolve (lazily index) these FQNs before
/// running `compute_diagnostics`, so that "Unknown class" warnings are not
/// emitted for classes reachable through namespace aliases.
pub fn collect_aliased_class_fqns(
    tree: &Tree,
    source: &str,
    file_symbols: &FileSymbols,
) -> Vec<String> {
    use crate::resolve::resolve_class_name_pub;
    use std::collections::HashSet;

    let src = source.as_bytes();
    let mut fqns = HashSet::new();
    let mut cursor = tree.root_node().walk();
    collect_qualified_names_recursive(
        &mut cursor,
        src,
        file_symbols,
        &mut fqns,
        &resolve_class_name_pub,
    );
    fqns.into_iter().collect()
}

/// Recursively walk the CST looking for `qualified_name` nodes whose first
/// segment matches one of the given `aliases`.
fn collect_qualified_names_recursive(
    cursor: &mut tree_sitter::TreeCursor,
    source: &[u8],
    file_symbols: &FileSymbols,
    out: &mut std::collections::HashSet<String>,
    resolver: &dyn Fn(&str, &FileSymbols) -> String,
) {
    loop {
        let node = cursor.node();
        if node.kind() == "qualified_name" {
            let text = node.utf8_text(source).unwrap_or_default();
            if let Some(first) = text.split('\\').next() {
                let start = node.start_position();
                let scoped_file_symbols =
                    file_symbols.scoped_at_byte_position(start.row as u32, start.column as u32);
                let has_alias = scoped_file_symbols.use_statements.iter().any(|statement| {
                    statement.kind == UseKind::Class
                        && statement
                            .alias
                            .as_deref()
                            .is_some_and(|alias| alias.eq_ignore_ascii_case(first))
                });
                if has_alias {
                    let fqn = resolver(text, &scoped_file_symbols);
                    out.insert(fqn);
                }
            }
        }
        // Recurse into children
        if cursor.goto_first_child() {
            collect_qualified_names_recursive(cursor, source, file_symbols, out, resolver);
            cursor.goto_parent();
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

#[cfg(test)]
#[path = "semantic_tests.rs"]
mod tests;
