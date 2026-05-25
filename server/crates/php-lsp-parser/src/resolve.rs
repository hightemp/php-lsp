//! Symbol resolution from a CST position.
//!
//! Given a position in a parsed PHP file, determines what symbol is at that
//! position and resolves it to an identifier name, considering namespace context
//! and use statements.

use crate::phpdoc::parse_phpdoc;
use php_lsp_types::{FileSymbols, TypeInfo, UseKind};
use std::cell::Cell;
use std::collections::HashSet;
use tree_sitter::{Node, Point, Tree};

const MAX_OBJECT_TYPE_RESOLVE_DEPTH: usize = 64;

thread_local! {
    static OBJECT_TYPE_RESOLVE_DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// Callback for resolving a member's type from an external source (e.g., workspace index).
///
/// Takes `(class_fqn, member_name)` and returns the member's type FQN.
/// For properties: `member_name` includes `$` prefix (e.g., `"$timer"`).
/// For methods: `member_name` is the method name (e.g., `"start"`).
///
/// Returns the resolved type FQN (e.g., `"App\\TimerService"`) or None.
pub type MemberTypeResolver<'a> = &'a dyn Fn(&str, &str) -> Option<String>;

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

/// Hover-related information for a local variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableHoverInfo {
    /// Variable name as written in code (`$name`).
    pub variable_name: String,
    /// Display type to show in hover (`Baz`, `?Foo`, `int`, etc.).
    pub type_display: Option<String>,
    /// Resolved class-like FQN when available (`App\\Baz`).
    pub resolved_type_fqn: Option<String>,
    /// Raw PHPDoc comment that produced this info, when available.
    pub phpdoc_comment: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct VariableInference {
    type_display: Option<String>,
    resolved_type_fqn: Option<String>,
    phpdoc_comment: Option<String>,
    type_info: Option<TypeInfo>,
}

impl VariableInference {
    fn has_data(&self) -> bool {
        self.type_display.is_some()
            || self.resolved_type_fqn.is_some()
            || self.phpdoc_comment.is_some()
    }
}

/// What kind of reference is this?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    /// A class/interface/trait/enum name reference.
    ClassName,
    /// A constructor call via `new ClassName()`.
    Constructor,
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
    symbol_at_position_with_resolver(tree, source, line, character, file_symbols, None)
}

/// Find the symbol at the given position, with an optional cross-file type resolver.
pub fn symbol_at_position_with_resolver(
    tree: &Tree,
    source: &str,
    line: u32,
    character: u32,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
) -> Option<SymbolAtPosition> {
    let root = tree.root_node();
    let point = Point::new(line as usize, character as usize);

    // Find the most specific node at the position
    let node = find_node_at_point(root, point)?;

    resolve_node(node, source, file_symbols, resolver)
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

/// Collect local variables declared before a position in the current scope.
///
/// This supports the same declaration forms as local goto-definition, including
/// by-reference output arguments such as `preg_match(..., $matches)`.
pub fn local_variable_names_at_position(
    tree: &Tree,
    source: &str,
    line: u32,
    character: u32,
) -> Vec<String> {
    let root = tree.root_node();
    let point = Point::new(line as usize, character as usize);
    let node = find_node_at_point(root, point).unwrap_or(root);
    let usage_start = position_to_byte(source, line, character);
    let scope = find_enclosing_function(node).unwrap_or(root);

    let mut vars = Vec::new();
    collect_variable_declarations_before(scope, usage_start, source, &mut vars);

    let mut seen = HashSet::new();
    vars.into_iter()
        .filter_map(|(_, name)| seen.insert(name.clone()).then_some(name))
        .collect()
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
    if let Some(resolved) =
        infer_textual_array_access_type(scope, &normalized, usage_start, source, file_symbols, None)
    {
        return Some(resolved);
    }
    infer_variable_type_in_scope(scope, &normalized, usage_start, source, file_symbols, None)
}

/// Infer variable type by name before a given position, using an external member resolver.
///
/// This lets completion resolve assignments such as
/// `$session = $request->getSession()` when the return type for `getSession`
/// lives in another file or an indexed dependency.
pub fn infer_variable_type_at_position_with_resolver(
    tree: &Tree,
    source: &str,
    file_symbols: &FileSymbols,
    line: u32,
    character: u32,
    var_name: &str,
    resolver: MemberTypeResolver<'_>,
) -> Option<String> {
    let root = tree.root_node();
    let point = Point::new(line as usize, character as usize);
    let node = find_node_at_point(root, point).unwrap_or(root);
    let usage_start = position_to_byte(source, line, character);
    let normalized = normalize_var_name(var_name);
    let scope = find_enclosing_function(node).unwrap_or_else(|| find_root_node(node));
    if let Some(resolved) = infer_textual_array_access_type(
        scope,
        &normalized,
        usage_start,
        source,
        file_symbols,
        Some(resolver),
    ) {
        return Some(resolved);
    }
    infer_variable_type_in_scope(
        scope,
        &normalized,
        usage_start,
        source,
        file_symbols,
        Some(resolver),
    )
}

/// Infer hover info for a variable under cursor at a given position.
pub fn variable_hover_info_at_position(
    tree: &Tree,
    source: &str,
    file_symbols: &FileSymbols,
    line: u32,
    character: u32,
) -> Option<VariableHoverInfo> {
    let root = tree.root_node();
    let point = Point::new(line as usize, character as usize);
    let mut node = find_node_at_point(root, point)?;

    loop {
        let text = &source[node.byte_range()];
        if node.kind() == "variable_name" || text.starts_with('$') {
            break;
        }
        node = node.parent()?;
    }

    let var_name = normalize_var_name(&source[node.byte_range()]);
    let usage_start = node.start_byte();
    let scope = find_enclosing_function(node).unwrap_or_else(|| find_root_node(node));
    let inference =
        infer_variable_in_scope(scope, &var_name, usage_start, source, file_symbols, None);
    if !inference.has_data() {
        return None;
    }

    Some(VariableHoverInfo {
        variable_name: var_name,
        type_display: inference.type_display,
        resolved_type_fqn: inference.resolved_type_fqn,
        phpdoc_comment: inference.phpdoc_comment,
    })
}

/// Infer all possible types of a class property by scanning for `$this->propName = <expr>`
/// assignments throughout the class body.
///
/// Returns all distinct resolved types found across all assignments.
/// This is used as a fallback when the declared property type doesn't have a
/// requested member (e.g., PHPUnit stubs where `$this->em` is typed as
/// `EntityManagerInterface` but assigned via `$this->createStub(...)` which
/// returns `Stub`, or `$this->createMock(...)` which returns `MockObject`).
pub fn infer_property_type_from_assignments(
    tree: &Tree,
    source: &str,
    prop_name: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
) -> Vec<String> {
    let root = tree.root_node();
    let mut results = Vec::new();
    find_all_property_assignment_types(
        root,
        source,
        prop_name,
        file_symbols,
        resolver,
        &mut results,
    );
    results
}

/// Recursively search the tree for `$this->propName = <expr>` assignments
/// and collect all distinct resolved RHS expression types.
fn find_all_property_assignment_types(
    node: Node,
    source: &str,
    prop_name: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
    results: &mut Vec<String>,
) {
    for i in 0..node.child_count() {
        let child = match node.child(i) {
            Some(c) => c,
            None => continue,
        };

        // Check expression_statement for property assignment
        if child.kind() == "expression_statement" {
            if let Some(rhs) = property_assignment_rhs(child, prop_name, source) {
                if let Some(resolved) = try_resolve_object_type(rhs, source, file_symbols, resolver)
                {
                    if !results.contains(&resolved) {
                        results.push(resolved);
                    }
                }
            }
        }

        // Recurse into child nodes, but skip anonymous functions/closures
        // to avoid matching assignments in nested scopes.
        // We DO enter class_declaration and method_declaration since
        // that's where $this->prop assignments live.
        if child.kind() != "anonymous_function"
            && child.kind() != "anonymous_function_creation_expression"
            && child.kind() != "arrow_function"
        {
            find_all_property_assignment_types(
                child,
                source,
                prop_name,
                file_symbols,
                resolver,
                results,
            );
        }
    }
}

/// Check if a statement is `$this->propName = <expr>` and return the RHS node.
fn property_assignment_rhs<'a>(stmt: Node<'a>, prop_name: &str, source: &str) -> Option<Node<'a>> {
    if stmt.kind() != "expression_statement" {
        return None;
    }

    let expr = stmt.named_child(0)?;
    if expr.kind() != "assignment_expression" {
        return None;
    }

    let left = expr.child_by_field_name("left")?;
    let right = expr.child_by_field_name("right")?;

    // Check that left is `$this->propName`
    if left.kind() != "member_access_expression" {
        return None;
    }

    let obj = left.child_by_field_name("object")?;
    let name = left.child_by_field_name("name")?;

    let obj_text = &source[obj.byte_range()];
    let name_text = &source[name.byte_range()];

    if obj_text == "$this" && name_text == prop_name {
        Some(right)
    } else {
        None
    }
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

/// Walk up from a `qualified_name` or `namespace_name` node to find the
/// `qualified_name` ancestor that sits directly inside a class-reference
/// context (object_creation_expression, named_type, etc.).
fn find_qualified_name_ancestor(start: Node) -> Node {
    let mut current = start;
    while matches!(
        current.kind(),
        "namespace_name" | "namespace_name_as_prefix" | "qualified_name"
    ) {
        if let Some(p) = current.parent() {
            if !matches!(
                p.kind(),
                "namespace_name" | "namespace_name_as_prefix" | "qualified_name"
            ) {
                // `current` is the topmost qualified/namespace node; its parent
                // is the actual class-reference context.
                return current;
            }
            current = p;
        } else {
            break;
        }
    }
    current
}

/// Check if a `qualified_name`/`namespace_name` node is inside a class-reference
/// context (object_creation_expression, named_type, etc.).
fn is_inside_class_reference_context(start: Node) -> bool {
    let qname = find_qualified_name_ancestor(start);
    qname
        .parent()
        .map(|p| {
            matches!(
                p.kind(),
                "object_creation_expression"
                    | "named_type"
                    | "optional_type"
                    | "base_clause"
                    | "class_interface_clause"
                    | "type_list"
                    | "class_constant_access_expression"
                    | "scoped_call_expression"
                    | "scoped_property_access_expression"
            )
        })
        .unwrap_or(false)
}

/// Check if a `qualified_name`/`namespace_name` node is specifically inside
/// an `object_creation_expression` context (`new ClassName`).
fn is_inside_object_creation_context(start: Node) -> bool {
    let qname = find_qualified_name_ancestor(start);
    qname
        .parent()
        .map(|p| p.kind() == "object_creation_expression")
        .unwrap_or(false)
}

/// Resolve a CST node to symbol information.
fn resolve_node(
    node: Node,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
) -> Option<SymbolAtPosition> {
    let parent = node.parent()?;
    let node_text = &source[node.byte_range()];
    let parent_kind = parent.kind();

    match parent_kind {
        // Member access: $obj->method() or $obj->property
        "member_access_expression" | "nullsafe_member_access_expression" => {
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
                let class_fqn = object_field
                    .and_then(|o| try_resolve_object_type(o, source, file_symbols, resolver));
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
        "member_call_expression" | "nullsafe_member_call_expression" => {
            let name_field = parent.child_by_field_name("name");
            let object_field = parent.child_by_field_name("object");

            if name_field.map(|n| n.id()) == Some(node.id()) {
                let object_text = object_field.map(|o| source[o.byte_range()].to_string());
                // Try to resolve object type to build a proper FQN
                let class_fqn = object_field
                    .and_then(|o| try_resolve_object_type(o, source, file_symbols, resolver));
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
                    .map(|s| resolve_scope_class_name(s, parent, source, file_symbols))
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
            let resolved = resolve_scope_class_name(node_text, parent, source, file_symbols);
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
                    .map(|s| resolve_scope_class_name(s, parent, source, file_symbols))
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

            let resolved = resolve_scope_class_name(node_text, parent, source, file_symbols);
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
            if let Some(func) = func_field {
                if func.kind() == "member_access_expression" {
                    let name_field = func.child_by_field_name("name");
                    let object_field = func.child_by_field_name("object");
                    if name_field.map(|n| n.id()) == Some(node.id()) {
                        let object_text = object_field.map(|o| source[o.byte_range()].to_string());
                        let class_fqn = object_field.and_then(|o| {
                            try_resolve_object_type(o, source, file_symbols, resolver)
                        });
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
                }
            }
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

        // Child name inside qualified_name used as a class reference (e.g. new Assert\NotBlank, type hints).
        // Walk up through qualified_name / namespace_name to find the context.
        "qualified_name" | "namespace_name" if is_inside_class_reference_context(parent) => {
            // Walk up to find the qualified_name ancestor and resolve its full text
            let qname_node = find_qualified_name_ancestor(parent);
            let qname_text = &source[qname_node.byte_range()];
            let resolved = resolve_class_name(qname_text, file_symbols);
            let is_new = is_inside_object_creation_context(parent);
            Some(SymbolAtPosition {
                fqn: if is_new {
                    format!("{}::__construct", resolved)
                } else {
                    resolved
                },
                name: node_text.to_string(),
                ref_kind: if is_new {
                    RefKind::Constructor
                } else {
                    RefKind::ClassName
                },
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
                    .map(|s| resolve_scope_class_name(s, parent, source, file_symbols))
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
                let resolved = resolve_scope_class_name(node_text, parent, source, file_symbols);
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
                fqn: format!("{}::__construct", resolved),
                name: node_text.to_string(),
                ref_kind: RefKind::Constructor,
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
        // Check if this is inside a use statement (qualified_name → namespace_use_clause)
        _ if node.kind() == "qualified_name" || node.kind() == "name" => {
            if is_inside_use_clause(node, parent) {
                // Extract the full FQN from the qualified_name or namespace_use_clause
                let fqn = extract_use_clause_fqn(node, parent, source);
                return Some(SymbolAtPosition {
                    fqn: fqn.trim_start_matches('\\').to_string(),
                    name: node_text.to_string(),
                    ref_kind: RefKind::ClassName,
                    object_expr: None,
                    range: node_range(node),
                });
            }
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
    resolver: Option<MemberTypeResolver<'_>>,
) -> Option<String> {
    OBJECT_TYPE_RESOLVE_DEPTH.with(|depth| {
        let current = depth.get();
        if current >= MAX_OBJECT_TYPE_RESOLVE_DEPTH {
            return None;
        }

        depth.set(current + 1);
        let result = try_resolve_object_type_inner(object_node, source, file_symbols, resolver);
        depth.set(current);
        result
    })
}

fn try_resolve_object_type_inner<'a>(
    object_node: Node<'a>,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
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
                    if let Some(resolved) =
                        try_resolve_object_type(child, source, file_symbols, resolver)
                    {
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
                infer_variable_type(object_node, text, source, file_symbols, resolver)
            }
        }
        // Name / qualified_name might be a class used as scope
        "name" | "qualified_name" => {
            let text = &source[object_node.byte_range()];
            Some(resolve_class_name(text, file_symbols))
        }
        // Member access: $obj->prop → try to resolve object type, then look up property type
        "member_access_expression" | "nullsafe_member_access_expression" => {
            let obj_field = object_node.child_by_field_name("object")?;
            let name_field = object_node.child_by_field_name("name")?;
            let prop_name = &source[name_field.byte_range()];

            // Resolve the object type first
            let class_fqn = try_resolve_object_type(obj_field, source, file_symbols, resolver)?;

            // Look up the property in the file's symbols to get its type
            let property_fqn_dollar = format!("{}::${}", class_fqn, prop_name);
            for sym in &file_symbols.symbols {
                if sym.fqn == property_fqn_dollar {
                    if let Some(ref sig) = sym.signature {
                        if let Some(ref ret) = sig.return_type {
                            if let Some(resolved) = resolve_symbol_type_info_to_object_fqn(
                                ret,
                                &class_fqn,
                                object_node,
                                source,
                                file_symbols,
                            ) {
                                return Some(resolved);
                            }
                        }
                    }
                    break;
                }
            }
            // Fallback: use the cross-file resolver for inherited properties
            if let Some(ref resolve_fn) = resolver {
                let member_name = format!("${}", prop_name);
                if let Some(type_fqn) = resolve_fn(&class_fqn, &member_name) {
                    return Some(type_fqn);
                }
            }
            None
        }
        // Member call: $obj->foo() → resolve object type, then look up method return type
        "member_call_expression" | "nullsafe_member_call_expression" => {
            let obj_field = object_node.child_by_field_name("object")?;
            let name_field = object_node.child_by_field_name("name")?;
            let method_name = &source[name_field.byte_range()];

            // Resolve the object type first
            let class_fqn = try_resolve_object_type(obj_field, source, file_symbols, resolver)?;

            // First: look up the method's return type in the current file's symbols
            let method_fqn = format!("{}::{}", class_fqn, method_name);
            for sym in &file_symbols.symbols {
                if sym.fqn == method_fqn {
                    if let Some(ref sig) = sym.signature {
                        if let Some(ref ret) = sig.return_type {
                            if let Some(resolved) = resolve_symbol_type_info_to_object_fqn(
                                ret,
                                &class_fqn,
                                object_node,
                                source,
                                file_symbols,
                            ) {
                                return Some(resolved);
                            }
                        }
                    }
                    break;
                }
            }

            // Fallback: use the cross-file resolver to get the method's return type
            if let Some(ref resolve_fn) = resolver {
                if let Some(type_fqn) = resolve_fn(&class_fqn, method_name) {
                    return Some(type_fqn);
                }
            }

            // Secondary fallback: if the object is `$this->prop` and the method
            // wasn't found on the declared type, try the assignment-inferred type.
            // This handles PHPUnit patterns: `$this->em = $this->createStub(...)` → Stub
            if matches!(
                obj_field.kind(),
                "member_access_expression" | "nullsafe_member_access_expression"
            ) {
                if let Some(this_obj) = obj_field.child_by_field_name("object") {
                    let this_text = &source[this_obj.byte_range()];
                    if this_text == "$this" {
                        if let Some(prop_field) = obj_field.child_by_field_name("name") {
                            let prop_name_text = &source[prop_field.byte_range()];
                            // Find the class body root to scan for assignments
                            if let Some(class_node) = find_enclosing_class_node(object_node) {
                                let mut alt_types = Vec::new();
                                find_all_property_assignment_types(
                                    class_node,
                                    source,
                                    prop_name_text,
                                    file_symbols,
                                    resolver,
                                    &mut alt_types,
                                );
                                for alt_type in &alt_types {
                                    if let Some(ref resolve_fn) = resolver {
                                        if let Some(type_fqn) = resolve_fn(alt_type, method_name) {
                                            return Some(type_fqn);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            None
        }
        // Array access: $items[0] or $row['user'] can be object-like when
        // the base expression is typed with PHPDoc generics or an array shape.
        "subscript_expression" => {
            let base = object_node.named_child(0)?;
            let key_text = object_node
                .named_child(1)
                .map(|node| source[node.byte_range()].trim().to_string());
            let base_type = infer_expression_type_info(base, source, file_symbols, resolver)?;
            let value_type = iterable_value_type_info(&base_type, key_text.as_deref())?;
            resolve_phpdoc_var_type(&value_type, object_node, source, file_symbols)
        }
        // Static call: Foo::create() — can't resolve return type without full type info
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
    resolver: Option<MemberTypeResolver<'_>>,
) -> Option<String> {
    let scope = find_enclosing_function(var_node).unwrap_or_else(|| find_root_node(var_node));
    infer_variable_type_in_scope(
        scope,
        var_name,
        var_node.start_byte(),
        source,
        file_symbols,
        resolver,
    )
}

/// Find the enclosing function/method node.
fn find_enclosing_function(node: Node) -> Option<Node> {
    let mut current = node.parent();
    while let Some(n) = current {
        match n.kind() {
            "method_declaration"
            | "function_definition"
            | "arrow_function"
            | "anonymous_function"
            | "anonymous_function_creation_expression" => {
                return Some(n);
            }
            _ => current = n.parent(),
        }
    }
    None
}

/// Find the enclosing class/interface/trait declaration node.
fn find_enclosing_class_node(node: Node) -> Option<Node> {
    let mut current = node.parent();
    while let Some(n) = current {
        match n.kind() {
            "class_declaration"
            | "interface_declaration"
            | "trait_declaration"
            | "enum_declaration" => {
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
    resolver: Option<MemberTypeResolver<'_>>,
) -> Option<String> {
    infer_variable_in_scope(
        scope_node,
        var_name,
        usage_start,
        source,
        file_symbols,
        resolver,
    )
    .resolved_type_fqn
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
fn find_variable_inference_before_usage(
    body: Node,
    var_name: &str,
    usage_start: usize,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
) -> Option<VariableInference> {
    let mut inferred: Option<(usize, VariableInference)> = None;

    for i in 0..body.named_child_count() {
        let stmt = match body.named_child(i) {
            Some(s) => s,
            None => continue,
        };

        // Only look at statements before the usage point.
        if stmt.start_byte() >= usage_start {
            break;
        }

        let assignment_rhs = assignment_rhs_for_var(stmt, var_name, source)
            .filter(|right| right.end_byte() <= usage_start);

        // Inline PHPDoc immediately before statement:
        //  - apply named @var always when variable matches
        //  - unnamed @var only for direct assignment to target variable
        if let Some(doc_info) = extract_preceding_phpdoc_var_inference(
            stmt,
            var_name,
            assignment_rhs.is_some(),
            source,
            file_symbols,
        ) {
            inferred = Some((stmt.start_byte(), doc_info));
            continue;
        }

        if let Some(guard_info) =
            instanceof_guard_inference(stmt, var_name, usage_start, source, file_symbols)
        {
            inferred = Some((stmt.start_byte(), guard_info));
            continue;
        }

        if let Some(foreach_info) =
            foreach_value_inference(stmt, var_name, usage_start, source, file_symbols, resolver)
        {
            inferred = Some((stmt.start_byte(), foreach_info));
            continue;
        }

        // Assignment inference: $var = <expr>;
        if let Some(right) = assignment_rhs {
            if let Some(resolved) = try_resolve_object_type(right, source, file_symbols, resolver) {
                let type_info = Some(TypeInfo::Simple(resolved.clone()));
                inferred = Some((
                    stmt.start_byte(),
                    VariableInference {
                        type_display: Some(resolved.clone()),
                        resolved_type_fqn: Some(resolved),
                        phpdoc_comment: None,
                        type_info,
                    },
                ));
            }
        }

        // Best-effort flow inference for assignments inside a completed branch:
        // `$session = null; if (...) { $session = $request->getSession(); } $session?->...`
        //
        // The variable is still nullable at runtime, but for member completion
        // and definition the assigned object type is useful.
        if stmt.end_byte() <= usage_start {
            if let Some(stmt_info) = find_nested_variable_inference_before_usage(
                stmt,
                var_name,
                usage_start,
                source,
                file_symbols,
                resolver,
            ) {
                inferred = Some((stmt.start_byte(), stmt_info));
                continue;
            }
        }

        // If the usage sits inside this statement, continue only down that
        // containing branch/block. This finds assignments in the active nested
        // scope without borrowing types from sibling branches.
        if stmt.end_byte() > usage_start {
            if let Some(stmt_info) = find_variable_inference_before_usage(
                stmt,
                var_name,
                usage_start,
                source,
                file_symbols,
                resolver,
            ) {
                inferred = Some((stmt.start_byte(), stmt_info));
            }
            break;
        }
    }

    inferred.map(|(_, info)| info)
}

fn find_nested_variable_inference_before_usage(
    node: Node,
    var_name: &str,
    usage_start: usize,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
) -> Option<VariableInference> {
    let mut inferred: Option<(usize, VariableInference)> = None;

    for i in 0..node.named_child_count() {
        let child = match node.named_child(i) {
            Some(child) => child,
            None => continue,
        };
        if child.start_byte() >= usage_start {
            break;
        }
        if is_variable_inference_scope_boundary(child) {
            continue;
        }

        let assignment_rhs = assignment_rhs_for_var(child, var_name, source)
            .filter(|right| right.end_byte() <= usage_start);

        if let Some(doc_info) = extract_preceding_phpdoc_var_inference(
            child,
            var_name,
            assignment_rhs.is_some(),
            source,
            file_symbols,
        ) {
            inferred = Some((child.start_byte(), doc_info));
        } else if let Some(right) = assignment_rhs {
            if let Some(resolved) = try_resolve_object_type(right, source, file_symbols, resolver) {
                inferred = Some((
                    child.start_byte(),
                    VariableInference {
                        type_display: Some(resolved.clone()),
                        resolved_type_fqn: Some(resolved.clone()),
                        phpdoc_comment: None,
                        type_info: Some(TypeInfo::Simple(resolved)),
                    },
                ));
            }
        }

        if child.end_byte() <= usage_start {
            if let Some(child_info) = find_nested_variable_inference_before_usage(
                child,
                var_name,
                usage_start,
                source,
                file_symbols,
                resolver,
            ) {
                inferred = Some((child.start_byte(), child_info));
            }
        }
    }

    inferred.map(|(_, info)| info)
}

fn is_variable_inference_scope_boundary(node: Node) -> bool {
    matches!(
        node.kind(),
        "method_declaration"
            | "function_definition"
            | "arrow_function"
            | "anonymous_function"
            | "anonymous_function_creation_expression"
            | "class_declaration"
            | "interface_declaration"
            | "trait_declaration"
            | "enum_declaration"
    )
}

fn instanceof_guard_inference(
    stmt: Node,
    var_name: &str,
    usage_start: usize,
    source: &str,
    file_symbols: &FileSymbols,
) -> Option<VariableInference> {
    if stmt.kind() != "if_statement" || stmt.end_byte() > usage_start {
        return positive_instanceof_branch_inference(
            stmt,
            var_name,
            usage_start,
            source,
            file_symbols,
        );
    }

    let condition = if_condition_text(stmt, source)?;
    if !negative_instanceof_guard_for_var(condition, var_name)
        || !if_then_branch_exits(stmt, source)
    {
        return None;
    }

    let class_name = class_name_after_instanceof(condition)?;
    let resolved = resolve_class_name(&class_name, file_symbols);
    Some(VariableInference {
        type_display: Some(class_name),
        resolved_type_fqn: Some(resolved.clone()),
        phpdoc_comment: None,
        type_info: Some(TypeInfo::Simple(resolved)),
    })
}

fn positive_instanceof_branch_inference(
    stmt: Node,
    var_name: &str,
    usage_start: usize,
    source: &str,
    file_symbols: &FileSymbols,
) -> Option<VariableInference> {
    if stmt.kind() != "if_statement" {
        return None;
    }

    let body = stmt.child_by_field_name("body")?;
    if usage_start < body.start_byte() || usage_start > body.end_byte() {
        return None;
    }

    let condition = if_condition_text(stmt, source)?;
    if !positive_instanceof_guard_for_var(condition, var_name) {
        return None;
    }

    let class_name = class_name_after_instanceof(condition)?;
    let resolved = resolve_class_name(&class_name, file_symbols);
    Some(VariableInference {
        type_display: Some(class_name),
        resolved_type_fqn: Some(resolved.clone()),
        phpdoc_comment: None,
        type_info: Some(TypeInfo::Simple(resolved)),
    })
}

fn if_condition_text<'a>(stmt: Node, source: &'a str) -> Option<&'a str> {
    let text = &source[stmt.byte_range()];
    let start = text.find('(')?;
    let mut depth = 0usize;
    for (offset, ch) in text[start..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(&text[start + 1..start + offset]);
                }
            }
            _ => {}
        }
    }
    None
}

fn negative_instanceof_guard_for_var(condition: &str, var_name: &str) -> bool {
    let compact: String = condition.chars().filter(|ch| !ch.is_whitespace()).collect();
    compact.starts_with(&format!("!{}instanceof", var_name))
        || compact.starts_with(&format!("!({}instanceof", var_name))
}

fn positive_instanceof_guard_for_var(condition: &str, var_name: &str) -> bool {
    let compact: String = condition.chars().filter(|ch| !ch.is_whitespace()).collect();
    compact.starts_with(&format!("{}instanceof", var_name))
        || compact.starts_with(&format!("({}instanceof", var_name))
}

fn if_then_branch_exits(stmt: Node, source: &str) -> bool {
    let text = &source[stmt.byte_range()];
    let then_text = text.split("else").next().unwrap_or(text);
    then_text.contains("throw ")
        || then_text.contains("return")
        || then_text.contains("exit")
        || then_text.contains("die(")
}

fn class_name_after_instanceof(condition: &str) -> Option<String> {
    let (_, after) = condition.split_once("instanceof")?;
    let class_name: String = after
        .trim_start()
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == '\\')
        .collect();
    (!class_name.is_empty()).then_some(class_name)
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

fn extract_preceding_phpdoc_var_inference(
    stmt: Node,
    var_name: &str,
    allow_unnamed_var_tag: bool,
    source: &str,
    file_symbols: &FileSymbols,
) -> Option<VariableInference> {
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

    let type_display = Some(type_info.to_string());
    let resolved_type_fqn = resolve_phpdoc_var_type(&type_info, stmt, source, file_symbols);
    Some(VariableInference {
        type_display,
        resolved_type_fqn,
        phpdoc_comment: Some(comment.to_string()),
        type_info: Some(type_info),
    })
}

fn infer_variable_in_scope(
    scope_node: Node,
    var_name: &str,
    usage_start: usize,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
) -> VariableInference {
    let mut inferred = VariableInference::default();

    // 1. Check function parameters for typed variables.
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
                                inferred.type_display =
                                    Some(source[type_node.byte_range()].trim().to_string());
                                if let Some(class_name) = extract_type_name(type_node, source) {
                                    let resolved = resolve_type_name_in_context(
                                        &class_name,
                                        param,
                                        source,
                                        file_symbols,
                                    );
                                    inferred.resolved_type_fqn = Some(resolved.clone());
                                    inferred.type_info = Some(TypeInfo::Simple(resolved));
                                }
                            }
                            break;
                        }
                    }
                }
            }
        }
    }

    // 2. Scan statements before usage for assignments and inline @var docs.
    let statements = scope_node.child_by_field_name("body").unwrap_or(scope_node);
    if let Some(stmt_info) = find_variable_inference_before_usage(
        statements,
        var_name,
        usage_start,
        source,
        file_symbols,
        resolver,
    ) {
        inferred = stmt_info;
    }

    inferred
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
        TypeInfo::Generic { base, .. } => {
            if is_builtin_non_object_type(base) {
                None
            } else {
                Some(resolve_class_name(base, file_symbols))
            }
        }
        TypeInfo::ClassString(_)
        | TypeInfo::ArrayShape(_)
        | TypeInfo::Callable { .. }
        | TypeInfo::LiteralString(_)
        | TypeInfo::LiteralInt(_)
        | TypeInfo::LiteralFloat(_)
        | TypeInfo::LiteralBool(_)
        | TypeInfo::LiteralNull
        | TypeInfo::Void
        | TypeInfo::Never
        | TypeInfo::Mixed => None,
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

fn resolve_symbol_type_info_to_object_fqn(
    type_info: &TypeInfo,
    owner_fqn: &str,
    context_node: Node,
    source: &str,
    file_symbols: &FileSymbols,
) -> Option<String> {
    match type_info {
        TypeInfo::Self_ | TypeInfo::Static_ => Some(owner_fqn.to_string()),
        TypeInfo::Simple(name) if matches!(name.as_str(), "$this" | "self" | "static") => {
            Some(owner_fqn.to_string())
        }
        TypeInfo::Parent_ => None,
        TypeInfo::Nullable(inner) => resolve_symbol_type_info_to_object_fqn(
            inner,
            owner_fqn,
            context_node,
            source,
            file_symbols,
        ),
        TypeInfo::Union(types) | TypeInfo::Intersection(types) => types.iter().find_map(|ty| {
            resolve_symbol_type_info_to_object_fqn(
                ty,
                owner_fqn,
                context_node,
                source,
                file_symbols,
            )
        }),
        _ => resolve_phpdoc_var_type(type_info, context_node, source, file_symbols),
    }
}

fn infer_expression_type_info(
    node: Node,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
) -> Option<TypeInfo> {
    match node.kind() {
        "parenthesized_expression" => {
            for i in 0..node.named_child_count() {
                if let Some(child) = node.named_child(i) {
                    if let Some(type_info) =
                        infer_expression_type_info(child, source, file_symbols, resolver)
                    {
                        return Some(type_info);
                    }
                }
            }
            None
        }
        "variable_name" => {
            let var_name = normalize_var_name(&source[node.byte_range()]);
            let scope = find_enclosing_function(node).unwrap_or_else(|| find_root_node(node));
            infer_variable_in_scope(
                scope,
                &var_name,
                node.start_byte(),
                source,
                file_symbols,
                resolver,
            )
            .type_info
        }
        "object_creation_expression" => {
            for i in 0..node.named_child_count() {
                if let Some(child) = node.named_child(i) {
                    if matches!(child.kind(), "name" | "qualified_name") {
                        return Some(TypeInfo::Simple(resolve_class_name(
                            &source[child.byte_range()],
                            file_symbols,
                        )));
                    }
                }
            }
            None
        }
        "member_access_expression" | "nullsafe_member_access_expression" => {
            let object = node.child_by_field_name("object")?;
            let name = node.child_by_field_name("name")?;
            let class_fqn = try_resolve_object_type(object, source, file_symbols, resolver)?;
            let prop_fqn = format!("{}::${}", class_fqn, &source[name.byte_range()]);
            file_symbols.symbols.iter().find_map(|sym| {
                (sym.fqn == prop_fqn)
                    .then(|| sym.signature.as_ref()?.return_type.clone())
                    .flatten()
            })
        }
        "member_call_expression" | "nullsafe_member_call_expression" => {
            let object = node.child_by_field_name("object")?;
            let name = node.child_by_field_name("name")?;
            let class_fqn = try_resolve_object_type(object, source, file_symbols, resolver)?;
            let method_fqn = format!("{}::{}", class_fqn, &source[name.byte_range()]);
            file_symbols.symbols.iter().find_map(|sym| {
                (sym.fqn == method_fqn)
                    .then(|| sym.signature.as_ref()?.return_type.clone())
                    .flatten()
            })
        }
        "subscript_expression" => {
            let base = node.named_child(0)?;
            let key_text = node
                .named_child(1)
                .map(|node| source[node.byte_range()].trim().to_string());
            let base_type = infer_expression_type_info(base, source, file_symbols, resolver)?;
            iterable_value_type_info(&base_type, key_text.as_deref())
        }
        _ => None,
    }
}

fn foreach_value_inference(
    stmt: Node,
    var_name: &str,
    usage_start: usize,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
) -> Option<VariableInference> {
    if stmt.kind() != "foreach_statement"
        || usage_start < stmt.start_byte()
        || usage_start > stmt.end_byte()
    {
        return None;
    }

    let value_node = foreach_value_variable_node(stmt, source)?;
    if normalize_var_name(&source[value_node.byte_range()]) != var_name {
        return None;
    }

    let iterable_node = foreach_iterable_node(stmt)?;
    let iterable_type = infer_expression_type_info(iterable_node, source, file_symbols, resolver)?;
    let value_type = iterable_value_type_info(&iterable_type, None)?;
    let resolved_type_fqn = resolve_phpdoc_var_type(&value_type, stmt, source, file_symbols);
    Some(VariableInference {
        type_display: Some(value_type.to_string()),
        resolved_type_fqn,
        phpdoc_comment: None,
        type_info: Some(value_type),
    })
}

fn foreach_iterable_node(stmt: Node) -> Option<Node> {
    stmt.named_child(0)
}

fn foreach_value_variable_node<'a>(stmt: Node<'a>, source: &str) -> Option<Node<'a>> {
    let value_expr = match stmt.named_child(1)? {
        pair if pair.kind() == "pair" => {
            let count = pair.named_child_count();
            pair.named_child(count.saturating_sub(1))?
        }
        value => value,
    };
    variable_node_in_foreach_part(value_expr, source)
}

fn variable_node_in_foreach_part<'a>(node: Node<'a>, source: &str) -> Option<Node<'a>> {
    if node.kind() == "variable_name" && source[node.byte_range()].starts_with('$') {
        return Some(node);
    }
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            if let Some(found) = variable_node_in_foreach_part(child, source) {
                return Some(found);
            }
        }
    }
    None
}

fn infer_textual_array_access_type(
    scope_node: Node,
    expr_text: &str,
    usage_start: usize,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
) -> Option<String> {
    let (base_var, key_text) = parse_textual_array_access(expr_text)?;
    let base = infer_variable_in_scope(
        scope_node,
        &base_var,
        usage_start,
        source,
        file_symbols,
        resolver,
    );
    let base_type = base.type_info.as_ref()?;
    let value_type = iterable_value_type_info(base_type, key_text.as_deref())?;
    resolve_phpdoc_var_type(&value_type, scope_node, source, file_symbols)
}

fn parse_textual_array_access(expr_text: &str) -> Option<(String, Option<String>)> {
    let bracket = expr_text.find('[')?;
    let base = expr_text[..bracket].trim();
    if !base.starts_with('$') || base.len() <= 1 {
        return None;
    }
    let key = expr_text[bracket + 1..]
        .split(']')
        .next()
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_string);
    Some((normalize_var_name(base), key))
}

fn iterable_value_type_info(type_info: &TypeInfo, key_text: Option<&str>) -> Option<TypeInfo> {
    match type_info {
        TypeInfo::Nullable(inner) => iterable_value_type_info(inner, key_text),
        TypeInfo::Union(types) | TypeInfo::Intersection(types) => types
            .iter()
            .find_map(|ty| iterable_value_type_info(ty, key_text)),
        TypeInfo::Generic { base, args } => generic_value_type_arg(base, args).cloned(),
        TypeInfo::ArrayShape(items) => array_shape_value_type(items, key_text).cloned(),
        _ => None,
    }
}

fn generic_value_type_arg<'a>(base: &str, args: &'a [TypeInfo]) -> Option<&'a TypeInfo> {
    if args.is_empty() {
        return None;
    }

    let base = base.trim_start_matches('\\').to_ascii_lowercase();
    let value_arg_index = match base.as_str() {
        "array" | "iterable" | "traversable" | "iterator" | "iteratoraggregate" | "generator" => {
            usize::from(args.len() > 1)
        }
        "list" | "non-empty-list" | "arrayobject" => 0,
        _ if base.ends_with("\\collection")
            || base.ends_with("collection")
            || base.ends_with("\\arrayobject") =>
        {
            usize::from(args.len() > 1)
        }
        _ => return None,
    };

    args.get(value_arg_index)
}

fn array_shape_value_type<'a>(
    items: &'a [php_lsp_types::ArrayShapeItem],
    key_text: Option<&str>,
) -> Option<&'a TypeInfo> {
    let key = key_text.and_then(normalize_array_access_key);
    if let Some(key) = key {
        if let Some(item) = items.iter().find(|item| {
            item.key
                .as_deref()
                .and_then(normalize_array_access_key)
                .as_deref()
                == Some(key.as_str())
        }) {
            return Some(&item.value);
        }
    }

    items
        .iter()
        .find(|item| item.key.is_none())
        .or_else(|| items.first())
        .map(|item| &item.value)
}

fn normalize_array_access_key(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_matches(|ch| ch == '\'' || ch == '"');
    (!trimmed.is_empty()).then(|| trimmed.to_string())
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

/// Resolve a static access scope name using the current CST context.
///
/// This handles `self`, `static`, and `parent` by walking to the enclosing
/// class-like declaration, while preserving normal namespace/use resolution for
/// explicit class names.
pub fn resolve_scope_class_name_pub(
    scope_name: &str,
    context_node: Node,
    source: &str,
    file_symbols: &FileSymbols,
) -> String {
    resolve_scope_class_name(scope_name, context_node, source, file_symbols)
}

/// Resolve a class name using use statements and current namespace.
pub fn resolve_class_name(name: &str, file_symbols: &FileSymbols) -> String {
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
            | "nullsafe_member_call_expression"
            | "namespace_use_clause"
            | "namespace_definition"
    )
}

/// Check if a node is inside a `namespace_use_clause` (a use statement).
fn is_inside_use_clause(node: Node, parent: Node) -> bool {
    // Walk up a few levels looking for namespace_use_clause.
    // Possible structures:
    //   name → namespace_use_clause  (single-segment)
    //   name → qualified_name → namespace_use_clause
    //   name → namespace_name → qualified_name → namespace_use_clause
    let _ = node; // suppress unused warning
    let mut current = parent;
    for _ in 0..3 {
        if current.kind() == "namespace_use_clause" {
            return true;
        }
        match current.parent() {
            Some(p) => current = p,
            None => break,
        }
    }
    false
}

/// Extract the full FQN string from a use clause.
///
/// For `use Doctrine\ORM\EntityManagerInterface;`, returns
/// `Doctrine\ORM\EntityManagerInterface` regardless of which segment
/// the cursor is on.
fn extract_use_clause_fqn(node: Node, parent: Node, source: &str) -> String {
    // Walk up to find the namespace_use_clause node
    let _ = node; // suppress unused
    let mut current = parent;
    for _ in 0..4 {
        if current.kind() == "namespace_use_clause" {
            // The namespace_use_clause contains a qualified_name or name child
            for i in 0..current.named_child_count() {
                if let Some(child) = current.named_child(i) {
                    match child.kind() {
                        "qualified_name" | "name" => {
                            return source[child.byte_range()].to_string();
                        }
                        _ => {}
                    }
                }
            }
            return source[current.byte_range()].to_string();
        }
        match current.parent() {
            Some(p) => current = p,
            None => break,
        }
    }
    source[parent.byte_range()].to_string()
}

#[allow(clippy::type_complexity)]
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
        "variable_name" if is_by_ref_output_argument_variable(node, source) => {
            if normalize_var_name(&source[node.byte_range()]) == var_name {
                let start = node.start_byte();
                if start < usage_start {
                    *best = Some((start, node_range(node)));
                }
            }
        }
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

fn collect_variable_declarations_before(
    node: Node,
    usage_start: usize,
    source: &str,
    vars: &mut Vec<(usize, String)>,
) {
    if node.start_byte() >= usage_start {
        return;
    }

    match node.kind() {
        "variable_name" if is_by_ref_output_argument_variable(node, source) => {
            collect_variable_node(node, usage_start, source, vars);
        }
        "simple_parameter" | "property_promotion_parameter" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                collect_variable_node(name_node, usage_start, source, vars);
            }
        }
        "assignment_expression" | "by_ref_assignment_expression" => {
            if let Some(left) = node.child_by_field_name("left") {
                collect_variable_node(left, usage_start, source, vars);
            }
        }
        "foreach_statement" => {
            for field in ["key", "value"] {
                if let Some(var_node) = node.child_by_field_name(field) {
                    collect_variable_node(var_node, usage_start, source, vars);
                }
            }
        }
        "catch_clause" => {
            for field in ["name", "variable"] {
                if let Some(var_node) = node.child_by_field_name(field) {
                    collect_variable_node(var_node, usage_start, source, vars);
                }
            }
        }
        _ => {}
    }

    let cursor = &mut node.walk();
    for child in node.named_children(cursor) {
        collect_variable_declarations_before(child, usage_start, source, vars);
    }
}

fn collect_variable_node(
    node: Node,
    usage_start: usize,
    source: &str,
    vars: &mut Vec<(usize, String)>,
) {
    if node.start_byte() >= usage_start {
        return;
    }
    let text = &source[node.byte_range()];
    if !text.trim_start().starts_with('$') {
        return;
    }
    vars.push((node.start_byte(), normalize_var_name(text)));
}

fn is_by_ref_output_argument_variable(node: Node, source: &str) -> bool {
    let Some(argument) = ancestor_before_scope(node, "argument") else {
        return false;
    };
    let Some(arguments) = argument
        .parent()
        .filter(|parent| parent.kind() == "arguments")
    else {
        return false;
    };
    let Some(call) = arguments
        .parent()
        .filter(|parent| parent.kind() == "function_call_expression")
    else {
        return false;
    };
    let Some(function_node) = call
        .child_by_field_name("function")
        .or_else(|| call.named_child(0))
    else {
        return false;
    };

    let function_name = source[function_node.byte_range()]
        .trim()
        .trim_start_matches('\\')
        .rsplit('\\')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();

    if !matches!(function_name.as_str(), "preg_match" | "preg_match_all") {
        return false;
    }

    argument_name(argument, source).is_some_and(|name| name == "matches")
        || argument_index(arguments, argument).is_some_and(|index| index == 2)
}

fn ancestor_before_scope<'tree>(node: Node<'tree>, ancestor_kind: &str) -> Option<Node<'tree>> {
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == ancestor_kind {
            return Some(parent);
        }
        if matches!(
            parent.kind(),
            "method_declaration"
                | "function_definition"
                | "anonymous_function"
                | "anonymous_function_creation_expression"
                | "program"
        ) {
            return None;
        }
        current = parent.parent();
    }
    None
}

fn argument_index(arguments: Node, argument: Node) -> Option<usize> {
    let mut cursor = arguments.walk();
    let index = arguments
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "argument")
        .position(|child| child.id() == argument.id());
    index
}

fn argument_name(argument: Node, source: &str) -> Option<String> {
    if let Some(name_node) = argument.child_by_field_name("name") {
        return Some(normalize_argument_name(&source[name_node.byte_range()]));
    }

    let text = &source[argument.byte_range()];
    let colon_index = text.find(':')?;
    let value_start = argument
        .child_by_field_name("value")
        .or_else(|| {
            let mut cursor = argument.walk();
            argument.named_children(&mut cursor).last()
        })
        .map(|value| value.start_byte().saturating_sub(argument.start_byte()))
        .unwrap_or(text.len());

    (colon_index < value_start).then(|| normalize_argument_name(&text[..colon_index]))
}

fn normalize_argument_name(name: &str) -> String {
    name.trim()
        .trim_start_matches('$')
        .trim_end_matches(':')
        .trim()
        .to_string()
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

fn find_anonymous_class_parent_fqn(
    context_node: Node,
    source: &str,
    file_symbols: &FileSymbols,
) -> Option<String> {
    let mut current = Some(context_node);
    while let Some(node) = current {
        if node.kind() == "object_creation_expression"
            && source[node.byte_range()]
                .trim_start()
                .starts_with("new class")
        {
            return first_base_clause_fqn(node, source, file_symbols);
        }
        current = node.parent();
    }
    None
}

fn first_base_clause_fqn(node: Node, source: &str, file_symbols: &FileSymbols) -> Option<String> {
    if node.kind() == "declaration_list" {
        return None;
    }
    if node.kind() == "base_clause" {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if matches!(child.kind(), "name" | "qualified_name") {
                let name = &source[child.byte_range()];
                return Some(resolve_class_name(name, file_symbols));
            }
        }
        return None;
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(fqn) = first_base_clause_fqn(child, source, file_symbols) {
            return Some(fqn);
        }
    }
    None
}

fn find_extended_parent_class_fqn(
    context_node: Node,
    source: &str,
    file_symbols: &FileSymbols,
) -> Option<String> {
    if let Some(parent_fqn) = find_anonymous_class_parent_fqn(context_node, source, file_symbols) {
        return Some(parent_fqn);
    }

    let current_class_fqn = find_parent_class_fqn(context_node, source, file_symbols)?;
    file_symbols
        .symbols
        .iter()
        .find(|sym| sym.fqn == current_class_fqn)
        .and_then(|sym| sym.extends.first().cloned())
}

fn resolve_scope_class_name(
    scope_name: &str,
    context_node: Node,
    source: &str,
    file_symbols: &FileSymbols,
) -> String {
    match scope_name {
        "self" | "static" => find_parent_class_fqn(context_node, source, file_symbols)
            .unwrap_or_else(|| scope_name.to_string()),
        "parent" => find_extended_parent_class_fqn(context_node, source, file_symbols)
            .unwrap_or_else(|| scope_name.to_string()),
        _ => resolve_class_name(scope_name, file_symbols),
    }
}

fn resolve_type_name_in_context(
    type_name: &str,
    context_node: Node,
    source: &str,
    file_symbols: &FileSymbols,
) -> String {
    match type_name.trim_start_matches('\\') {
        "self" | "static" => find_parent_class_fqn(context_node, source, file_symbols)
            .unwrap_or_else(|| type_name.to_string()),
        "parent" => find_extended_parent_class_fqn(context_node, source, file_symbols)
            .unwrap_or_else(|| type_name.to_string()),
        _ => resolve_class_name(type_name, file_symbols),
    }
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

    fn parse_and_local_variable_names(code: &str, line: u32, col: u32) -> Vec<String> {
        let mut parser = FileParser::new();
        parser.parse_full(code);
        let tree = parser.tree().unwrap();
        local_variable_names_at_position(tree, code, line, col)
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

    fn parse_and_variable_hover_info(code: &str, line: u32, col: u32) -> Option<VariableHoverInfo> {
        let mut parser = FileParser::new();
        parser.parse_full(code);
        let tree = parser.tree().unwrap();
        let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
        variable_hover_info_at_position(tree, code, &file_symbols, line, col)
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
        assert_eq!(sym.fqn, "App\\Service\\UserService::__construct");
        assert_eq!(sym.ref_kind, RefKind::Constructor);
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
    fn test_infer_variable_type_after_negative_instanceof_guard() {
        let code = r#"<?php
namespace App\Repository;

use App\Entity\User;
use Symfony\Component\Security\Core\User\PasswordAuthenticatedUserInterface;

class UserRepository {
    public function upgradePassword(PasswordAuthenticatedUserInterface $user): void {
        if (!$user instanceof User) {
            throw new \LogicException();
        }

        $user->setPassword('secret');
    }
}
"#;
        let (line, col) = find_line_col(code, "setPassword");
        let result = parse_and_infer_var_type_at(code, line, col, "$user");

        assert_eq!(result.as_deref(), Some("App\\Entity\\User"));
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
    fn test_resolve_parent_scope_to_extended_class() {
        let code = r#"<?php
namespace App;

class Base {
    public function run(): void {}
}

class Child extends Base {
    public function test(): void {
        parent::run();
    }
}
"#;
        let (line, col) = find_line_col(code, "parent::run");

        let scope = parse_and_resolve(code, line, col).expect("parent scope should resolve");
        assert_eq!(scope.name, "parent");
        assert_eq!(scope.ref_kind, RefKind::ClassName);
        assert_eq!(scope.fqn, "App\\Base");

        let method_col = col + "parent::".len() as u32;
        let method =
            parse_and_resolve(code, line, method_col).expect("parent method should resolve");
        assert_eq!(method.name, "run");
        assert_eq!(method.ref_kind, RefKind::MethodCall);
        assert_eq!(method.fqn, "App\\Base::run");
    }

    #[test]
    fn test_resolve_parent_scope_inside_anonymous_class() {
        let code = r#"<?php
namespace App;

class ControllerHelper {
    public function __construct() {}
}

class Outer {
    public function create(): object {
        return new class extends ControllerHelper {
            public function setContainer(): void {
                parent::__construct();
            }
        };
    }
}
"#;
        let (line, col) = find_line_col(code, "parent::__construct");

        let scope = parse_and_resolve(code, line, col)
            .expect("anonymous class parent scope should resolve");
        assert_eq!(scope.name, "parent");
        assert_eq!(scope.ref_kind, RefKind::ClassName);
        assert_eq!(scope.fqn, "App\\ControllerHelper");

        let method_col = col + "parent::".len() as u32;
        let method = parse_and_resolve(code, line, method_col)
            .expect("anonymous class parent method should resolve");
        assert_eq!(method.name, "__construct");
        assert_eq!(method.fqn, "App\\ControllerHelper::__construct");
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
    fn test_infer_variable_type_from_fully_qualified_new_expression() {
        let code = r#"<?php
namespace App;

function run(object $object, string $method): void
{
    $reflMethod = new \ReflectionMethod($object, $method);
    $reflMethod->
}
"#;
        let (line, col) = find_line_col(code, "$reflMethod->");
        let result = parse_and_infer_var_type_at(
            code,
            line,
            col + "$reflMethod->".len() as u32,
            "$reflMethod",
        );

        assert_eq!(result.as_deref(), Some("ReflectionMethod"));

        let result_inside_member_name = parse_and_infer_var_type_at(
            code,
            line,
            col + "$reflMethod->isSt".len() as u32,
            "$reflMethod",
        );
        assert_eq!(
            result_inside_member_name.as_deref(),
            Some("ReflectionMethod")
        );
    }

    #[test]
    fn test_infer_variable_type_from_assignment_inside_elseif_branch() {
        let code = r#"<?php
namespace App;

function run(object $object, mixed $method): void
{
    if ($method instanceof \Closure) {
        $method($object);
    } elseif (\is_array($method)) {
        $method($object);
    } elseif (null !== $object) {
        if (!method_exists($object, $method)) {
            throw new \RuntimeException();
        }

        $reflMethod = new \ReflectionMethod($object, $method);

        if ($reflMethod->isStatic()) {
        }
    }
}
"#;
        let (line, col) = find_line_col(code, "$reflMethod->");
        let result = parse_and_infer_var_type_at(
            code,
            line,
            col + "$reflMethod->".len() as u32,
            "$reflMethod",
        );

        assert_eq!(result.as_deref(), Some("ReflectionMethod"));
    }

    #[test]
    fn test_infer_variable_type_from_completed_if_assignment() {
        let code = r#"<?php
namespace App;

class Session {
    public function get(): string { return ''; }
}

function run(bool $enabled): void
{
    $session = null;
    if ($enabled) {
        $session = new Session();
    }

    $session?->get();
}
"#;
        let (line, col) = find_line_col(code, "$session?->");
        let result =
            parse_and_infer_var_type_at(code, line, col + "$session?->".len() as u32, "$session");

        assert_eq!(result.as_deref(), Some("App\\Session"));
    }

    #[test]
    fn test_infer_variable_type_from_completed_if_method_return_with_resolver() {
        let code = r#"<?php
namespace App;

use Symfony\Component\HttpFoundation\Request;

function run(Request $request, bool $enabled): void
{
    $session = null;
    if ($enabled) {
        $session = $request->getSession();
    }

    $session?->get();
}
"#;
        let mut parser = FileParser::new();
        parser.parse_full(code);
        let tree = parser.tree().unwrap();
        let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
        let (line, col) = find_line_col(code, "$session?->");
        let resolver = |class_fqn: &str, member_name: &str| {
            (class_fqn == "Symfony\\Component\\HttpFoundation\\Request"
                && member_name == "getSession")
                .then(|| {
                    "Symfony\\Component\\HttpFoundation\\Session\\SessionInterface".to_string()
                })
        };
        let result = infer_variable_type_at_position_with_resolver(
            tree,
            code,
            &file_symbols,
            line,
            col + "$session?->".len() as u32,
            "$session",
            &resolver,
        );

        assert_eq!(
            result.as_deref(),
            Some("Symfony\\Component\\HttpFoundation\\Session\\SessionInterface")
        );
    }

    #[test]
    fn test_resolve_nullable_method_call_from_completed_if_assignment() {
        let code = r#"<?php
namespace App;

class Session {
    public function get(string $key): string { return ''; }
}

function run(bool $enabled): void
{
    $session = null;
    if ($enabled) {
        $session = new Session();
    }

    $session?->get('token');
}
"#;
        let (line, col) = find_line_col(code, "get('token')");
        let result = parse_and_resolve(code, line, col)
            .expect("nullable method call should resolve from completed if assignment");

        assert_eq!(result.fqn, "App\\Session::get");
    }

    #[test]
    fn test_resolve_self_reassignment_rhs_does_not_recurse() {
        let code = r#"<?php
namespace App;

class Generator {
    public function randomBased(): self { return $this; }
    public function generateId(): void {}
}

class Demo {
    public function run(): void {
        $generator = new Generator();
        $generator = $generator->randomBased();
        $generator->generateId();
    }
}
"#;
        let (line, col) = find_line_col(code, "randomBased");
        let reassignment_call =
            parse_and_resolve(code, line, col).expect("self-reassignment RHS should resolve");
        assert_eq!(reassignment_call.fqn, "App\\Generator::randomBased");

        let (line, col) = find_line_col(code, "generateId");
        let later_call =
            parse_and_resolve(code, line, col).expect("later method call should resolve");
        assert_eq!(later_call.fqn, "App\\Generator::generateId");
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
    fn test_resolve_property_access_on_self_typed_parameter() {
        let code = r#"<?php
namespace App;

final class PromotedSelfDefaults {
    public function __construct(
        public ?string $objectManager = null,
        public ?array $mapping = null,
    ) {}

    public function withDefaults(self $defaults): static {
        $clone = clone $this;
        $clone->objectManager ??= $defaults->objectManager;
        $clone->mapping ??= $defaults->mapping ?? [];
        return $clone;
    }
}
"#;

        let (line, col) = find_line_col(code, "$defaults->objectManager");
        let result = parse_and_resolve(code, line, col + "$defaults->".len() as u32)
            .expect("self typed parameter property access should resolve");

        assert_eq!(result.name, "objectManager");
        assert_eq!(result.fqn, "App\\PromotedSelfDefaults::$objectManager");
        assert_eq!(result.ref_kind, RefKind::PropertyAccess);
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
    fn test_variable_hover_info_from_inline_phpdoc_var() {
        let code = "<?php\nnamespace App;\nuse App\\Test\\Baz;\n\nfunction run(): void {\n    /**\n     * Local baz variable.\n     * @var Baz $baz2\n     */\n    $baz2 = makeBaz();\n    $baz2->test();\n}\n";
        let info = parse_and_variable_hover_info(code, 10, 7).expect("hover info should exist");
        assert_eq!(info.variable_name, "$baz2");
        assert_eq!(info.type_display.as_deref(), Some("Baz"));
        assert_eq!(info.resolved_type_fqn.as_deref(), Some("App\\Test\\Baz"));
        assert!(info
            .phpdoc_comment
            .as_deref()
            .unwrap_or("")
            .contains("@var Baz $baz2"));
    }

    #[test]
    fn test_resolve_foreach_value_from_phpdoc_generic_array() {
        let code = r#"<?php
namespace App;

use App\Entity\User;

function run(): void {
    /** @var array<int, User> $users */
    $users = loadUsers();
    foreach ($users as $user) {
        $user->getName();
    }
}
"#;
        let (line, col) = find_line_col(code, "getName");
        let result = parse_and_resolve(code, line, col).expect("foreach value should resolve");
        assert_eq!(result.fqn, "App\\Entity\\User::getName");
        assert_eq!(result.ref_kind, RefKind::MethodCall);
    }

    #[test]
    fn test_resolve_array_access_from_phpdoc_generic_array() {
        let code = r#"<?php
namespace App;

use App\Entity\User;

function run(): void {
    /** @var array<int, User> $users */
    $users = loadUsers();
    $users[0]->getName();
}
"#;
        let (line, col) = find_line_col(code, "getName");
        let result = parse_and_resolve(code, line, col).expect("array element should resolve");
        assert_eq!(result.fqn, "App\\Entity\\User::getName");
        assert_eq!(result.ref_kind, RefKind::MethodCall);
    }

    #[test]
    fn test_resolve_array_shape_access_from_phpdoc_var() {
        let code = r#"<?php
namespace App;

use App\Entity\User;

function run(): void {
    /** @var array{user: User} $row */
    $row = [];
    $row['user']->getName();
}
"#;
        let (line, col) = find_line_col(code, "getName");
        let result =
            parse_and_resolve(code, line, col).expect("array-shape element should resolve");
        assert_eq!(result.fqn, "App\\Entity\\User::getName");
        assert_eq!(result.ref_kind, RefKind::MethodCall);
    }

    #[test]
    fn test_resolve_array_access_from_phpdoc_generic_method_return() {
        let code = r#"<?php
namespace App;

use App\Entity\User;

class UserRepository {
    /** @return array<int, User> */
    public function findAll() {
        return [];
    }
}

function run(UserRepository $repo): void {
    $repo->findAll()[0]->getName();
}
"#;
        let (line, col) = find_line_col(code, "getName");
        let result =
            parse_and_resolve(code, line, col).expect("generic method return item should resolve");
        assert_eq!(result.fqn, "App\\Entity\\User::getName");
        assert_eq!(result.ref_kind, RefKind::MethodCall);
    }

    #[test]
    fn test_infer_variable_type_at_position_from_phpdoc_array_access_text() {
        let code = r#"<?php
namespace App;

use App\Entity\User;

function run(): void {
    /** @var list<User> $users */
    $users = loadUsers();
    $users[0]->
}
"#;
        let inferred = parse_and_infer_var_type_at(code, 8, 16, "$users[0]")
            .expect("array access object type should be inferred for completion");
        assert_eq!(inferred, "App\\Entity\\User");
    }

    #[test]
    fn test_infer_variable_type_inside_positive_instanceof_branch() {
        let code = r#"<?php
namespace App\Repository;

use App\Entity\User;
use Symfony\Component\Security\Core\User\PasswordAuthenticatedUserInterface;

class UserRepository {
    public function upgradePassword(PasswordAuthenticatedUserInterface $user): void {
        if ($user instanceof User) {
            $user->setPassword('secret');
        }
    }
}
"#;
        let (line, col) = find_line_col(code, "setPassword");
        let result = parse_and_infer_var_type_at(code, line, col, "$user");
        assert_eq!(result.as_deref(), Some("App\\Entity\\User"));
    }

    #[test]
    fn test_resolve_property_access_type_from_property_phpdoc_var() {
        let code = r#"<?php
namespace App;

use App\Entity\User;

class Holder {
    /** @var User */
    private $user;

    public function run(): void {
        $this->user->getName();
    }
}
"#;
        let (line, col) = find_line_col(code, "getName");
        let result = parse_and_resolve(code, line, col).expect("property @var type should resolve");
        assert_eq!(result.fqn, "App\\Entity\\User::getName");
        assert_eq!(result.ref_kind, RefKind::MethodCall);
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
    fn test_find_variable_definition_preg_match_output_argument() {
        let code = r#"<?php
function demo(string $value): void {
    if (!preg_match('/(?P<year>\d+)/', $value, $matches)) {
        return;
    }
    echo $matches['year'];
}
"#;
        let (line, col) = find_line_col(code, "$matches['year']");
        let def = parse_and_find_var_def(code, line, col + 2)
            .expect("preg_match output variable definition should be found");
        let (def_line, def_col) = find_line_col(code, "$matches))");
        assert_eq!(def.0, def_line);
        assert_eq!(def.1, def_col);
    }

    #[test]
    fn test_local_variable_names_include_preg_match_output_argument() {
        let code = r#"<?php
function demo(string $value): void {
    if (!preg_match('/(?P<year>\d+)/', $value, $matches)) {
        return;
    }
    $mat
}
"#;
        let (line, col) = find_line_col(code, "$mat");
        let names = parse_and_local_variable_names(code, line, col + "$mat".len() as u32);
        assert!(
            names.iter().any(|name| name == "$matches"),
            "expected $matches in local variable names, got: {:?}",
            names
        );
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

    #[test]
    fn test_infer_property_type_from_assignments() {
        use crate::parser::FileParser;
        use crate::symbols::extract_file_symbols;

        let code = r#"<?php
namespace App\Tests;

use App\Service\TimerService;
use Doctrine\ORM\EntityManagerInterface;

class MyTest {
    private EntityManagerInterface $em;
    private TimerService $timerService;

    protected function setUp(): void {
        $this->em = $this->createStub(EntityManagerInterface::class);
        $this->timerService = $this->createStub(TimerService::class);
    }

    public function testSomething(): void {
        $this->em->method('findAll');
    }
}
"#;

        let mut parser = FileParser::new();
        parser.parse_full(code);
        let tree = parser.tree().unwrap();
        let file_symbols = extract_file_symbols(tree, code, "test://file");

        // createStub returns Stub type via the resolver
        let resolver = |_class_fqn: &str, member_name: &str| -> Option<String> {
            if member_name == "createStub" {
                Some("PHPUnit\\Framework\\MockObject\\Stub".to_string())
            } else {
                None
            }
        };

        let result = super::infer_property_type_from_assignments(
            tree,
            code,
            "em",
            &file_symbols,
            Some(&resolver),
        );
        assert_eq!(
            result,
            vec!["PHPUnit\\Framework\\MockObject\\Stub".to_string()]
        );

        let result2 = super::infer_property_type_from_assignments(
            tree,
            code,
            "timerService",
            &file_symbols,
            Some(&resolver),
        );
        assert_eq!(
            result2,
            vec!["PHPUnit\\Framework\\MockObject\\Stub".to_string()]
        );

        // Non-existent property should return empty vec
        let result3 = super::infer_property_type_from_assignments(
            tree,
            code,
            "nonexistent",
            &file_symbols,
            Some(&resolver),
        );
        assert!(result3.is_empty());
    }

    #[test]
    fn test_resolve_use_statement_goto_def() {
        let code = "<?php\nuse Doctrine\\ORM\\EntityManagerInterface;\n";

        // Cursor on "EntityManagerInterface" — should resolve full FQN
        let result = parse_and_resolve(code, 1, 20).unwrap();
        assert_eq!(result.fqn, "Doctrine\\ORM\\EntityManagerInterface");
        assert_eq!(result.ref_kind, RefKind::ClassName);

        // Cursor on "Doctrine" (first segment)
        let result2 = parse_and_resolve(code, 1, 4).unwrap();
        assert_eq!(result2.fqn, "Doctrine\\ORM\\EntityManagerInterface");
        assert_eq!(result2.ref_kind, RefKind::ClassName);

        // Cursor on "ORM" (middle segment)
        let result3 = parse_and_resolve(code, 1, 13).unwrap();
        assert_eq!(result3.fqn, "Doctrine\\ORM\\EntityManagerInterface");
        assert_eq!(result3.ref_kind, RefKind::ClassName);

        // Single-segment use statement
        let code2 = "<?php\nuse TestCase;\n";
        let result4 = parse_and_resolve(code2, 1, 4).unwrap();
        assert_eq!(result4.fqn, "TestCase");
        assert_eq!(result4.ref_kind, RefKind::ClassName);
    }

    #[test]
    fn test_resolve_new_qualified_name() {
        // new Assert\NotBlank — qualified name in object_creation_expression
        let code = r#"<?php
namespace App\Form;

use Symfony\Component\Validator\Constraints as Assert;

class Foo {
    public function build(): void {
        $x = new Assert\NotBlank(message: 'Test');
    }
}
"#;
        // Cursor on "NotBlank"
        let (l1, c1) = find_line_col(code, "Assert\\NotBlank");
        let result = parse_and_resolve(code, l1, c1 + 7).unwrap();
        assert_eq!(
            result.fqn,
            "Symfony\\Component\\Validator\\Constraints\\NotBlank::__construct"
        );
        assert_eq!(result.ref_kind, RefKind::Constructor);

        // Cursor on "Assert" (namespace part)
        let result2 = parse_and_resolve(code, l1, c1).unwrap();
        assert_eq!(
            result2.fqn,
            "Symfony\\Component\\Validator\\Constraints\\NotBlank::__construct"
        );
        assert_eq!(result2.ref_kind, RefKind::Constructor);
    }

    #[test]
    fn test_resolve_closure_param_method_call() {
        // Method call on closure parameter with type hint
        let code = r#"<?php
namespace App\Form;

use App\Repository\CatalogRepository;

class Foo {
    public function build(): void {
        $fn = static function (CatalogRepository $repository) {
            return $repository->createQueryBuilder('item');
        };
    }
}
"#;
        // Cursor on "createQueryBuilder"
        let (l1, c1) = find_line_col(code, "createQueryBuilder");
        let result = parse_and_resolve(code, l1, c1).unwrap();
        assert_eq!(
            result.fqn,
            "App\\Repository\\CatalogRepository::createQueryBuilder"
        );
        assert_eq!(result.ref_kind, RefKind::MethodCall);
    }

    #[test]
    fn test_resolve_closure_param_method_chain() {
        // Method call chain on closure parameter: $subscriber->getLastName()
        let code = r#"<?php
namespace App\Form;

use App\Entity\Subscriber;

class Foo {
    public function build(): void {
        $fn = static function (Subscriber $subscriber) {
            return $subscriber->getLastName();
        };
    }
}
"#;
        // Cursor on "getLastName"
        let (l1, c1) = find_line_col(code, "getLastName");
        let result = parse_and_resolve(code, l1, c1).unwrap();
        assert_eq!(result.fqn, "App\\Entity\\Subscriber::getLastName");
        assert_eq!(result.ref_kind, RefKind::MethodCall);
    }

    #[test]
    fn test_resolve_method_chain_static_return_type() {
        // Method chain: $qb->orderBy(...)->addOrderBy(...)
        // orderBy() returns `static`, addOrderBy is on same class
        let code = r#"<?php
namespace App\ORM;

class QueryBuilder {
    public function orderBy(string $sort): static {
        return $this;
    }
    public function addOrderBy(string $sort): static {
        return $this;
    }
}

class Foo {
    public function test(): void {
        $qb = new QueryBuilder();
        $qb->orderBy('a')->addOrderBy('b');
    }
}
"#;
        // Cursor on "addOrderBy" in the chain
        let (l, c) = find_line_col(code, "addOrderBy('b')");
        let result = parse_and_resolve(code, l, c).unwrap();
        assert_eq!(result.fqn, "App\\ORM\\QueryBuilder::addOrderBy");
        assert_eq!(result.ref_kind, RefKind::MethodCall);

        // Cursor on "orderBy" — first in chain
        let (l2, c2) = find_line_col(code, "orderBy('a')");
        let result2 = parse_and_resolve(code, l2, c2).unwrap();
        assert_eq!(result2.fqn, "App\\ORM\\QueryBuilder::orderBy");
        assert_eq!(result2.ref_kind, RefKind::MethodCall);
    }

    #[test]
    fn test_resolve_method_chain_phpdoc_return_this() {
        // Method chain where return type comes from PHPDoc @return $this
        let code = r#"<?php
namespace App\ORM;

class Builder {
    /** @return $this */
    public function where(string $cond) {
        return $this;
    }
    /** @return $this */
    public function setParameter(string $name, $value) {
        return $this;
    }
    /** @return $this */
    public function orderBy(string $sort) {
        return $this;
    }
}

class Foo {
    public function test(): void {
        $b = new Builder();
        $b->where('x')->setParameter('y', 1)->orderBy('z');
    }
}
"#;
        // Cursor on "orderBy" — 3rd in chain
        let (l, c) = find_line_col(code, "orderBy('z')");
        let result = parse_and_resolve(code, l, c).unwrap();
        assert_eq!(result.fqn, "App\\ORM\\Builder::orderBy");
        assert_eq!(result.ref_kind, RefKind::MethodCall);

        // Cursor on "setParameter" — 2nd in chain
        let (l2, c2) = find_line_col(code, "setParameter('y'");
        let result2 = parse_and_resolve(code, l2, c2).unwrap();
        assert_eq!(result2.fqn, "App\\ORM\\Builder::setParameter");
        assert_eq!(result2.ref_kind, RefKind::MethodCall);
    }

    #[test]
    fn test_resolve_method_chain_cross_class_return() {
        // Chain where createQueryBuilder() returns a different class
        let code = r#"<?php
namespace App\ORM;

class QueryBuilder {
    public function orderBy(string $sort): static {
        return $this;
    }
    public function addOrderBy(string $sort): static {
        return $this;
    }
}

class EntityRepository {
    public function createQueryBuilder(string $alias): QueryBuilder {
        return new QueryBuilder();
    }
}

class Foo {
    public function test(): void {
        $er = new EntityRepository();
        $er->createQueryBuilder('s')->orderBy('a')->addOrderBy('b');
    }
}
"#;
        // Cursor on "addOrderBy" — 3rd level chain
        let (l, c) = find_line_col(code, "addOrderBy('b')");
        let result = parse_and_resolve(code, l, c).unwrap();
        assert_eq!(result.fqn, "App\\ORM\\QueryBuilder::addOrderBy");
        assert_eq!(result.ref_kind, RefKind::MethodCall);

        // Cursor on "orderBy" — 2nd level
        let (l2, c2) = find_line_col(code, "orderBy('a')");
        let result2 = parse_and_resolve(code, l2, c2).unwrap();
        assert_eq!(result2.fqn, "App\\ORM\\QueryBuilder::orderBy");
        assert_eq!(result2.ref_kind, RefKind::MethodCall);

        // Cursor on "createQueryBuilder" — 1st level
        let (l3, c3) = find_line_col(code, "createQueryBuilder('s')");
        let result3 = parse_and_resolve(code, l3, c3).unwrap();
        assert_eq!(
            result3.fqn,
            "App\\ORM\\EntityRepository::createQueryBuilder"
        );
        assert_eq!(result3.ref_kind, RefKind::MethodCall);
    }
}
