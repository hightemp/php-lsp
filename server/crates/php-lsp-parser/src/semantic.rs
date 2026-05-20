//! Semantic diagnostics for PHP files.
//!
//! Walks the CST and checks class/function/use references
//! against a resolver function (typically backed by the workspace index).

use php_lsp_types::{FileSymbols, SymbolInfo, UseKind};
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
    check_variable_diagnostics(root, source, &mut diagnostics);
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

        if !import_name_is_used(root, source, imported_name, use_stmt.range) {
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
}

#[derive(Debug, Clone)]
struct VariableOccurrence {
    name: String,
    range: (u32, u32, u32, u32),
    start_byte: usize,
    declaration_kind: Option<VariableDeclarationKind>,
}

type ByteRange = (u32, u32, u32, u32);
type SymbolKey<'a> = (php_lsp_types::PhpSymbolKind, &'a str);

fn check_variable_diagnostics(
    root: tree_sitter::Node,
    source: &str,
    diagnostics: &mut Vec<SemanticDiagnostic>,
) {
    check_variables_in_scope(root, source, diagnostics);
}

fn check_variables_in_scope(
    scope: tree_sitter::Node,
    source: &str,
    diagnostics: &mut Vec<SemanticDiagnostic>,
) {
    let mut occurrences = Vec::new();
    collect_variable_occurrences(scope, scope.id(), source, &mut occurrences);
    report_variable_diagnostics(&occurrences, is_variable_scope(scope), diagnostics);

    let mut cursor = scope.walk();
    for child in scope.named_children(&mut cursor) {
        walk_nested_scopes(child, source, diagnostics);
    }
}

fn walk_nested_scopes(
    node: tree_sitter::Node,
    source: &str,
    diagnostics: &mut Vec<SemanticDiagnostic>,
) {
    if is_variable_scope(node) {
        check_variables_in_scope(node, source, diagnostics);
        return;
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_nested_scopes(child, source, diagnostics);
    }
}

fn collect_variable_occurrences(
    node: tree_sitter::Node,
    scope_id: usize,
    source: &str,
    occurrences: &mut Vec<VariableOccurrence>,
) {
    if node.id() != scope_id && is_variable_scope(node) {
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
            });
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_variable_occurrences(child, scope_id, source, occurrences);
    }
}

fn is_non_local_variable_context(node: tree_sitter::Node) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "property_declaration"
            | "property_element"
            | "scoped_property_access_expression"
            | "member_access_expression" => return true,
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

fn report_variable_diagnostics(
    occurrences: &[VariableOccurrence],
    report_unused_declarations: bool,
    diagnostics: &mut Vec<SemanticDiagnostic>,
) {
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
            Some(VariableDeclarationKind::Parameter) => diagnostics.push(SemanticDiagnostic {
                range: first_declaration.range,
                message: format!("Unused parameter: {}", first_declaration.name),
                kind: SemanticDiagnosticKind::UnusedParameter,
            }),
            Some(VariableDeclarationKind::Variable) => diagnostics.push(SemanticDiagnostic {
                range: first_declaration.range,
                message: format!("Unused variable: {}", first_declaration.name),
                kind: SemanticDiagnosticKind::UnusedVariable,
            }),
            Some(VariableDeclarationKind::ClosureUse) | None => {}
        }
    }
}

fn is_variable_scope(node: tree_sitter::Node) -> bool {
    matches!(
        node.kind(),
        "method_declaration"
            | "function_definition"
            | "arrow_function"
            | "anonymous_function"
            | "anonymous_function_creation_expression"
    )
}

fn variable_declaration_kind(
    node: tree_sitter::Node,
    source: &str,
    var_name: &str,
) -> Option<VariableDeclarationKind> {
    let parent = node.parent()?;

    match parent.kind() {
        "simple_parameter" | "property_promotion_parameter" => parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id())
            .then_some(VariableDeclarationKind::Parameter),
        "assignment_expression" => parent
            .child_by_field_name("left")
            .is_some_and(|left| left.id() == node.id() || node_contains(left, node))
            .then_some(VariableDeclarationKind::Variable),
        "foreach_statement" => ["key", "value"]
            .iter()
            .any(|field| {
                parent
                    .child_by_field_name(field)
                    .is_some_and(|field_node| field_node.id() == node.id())
            })
            .then_some(VariableDeclarationKind::Variable),
        "catch_clause" => ["name", "variable"]
            .iter()
            .any(|field| {
                parent
                    .child_by_field_name(field)
                    .is_some_and(|field_node| field_node.id() == node.id())
            })
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

fn node_contains(parent: tree_sitter::Node, child: tree_sitter::Node) -> bool {
    parent.start_byte() <= child.start_byte() && parent.end_byte() >= child.end_byte()
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
