//! Symbol resolution from a CST position.
//!
//! Given a position in a parsed PHP file, determines what symbol is at that
//! position and resolves it to an identifier name, considering namespace context
//! and use statements.

use php_lsp_types::{FileSymbols, UseKind};
use tree_sitter::{Node, Point, Tree};

/// Information about the symbol under the cursor.
#[derive(Debug, Clone)]
pub struct SymbolAtPosition {
    /// The resolved fully qualified name (or best guess).
    pub fqn: String,
    /// The short name as written in source.
    pub name: String,
    /// The kind of reference (class, function, method, property, variable, etc.).
    pub ref_kind: RefKind,
    /// For member access: the object expression text (e.g., "$this", "$foo").
    pub object_expr: Option<String>,
    /// The node text range.
    pub range: (u32, u32, u32, u32),
}

/// What kind of reference is this?
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefKind {
    /// A class/interface/trait/enum name reference.
    ClassName,
    /// A function call.
    FunctionCall,
    /// A method call (->method or ::method).
    MethodCall,
    /// A property access (->property).
    PropertyAccess,
    /// A static property access (::$property).
    StaticPropertyAccess,
    /// A class constant access (::CONST).
    ClassConstant,
    /// A variable ($var).
    Variable,
    /// A namespace name.
    NamespaceName,
    /// Unknown / cannot determine.
    Unknown,
}

/// Find the symbol at the given position in the tree.
pub fn symbol_at_position(
    tree: &Tree,
    source: &str,
    line: u32,
    character: u32,
    file_symbols: &FileSymbols,
) -> Option<SymbolAtPosition> {
    let root = tree.root_node();
    let point = Point::new(line as usize, character as usize);

    // Find the most specific node at the position
    let node = find_node_at_point(root, point)?;

    resolve_node(node, source, file_symbols)
}

/// Find the deepest (most specific) named node at the given point.
fn find_node_at_point(root: Node, point: Point) -> Option<Node> {
    let mut node = root.descendant_for_point_range(point, point)?;

    // If we landed on an unnamed node, try to go to its parent
    while !node.is_named() {
        node = node.parent()?;
    }

    Some(node)
}

/// Resolve a CST node to symbol information.
fn resolve_node(node: Node, source: &str, file_symbols: &FileSymbols) -> Option<SymbolAtPosition> {
    let parent = node.parent()?;
    let node_text = &source[node.byte_range()];
    let parent_kind = parent.kind();

    match parent_kind {
        // Member access: $obj->method() or $obj->property
        "member_access_expression" => {
            let name_field = parent.child_by_field_name("name");
            let object_field = parent.child_by_field_name("object");

            if name_field.map(|n| n.id()) == Some(node.id()) {
                // Cursor is on the member name
                let object_text = object_field.map(|o| source[o.byte_range()].to_string());

                // Check if grandparent is a function call
                let is_call = parent
                    .parent()
                    .map(|gp| gp.kind() == "member_call_expression" || gp.kind() == "function_call_expression")
                    .unwrap_or(false)
                    || parent.kind() == "member_call_expression";

                // Re-check: might actually be member_call_expression itself
                let grandparent_kind = parent.parent().map(|gp| gp.kind()).unwrap_or("");

                let ref_kind = if is_call || grandparent_kind == "member_call_expression" {
                    RefKind::MethodCall
                } else {
                    RefKind::PropertyAccess
                };

                // Try to resolve object type to build a proper FQN
                let class_fqn = object_field
                    .and_then(|o| try_resolve_object_type(o, source, file_symbols));
                let fqn = if let Some(ref cls) = class_fqn {
                    format!("{}::{}", cls, node_text)
                } else {
                    node_text.to_string()
                };

                return Some(SymbolAtPosition {
                    fqn,
                    name: node_text.to_string(),
                    ref_kind,
                    object_expr: object_text,
                    range: node_range(node),
                });
            }

            // Cursor is on the object
            resolve_name_node(node, source, file_symbols)
        }

        // Member call expression: $obj->method()
        "member_call_expression" => {
            let name_field = parent.child_by_field_name("name");
            let object_field = parent.child_by_field_name("object");

            if name_field.map(|n| n.id()) == Some(node.id()) {
                let object_text = object_field.map(|o| source[o.byte_range()].to_string());
                // Try to resolve object type to build a proper FQN
                let class_fqn = object_field
                    .and_then(|o| try_resolve_object_type(o, source, file_symbols));
                let fqn = if let Some(ref cls) = class_fqn {
                    format!("{}::{}", cls, node_text)
                } else {
                    node_text.to_string()
                };
                return Some(SymbolAtPosition {
                    fqn,
                    name: node_text.to_string(),
                    ref_kind: RefKind::MethodCall,
                    object_expr: object_text,
                    range: node_range(node),
                });
            }

            resolve_name_node(node, source, file_symbols)
        }

        // Scoped call expression: ClassName::method()
        "scoped_call_expression" => {
            let name_field = parent.child_by_field_name("name");
            let scope_field = parent.child_by_field_name("scope");

            if name_field.map(|n| n.id()) == Some(node.id()) {
                let scope_text = scope_field.map(|s| source[s.byte_range()].to_string());
                let scope_fqn = scope_text
                    .as_ref()
                    .map(|s| resolve_class_name(s, file_symbols))
                    .unwrap_or_default();

                return Some(SymbolAtPosition {
                    fqn: if scope_fqn.is_empty() {
                        node_text.to_string()
                    } else {
                        format!("{}::{}", scope_fqn, node_text)
                    },
                    name: node_text.to_string(),
                    ref_kind: RefKind::MethodCall,
                    object_expr: scope_text,
                    range: node_range(node),
                });
            }

            // Cursor on scope (class name)
            let resolved = resolve_class_name(node_text, file_symbols);
            Some(SymbolAtPosition {
                fqn: resolved,
                name: node_text.to_string(),
                ref_kind: RefKind::ClassName,
                object_expr: None,
                range: node_range(node),
            })
        }

        // Scoped property access: ClassName::$prop or ClassName::CONST
        "scoped_property_access_expression" => {
            let name_field = parent.child_by_field_name("name");
            let scope_field = parent.child_by_field_name("scope");

            if name_field.map(|n| n.id()) == Some(node.id()) {
                let scope_text = scope_field.map(|s| source[s.byte_range()].to_string());
                let scope_fqn = scope_text
                    .as_ref()
                    .map(|s| resolve_class_name(s, file_symbols))
                    .unwrap_or_default();

                let (ref_kind, member_name) = if node_text.starts_with('$') {
                    (RefKind::StaticPropertyAccess, node_text.to_string())
                } else {
                    (RefKind::ClassConstant, node_text.to_string())
                };

                return Some(SymbolAtPosition {
                    fqn: if scope_fqn.is_empty() {
                        member_name.clone()
                    } else {
                        format!("{}::{}", scope_fqn, member_name)
                    },
                    name: member_name,
                    ref_kind,
                    object_expr: scope_text,
                    range: node_range(node),
                });
            }

            let resolved = resolve_class_name(node_text, file_symbols);
            Some(SymbolAtPosition {
                fqn: resolved,
                name: node_text.to_string(),
                ref_kind: RefKind::ClassName,
                object_expr: None,
                range: node_range(node),
            })
        }

        // Function call
        "function_call_expression" => {
            let func_field = parent.child_by_field_name("function");
            if func_field.map(|n| n.id()) == Some(node.id())
                || (node.kind() == "name" || node.kind() == "qualified_name" || node.kind() == "namespace_name")
            {
                let resolved = resolve_function_name(node_text, file_symbols);
                return Some(SymbolAtPosition {
                    fqn: resolved,
                    name: node_text.to_string(),
                    ref_kind: RefKind::FunctionCall,
                    object_expr: None,
                    range: node_range(node),
                });
            }

            resolve_name_node(node, source, file_symbols)
        }

        // Object creation expression: new ClassName()
        "object_creation_expression" => {
            let resolved = resolve_class_name(node_text, file_symbols);
            Some(SymbolAtPosition {
                fqn: resolved,
                name: node_text.to_string(),
                ref_kind: RefKind::ClassName,
                object_expr: None,
                range: node_range(node),
            })
        }

        // Class declaration, interface, trait, enum — hovering on name
        "class_declaration" | "interface_declaration" | "trait_declaration" | "enum_declaration" => {
            let name_field = parent.child_by_field_name("name");
            if name_field.map(|n| n.id()) == Some(node.id()) {
                let fqn = resolve_class_name(node_text, file_symbols);
                return Some(SymbolAtPosition {
                    fqn,
                    name: node_text.to_string(),
                    ref_kind: RefKind::ClassName,
                    object_expr: None,
                    range: node_range(node),
                });
            }
            None
        }

        // Function/method definition — hovering on name
        "function_definition" | "method_declaration" => {
            let name_field = parent.child_by_field_name("name");
            if name_field.map(|n| n.id()) == Some(node.id()) {
                let fqn = if parent_kind == "method_declaration" {
                    // Try to find parent class FQN
                    find_parent_class_fqn(parent, source, file_symbols)
                        .map(|cls| format!("{}::{}", cls, node_text))
                        .unwrap_or_else(|| node_text.to_string())
                } else {
                    resolve_function_name(node_text, file_symbols)
                };

                let ref_kind = if parent_kind == "method_declaration" {
                    RefKind::MethodCall
                } else {
                    RefKind::FunctionCall
                };

                return Some(SymbolAtPosition {
                    fqn,
                    name: node_text.to_string(),
                    ref_kind,
                    object_expr: None,
                    range: node_range(node),
                });
            }
            None
        }

        // Type hints in signatures, extends, implements, etc.
        "base_clause" | "class_interface_clause" | "type_list" => {
            let resolved = resolve_class_name(node_text, file_symbols);
            Some(SymbolAtPosition {
                fqn: resolved,
                name: node_text.to_string(),
                ref_kind: RefKind::ClassName,
                object_expr: None,
                range: node_range(node),
            })
        }

        // Named type in signatures
        "named_type" | "optional_type" | "union_type" | "intersection_type" => {
            if node.kind() == "name" || node.kind() == "qualified_name" {
                let resolved = resolve_class_name(node_text, file_symbols);
                return Some(SymbolAtPosition {
                    fqn: resolved,
                    name: node_text.to_string(),
                    ref_kind: RefKind::ClassName,
                    object_expr: None,
                    range: node_range(node),
                });
            }
            None
        }

        // Variable
        _ if node.kind() == "variable_name" || (node.kind() == "name" && node_text.starts_with('$')) => {
            Some(SymbolAtPosition {
                fqn: node_text.to_string(),
                name: node_text.to_string(),
                ref_kind: RefKind::Variable,
                object_expr: None,
                range: node_range(node),
            })
        }

        // Qualified name used as type or reference
        _ if node.kind() == "qualified_name" || node.kind() == "name" => {
            resolve_name_node(node, source, file_symbols)
        }

        _ => {
            // Try to resolve as a generic name
            if node.kind() == "name" || node.kind() == "qualified_name" || node.kind() == "namespace_name" {
                resolve_name_node(node, source, file_symbols)
            } else {
                None
            }
        }
    }
}

/// Try to infer the class name from an object expression node.
///
/// Handles common patterns:
/// - `new Foo()` / `(new Foo())` → `Foo`
/// - `$this` → looks up parent class
/// - `Foo::create()` (static call returning self/static) → `Foo`
/// - `ClassName` (as scope in scoped expressions) → `ClassName`
fn try_resolve_object_type<'a>(object_node: Node<'a>, source: &str, file_symbols: &FileSymbols) -> Option<String> {
    let kind = object_node.kind();
    match kind {
        // Direct: new Foo()
        "object_creation_expression" => {
            // The class name is a named child with kind "name" or "qualified_name"
            let child_count = object_node.named_child_count();
            for i in 0..child_count {
                if let Some(child) = object_node.named_child(i) {
                    match child.kind() {
                        "name" | "qualified_name" => {
                            let class_name = &source[child.byte_range()];
                            return Some(resolve_class_name(class_name, file_symbols));
                        }
                        _ => {}
                    }
                }
            }
            None
        }
        // Parenthesized: (new Foo())
        "parenthesized_expression" => {
            // Look for object_creation_expression inside
            let child_count = object_node.named_child_count();
            for i in 0..child_count {
                if let Some(child) = object_node.named_child(i) {
                    if let Some(resolved) = try_resolve_object_type(child, source, file_symbols) {
                        return Some(resolved);
                    }
                }
            }
            None
        }
        // $this → find enclosing class
        "variable_name" => {
            let text = &source[object_node.byte_range()];
            if text == "$this" {
                find_parent_class_fqn(object_node, source, file_symbols)
            } else {
                None
            }
        }
        // Name / qualified_name might be a class used as scope
        "name" | "qualified_name" => {
            let text = &source[object_node.byte_range()];
            Some(resolve_class_name(text, file_symbols))
        }
        // Member call chain: $obj->foo()->bar() — can't resolve without full type inference
        // Static call: Foo::create() — can't resolve return type without type info
        _ => None,
    }
}

/// Resolve a simple name node to a SymbolAtPosition.
fn resolve_name_node(node: Node, source: &str, file_symbols: &FileSymbols) -> Option<SymbolAtPosition> {
    let text = &source[node.byte_range()];

    if text.starts_with('$') {
        return Some(SymbolAtPosition {
            fqn: text.to_string(),
            name: text.to_string(),
            ref_kind: RefKind::Variable,
            object_expr: None,
            range: node_range(node),
        });
    }

    // Try to resolve as class name first
    let resolved = resolve_class_name(text, file_symbols);
    Some(SymbolAtPosition {
        fqn: resolved,
        name: text.to_string(),
        ref_kind: RefKind::ClassName,
        object_expr: None,
        range: node_range(node),
    })
}

/// Resolve a class name using use statements and current namespace (public API).
pub fn resolve_class_name_pub(name: &str, file_symbols: &FileSymbols) -> String {
    resolve_class_name(name, file_symbols)
}

/// Resolve a class name using use statements and current namespace.
fn resolve_class_name(name: &str, file_symbols: &FileSymbols) -> String {
    // Already fully qualified
    if name.starts_with('\\') {
        return name.trim_start_matches('\\').to_string();
    }

    // Special names
    match name {
        "self" | "static" | "parent" | "$this" => return name.to_string(),
        _ => {}
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
            .unwrap_or_else(|| {
                use_stmt.fqn.rsplit('\\').next().unwrap_or(&use_stmt.fqn)
            });

        if alias == first_part {
            if parts.len() == 1 {
                return use_stmt.fqn.clone();
            } else {
                // Partial match: use App\Foo; then Foo\Bar → App\Foo\Bar
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

/// Resolve a function name using use statements and current namespace.
fn resolve_function_name(name: &str, file_symbols: &FileSymbols) -> String {
    // Already fully qualified
    if name.starts_with('\\') {
        return name.trim_start_matches('\\').to_string();
    }

    // Try use statements (function kind)
    for use_stmt in &file_symbols.use_statements {
        if use_stmt.kind != UseKind::Function {
            continue;
        }

        let alias = use_stmt
            .alias
            .as_deref()
            .unwrap_or_else(|| {
                use_stmt.fqn.rsplit('\\').next().unwrap_or(&use_stmt.fqn)
            });

        if alias == name {
            return use_stmt.fqn.clone();
        }
    }

    // For functions, try namespace-qualified first, then global
    if let Some(ref ns) = file_symbols.namespace {
        format!("{}\\{}", ns, name)
    } else {
        name.to_string()
    }
}

/// Try to find the FQN of the class containing a method node.
fn find_parent_class_fqn(method_node: Node, source: &str, file_symbols: &FileSymbols) -> Option<String> {
    let mut current = method_node.parent();
    while let Some(node) = current {
        match node.kind() {
            "class_declaration" | "interface_declaration" | "trait_declaration" | "enum_declaration" => {
                let name_node = node.child_by_field_name("name")?;
                let name = &source[name_node.byte_range()];
                return Some(resolve_class_name(name, file_symbols));
            }
            _ => {
                current = node.parent();
            }
        }
    }
    None
}

fn node_range(node: Node) -> (u32, u32, u32, u32) {
    let start = node.start_position();
    let end = node.end_position();
    (
        start.row as u32,
        start.column as u32,
        end.row as u32,
        end.column as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::FileParser;
    use crate::symbols::extract_file_symbols;

    fn parse_and_resolve(code: &str, line: u32, col: u32) -> Option<SymbolAtPosition> {
        let mut parser = FileParser::new();
        parser.parse_full(code);
        let tree = parser.tree().unwrap();
        let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
        symbol_at_position(tree, code, line, col, &file_symbols)
    }

    #[test]
    fn test_resolve_class_name_with_use() {
        let code = "<?php\nuse App\\Service\\UserService;\n\nnew UserService();\n";
        // "UserService" in "new UserService()" is at line 3
        let result = parse_and_resolve(code, 3, 5);
        assert!(result.is_some());
        let sym = result.unwrap();
        assert_eq!(sym.fqn, "App\\Service\\UserService");
        assert_eq!(sym.ref_kind, RefKind::ClassName);
    }

    #[test]
    fn test_resolve_function_call() {
        let code = "<?php\nnamespace App;\n\nstrlen('hello');\n";
        let result = parse_and_resolve(code, 3, 0);
        assert!(result.is_some());
        let sym = result.unwrap();
        assert_eq!(sym.ref_kind, RefKind::FunctionCall);
    }

    #[test]
    fn test_resolve_class_definition() {
        let code = "<?php\nnamespace App;\n\nclass Foo {\n}\n";
        // "Foo" is at line 3, col 6
        let result = parse_and_resolve(code, 3, 6);
        assert!(result.is_some());
        let sym = result.unwrap();
        assert_eq!(sym.name, "Foo");
        assert_eq!(sym.fqn, "App\\Foo");
    }

    #[test]
    fn test_resolve_method_call_on_new() {
        // (new Foo())->increment(5)
        let code = "<?php\nnamespace App;\nuse App\\Foo;\n\n(new Foo())->increment(5);\n";
        // "increment" is at line 4, col 13
        let result = parse_and_resolve(code, 4, 13);
        assert!(result.is_some(), "Should resolve method call on new expression");
        let sym = result.unwrap();
        assert_eq!(sym.name, "increment");
        assert_eq!(sym.ref_kind, RefKind::MethodCall);
        assert_eq!(sym.fqn, "App\\Foo::increment");
    }

    #[test]
    fn test_resolve_method_call_on_this() {
        let code = "<?php\nnamespace App;\n\nclass Foo {\n    public function bar(): void {\n        $this->baz();\n    }\n}\n";
        // "baz" in "$this->baz()" at line 5, col 16
        let result = parse_and_resolve(code, 5, 16);
        assert!(result.is_some(), "Should resolve method call on $this");
        let sym = result.unwrap();
        assert_eq!(sym.name, "baz");
        assert_eq!(sym.ref_kind, RefKind::MethodCall);
        assert_eq!(sym.fqn, "App\\Foo::baz");
    }

    #[test]
    fn test_resolve_property_access_on_this() {
        let code = "<?php\nnamespace App;\n\nclass Foo {\n    private string $name;\n    public function bar(): string {\n        return $this->name;\n    }\n}\n";
        // "name" in "$this->name" at line 6, col 22
        let result = parse_and_resolve(code, 6, 22);
        assert!(result.is_some(), "Should resolve property access on $this");
        let sym = result.unwrap();
        assert_eq!(sym.name, "name");
        assert_eq!(sym.fqn, "App\\Foo::name");
    }

    #[test]
    fn test_resolve_fully_qualified() {
        let code = "<?php\n\\DateTime::createFromFormat('Y-m-d', '2024-01-01');\n";
        // \\DateTime at line 1
        let result = parse_and_resolve(code, 1, 1);
        assert!(result.is_some());
    }
}
