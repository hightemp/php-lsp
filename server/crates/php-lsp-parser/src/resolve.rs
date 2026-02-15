//! Symbol resolution from a CST position.
//!
//! Given a position in a parsed PHP file, determines what symbol is at that
//! position and resolves it to an identifier name, considering namespace context
//! and use statements.

use crate::phpdoc::parse_phpdoc;
use php_lsp_types::{FileSymbols, TypeInfo, UseKind};
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
    /// A global/user constant (CONST_NAME).
    GlobalConstant,
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

/// Find local variable definition range for the variable under cursor.
///
/// This supports function/method parameters and assignment-based definitions
/// in the current scope (function body or top-level program).
pub fn variable_definition_at_position(
    tree: &Tree,
    source: &str,
    line: u32,
    character: u32,
) -> Option<(u32, u32, u32, u32)> {
    let root = tree.root_node();
    let point = Point::new(line as usize, character as usize);
    let mut node = find_node_at_point(root, point)?;

    // Climb to a variable-like node.
    loop {
        let text = &source[node.byte_range()];
        if node.kind() == "variable_name" || text.starts_with('$') {
            break;
        }
        node = node.parent()?;
    }

    let var_name = normalize_var_name(&source[node.byte_range()]);
    let usage_start = node.start_byte();
    let scope = find_enclosing_function(node).unwrap_or(root);

    let mut best: Option<(usize, (u32, u32, u32, u32))> = None;
    find_variable_definition_before(scope, &var_name, usage_start, source, &mut best);

    best.map(|(_, range)| range)
}

/// Infer variable type by name before a given position.
///
/// This is used by completion to resolve `$var->...` when cursor is at `...`.
pub fn infer_variable_type_at_position(
    tree: &Tree,
    source: &str,
    file_symbols: &FileSymbols,
    line: u32,
    character: u32,
    var_name: &str,
) -> Option<String> {
    let root = tree.root_node();
    let point = Point::new(line as usize, character as usize);
    let node = find_node_at_point(root, point).unwrap_or(root);
    let usage_start = position_to_byte(source, line, character);
    let normalized = normalize_var_name(var_name);
    let scope = find_enclosing_function(node).unwrap_or_else(|| find_root_node(node));
    infer_variable_type_in_scope(scope, &normalized, usage_start, source, file_symbols)
}

/// Find the deepest (most specific) named node at the given point.
fn find_node_at_point(root: Node, point: Point) -> Option<Node> {
    let mut node = root.descendant_for_point_range(point, point)?;

    // If we landed on an unnamed node, try to go to its parent
    while !node.is_named() {
        node = node.parent()?;
    }

    // Prefer the full variable node when cursor is inside "$name" token.
    if node.kind() == "name" {
        if let Some(parent) = node.parent() {
            if parent.kind() == "variable_name" {
                node = parent;
            }
        }
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
                let ref_kind = RefKind::PropertyAccess;

                // Try to resolve object type to build a proper FQN
                let property_name = if node_text.starts_with('$') {
                    node_text.to_string()
                } else {
                    format!("${}", node_text)
                };
                let class_fqn =
                    object_field.and_then(|o| try_resolve_object_type(o, source, file_symbols));
                let fqn = if let Some(ref cls) = class_fqn {
                    format!("{}::{}", cls, property_name)
                } else {
                    property_name
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
                let class_fqn =
                    object_field.and_then(|o| try_resolve_object_type(o, source, file_symbols));
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
                    .map(|s| match s.as_str() {
                        "self" | "static" => find_parent_class_fqn(parent, source, file_symbols)
                            .unwrap_or_else(|| s.to_string()),
                        _ => resolve_class_name(s, file_symbols),
                    })
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
            let resolved = match node_text {
                "self" | "static" => find_parent_class_fqn(parent, source, file_symbols)
                    .unwrap_or_else(|| resolve_class_name(node_text, file_symbols)),
                _ => resolve_class_name(node_text, file_symbols),
            };
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
                    .map(|s| match s.as_str() {
                        "self" | "static" => find_parent_class_fqn(parent, source, file_symbols)
                            .unwrap_or_else(|| s.to_string()),
                        _ => resolve_class_name(s, file_symbols),
                    })
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

            let resolved = match node_text {
                "self" | "static" => find_parent_class_fqn(parent, source, file_symbols)
                    .unwrap_or_else(|| resolve_class_name(node_text, file_symbols)),
                _ => resolve_class_name(node_text, file_symbols),
            };
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
                || (node.kind() == "name"
                    || node.kind() == "qualified_name"
                    || node.kind() == "namespace_name")
            {
                let function_text = func_field
                    .map(|n| &source[n.byte_range()])
                    .unwrap_or(node_text);
                let resolved = resolve_function_name(function_text, file_symbols);
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

        // Child name inside qualified_name used by function call (e.g. App\Utils\fn()).
        "qualified_name" | "namespace_name"
            if parent
                .parent()
                .map(|gp| gp.kind() == "function_call_expression")
                .unwrap_or(false) =>
        {
            let qname_text = &source[parent.byte_range()];
            let resolved = resolve_function_name(qname_text, file_symbols);
            Some(SymbolAtPosition {
                fqn: resolved,
                name: node_text.to_string(),
                ref_kind: RefKind::FunctionCall,
                object_expr: None,
                range: node_range(node),
            })
        }

        // Class constant access: self::CONST / ClassName::CONST
        "class_constant_access_expression" => {
            let scope_node = parent.named_child(0);
            let name_node = parent.named_child(1);

            if name_node.map(|n| n.id()) == Some(node.id()) {
                let scope_text = scope_node.map(|s| source[s.byte_range()].to_string());
                let scope_fqn = scope_text
                    .as_ref()
                    .map(|s| match s.as_str() {
                        "self" | "static" => find_parent_class_fqn(parent, source, file_symbols)
                            .unwrap_or_else(|| s.to_string()),
                        _ => resolve_class_name(s, file_symbols),
                    })
                    .unwrap_or_default();
                return Some(SymbolAtPosition {
                    fqn: if scope_fqn.is_empty() {
                        node_text.to_string()
                    } else {
                        format!("{}::{}", scope_fqn, node_text)
                    },
                    name: node_text.to_string(),
                    ref_kind: RefKind::ClassConstant,
                    object_expr: scope_text,
                    range: node_range(node),
                });
            }

            if scope_node.map(|n| n.id()) == Some(node.id()) {
                let resolved = match node_text {
                    "self" | "static" => find_parent_class_fqn(parent, source, file_symbols)
                        .unwrap_or_else(|| resolve_class_name(node_text, file_symbols)),
                    _ => resolve_class_name(node_text, file_symbols),
                };
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
        "class_declaration"
        | "interface_declaration"
        | "trait_declaration"
        | "enum_declaration" => {
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
        _ if node.kind() == "variable_name"
            || (node.kind() == "name" && node_text.starts_with('$')) =>
        {
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
            if node.kind() == "name"
                || node.kind() == "qualified_name"
                || node.kind() == "namespace_name"
            {
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
fn try_resolve_object_type<'a>(
    object_node: Node<'a>,
    source: &str,
    file_symbols: &FileSymbols,
) -> Option<String> {
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
        // $var → try to infer type from assignment or parameter
        "variable_name" => {
            let text = &source[object_node.byte_range()];
            if text == "$this" {
                find_parent_class_fqn(object_node, source, file_symbols)
            } else {
                infer_variable_type(object_node, text, source, file_symbols)
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

/// Infer the type of a variable by scanning for assignments and typed parameters.
///
/// Handles:
/// - `$var = new ClassName()` → ClassName
/// - `function foo(ClassName $var)` → ClassName (typed parameter)
fn infer_variable_type(
    var_node: Node,
    var_name: &str,
    source: &str,
    file_symbols: &FileSymbols,
) -> Option<String> {
    let scope = find_enclosing_function(var_node).unwrap_or_else(|| find_root_node(var_node));
    infer_variable_type_in_scope(scope, var_name, var_node.start_byte(), source, file_symbols)
}

/// Find the enclosing function/method node.
fn find_enclosing_function(node: Node) -> Option<Node> {
    let mut current = node.parent();
    while let Some(n) = current {
        match n.kind() {
            "method_declaration"
            | "function_definition"
            | "arrow_function"
            | "anonymous_function_creation_expression" => {
                return Some(n);
            }
            _ => current = n.parent(),
        }
    }
    None
}

fn find_root_node(node: Node) -> Node {
    let mut current = node;
    while let Some(parent) = current.parent() {
        current = parent;
    }
    current
}

fn infer_variable_type_in_scope(
    scope_node: Node,
    var_name: &str,
    usage_start: usize,
    source: &str,
    file_symbols: &FileSymbols,
) -> Option<String> {
    // 1. Check function parameters for typed variables
    if let Some(params) = scope_node.child_by_field_name("parameters") {
        for i in 0..params.named_child_count() {
            if let Some(param) = params.named_child(i) {
                if param.kind() == "simple_parameter"
                    || param.kind() == "property_promotion_parameter"
                {
                    if let Some(name_node) = param.child_by_field_name("name") {
                        let param_name = normalize_var_name(&source[name_node.byte_range()]);
                        if param_name == var_name {
                            if let Some(type_node) = param.child_by_field_name("type") {
                                if let Some(class_name) = extract_type_name(type_node, source) {
                                    return Some(resolve_class_name(&class_name, file_symbols));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // 2. Scan statements before usage for assignments and inline @var docs.
    let statements = scope_node.child_by_field_name("body").unwrap_or(scope_node);
    find_variable_type_before_usage(statements, var_name, usage_start, source, file_symbols)
}

/// Extract a type name from a type node (named_type, optional_type, etc.).
fn extract_type_name(type_node: Node, source: &str) -> Option<String> {
    match type_node.kind() {
        "named_type" => {
            // named_type contains a name or qualified_name child
            for i in 0..type_node.named_child_count() {
                if let Some(child) = type_node.named_child(i) {
                    if child.kind() == "name" || child.kind() == "qualified_name" {
                        return Some(source[child.byte_range()].to_string());
                    }
                }
            }
            None
        }
        "optional_type" => {
            // ?Type — recurse into inner type
            for i in 0..type_node.named_child_count() {
                if let Some(child) = type_node.named_child(i) {
                    if let Some(name) = extract_type_name(child, source) {
                        return Some(name);
                    }
                }
            }
            None
        }
        "name" | "qualified_name" => Some(source[type_node.byte_range()].to_string()),
        _ => None,
    }
}

/// Scan a compound_statement for `$var = new ClassName()` before the usage point.
fn find_variable_type_before_usage(
    body: Node,
    var_name: &str,
    usage_start: usize,
    source: &str,
    file_symbols: &FileSymbols,
) -> Option<String> {
    let mut inferred: Option<(usize, String)> = None;

    for i in 0..body.named_child_count() {
        let stmt = match body.named_child(i) {
            Some(s) => s,
            None => continue,
        };

        // Only look at statements before the usage point
        if stmt.start_byte() >= usage_start {
            break;
        }

        let assignment_rhs = assignment_rhs_for_var(stmt, var_name, source);

        // Inline PHPDoc immediately before statement:
        //  - apply named @var always when variable matches
        //  - unnamed @var only for direct assignment to target variable
        if let Some(doc_type) = extract_preceding_phpdoc_var_type(
            stmt,
            var_name,
            assignment_rhs.is_some(),
            source,
            file_symbols,
        ) {
            inferred = Some((stmt.start_byte(), doc_type));
            continue;
        }

        // Assignment inference: $var = <expr>;
        if let Some(right) = assignment_rhs {
            if let Some(resolved) = try_resolve_object_type(right, source, file_symbols) {
                inferred = Some((stmt.start_byte(), resolved));
            }
        }
    }

    inferred.map(|(_, ty)| ty)
}

fn assignment_rhs_for_var<'a>(stmt: Node<'a>, var_name: &str, source: &str) -> Option<Node<'a>> {
    if stmt.kind() != "expression_statement" {
        return None;
    }

    let expr = stmt.named_child(0)?;
    if expr.kind() != "assignment_expression" {
        return None;
    }

    let left = expr.child_by_field_name("left")?;
    let right = expr.child_by_field_name("right")?;
    let left_text = normalize_var_name(&source[left.byte_range()]);
    if left_text == var_name {
        Some(right)
    } else {
        None
    }
}

fn extract_preceding_phpdoc_var_type(
    stmt: Node,
    var_name: &str,
    allow_unnamed_var_tag: bool,
    source: &str,
    file_symbols: &FileSymbols,
) -> Option<String> {
    let comment = find_preceding_phpdoc_comment(stmt, source)?;
    let phpdoc = parse_phpdoc(comment);
    let type_info = phpdoc.var_type?;
    let tagged_var = parse_tagged_var_name(comment);

    if let Some(name) = tagged_var {
        if normalize_var_name(&name) != var_name {
            return None;
        }
    } else if !allow_unnamed_var_tag {
        return None;
    }

    resolve_phpdoc_var_type(&type_info, stmt, source, file_symbols)
}

fn find_preceding_phpdoc_comment<'a>(node: Node<'a>, source: &'a str) -> Option<&'a str> {
    let mut prev = node.prev_sibling();
    while let Some(p) = prev {
        if p.kind() == "comment" {
            let text = &source[p.byte_range()];
            return if text.starts_with("/**") {
                Some(text)
            } else {
                None
            };
        }
        // A statement between comment and target means comment is not attached.
        if p.is_named() {
            return None;
        }
        prev = p.prev_sibling();
    }
    None
}

fn parse_tagged_var_name(comment: &str) -> Option<String> {
    for raw_line in comment.lines() {
        let mut line = raw_line.trim();
        if let Some(rest) = line.strip_prefix("/**") {
            line = rest.trim_start();
        }
        if let Some(rest) = line.strip_prefix('*') {
            line = rest.trim_start();
        }
        if line.starts_with("*/") || line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("@var") {
            for token in rest.split_whitespace() {
                if let Some(name) = normalize_doc_var_token(token) {
                    return Some(name);
                }
            }
        }
    }
    None
}

fn normalize_doc_var_token(token: &str) -> Option<String> {
    let trimmed = token.trim_matches(|c: char| c == ',' || c == ';' || c == ')' || c == '(');
    if !trimmed.starts_with('$') {
        return None;
    }

    let ident: String = trimmed
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '$')
        .collect();
    if ident.len() > 1 {
        Some(ident)
    } else {
        None
    }
}

fn resolve_phpdoc_var_type(
    type_info: &TypeInfo,
    context_node: Node,
    source: &str,
    file_symbols: &FileSymbols,
) -> Option<String> {
    match type_info {
        TypeInfo::Simple(name) => {
            if is_builtin_non_object_type(name) {
                None
            } else {
                Some(resolve_class_name(name, file_symbols))
            }
        }
        TypeInfo::Nullable(inner) => {
            resolve_phpdoc_var_type(inner, context_node, source, file_symbols)
        }
        TypeInfo::Union(types) | TypeInfo::Intersection(types) => {
            for ty in types {
                if let Some(resolved) =
                    resolve_phpdoc_var_type(ty, context_node, source, file_symbols)
                {
                    return Some(resolved);
                }
            }
            None
        }
        TypeInfo::Self_ | TypeInfo::Static_ => {
            find_parent_class_fqn(context_node, source, file_symbols)
        }
        TypeInfo::Parent_ => None,
        TypeInfo::Void | TypeInfo::Never | TypeInfo::Mixed => None,
    }
}

fn is_builtin_non_object_type(name: &str) -> bool {
    matches!(
        name.trim_start_matches('\\').to_ascii_lowercase().as_str(),
        "int"
            | "float"
            | "string"
            | "bool"
            | "boolean"
            | "array"
            | "object"
            | "null"
            | "void"
            | "never"
            | "mixed"
            | "callable"
            | "iterable"
            | "true"
            | "false"
            | "resource"
            | "self"
            | "static"
            | "parent"
    )
}

fn position_to_byte(source: &str, line: u32, character: u32) -> usize {
    let mut offset = 0usize;
    let line_idx = line as usize;
    for (i, row) in source.lines().enumerate() {
        if i == line_idx {
            let col = character as usize;
            return offset + col.min(row.len());
        }
        offset += row.len() + 1;
    }
    source.len()
}

/// Resolve a simple name node to a SymbolAtPosition.
fn resolve_name_node(
    node: Node,
    source: &str,
    file_symbols: &FileSymbols,
) -> Option<SymbolAtPosition> {
    let text = &source[node.byte_range()];
    let parent_kind = node.parent().map(|p| p.kind()).unwrap_or_default();

    if text.starts_with('$') {
        return Some(SymbolAtPosition {
            fqn: text.to_string(),
            name: text.to_string(),
            ref_kind: RefKind::Variable,
            object_expr: None,
            range: node_range(node),
        });
    }

    // Resolve as global/user constant in expression-like contexts.
    if is_constant_reference_context(parent_kind) {
        let resolved = resolve_constant_name(text, file_symbols);
        return Some(SymbolAtPosition {
            fqn: resolved,
            name: text.to_string(),
            ref_kind: RefKind::GlobalConstant,
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
            .unwrap_or_else(|| use_stmt.fqn.rsplit('\\').next().unwrap_or(&use_stmt.fqn));

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
            .unwrap_or_else(|| use_stmt.fqn.rsplit('\\').next().unwrap_or(&use_stmt.fqn));

        if alias == name {
            return use_stmt.fqn.clone();
        }
    }

    // Keep already-qualified names stable.
    if name.contains('\\') {
        return name.to_string();
    }

    // For simple function names, try namespace-qualified first.
    if let Some(ref ns) = file_symbols.namespace {
        format!("{}\\{}", ns, name)
    } else {
        name.to_string()
    }
}

/// Resolve a constant name using use statements and current namespace.
fn resolve_constant_name(name: &str, file_symbols: &FileSymbols) -> String {
    if name.starts_with('\\') {
        return name.trim_start_matches('\\').to_string();
    }

    let parts: Vec<&str> = name.split('\\').collect();
    let first_part = parts[0];

    for use_stmt in &file_symbols.use_statements {
        if use_stmt.kind != UseKind::Constant {
            continue;
        }

        let alias = use_stmt
            .alias
            .as_deref()
            .unwrap_or_else(|| use_stmt.fqn.rsplit('\\').next().unwrap_or(&use_stmt.fqn));

        if alias == first_part {
            if parts.len() == 1 {
                return use_stmt.fqn.clone();
            }
            return format!("{}\\{}", use_stmt.fqn, parts[1..].join("\\"));
        }
    }

    // Keep already-qualified names stable.
    if name.contains('\\') {
        if let Some(ref ns) = file_symbols.namespace {
            return format!("{}\\{}", ns, name);
        }
        return name.to_string();
    }

    if let Some(ref ns) = file_symbols.namespace {
        format!("{}\\{}", ns, name)
    } else {
        name.to_string()
    }
}

fn is_constant_reference_context(parent_kind: &str) -> bool {
    !matches!(
        parent_kind,
        "class_declaration"
            | "interface_declaration"
            | "trait_declaration"
            | "enum_declaration"
            | "function_definition"
            | "method_declaration"
            | "named_type"
            | "optional_type"
            | "union_type"
            | "intersection_type"
            | "object_creation_expression"
            | "function_call_expression"
            | "scoped_call_expression"
            | "member_call_expression"
            | "namespace_use_clause"
            | "namespace_definition"
    )
}

fn find_variable_definition_before(
    node: Node,
    var_name: &str,
    usage_start: usize,
    source: &str,
    best: &mut Option<(usize, (u32, u32, u32, u32))>,
) {
    if node.start_byte() >= usage_start {
        return;
    }

    match node.kind() {
        "simple_parameter" | "property_promotion_parameter" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                if normalize_var_name(&source[name_node.byte_range()]) == var_name {
                    let start = name_node.start_byte();
                    if start < usage_start {
                        *best = Some((start, node_range(name_node)));
                    }
                }
            }
        }
        "assignment_expression" => {
            if let Some(left) = node.child_by_field_name("left") {
                if normalize_var_name(&source[left.byte_range()]) == var_name {
                    let start = left.start_byte();
                    if start < usage_start {
                        *best = Some((start, node_range(left)));
                    }
                }
            }
        }
        "foreach_statement" => {
            for field in ["key", "value"] {
                if let Some(var_node) = node.child_by_field_name(field) {
                    if normalize_var_name(&source[var_node.byte_range()]) == var_name {
                        let start = var_node.start_byte();
                        if start < usage_start {
                            *best = Some((start, node_range(var_node)));
                        }
                    }
                }
            }
        }
        "catch_clause" => {
            for field in ["name", "variable"] {
                if let Some(var_node) = node.child_by_field_name(field) {
                    if normalize_var_name(&source[var_node.byte_range()]) == var_name {
                        let start = var_node.start_byte();
                        if start < usage_start {
                            *best = Some((start, node_range(var_node)));
                        }
                    }
                }
            }
        }
        _ => {}
    }

    let cursor = &mut node.walk();
    for child in node.named_children(cursor) {
        find_variable_definition_before(child, var_name, usage_start, source, best);
    }
}

fn normalize_var_name(text: &str) -> String {
    if text.starts_with('$') {
        text.to_string()
    } else {
        format!("${}", text)
    }
}

/// Try to find the FQN of the class containing a method node.
fn find_parent_class_fqn(
    method_node: Node,
    source: &str,
    file_symbols: &FileSymbols,
) -> Option<String> {
    let mut current = method_node.parent();
    while let Some(node) = current {
        match node.kind() {
            "class_declaration"
            | "interface_declaration"
            | "trait_declaration"
            | "enum_declaration" => {
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

    fn parse_and_find_var_def(code: &str, line: u32, col: u32) -> Option<(u32, u32, u32, u32)> {
        let mut parser = FileParser::new();
        parser.parse_full(code);
        let tree = parser.tree().unwrap();
        variable_definition_at_position(tree, code, line, col)
    }

    fn parse_and_infer_var_type_at(
        code: &str,
        line: u32,
        col: u32,
        var_name: &str,
    ) -> Option<String> {
        let mut parser = FileParser::new();
        parser.parse_full(code);
        let tree = parser.tree().unwrap();
        let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
        infer_variable_type_at_position(tree, code, &file_symbols, line, col, var_name)
    }

    fn find_line_col(code: &str, needle: &str) -> (u32, u32) {
        for (line, row) in code.lines().enumerate() {
            if let Some(col) = row.find(needle) {
                return (line as u32, col as u32);
            }
        }
        panic!("needle not found: {}", needle);
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
    fn test_resolve_qualified_function_call_without_double_namespace() {
        let code = "<?php\nnamespace App\\Diagnostics;\n\nApp\\Utils\\helper();\n";
        let result = parse_and_resolve(code, 3, 13);
        assert!(result.is_some());
        let sym = result.unwrap();
        assert_eq!(sym.ref_kind, RefKind::FunctionCall);
        assert_eq!(sym.fqn, "App\\Utils\\helper");
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
        assert!(
            result.is_some(),
            "Should resolve method call on new expression"
        );
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
        assert_eq!(sym.fqn, "App\\Foo::$name");
        assert_eq!(sym.ref_kind, RefKind::PropertyAccess);
    }

    #[test]
    fn test_resolve_fully_qualified() {
        let code = "<?php\n\\DateTime::createFromFormat('Y-m-d', '2024-01-01');\n";
        // \\DateTime at line 1
        let result = parse_and_resolve(code, 1, 1);
        assert!(result.is_some());
    }

    #[test]
    fn test_resolve_method_call_on_variable_assigned_new() {
        let code = "<?php\nnamespace App;\nuse App\\Test\\Baz;\n\nclass Bar {\n    public function greet(): void {\n        $baz = new Baz();\n        $baz->test();\n    }\n}\n";
        // "test" in "$baz->test()" at line 7, col 15
        let result = parse_and_resolve(code, 7, 15);
        assert!(
            result.is_some(),
            "Should resolve method on variable assigned via new"
        );
        let sym = result.unwrap();
        assert_eq!(sym.name, "test");
        assert_eq!(sym.ref_kind, RefKind::MethodCall);
        assert_eq!(sym.fqn, "App\\Test\\Baz::test");
    }

    #[test]
    fn test_resolve_method_call_on_typed_parameter() {
        let code = "<?php\nnamespace App;\nuse App\\Test\\Baz;\n\nclass Bar {\n    public function greet(Baz $baz2): void {\n        $baz2->test();\n    }\n}\n";
        // "test" in "$baz2->test()" at line 6, col 16
        let result = parse_and_resolve(code, 6, 16);
        assert!(result.is_some(), "Should resolve method on typed parameter");
        let sym = result.unwrap();
        assert_eq!(sym.name, "test");
        assert_eq!(sym.ref_kind, RefKind::MethodCall);
        assert_eq!(sym.fqn, "App\\Test\\Baz::test");
    }

    #[test]
    fn test_resolve_property_access_on_typed_parameter() {
        let code = "<?php\nnamespace App;\nuse App\\Test\\Baz;\n\nclass Bar {\n    public function greet(Baz $baz2): void {\n        echo $baz2->name;\n    }\n}\n";
        // "name" in "$baz2->name" at line 6, col 20
        let result = parse_and_resolve(code, 6, 20);
        assert!(
            result.is_some(),
            "Should resolve property on typed parameter"
        );
        let sym = result.unwrap();
        assert_eq!(sym.name, "name");
        assert_eq!(sym.fqn, "App\\Test\\Baz::$name");
        assert_eq!(sym.ref_kind, RefKind::PropertyAccess);
    }

    #[test]
    fn test_resolve_method_call_on_variable_typed_by_inline_phpdoc_var() {
        let code = "<?php\nnamespace App;\nuse App\\Test\\Baz;\n\nclass Bar {\n    public function greet(): void {\n        /** @var Baz $baz2 */\n        $baz2 = makeBaz();\n        $baz2->test();\n    }\n}\n";
        // "test" in "$baz2->test()" at line 8
        let result = parse_and_resolve(code, 8, 16);
        assert!(
            result.is_some(),
            "Should resolve method on variable typed by inline @var"
        );
        let sym = result.unwrap();
        assert_eq!(sym.name, "test");
        assert_eq!(sym.ref_kind, RefKind::MethodCall);
        assert_eq!(sym.fqn, "App\\Test\\Baz::test");
    }

    #[test]
    fn test_inline_phpdoc_var_must_match_variable_name() {
        let code = "<?php\nnamespace App;\nuse App\\Test\\Baz;\n\nclass Bar {\n    public function greet(): void {\n        /** @var Baz $other */\n        $baz2 = makeBaz();\n        $baz2->test();\n    }\n}\n";
        // No matching @var for $baz2, so it should not be force-resolved as Baz.
        let result = parse_and_resolve(code, 8, 16).expect("symbol should resolve");
        assert_ne!(result.fqn, "App\\Test\\Baz::test");
    }

    #[test]
    fn test_unnamed_inline_phpdoc_var_applies_to_immediate_assignment() {
        let code = "<?php\nnamespace App;\nuse App\\Test\\Baz;\n\nclass Bar {\n    public function greet(): void {\n        /** @var Baz */\n        $baz2 = makeBaz();\n        $baz2->test();\n    }\n}\n";
        let result = parse_and_resolve(code, 8, 16).expect("symbol should resolve");
        assert_eq!(result.fqn, "App\\Test\\Baz::test");
    }

    #[test]
    fn test_unnamed_inline_phpdoc_var_does_not_apply_without_assignment() {
        let code = "<?php\nnamespace App;\nuse App\\Test\\Baz;\n\nclass Bar {\n    public function greet(): void {\n        /** @var Baz */\n        consume($baz2);\n        $baz2->test();\n    }\n}\n";
        let result = parse_and_resolve(code, 8, 16).expect("symbol should resolve");
        assert_ne!(result.fqn, "App\\Test\\Baz::test");
    }

    #[test]
    fn test_infer_variable_type_at_position_from_inline_phpdoc_var() {
        let code = "<?php\nnamespace App;\nuse App\\Test\\Baz;\n\nfunction run(): void {\n    /** @var Baz $baz2 */\n    $baz2 = makeBaz();\n    $baz2->\n}\n";
        // Cursor is after "$baz2->"
        let inferred =
            parse_and_infer_var_type_at(code, 7, 11, "$baz2").expect("type should be inferred");
        assert_eq!(inferred, "App\\Test\\Baz");
    }

    #[test]
    fn test_resolve_property_vs_method_same_name() {
        let code = "<?php\nnamespace App\\Test;\n\nclass Baz {\n    public string $test = 'x';\n    public function test(): string { return 'ok'; }\n}\n\nfunction go(Baz $baz2): void {\n    echo $baz2->test;\n    $baz2->test();\n}\n";

        // Property access should resolve to Baz::$test
        let prop = parse_and_resolve(code, 9, 17).expect("property should resolve");
        assert_eq!(prop.ref_kind, RefKind::PropertyAccess);
        assert_eq!(prop.fqn, "App\\Test\\Baz::$test");

        // Method call should resolve to Baz::test
        let method = parse_and_resolve(code, 10, 12).expect("method should resolve");
        assert_eq!(method.ref_kind, RefKind::MethodCall);
        assert_eq!(method.fqn, "App\\Test\\Baz::test");
    }

    #[test]
    fn test_resolve_class_constant_access() {
        let code = "<?php\nnamespace App;\n\nclass Foo {\n    public const VERSION = '1.0';\n    public function run(): string {\n        return self::VERSION;\n    }\n}\n";
        // VERSION in self::VERSION
        let result = parse_and_resolve(code, 6, 21);
        assert!(result.is_some(), "Should resolve class constant access");
        let sym = result.unwrap();
        assert_eq!(sym.ref_kind, RefKind::ClassConstant);
        assert_eq!(sym.fqn, "App\\Foo::VERSION");
    }

    #[test]
    fn test_resolve_global_constant_reference() {
        let code = "<?php\nnamespace App;\n\nconst BUILD = 'dev';\n\necho BUILD;\n";
        let result = parse_and_resolve(code, 5, 5);
        assert!(result.is_some(), "Should resolve global constant usage");
        let sym = result.unwrap();
        assert_eq!(sym.ref_kind, RefKind::GlobalConstant);
        assert_eq!(sym.fqn, "App\\BUILD");
    }

    #[test]
    fn test_find_variable_definition_assignment() {
        let code = "<?php\nfunction demo(): void {\n    $value = 1;\n    echo $value;\n}\n";
        // $value in echo $value;
        let def = parse_and_find_var_def(code, 3, 10).expect("definition should be found");
        // points to assignment L3
        assert_eq!(def.0, 2);
    }

    #[test]
    fn test_find_variable_definition_parameter() {
        let code = "<?php\nfunction demo(string $name): void {\n    echo $name;\n}\n";
        // $name in echo $name;
        let def =
            parse_and_find_var_def(code, 2, 10).expect("parameter definition should be found");
        // points to parameter line
        assert_eq!(def.0, 1);
    }

    #[test]
    fn test_resolve_global_constant_in_method_body() {
        let code = "<?php\nnamespace App;\n\nconst BUILD = 'dev';\n\nclass Demo {\n    public const VERSION = '1.0';\n\n    public function run(): string {\n        $value = BUILD;\n        return self::VERSION . $value;\n    }\n}\n";
        let sym = parse_and_resolve(code, 9, 17).expect("BUILD symbol should resolve");
        assert_eq!(sym.ref_kind, RefKind::GlobalConstant);
        assert_eq!(sym.fqn, "App\\BUILD");
    }

    #[test]
    fn test_resolve_static_property_access_variants() {
        let code = "<?php\nnamespace App;\n\nclass User { public static string $var = 'u'; }\n\nclass Demo {\n    public static string $created = 'c';\n    public static string $var = 'd';\n\n    public function run(): void {\n        echo self::$created;\n        echo static::$var;\n        echo User::$var;\n    }\n}\n";

        let (l1, c1) = find_line_col(code, "self::$created");
        let self_prop = parse_and_resolve(code, l1, c1 + 8).expect("self::$created should resolve");
        assert_eq!(self_prop.ref_kind, RefKind::StaticPropertyAccess);
        assert_eq!(self_prop.fqn, "App\\Demo::$created");

        let (l2, c2) = find_line_col(code, "static::$var");
        let static_prop = parse_and_resolve(code, l2, c2 + 9).expect("static::$var should resolve");
        assert_eq!(static_prop.ref_kind, RefKind::StaticPropertyAccess);
        assert_eq!(static_prop.fqn, "App\\Demo::$var");

        let (l3, c3) = find_line_col(code, "User::$var");
        let user_prop = parse_and_resolve(code, l3, c3 + 7).expect("User::$var should resolve");
        assert_eq!(user_prop.ref_kind, RefKind::StaticPropertyAccess);
        assert_eq!(user_prop.fqn, "App\\User::$var");
    }
}
