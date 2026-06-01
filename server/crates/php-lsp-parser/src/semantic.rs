//! Semantic diagnostics for PHP files.
//!
//! Walks the CST and checks class/function/use references
//! against a resolver function (typically backed by the workspace index).

use crate::cst::{
    ancestor_field_contains, has_ancestor_before_scope, is_by_ref_output_argument_variable,
    is_foreach_header_declared_variable, node_contains,
};
use php_lsp_types::{FileSymbols, PhpDoc, SymbolInfo, TypeInfo, UseKind};
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
    F: Fn(&str) -> Option<Arc<SymbolInfo>>,
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

        if resolver(fqn).is_none() {
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
    F: Fn(&str) -> Option<Arc<SymbolInfo>>,
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
        if should_check_class(&fqn) && resolver(&fqn).is_none() {
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
    // Prefer the explicit "function" field to preserve qualified names.
    let target_node = node
        .child_by_field_name("function")
        .or_else(|| node.named_child(0));

    if let Some(name_node) = target_node {
        let nk = name_node.kind();
        if nk == "name" || nk == "qualified_name" || nk == "namespace_name" {
            let name = &source[name_node.byte_range()];

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

fn resolve_function_call_target<F>(
    name: &str,
    resolved_name: &str,
    file_symbols: &FileSymbols,
    resolver: &F,
) -> Option<(String, Arc<SymbolInfo>)>
where
    F: Fn(&str) -> Option<Arc<SymbolInfo>>,
{
    if is_unqualified_name(name) {
        if resolved_name != name {
            return resolve_function_symbol(resolver, resolved_name)
                .map(|sym| (resolved_name.to_string(), sym));
        }

        if let Some(ref ns) = file_symbols.namespace {
            let namespaced = format!("{}\\{}", ns, name);
            if let Some(sym) = resolve_function_symbol(resolver, &namespaced) {
                return Some((namespaced, sym));
            }
        }

        return resolve_function_symbol(resolver, name).map(|sym| (name.to_string(), sym));
    }

    resolve_function_symbol(resolver, resolved_name).map(|sym| (resolved_name.to_string(), sym))
}

fn unknown_function_diagnostic_fqn<F>(
    name: &str,
    resolved_name: &str,
    file_symbols: &FileSymbols,
    resolver: &F,
) -> Option<String>
where
    F: Fn(&str) -> Option<Arc<SymbolInfo>>,
{
    if resolve_function_call_target(name, resolved_name, file_symbols, resolver).is_some() {
        return None;
    }

    if is_unqualified_name(name) {
        if resolved_name != name {
            return Some(resolved_name.to_string());
        }

        return Some(
            file_symbols
                .namespace
                .as_ref()
                .map(|ns| format!("{}\\{}", ns, name))
                .unwrap_or_else(|| name.to_string()),
        );
    }

    Some(resolved_name.to_string())
}

fn resolve_function_symbol<F>(resolver: &F, fqn: &str) -> Option<Arc<SymbolInfo>>
where
    F: Fn(&str) -> Option<Arc<SymbolInfo>>,
{
    resolver(fqn)
}

fn is_unqualified_name(name: &str) -> bool {
    !name.starts_with('\\') && !name.contains('\\')
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
    // Already fully qualified
    if name.starts_with('\\') {
        return name.trim_start_matches('\\').to_string();
    }

    // Special names
    if is_builtin_type_name(name) {
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

fn is_builtin_type_name(name: &str) -> bool {
    let lower = name.trim_start_matches('\\').to_ascii_lowercase();
    BUILTIN_TYPE_NAMES.contains(&lower.as_str())
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

    // Keep already-qualified names stable.
    if name.contains('\\') {
        return name.to_string();
    }

    // Simple name — could be global or namespace function.
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

        let is_used_in_phpdoc =
            use_stmt.kind == UseKind::Class && import_name_is_used_in_phpdoc(source, imported_name);

        if !import_name_is_used(root, source, imported_name, use_stmt.range) && !is_used_in_phpdoc {
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
) -> bool {
    if range_contains(import_range, node_range(&node)) {
        return false;
    }

    if matches!(node.kind(), "name" | "qualified_name" | "namespace_name") {
        let text = &source[node.byte_range()];
        if first_name_segment(text) == imported_name {
            return true;
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if import_name_is_used(child, source, imported_name, import_range) {
            return true;
        }
    }

    false
}

fn import_name_is_used_in_phpdoc(source: &str, imported_name: &str) -> bool {
    let mut offset = 0usize;
    while let Some(relative_start) = source[offset..].find("/**") {
        let start = offset + relative_start;
        let Some(relative_end) = source[start..].find("*/") else {
            break;
        };
        let end = start + relative_end + 2;
        let phpdoc = crate::phpdoc::parse_phpdoc(&source[start..end]);
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
    first_name_segment(name) == imported_name
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
type SymbolKey<'a> = (php_lsp_types::PhpSymbolKind, &'a str);

fn check_variable_diagnostics<F>(
    root: tree_sitter::Node,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: &F,
    diagnostics: &mut Vec<SemanticDiagnostic>,
) where
    F: Fn(&str) -> Option<Arc<SymbolInfo>>,
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
    F: Fn(&str) -> Option<Arc<SymbolInfo>>,
{
    let mut occurrences = Vec::new();
    collect_variable_occurrences(scope, scope.id(), source, &mut occurrences);
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
    F: Fn(&str) -> Option<Arc<SymbolInfo>>,
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
    occurrences: &mut Vec<VariableOccurrence>,
) {
    if node.id() != scope_id && is_variable_scope(node) {
        collect_closure_use_reads(node, source, occurrences);
        return;
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
        collect_variable_occurrences(child, scope_id, source, occurrences);
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
    F: Fn(&str) -> Option<Arc<SymbolInfo>>,
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
    F: Fn(&str) -> Option<Arc<SymbolInfo>>,
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
    F: Fn(&str) -> Option<Arc<SymbolInfo>>,
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
    F: Fn(&str) -> Option<Arc<SymbolInfo>>,
{
    let class_fqn = class_fqn.trim_start_matches('\\');
    if !visited.insert(class_fqn.to_string()) {
        return false;
    }

    let method_fqn = format!("{}::{}", class_fqn, method_name);
    if resolver(&method_fqn).is_some_and(|sym| sym.kind == php_lsp_types::PhpSymbolKind::Method) {
        return true;
    }

    let Some(class_sym) = resolver(class_fqn) else {
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
    let mut seen: HashMap<SymbolKey<'_>, Vec<ByteRange>> = HashMap::new();

    for sym in &file_symbols.symbols {
        if !is_duplicate_checked_symbol(sym.kind) {
            continue;
        }
        seen.entry((sym.kind, sym.fqn.as_str()))
            .or_default()
            .push(sym.selection_range);
    }

    for ((_, fqn), ranges) in seen {
        if ranges.len() <= 1 {
            continue;
        }
        for range in ranges {
            diagnostics.push(SemanticDiagnostic {
                range,
                message: format!("Duplicate symbol: {}", fqn),
                kind: SemanticDiagnosticKind::DuplicateSymbol,
            });
        }
    }
}

fn is_duplicate_checked_symbol(kind: php_lsp_types::PhpSymbolKind) -> bool {
    matches!(
        kind,
        php_lsp_types::PhpSymbolKind::Class
            | php_lsp_types::PhpSymbolKind::Interface
            | php_lsp_types::PhpSymbolKind::Trait
            | php_lsp_types::PhpSymbolKind::Enum
            | php_lsp_types::PhpSymbolKind::Function
            | php_lsp_types::PhpSymbolKind::GlobalConstant
    )
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

    // Build a set of alias prefixes for quick lookup.
    let aliases: HashSet<&str> = file_symbols
        .use_statements
        .iter()
        .filter(|u| u.kind == UseKind::Class && u.alias.is_some())
        .filter_map(|u| u.alias.as_deref())
        .collect();

    if aliases.is_empty() {
        return vec![];
    }

    let src = source.as_bytes();
    let mut fqns = HashSet::new();
    let mut cursor = tree.root_node().walk();
    collect_qualified_names_recursive(
        &mut cursor,
        src,
        &aliases,
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
    aliases: &std::collections::HashSet<&str>,
    file_symbols: &FileSymbols,
    out: &mut std::collections::HashSet<String>,
    resolver: &dyn Fn(&str, &FileSymbols) -> String,
) {
    loop {
        let node = cursor.node();
        if node.kind() == "qualified_name" {
            let text = node.utf8_text(source).unwrap_or_default();
            if let Some(first) = text.split('\\').next() {
                if aliases.contains(first) {
                    let fqn = resolver(text, file_symbols);
                    out.insert(fqn);
                }
            }
        }
        // Recurse into children
        if cursor.goto_first_child() {
            collect_qualified_names_recursive(cursor, source, aliases, file_symbols, out, resolver);
            cursor.goto_parent();
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
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
            extends: vec![],
            implements: vec![],
            traits: vec![],
            templates: vec![],
            template_bindings: vec![],
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
            extends: vec![],
            implements: vec![],
            traits: vec![],
            templates: vec![],
            template_bindings: vec![],
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

    fn parse_and_check_with_file_resolver(code: &str) -> Vec<SemanticDiagnostic> {
        let mut parser = FileParser::new();
        parser.parse_full(code);
        let tree = parser.tree().unwrap();
        let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
        let symbols = file_symbols.symbols.clone();
        extract_semantic_diagnostics(tree, code, &file_symbols, |fqn| {
            symbols
                .iter()
                .find(|sym| sym.fqn == fqn)
                .cloned()
                .map(Arc::new)
        })
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
    fn test_aliased_use_no_false_diagnostic() {
        // use ... as Alias; should NOT produce an unresolved diagnostic even
        // when the FQN doesn't resolve (it may be a namespace prefix import).
        let code = r#"<?php
namespace App;

use Symfony\Component\Validator\Constraints as Assert;
use App\Missing\SomeClass;
"#;
        let diags = parse_and_check(code, |_fqn| None);

        let unresolved: Vec<_> = diags
            .iter()
            .filter(|d| d.kind == SemanticDiagnosticKind::UnresolvedUse)
            .collect();

        // Only the non-aliased one should be reported
        assert_eq!(
            unresolved.len(),
            1,
            "Expected 1 unresolved use (not the aliased one), got: {:?}",
            unresolved
        );
        assert!(unresolved[0].message.contains("App\\Missing\\SomeClass"));
        assert!(
            !unresolved.iter().any(|d| d.message.contains("Constraints")),
            "Aliased use statement should NOT be reported as unresolved"
        );
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
    fn test_unknown_unqualified_function_after_namespace_and_global_fallbacks() {
        let code = r#"<?php
namespace App;

missing_helper();
"#;
        let diags = parse_and_check(code, |_fqn| None);

        let unknown_funcs: Vec<_> = diags
            .iter()
            .filter(|d| d.kind == SemanticDiagnosticKind::UnknownFunction)
            .collect();

        assert_eq!(
            unknown_funcs.len(),
            1,
            "Expected one unknown function diagnostic, got: {:?}",
            unknown_funcs
        );
        assert!(unknown_funcs[0]
            .message
            .contains("Unknown function: App\\missing_helper"));
    }

    #[test]
    fn test_unqualified_function_uses_current_namespace_or_global_fallback() {
        let code = r#"<?php
namespace App;

helper();
strlen("hello");
"#;
        let diags = parse_and_check(code, |fqn| {
            if fqn == "App\\helper" || fqn == "strlen" {
                Some(dummy_symbol())
            } else {
                None
            }
        });

        assert!(
            !diags
                .iter()
                .any(|d| d.kind == SemanticDiagnosticKind::UnknownFunction),
            "Expected namespace/global function fallback to avoid unknown diagnostics, got: {:?}",
            diags
        );
    }

    #[test]
    fn test_imported_function_reports_import_fqn_when_missing() {
        let code = r#"<?php
namespace App;

use function Vendor\helper;

helper();
"#;
        let diags = parse_and_check(code, |_fqn| None);

        assert!(
            diags.iter().any(|d| {
                d.kind == SemanticDiagnosticKind::UnknownFunction
                    && d.message.contains("Unknown function: Vendor\\helper")
            }),
            "Expected imported function FQN diagnostic, got: {:?}",
            diags
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

    #[test]
    fn test_no_unknown_class_for_self_static_parent_type_hints() {
        let code = r#"<?php
namespace App;

class Base {}

class Child extends Base {
    public function withSelf(self $arg): static {
        return $this;
    }

    public function withParent(parent $arg): parent {
        return $arg;
    }
}
"#;
        let diags = parse_and_check(code, |fqn| {
            if fqn == "App\\Base" {
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
            unknown.is_empty(),
            "Expected no unknown-class diagnostics for self/static/parent, got: {:?}",
            unknown
        );
    }

    #[test]
    fn test_no_unknown_class_for_case_insensitive_special_type_hints() {
        let code = r#"<?php
namespace App;

class Base {}

class Child extends Base {
    public function withSelf(Self $arg): STATIC {
        return $this;
    }

    public function withParent(PARENT $arg): PARENT {
        return $arg;
    }
}
"#;
        let diags = parse_and_check(code, |fqn| {
            if fqn == "App\\Base" {
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
            unknown.is_empty(),
            "Expected no unknown-class diagnostics for case-insensitive self/static/parent, got: {:?}",
            unknown
        );
    }

    /// Params after the first default-value param are implicitly optional even
    /// without their own default value (common in phpstorm-stubs, e.g.
    /// `preg_replace_callback`, `file_get_contents`).
    #[test]
    fn test_no_false_positive_for_optional_params_after_default() {
        // Simulates preg_replace_callback($pattern, $callback, $subject, int $limit = -1, &$count, int $flags = 0)
        // Only the first 3 params (before $limit which has a default) are truly required.
        let code = r#"<?php
preg_replace_callback('/x/', function(){}, 'input');
"#;
        let diags = parse_and_check(code, |fqn| {
            if fqn == "preg_replace_callback" {
                Some(function_symbol(
                    fqn,
                    vec![
                        ParamInfo {
                            name: "pattern".to_string(),
                            type_info: None,
                            default_value: None,
                            is_variadic: false,
                            is_by_ref: false,
                            is_promoted: false,
                        },
                        ParamInfo {
                            name: "callback".to_string(),
                            type_info: None,
                            default_value: None,
                            is_variadic: false,
                            is_by_ref: false,
                            is_promoted: false,
                        },
                        ParamInfo {
                            name: "subject".to_string(),
                            type_info: None,
                            default_value: None,
                            is_variadic: false,
                            is_by_ref: false,
                            is_promoted: false,
                        },
                        ParamInfo {
                            name: "limit".to_string(),
                            type_info: None,
                            default_value: Some("-1".to_string()),
                            is_variadic: false,
                            is_by_ref: false,
                            is_promoted: false,
                        },
                        ParamInfo {
                            name: "count".to_string(),
                            type_info: None,
                            default_value: None, // no default but after a defaulted param
                            is_variadic: false,
                            is_by_ref: true,
                            is_promoted: false,
                        },
                        ParamInfo {
                            name: "flags".to_string(),
                            type_info: None,
                            default_value: Some("0".to_string()),
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
            arg_diags.is_empty(),
            "Expected NO argument-count diagnostic for 3 args to preg_replace_callback (required prefix = 3), got: {:?}",
            arg_diags
        );
    }

    #[test]
    fn test_unused_import_reports_only_unreferenced_alias() {
        let code = r#"<?php
namespace App;

use Vendor\UsedService;
use Vendor\UnusedService;

new UsedService();
"#;
        let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));
        let unused_imports: Vec<_> = diags
            .iter()
            .filter(|d| d.kind == SemanticDiagnosticKind::UnusedImport)
            .collect();

        assert_eq!(
            unused_imports.len(),
            1,
            "Expected one unused import, got: {:?}",
            unused_imports
        );
        assert!(unused_imports[0].message.contains("Vendor\\UnusedService"));
    }

    #[test]
    fn test_phpdoc_reference_counts_import_as_used() {
        let code = r#"<?php
namespace App;

use Random\RandomException;

class Generator {
    /**
     * @throws RandomException
     */
    public function run(): void {
    }
}
"#;
        let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

        assert!(
            !diags.iter().any(|d| {
                d.kind == SemanticDiagnosticKind::UnusedImport
                    && d.message.contains("Random\\RandomException")
            }),
            "PHPDoc type references should count as import usage, got: {:?}",
            diags
        );
    }

    #[test]
    fn test_phpdoc_prose_does_not_count_import_as_used() {
        let code = r#"<?php
namespace App;

use Vendor\DocTextOnly;

class Generator {
    /**
     * @param string $value DocTextOnly appears only in prose.
     */
    public function run(string $value): void {
    }
}
"#;
        let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

        assert!(
            diags.iter().any(|d| {
                d.kind == SemanticDiagnosticKind::UnusedImport
                    && d.message.contains("Vendor\\DocTextOnly")
            }),
            "PHPDoc prose should not count as import usage, got: {:?}",
            diags
        );
    }

    #[test]
    fn test_phpdoc_type_does_not_count_function_import_as_used() {
        let code = r#"<?php
namespace App;

use function Vendor\DocType;

class Generator {
    /**
     * @param DocType $value
     */
    public function run($value): void {
    }
}
"#;
        let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

        assert!(
            diags.iter().any(|d| {
                d.kind == SemanticDiagnosticKind::UnusedImport
                    && d.message.contains("Vendor\\DocType")
            }),
            "PHPDoc class-like types should not count as function import usage, got: {:?}",
            diags
        );
    }

    #[test]
    fn test_undefined_variable_and_unused_local_diagnostics() {
        let code = r#"<?php
function run(string $used, string $unusedParam): void {
    echo $missing;
    $unusedLocal = 1;
    $usedLocal = 2;
    echo $used;
    echo $usedLocal;
}
"#;
        let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

        assert!(
            diags.iter().any(|d| {
                d.kind == SemanticDiagnosticKind::UndefinedVariable
                    && d.message.contains("$missing")
            }),
            "Expected undefined variable diagnostic, got: {:?}",
            diags
        );
        assert!(
            diags.iter().any(|d| {
                d.kind == SemanticDiagnosticKind::UnusedParameter
                    && d.message.contains("$unusedParam")
            }),
            "Expected unused parameter diagnostic, got: {:?}",
            diags
        );
        assert!(
            diags.iter().any(|d| {
                d.kind == SemanticDiagnosticKind::UnusedVariable
                    && d.message.contains("$unusedLocal")
            }),
            "Expected unused local diagnostic, got: {:?}",
            diags
        );
        assert!(
            !diags.iter().any(|d| d.message.contains("$usedLocal"))
                && !diags.iter().any(|d| d.message.contains("$used")),
            "Used variables/params should not be reported, got: {:?}",
            diags
        );
    }

    #[test]
    fn test_null_coalesce_probe_does_not_report_undefined_variable() {
        let code = r#"<?php
function run(): bool {
    return $maybeResult ?? false;
}
"#;
        let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

        assert!(
            !diags.iter().any(|d| {
                d.kind == SemanticDiagnosticKind::UndefinedVariable
                    && d.message.contains("$maybeResult")
            }),
            "Null coalesce left operand should not be reported as undefined, got: {:?}",
            diags
        );
    }

    #[test]
    fn test_arrow_function_auto_captures_outer_variables() {
        let code = r#"<?php
function run(): void {
    $npId = 'NP-1';
    $callback = static fn (array $context): bool => ($context['npId'] ?? null) === $npId;
    $callback([]);
}
"#;
        let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

        assert!(
            !diags.iter().any(|d| {
                d.kind == SemanticDiagnosticKind::UndefinedVariable && d.message.contains("$npId")
            }),
            "Arrow functions should auto-capture outer variables, got: {:?}",
            diags
        );
    }

    #[test]
    fn test_foreach_value_variable_is_declared() {
        let code = r#"<?php
function run(array $requests): void {
    foreach ($requests as $index => $request) {
        echo $index;
        echo $request;
    }
}
"#;
        let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

        assert!(
            !diags.iter().any(|d| {
                d.kind == SemanticDiagnosticKind::UndefinedVariable
                    && d.message.contains("$request")
            }),
            "foreach value variable should be declared, got: {:?}",
            diags
        );
        assert!(
            !diags.iter().any(|d| {
                d.kind == SemanticDiagnosticKind::UndefinedVariable && d.message.contains("$index")
            }),
            "foreach key variable should be declared, got: {:?}",
            diags
        );
    }

    #[test]
    fn test_member_access_counts_variable_as_read() {
        let code = r#"<?php
function run(array $items): void {
    foreach ($items as $item) {
        echo $item->value;
    }
    $names = array_map(static fn ($case) => $case->name, $items);
    echo $names[0] ?? null;
}
"#;
        let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

        for unexpected in ["$item", "$case"] {
            assert!(
                !diags.iter().any(|d| {
                    (d.kind == SemanticDiagnosticKind::UnusedVariable
                        || d.kind == SemanticDiagnosticKind::UnusedParameter)
                        && d.message.contains(unexpected)
                }),
                "Member access receiver `{}` should count as a read, got: {:?}",
                unexpected,
                diags
            );
        }
    }

    #[test]
    fn test_bodyless_method_parameters_are_not_unused() {
        let code = r#"<?php
interface Notifier {
    public function send(string $message, int $priority): void;
}

abstract class BaseHandler {
    abstract public function handle(object $message): array;
}
"#;
        let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

        assert!(
            !diags
                .iter()
                .any(|d| d.kind == SemanticDiagnosticKind::UnusedParameter),
            "Interface/abstract declarations should not report unused params, got: {:?}",
            diags
        );
    }

    #[test]
    fn test_override_unused_parameters_are_not_reported_without_name_hardcode() {
        let code = r#"<?php
namespace Vendor;

class BaseType {
    public function configure(object $builder, array $contractOnly = []): void {
        echo $builder;
        echo $contractOnly;
    }
}

interface VoteContract {
    public function voteOn(object $token, ?object $vote = null): bool;
}

namespace App;

class UserType extends \Vendor\BaseType {
    public function configure(object $builder, array $contractOnly = []): void {
        $builder->add('email');
    }
}

class ConcreteVote implements \Vendor\VoteContract {
    public function voteOn(object $token, ?object $vote = null): bool {
        echo $token;
        return true;
    }
}

class PlainType {
    public function buildForm(object $builder, array $options = []): void {
        echo $builder;
    }
}
"#;
        let diags = parse_and_check_with_file_resolver(code);

        for unexpected in ["$contractOnly", "$vote"] {
            assert!(
                !diags.iter().any(|d| {
                    d.kind == SemanticDiagnosticKind::UnusedParameter
                        && d.message.contains(unexpected)
                }),
                "Override parameter `{}` should not be reported, got: {:?}",
                unexpected,
                diags
            );
        }

        assert!(
            diags.iter().any(|d| {
                d.kind == SemanticDiagnosticKind::UnusedParameter && d.message.contains("$options")
            }),
            "Non-override `buildForm` must still report unused params; no method-name hardcode, got: {:?}",
            diags
        );
    }

    #[test]
    fn test_preg_match_output_argument_declares_variable() {
        let code = r#"<?php
function run(string $content): void {
    if (preg_match('/<id>(\d+)<\/id>/', $content, $m)) {
        echo $m[1];
    }
}
"#;
        let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

        assert!(
            !diags.iter().any(|d| {
                d.kind == SemanticDiagnosticKind::UndefinedVariable && d.message.contains("$m")
            }),
            "preg_match output variable should be declared, got: {:?}",
            diags
        );
    }

    #[test]
    fn test_closure_use_by_reference_is_declared() {
        let code = r#"<?php
function run(): void {
    $persisted = null;
    $callback = function (object $entity) use (&$persisted): void {
        $persisted = $entity;
    };
    $callback(new stdClass());
}
"#;
        let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

        assert!(
            !diags.iter().any(|d| {
                d.kind == SemanticDiagnosticKind::UndefinedVariable
                    && d.message.contains("$persisted")
            }),
            "Closure use variables should be declared inside closures, got: {:?}",
            diags
        );
    }

    #[test]
    fn test_closure_use_counts_as_outer_variable_read() {
        let code = r#"<?php
function run(): void {
    $callCount = 0;
    $callback = function () use (&$callCount): void {
        $callCount++;
    };
    $callback();
}
"#;
        let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

        assert!(
            !diags.iter().any(|d| {
                d.kind == SemanticDiagnosticKind::UnusedVariable && d.message.contains("$callCount")
            }),
            "Closure use variables should count as reads in the outer scope, got: {:?}",
            diags
        );
    }

    #[test]
    fn test_array_destructuring_assignment_declares_variables() {
        let code = r#"<?php
function pair(): array { return [1, 2]; }
function run(): void {
    [$left, $right] = pair();
    echo $left;
    echo $right;
}
"#;
        let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

        for unexpected in ["$left", "$right"] {
            assert!(
                !diags.iter().any(|d| {
                    d.kind == SemanticDiagnosticKind::UndefinedVariable
                        && d.message.contains(unexpected)
                }),
                "Array destructuring target `{}` should be declared, got: {:?}",
                unexpected,
                diags
            );
        }
    }

    #[test]
    fn test_promoted_constructor_property_is_not_unused_parameter() {
        let code = r#"<?php
class Demo {
    public function __construct(private string $logger) {}
    public function run(): void {
        echo $this->logger;
    }
}
"#;
        let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

        assert!(
            !diags.iter().any(|d| {
                d.kind == SemanticDiagnosticKind::UnusedParameter && d.message.contains("$logger")
            }),
            "Promoted constructor property should not be reported as unused parameter, got: {:?}",
            diags
        );
    }

    #[test]
    fn test_duplicate_symbols_in_same_file() {
        let code = r#"<?php
namespace App;

class Duplicate {}
class Duplicate {}
"#;
        let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));
        let duplicates: Vec<_> = diags
            .iter()
            .filter(|d| d.kind == SemanticDiagnosticKind::DuplicateSymbol)
            .collect();

        assert_eq!(
            duplicates.len(),
            2,
            "Expected both duplicate declarations to be reported, got: {:?}",
            duplicates
        );
        assert!(duplicates
            .iter()
            .all(|d| d.message.contains("App\\Duplicate")));
    }
}
