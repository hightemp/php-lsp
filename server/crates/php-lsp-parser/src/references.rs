//! Find references to a symbol within a single file's CST.
//!
//! Given a target FQN and the file's CST + symbols, returns all locations
//! in the file that reference the target.

use php_lsp_types::{FileSymbols, PhpSymbolKind, UseKind};
use tree_sitter::{Node, Tree};

/// A location within a file where a reference was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceLocation {
    pub range: (u32, u32, u32, u32),
}

/// Find all references to the given FQN within a single file.
///
/// `target_fqn` is the fully qualified name to search for.
/// `target_kind` helps narrow the search (class vs function vs member).
/// `include_declaration` if true, also includes the declaration site.
pub fn find_references_in_file(
    tree: &Tree,
    source: &str,
    file_symbols: &FileSymbols,
    target_fqn: &str,
    target_kind: PhpSymbolKind,
    include_declaration: bool,
) -> Vec<ReferenceLocation> {
    let mut results = Vec::new();
    let root = tree.root_node();

    match target_kind {
        PhpSymbolKind::Class
        | PhpSymbolKind::Interface
        | PhpSymbolKind::Trait
        | PhpSymbolKind::Enum => {
            find_class_references(
                root,
                source,
                file_symbols,
                target_fqn,
                include_declaration,
                &mut results,
            );
        }
        PhpSymbolKind::Function => {
            find_function_references(
                root,
                source,
                file_symbols,
                target_fqn,
                include_declaration,
                &mut results,
            );
        }
        PhpSymbolKind::Method
        | PhpSymbolKind::Property
        | PhpSymbolKind::ClassConstant
        | PhpSymbolKind::EnumCase => {
            find_member_references(
                root,
                source,
                file_symbols,
                target_fqn,
                include_declaration,
                &mut results,
            );
        }
        PhpSymbolKind::GlobalConstant => {
            find_constant_references(
                root,
                source,
                file_symbols,
                target_fqn,
                include_declaration,
                &mut results,
            );
        }
        PhpSymbolKind::Namespace => {
            // Namespace references not typically searched
        }
    }

    results
}

/// Find all references to a class/interface/trait/enum in a file.
fn find_class_references(
    root: Node,
    source: &str,
    file_symbols: &FileSymbols,
    target_fqn: &str,
    include_declaration: bool,
    results: &mut Vec<ReferenceLocation>,
) {
    // Check declarations in this file
    if include_declaration {
        for sym in &file_symbols.symbols {
            if sym.fqn == target_fqn
                && matches!(
                    sym.kind,
                    PhpSymbolKind::Class
                        | PhpSymbolKind::Interface
                        | PhpSymbolKind::Trait
                        | PhpSymbolKind::Enum
                )
            {
                results.push(ReferenceLocation {
                    range: sym.selection_range,
                });
            }
        }
    }

    // Walk the CST looking for name nodes that resolve to the target FQN
    walk_for_class_refs(root, source, file_symbols, target_fqn, results);
}

/// Recursively walk the CST to find class name references.
fn walk_for_class_refs(
    node: Node,
    source: &str,
    file_symbols: &FileSymbols,
    target_fqn: &str,
    results: &mut Vec<ReferenceLocation>,
) {
    let kind = node.kind();

    // Check nodes that can contain class name references
    match kind {
        // new ClassName()
        "object_creation_expression" => {
            // The class name is a direct child (name or qualified_name node)
            let cursor = &mut node.walk();
            for child in node.named_children(cursor) {
                if child.kind() == "name" || child.kind() == "qualified_name" {
                    check_class_name_ref(child, source, file_symbols, target_fqn, results);
                    break;
                }
            }
        }

        // ClassName::method() or ClassName::$prop or ClassName::CONST
        "scoped_call_expression" | "scoped_property_access_expression" => {
            if let Some(scope_node) = node.child_by_field_name("scope") {
                check_class_name_ref(scope_node, source, file_symbols, target_fqn, results);
            }
        }

        // Type hints: function(ClassName $x): ClassName
        "named_type" => {
            // named_type contains a child name or qualified_name
            let cursor = &mut node.walk();
            for child in node.named_children(cursor) {
                if child.kind() == "name" || child.kind() == "qualified_name" {
                    check_class_name_ref(child, source, file_symbols, target_fqn, results);
                }
            }
            // Also check the node itself if it's a name (fallback)
            if node.named_child_count() == 0 {
                check_class_name_ref(node, source, file_symbols, target_fqn, results);
            }
        }

        // extends/implements
        "base_clause" | "class_interface_clause" => {
            let cursor = &mut node.walk();
            for child in node.named_children(cursor) {
                if child.kind() == "name" || child.kind() == "qualified_name" {
                    check_class_name_ref(child, source, file_symbols, target_fqn, results);
                }
            }
        }

        // instanceof
        "instanceof_expression" => {
            if let Some(right) = node.child_by_field_name("right") {
                check_class_name_ref(right, source, file_symbols, target_fqn, results);
            }
        }

        // catch clause
        "catch_clause" => {
            if let Some(type_node) = node.child_by_field_name("type") {
                let cursor = &mut type_node.walk();
                for child in type_node.named_children(cursor) {
                    if child.kind() == "name" || child.kind() == "qualified_name" {
                        check_class_name_ref(child, source, file_symbols, target_fqn, results);
                    }
                }
                // Also check the type node itself if it's a name
                if type_node.kind() == "name" || type_node.kind() == "qualified_name" {
                    check_class_name_ref(type_node, source, file_symbols, target_fqn, results);
                }
            }
        }

        _ => {}
    }

    // Recurse into children
    let cursor = &mut node.walk();
    for child in node.named_children(cursor) {
        walk_for_class_refs(child, source, file_symbols, target_fqn, results);
    }
}

/// Check if a node is a class name reference to the target FQN.
fn check_class_name_ref(
    node: Node,
    source: &str,
    file_symbols: &FileSymbols,
    target_fqn: &str,
    results: &mut Vec<ReferenceLocation>,
) {
    let text = &source[node.byte_range()];
    let resolved = resolve_name_to_fqn(text, file_symbols);

    if resolved == target_fqn {
        let start = node.start_position();
        let end = node.end_position();
        results.push(ReferenceLocation {
            range: (
                start.row as u32,
                start.column as u32,
                end.row as u32,
                end.column as u32,
            ),
        });
    }
}

/// Resolve a name to FQN using use statements and namespace context.
fn resolve_name_to_fqn(name: &str, file_symbols: &FileSymbols) -> String {
    // Already fully qualified
    if name.starts_with('\\') {
        return name.trim_start_matches('\\').to_string();
    }

    // Special names
    match name {
        "self" | "static" | "parent" | "$this" | "string" | "int" | "float" | "bool" | "array"
        | "callable" | "iterable" | "object" | "mixed" | "void" | "never" | "null" | "false"
        | "true" => return name.to_string(),
        _ => {}
    }

    // Try use statements
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

    // Prepend namespace
    if let Some(ref ns) = file_symbols.namespace {
        format!("{}\\{}", ns, name)
    } else {
        name.to_string()
    }
}

/// Find all references to a function in a file.
fn find_function_references(
    root: Node,
    source: &str,
    file_symbols: &FileSymbols,
    target_fqn: &str,
    include_declaration: bool,
    results: &mut Vec<ReferenceLocation>,
) {
    if include_declaration {
        for sym in &file_symbols.symbols {
            if sym.fqn == target_fqn && sym.kind == PhpSymbolKind::Function {
                results.push(ReferenceLocation {
                    range: sym.selection_range,
                });
            }
        }
    }

    walk_for_function_refs(root, source, file_symbols, target_fqn, results);
}

/// Walk CST looking for function call references.
fn walk_for_function_refs(
    node: Node,
    source: &str,
    file_symbols: &FileSymbols,
    target_fqn: &str,
    results: &mut Vec<ReferenceLocation>,
) {
    if node.kind() == "function_call_expression" {
        if let Some(func_node) = node.child_by_field_name("function") {
            let text = &source[func_node.byte_range()];
            let resolved = resolve_function_name_to_fqn(text, file_symbols);
            if resolved == target_fqn {
                let start = func_node.start_position();
                let end = func_node.end_position();
                results.push(ReferenceLocation {
                    range: (
                        start.row as u32,
                        start.column as u32,
                        end.row as u32,
                        end.column as u32,
                    ),
                });
            }
        }
    }

    let cursor = &mut node.walk();
    for child in node.named_children(cursor) {
        walk_for_function_refs(child, source, file_symbols, target_fqn, results);
    }
}

/// Resolve a function name to FQN.
fn resolve_function_name_to_fqn(name: &str, file_symbols: &FileSymbols) -> String {
    if name.starts_with('\\') {
        return name.trim_start_matches('\\').to_string();
    }

    // Try use statements (function)
    for use_stmt in &file_symbols.use_statements {
        if use_stmt.kind != UseKind::Function {
            continue;
        }

        let alias = use_stmt
            .alias
            .as_deref()
            .unwrap_or_else(|| use_stmt.fqn.rsplit('\\').next().unwrap_or(&use_stmt.fqn));

        if alias == name {
            return use_stmt.fqn.clone();
        }
    }

    // Namespace-qualified
    if let Some(ref ns) = file_symbols.namespace {
        format!("{}\\{}", ns, name)
    } else {
        name.to_string()
    }
}

/// Find all references to a class member (method, property, class constant, enum case).
fn find_member_references(
    root: Node,
    source: &str,
    file_symbols: &FileSymbols,
    target_fqn: &str,
    include_declaration: bool,
    results: &mut Vec<ReferenceLocation>,
) {
    // Parse the target FQN: "ClassName::memberName"
    let member_name = if let Some(pos) = target_fqn.rfind("::") {
        &target_fqn[pos + 2..]
    } else {
        return;
    };

    if include_declaration {
        for sym in &file_symbols.symbols {
            if sym.fqn == target_fqn {
                results.push(ReferenceLocation {
                    range: sym.selection_range,
                });
            }
        }
    }

    walk_for_member_refs(root, source, file_symbols, target_fqn, member_name, results);
}

/// Walk CST for member access references.
fn walk_for_member_refs(
    node: Node,
    source: &str,
    _file_symbols: &FileSymbols,
    target_fqn: &str,
    member_name: &str,
    results: &mut Vec<ReferenceLocation>,
) {
    let kind = node.kind();

    match kind {
        // $obj->method() or $obj->property
        "member_access_expression" | "member_call_expression" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let text = &source[name_node.byte_range()];
                // Simple name match (without type resolution for now)
                if text == member_name {
                    let start = name_node.start_position();
                    let end = name_node.end_position();
                    results.push(ReferenceLocation {
                        range: (
                            start.row as u32,
                            start.column as u32,
                            end.row as u32,
                            end.column as u32,
                        ),
                    });
                }
            }
        }

        // ClassName::method() or ClassName::$prop or ClassName::CONST
        "scoped_call_expression" | "scoped_property_access_expression" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let text = &source[name_node.byte_range()];
                if text == member_name {
                    // For scoped access, also check that the scope resolves to the right class
                    if let Some(scope_node) = node.child_by_field_name("scope") {
                        let scope_text = &source[scope_node.byte_range()];
                        let scope_fqn = resolve_name_to_fqn(scope_text, _file_symbols);
                        let expected_class = &target_fqn[..target_fqn.rfind("::").unwrap_or(0)];

                        if scope_fqn == expected_class
                            || scope_text == "self"
                            || scope_text == "static"
                            || scope_text == "parent"
                        {
                            let start = name_node.start_position();
                            let end = name_node.end_position();
                            results.push(ReferenceLocation {
                                range: (
                                    start.row as u32,
                                    start.column as u32,
                                    end.row as u32,
                                    end.column as u32,
                                ),
                            });
                        }
                    }
                }
            }
        }

        _ => {}
    }

    let cursor = &mut node.walk();
    for child in node.named_children(cursor) {
        walk_for_member_refs(child, source, _file_symbols, target_fqn, member_name, results);
    }
}

/// Find references to a global constant.
fn find_constant_references(
    root: Node,
    source: &str,
    file_symbols: &FileSymbols,
    target_fqn: &str,
    include_declaration: bool,
    results: &mut Vec<ReferenceLocation>,
) {
    if include_declaration {
        for sym in &file_symbols.symbols {
            if sym.fqn == target_fqn && sym.kind == PhpSymbolKind::GlobalConstant {
                results.push(ReferenceLocation {
                    range: sym.selection_range,
                });
            }
        }
    }

    // Constants are referenced as plain names — similar to class names
    walk_for_constant_refs(root, source, file_symbols, target_fqn, results);
}

/// Walk CST for constant references.
fn walk_for_constant_refs(
    node: Node,
    source: &str,
    file_symbols: &FileSymbols,
    target_fqn: &str,
    results: &mut Vec<ReferenceLocation>,
) {
    // Constants appear as "name" nodes that are not function calls, class names, etc.
    if node.kind() == "name" || node.kind() == "qualified_name" {
        let parent = node.parent();
        let parent_kind = parent.map(|p| p.kind()).unwrap_or("");

        // Skip nodes that are part of other constructs
        if parent_kind != "function_call_expression"
            && parent_kind != "object_creation_expression"
            && parent_kind != "class_declaration"
            && parent_kind != "interface_declaration"
            && parent_kind != "trait_declaration"
            && parent_kind != "enum_declaration"
            && parent_kind != "function_definition"
            && parent_kind != "named_type"
            && parent_kind != "use_declaration"
            && parent_kind != "namespace_use_clause"
        {
            let text = &source[node.byte_range()];
            // Try resolving as constant
            let resolved = resolve_name_to_fqn(text, file_symbols);
            if resolved == target_fqn {
                let start = node.start_position();
                let end = node.end_position();
                results.push(ReferenceLocation {
                    range: (
                        start.row as u32,
                        start.column as u32,
                        end.row as u32,
                        end.column as u32,
                    ),
                });
            }
        }
    }

    let cursor = &mut node.walk();
    for child in node.named_children(cursor) {
        walk_for_constant_refs(child, source, file_symbols, target_fqn, results);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::FileParser;
    use crate::symbols::extract_file_symbols;

    fn find_refs(code: &str, target_fqn: &str, kind: PhpSymbolKind) -> Vec<ReferenceLocation> {
        let mut parser = FileParser::new();
        parser.parse_full(code);
        let tree = parser.tree().unwrap();
        let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
        find_references_in_file(tree, code, &file_symbols, target_fqn, kind, true)
    }

    #[test]
    fn test_find_class_references_new() {
        let code = r#"<?php
namespace App;

use App\Service\UserService;

$svc = new UserService();
$svc2 = new UserService();
"#;
        let refs = find_refs(code, "App\\Service\\UserService", PhpSymbolKind::Class);
        assert_eq!(refs.len(), 2, "Should find 2 new-expression references");
    }

    #[test]
    fn test_find_class_references_type_hint() {
        let code = r#"<?php
namespace App;

use App\Model\User;

class Controller {
    public function show(User $user): User {
        return $user;
    }
}
"#;
        let refs = find_refs(code, "App\\Model\\User", PhpSymbolKind::Class);
        // Should find type hint in param + return type = 2
        assert!(refs.len() >= 2, "Should find at least 2 type hint references, found {}", refs.len());
    }

    #[test]
    fn test_find_function_references() {
        let code = r#"<?php
namespace App;

function helper() {}

helper();
helper();
"#;
        let refs = find_refs(code, "App\\helper", PhpSymbolKind::Function);
        // 1 declaration + 2 calls = 3
        assert_eq!(refs.len(), 3, "Should find declaration + 2 calls");
    }

    #[test]
    fn test_find_static_method_references() {
        let code = r#"<?php
namespace App;

class Foo {
    public static function bar() {}
}

Foo::bar();
"#;
        let refs = find_refs(code, "App\\Foo::bar", PhpSymbolKind::Method);
        // declaration + 1 call = 2
        assert!(!refs.is_empty(), "Should find at least 1 reference");
    }
}
