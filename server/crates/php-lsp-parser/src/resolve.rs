//! Symbol resolution from a CST position.
//!
//! Given a position in a parsed PHP file, determines what symbol is at that
//! position and resolves it to an identifier name, considering namespace context
//! and use statements.

use crate::cst::{argument_index, argument_name, is_by_ref_output_argument_variable};
use crate::phpdoc::{parse_phpdoc, strip_exact_tag};
use crate::utf16::utf16_col_to_byte;
use php_lsp_types::{
    normalize_shape_key_text, FileSymbols, Signature, SymbolInfo, TypeInfo, UseKind,
};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use tree_sitter::{Node, Point, Tree};

const MAX_OBJECT_TYPE_RESOLVE_DEPTH: usize = 64;

thread_local! {
    static OBJECT_TYPE_RESOLVE_DEPTH: Cell<usize> = const { Cell::new(0) };
}

struct ObjectTypeResolveDepthGuard {
    previous: usize,
}

impl ObjectTypeResolveDepthGuard {
    fn enter() -> Option<Self> {
        OBJECT_TYPE_RESOLVE_DEPTH.with(|depth| {
            let previous = depth.get();
            if previous >= MAX_OBJECT_TYPE_RESOLVE_DEPTH {
                return None;
            }

            depth.set(previous + 1);
            Some(Self { previous })
        })
    }
}

impl Drop for ObjectTypeResolveDepthGuard {
    fn drop(&mut self) {
        OBJECT_TYPE_RESOLVE_DEPTH.with(|depth| depth.set(self.previous));
    }
}

/// Callback for resolving a member's type from an external source (e.g., workspace index).
///
/// Takes `(class_fqn, member_name)` and returns the member's type FQN.
/// For properties: `member_name` includes `$` prefix (e.g., `"$timer"`).
/// For methods: `member_name` is the method name (e.g., `"start"`).
///
/// Returns resolved type text (for example, `"App\\TimerService"` or
/// `"Collection<int, App\\Entity\\User>"`) or None.
pub type MemberTypeResolver<'a> = &'a dyn Fn(&str, &str) -> Option<String>;

#[derive(Debug, Clone)]
pub struct ResolvedFunctionType {
    pub type_text: String,
    pub signature: Option<Signature>,
}

impl ResolvedFunctionType {
    pub fn new(type_text: impl Into<String>) -> Self {
        Self {
            type_text: type_text.into(),
            signature: None,
        }
    }

    pub fn with_signature(type_text: impl Into<String>, signature: Option<Signature>) -> Self {
        Self {
            type_text: type_text.into(),
            signature,
        }
    }
}

pub type FunctionTypeResolver<'a> = &'a dyn Fn(&str) -> Option<ResolvedFunctionType>;

/// Context for resolving the expected type of an untyped closure/arrow-function
/// parameter from the callable parameter at the call site.
#[derive(Debug, Clone)]
pub struct CallableArgumentType {
    pub argument_index: usize,
    pub argument_name: Option<String>,
    pub type_info: TypeInfo,
}

#[derive(Debug, Clone)]
pub struct CallableParameterContext<'a> {
    pub target_fqn: &'a str,
    pub argument_index: usize,
    pub argument_name: Option<&'a str>,
    pub parameter_index: usize,
    pub parameter_name: &'a str,
    pub receiver_type: Option<&'a TypeInfo>,
    pub argument_types: &'a [CallableArgumentType],
}

/// Callback for resolving closure/arrow-function parameter types from indexed
/// function or method signatures.
pub type CallableParamTypeResolver<'a> =
    &'a dyn for<'ctx> Fn(CallableParameterContext<'ctx>) -> Option<TypeInfo>;

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
    symbol_at_position_with_resolvers(tree, source, line, character, file_symbols, resolver, None)
}

/// Find the symbol at the given position, with optional cross-file type and
/// callable-parameter resolvers.
pub fn symbol_at_position_with_resolvers(
    tree: &Tree,
    source: &str,
    line: u32,
    character: u32,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
) -> Option<SymbolAtPosition> {
    symbol_at_position_with_full_resolvers(
        tree,
        source,
        line,
        character,
        file_symbols,
        resolver,
        callable_resolver,
        None,
    )
}

/// Find the symbol at the given position, with optional cross-file type,
/// callable-parameter, and function-signature resolvers.
#[allow(clippy::too_many_arguments)]
pub fn symbol_at_position_with_full_resolvers(
    tree: &Tree,
    source: &str,
    line: u32,
    character: u32,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
    function_resolver: Option<FunctionTypeResolver<'_>>,
) -> Option<SymbolAtPosition> {
    let root = tree.root_node();
    let point = Point::new(line as usize, character as usize);

    // Find the most specific node at the position
    let node = find_node_at_point(root, point)?;

    resolve_node(
        node,
        source,
        file_symbols,
        resolver,
        callable_resolver,
        function_resolver,
    )
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
    infer_variable_type_at_position_internal(
        tree,
        source,
        file_symbols,
        line,
        character,
        var_name,
        None,
        None,
    )
    .resolved_type_fqn
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
    infer_variable_type_at_position_with_resolvers(
        tree,
        source,
        file_symbols,
        line,
        character,
        var_name,
        Some(resolver),
        None,
    )
}

/// Infer variable type by name before a given position, using external member
/// and callable-parameter resolvers.
#[allow(clippy::too_many_arguments)]
pub fn infer_variable_type_at_position_with_resolvers(
    tree: &Tree,
    source: &str,
    file_symbols: &FileSymbols,
    line: u32,
    character: u32,
    var_name: &str,
    resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
) -> Option<String> {
    infer_variable_type_at_position_internal(
        tree,
        source,
        file_symbols,
        line,
        character,
        var_name,
        resolver,
        callable_resolver,
    )
    .resolved_type_fqn
}

/// Infer full PHPDoc/native type information for a variable-like expression
/// before a given position.
///
/// This keeps shape/generic metadata for features that need more than an object
/// FQN, for example array-shape key completion.
pub fn infer_variable_type_info_at_position(
    tree: &Tree,
    source: &str,
    file_symbols: &FileSymbols,
    line: u32,
    character: u32,
    var_name: &str,
) -> Option<TypeInfo> {
    infer_variable_type_at_position_internal(
        tree,
        source,
        file_symbols,
        line,
        character,
        var_name,
        None,
        None,
    )
    .type_info
}

/// Infer full type information by name before a given position, using an
/// external member resolver.
pub fn infer_variable_type_info_at_position_with_resolver(
    tree: &Tree,
    source: &str,
    file_symbols: &FileSymbols,
    line: u32,
    character: u32,
    var_name: &str,
    resolver: MemberTypeResolver<'_>,
) -> Option<TypeInfo> {
    infer_variable_type_info_at_position_with_resolvers(
        tree,
        source,
        file_symbols,
        line,
        character,
        var_name,
        Some(resolver),
        None,
    )
}

/// Infer full type information by name before a given position, using external
/// member and callable-parameter resolvers.
#[allow(clippy::too_many_arguments)]
pub fn infer_variable_type_info_at_position_with_resolvers(
    tree: &Tree,
    source: &str,
    file_symbols: &FileSymbols,
    line: u32,
    character: u32,
    var_name: &str,
    resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
) -> Option<TypeInfo> {
    infer_variable_type_at_position_internal(
        tree,
        source,
        file_symbols,
        line,
        character,
        var_name,
        resolver,
        callable_resolver,
    )
    .type_info
}

#[allow(clippy::too_many_arguments)]
fn infer_variable_type_at_position_internal(
    tree: &Tree,
    source: &str,
    file_symbols: &FileSymbols,
    line: u32,
    character: u32,
    var_name: &str,
    resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
) -> VariableInference {
    let root = tree.root_node();
    let point = Point::new(line as usize, character as usize);
    let node = find_node_at_point(root, point).unwrap_or(root);
    let usage_start = position_to_byte(source, line, character);
    let scope = find_enclosing_function(node).unwrap_or_else(|| find_root_node(node));
    infer_textual_expression_type_info(
        scope,
        var_name,
        usage_start,
        source,
        file_symbols,
        resolver,
        callable_resolver,
    )
    .unwrap_or_else(|| {
        let normalized = normalize_var_name(var_name);
        infer_variable_in_scope(
            scope,
            &normalized,
            usage_start,
            source,
            file_symbols,
            resolver,
            callable_resolver,
        )
    })
}

/// Infer hover-style type information for a variable at an arbitrary usage byte.
///
/// `context_node` is used to find the local scope, while `usage_start` controls
/// which preceding assignments/PHPDoc/foreach bindings are visible.
pub fn infer_variable_hover_info_at_node(
    context_node: Node,
    source: &str,
    file_symbols: &FileSymbols,
    usage_start: usize,
    var_name: &str,
    resolver: Option<MemberTypeResolver<'_>>,
) -> Option<VariableHoverInfo> {
    infer_variable_hover_info_at_node_with_resolvers(
        context_node,
        source,
        file_symbols,
        usage_start,
        var_name,
        resolver,
        None,
    )
}

/// Infer hover-style type information using external member and
/// callable-parameter resolvers.
pub fn infer_variable_hover_info_at_node_with_resolvers(
    context_node: Node,
    source: &str,
    file_symbols: &FileSymbols,
    usage_start: usize,
    var_name: &str,
    resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
) -> Option<VariableHoverInfo> {
    let normalized = normalize_var_name(var_name);
    let scope =
        find_enclosing_function(context_node).unwrap_or_else(|| find_root_node(context_node));
    let inference = infer_variable_in_scope(
        scope,
        &normalized,
        usage_start,
        source,
        file_symbols,
        resolver,
        callable_resolver,
    );
    if !inference.has_data() {
        return None;
    }

    Some(VariableHoverInfo {
        variable_name: normalized,
        type_display: inference.type_display,
        resolved_type_fqn: inference.resolved_type_fqn,
        phpdoc_comment: inference.phpdoc_comment,
    })
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
    let inference = infer_variable_in_scope(
        scope,
        &var_name,
        usage_start,
        source,
        file_symbols,
        None,
        None,
    );
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
        None,
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
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
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
                if let Some(resolved) = try_resolve_object_type(
                    rhs,
                    source,
                    file_symbols,
                    resolver,
                    callable_resolver,
                    None,
                ) {
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
                callable_resolver,
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
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
    function_resolver: Option<FunctionTypeResolver<'_>>,
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
                let class_fqn = object_field.and_then(|o| {
                    try_resolve_object_type(
                        o,
                        source,
                        file_symbols,
                        resolver,
                        callable_resolver,
                        function_resolver,
                    )
                });
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
                let class_fqn = object_field.and_then(|o| {
                    try_resolve_object_type(
                        o,
                        source,
                        file_symbols,
                        resolver,
                        callable_resolver,
                        function_resolver,
                    )
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
                            try_resolve_object_type(
                                o,
                                source,
                                file_symbols,
                                resolver,
                                callable_resolver,
                                function_resolver,
                            )
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
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
    function_resolver: Option<FunctionTypeResolver<'_>>,
) -> Option<String> {
    let _depth_guard = ObjectTypeResolveDepthGuard::enter()?;
    try_resolve_object_type_inner(
        object_node,
        source,
        file_symbols,
        resolver,
        callable_resolver,
        function_resolver,
    )
}

fn try_resolve_object_type_inner<'a>(
    object_node: Node<'a>,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
    function_resolver: Option<FunctionTypeResolver<'_>>,
) -> Option<String> {
    let kind = object_node.kind();
    match kind {
        "function_call_expression" => try_resolve_function_call_object_fqn(
            object_node,
            source,
            file_symbols,
            resolver,
            callable_resolver,
            function_resolver,
        ),
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
                    if let Some(resolved) = try_resolve_object_type(
                        child,
                        source,
                        file_symbols,
                        resolver,
                        callable_resolver,
                        function_resolver,
                    ) {
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
                infer_variable_type(
                    object_node,
                    text,
                    source,
                    file_symbols,
                    resolver,
                    callable_resolver,
                )
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
            let class_fqn = try_resolve_object_type(
                obj_field,
                source,
                file_symbols,
                resolver,
                callable_resolver,
                function_resolver,
            )?;

            // Look up the property in the file's symbols to get its type
            let property_fqn_dollar = format!("{}::${}", class_fqn, prop_name);
            for sym in &file_symbols.symbols {
                if sym.fqn == property_fqn_dollar {
                    if let Some(ret) = symbol_effective_type_info(sym, file_symbols) {
                        if let Some(resolved) = resolve_symbol_type_info_to_object_fqn(
                            &ret,
                            &class_fqn,
                            object_node,
                            source,
                            file_symbols,
                        ) {
                            return Some(resolved);
                        }
                    }
                    break;
                }
            }
            // Fallback: use the cross-file resolver for inherited properties
            if let Some(ref resolve_fn) = resolver {
                let member_name = format!("${}", prop_name);
                let resolver_owner = resolver_owner_type_text_for_object(
                    obj_field,
                    &class_fqn,
                    source,
                    file_symbols,
                    resolver,
                    callable_resolver,
                )
                .unwrap_or_else(|| class_fqn.trim_start_matches('\\').to_string());
                if let Some(type_text) = resolve_fn(&resolver_owner, &member_name) {
                    if let Some(type_fqn) = resolve_object_fqn_from_member_type_text(
                        &type_text,
                        object_node,
                        source,
                        file_symbols,
                    ) {
                        return Some(type_fqn);
                    }
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
            let class_fqn = try_resolve_object_type(
                obj_field,
                source,
                file_symbols,
                resolver,
                callable_resolver,
                function_resolver,
            )?;

            // First: look up the method's return type in the current file's symbols
            let method_fqn = format!("{}::{}", class_fqn, method_name);
            for sym in &file_symbols.symbols {
                if sym.fqn == method_fqn {
                    if let Some(ret) = symbol_effective_type_info(sym, file_symbols) {
                        if let Some(resolved) = resolve_symbol_type_info_to_object_fqn(
                            &ret,
                            &class_fqn,
                            object_node,
                            source,
                            file_symbols,
                        ) {
                            return Some(resolved);
                        }
                    }
                    break;
                }
            }

            // Fallback: use the cross-file resolver to get the method's return type
            if let Some(ref resolve_fn) = resolver {
                let resolver_owner = resolver_owner_type_text_for_object(
                    obj_field,
                    &class_fqn,
                    source,
                    file_symbols,
                    resolver,
                    callable_resolver,
                )
                .unwrap_or_else(|| class_fqn.trim_start_matches('\\').to_string());
                if let Some(type_text) = resolve_fn(&resolver_owner, method_name) {
                    if let Some(type_fqn) = resolve_object_fqn_from_member_type_text(
                        &type_text,
                        object_node,
                        source,
                        file_symbols,
                    ) {
                        return Some(type_fqn);
                    }
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
                                    callable_resolver,
                                    &mut alt_types,
                                );
                                for alt_type in &alt_types {
                                    if let Some(ref resolve_fn) = resolver {
                                        if let Some(type_text) = resolve_fn(alt_type, method_name) {
                                            if let Some(type_fqn) =
                                                resolve_object_fqn_from_member_type_text(
                                                    &type_text,
                                                    object_node,
                                                    source,
                                                    file_symbols,
                                                )
                                            {
                                                return Some(type_fqn);
                                            }
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
            let base_type = infer_expression_type_info(
                base,
                source,
                file_symbols,
                resolver,
                callable_resolver,
            )?;
            let value_type = iterable_value_type_info(&base_type, key_text.as_deref())?;
            resolve_phpdoc_var_type(&value_type, object_node, source, file_symbols)
        }
        // Static call: Foo::create() — can't resolve return type without full type info
        _ => None,
    }
}

fn resolve_object_fqn_from_member_type_text(
    type_text: &str,
    context_node: Node,
    source: &str,
    file_symbols: &FileSymbols,
) -> Option<String> {
    let type_info = type_info_from_type_text(type_text);
    object_fqn_from_resolved_member_type_info(&type_info, context_node, source, file_symbols)
}

fn resolver_owner_type_text_for_object(
    object_node: Node,
    class_fqn: &str,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
) -> Option<String> {
    let type_info = infer_expression_type_info(
        object_node,
        source,
        file_symbols,
        resolver,
        callable_resolver,
    )?;
    if !type_info_has_generic_base_fqn(&type_info, class_fqn) {
        return None;
    }

    Some(resolver_type_info_for_parser(&type_info).to_string())
}

fn type_info_has_generic_base_fqn(type_info: &TypeInfo, class_fqn: &str) -> bool {
    let class_fqn = class_fqn.trim_start_matches('\\');
    match type_info {
        TypeInfo::Generic { base, .. } => base.trim_start_matches('\\') == class_fqn,
        TypeInfo::Nullable(inner) => type_info_has_generic_base_fqn(inner, class_fqn),
        TypeInfo::Union(types) | TypeInfo::Intersection(types) => types
            .iter()
            .any(|type_info| type_info_has_generic_base_fqn(type_info, class_fqn)),
        _ => false,
    }
}

fn object_fqn_from_resolved_member_type_info(
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
                Some(name.trim_start_matches('\\').to_string())
            }
        }
        TypeInfo::Nullable(inner) => {
            object_fqn_from_resolved_member_type_info(inner, context_node, source, file_symbols)
        }
        TypeInfo::Union(types) | TypeInfo::Intersection(types) => {
            for ty in types {
                if let Some(resolved) = object_fqn_from_resolved_member_type_info(
                    ty,
                    context_node,
                    source,
                    file_symbols,
                ) {
                    return Some(resolved);
                }
            }
            None
        }
        TypeInfo::Self_ | TypeInfo::Static_ => {
            find_parent_class_fqn(context_node, source, file_symbols)
        }
        TypeInfo::Parent_ => find_extended_parent_class_fqn(context_node, source, file_symbols),
        TypeInfo::Generic { base, .. } => {
            if is_builtin_non_object_type(base) {
                None
            } else {
                Some(base.trim_start_matches('\\').to_string())
            }
        }
        TypeInfo::Conditional {
            if_type, else_type, ..
        } => object_fqn_from_resolved_member_type_info(if_type, context_node, source, file_symbols)
            .or_else(|| {
                object_fqn_from_resolved_member_type_info(
                    else_type,
                    context_node,
                    source,
                    file_symbols,
                )
            }),
        TypeInfo::ClassString(_)
        | TypeInfo::ArrayShape(_)
        | TypeInfo::ObjectShape(_)
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
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
) -> Option<String> {
    let scope = find_enclosing_function(var_node).unwrap_or_else(|| find_root_node(var_node));
    infer_variable_type_in_scope(
        scope,
        var_name,
        var_node.start_byte(),
        source,
        file_symbols,
        resolver,
        callable_resolver,
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
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
) -> Option<String> {
    infer_variable_in_scope(
        scope_node,
        var_name,
        usage_start,
        source,
        file_symbols,
        resolver,
        callable_resolver,
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

fn resolved_fqn_type_info(resolved: &str) -> TypeInfo {
    let resolved = resolved.trim();
    if resolved.is_empty() {
        return TypeInfo::Simple(String::new());
    }

    if !resolved.starts_with('\\')
        && resolved.contains('\\')
        && !is_builtin_non_object_type(resolved)
    {
        TypeInfo::Simple(format!("\\{resolved}"))
    } else {
        TypeInfo::Simple(resolved.to_string())
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
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
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

        if let Some(array_write_info) = array_write_inference_for_var(
            stmt,
            var_name,
            source,
            file_symbols,
            resolver,
            callable_resolver,
        ) {
            inferred = Some((stmt.start_byte(), array_write_info));
            continue;
        }

        if let Some(foreach_info) = foreach_variable_inference(
            stmt,
            var_name,
            usage_start,
            source,
            file_symbols,
            resolver,
            callable_resolver,
        ) {
            inferred = Some((stmt.start_byte(), foreach_info));
            continue;
        }

        // Assignment inference: $var = <expr>;
        if let Some(right) = assignment_rhs {
            if let Some(type_info) = infer_literal_array_shape_type(
                right,
                source,
                file_symbols,
                resolver,
                callable_resolver,
            ) {
                inferred = Some((
                    stmt.start_byte(),
                    VariableInference {
                        type_display: Some(type_info.to_string()),
                        resolved_type_fqn: None,
                        phpdoc_comment: None,
                        type_info: Some(type_info),
                    },
                ));
            } else if let Some(resolved) = try_resolve_object_type(
                right,
                source,
                file_symbols,
                resolver,
                callable_resolver,
                None,
            ) {
                let type_info = Some(resolved_fqn_type_info(&resolved));
                inferred = Some((
                    stmt.start_byte(),
                    VariableInference {
                        type_display: Some(resolved.clone()),
                        resolved_type_fqn: Some(resolved),
                        phpdoc_comment: None,
                        type_info,
                    },
                ));
            } else if let Some(type_info) =
                infer_expression_type_info(right, source, file_symbols, resolver, callable_resolver)
            {
                let resolved_type_fqn =
                    resolve_phpdoc_var_type(&type_info, right, source, file_symbols);
                inferred = Some((
                    stmt.start_byte(),
                    VariableInference {
                        type_display: Some(type_info.to_string()),
                        resolved_type_fqn,
                        phpdoc_comment: None,
                        type_info: Some(type_info),
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
                callable_resolver,
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
                callable_resolver,
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
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
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
        } else if let Some(array_write_info) = array_write_inference_for_var(
            child,
            var_name,
            source,
            file_symbols,
            resolver,
            callable_resolver,
        ) {
            inferred = Some((child.start_byte(), array_write_info));
        } else if let Some(right) = assignment_rhs {
            if let Some(type_info) = infer_literal_array_shape_type(
                right,
                source,
                file_symbols,
                resolver,
                callable_resolver,
            ) {
                inferred = Some((
                    child.start_byte(),
                    VariableInference {
                        type_display: Some(type_info.to_string()),
                        resolved_type_fqn: None,
                        phpdoc_comment: None,
                        type_info: Some(type_info),
                    },
                ));
            } else if let Some(resolved) = try_resolve_object_type(
                right,
                source,
                file_symbols,
                resolver,
                callable_resolver,
                None,
            ) {
                inferred = Some((
                    child.start_byte(),
                    VariableInference {
                        type_display: Some(resolved.clone()),
                        resolved_type_fqn: Some(resolved.clone()),
                        phpdoc_comment: None,
                        type_info: Some(resolved_fqn_type_info(&resolved)),
                    },
                ));
            } else if let Some(type_info) =
                infer_expression_type_info(right, source, file_symbols, resolver, callable_resolver)
            {
                let resolved_type_fqn =
                    resolve_phpdoc_var_type(&type_info, right, source, file_symbols);
                inferred = Some((
                    child.start_byte(),
                    VariableInference {
                        type_display: Some(type_info.to_string()),
                        resolved_type_fqn,
                        phpdoc_comment: None,
                        type_info: Some(type_info),
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
                callable_resolver,
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
        type_info: Some(resolved_fqn_type_info(&resolved)),
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
        type_info: Some(resolved_fqn_type_info(&resolved)),
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

fn array_write_inference_for_var(
    stmt: Node,
    var_name: &str,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
) -> Option<VariableInference> {
    if stmt.kind() != "expression_statement" {
        return None;
    }

    let expr = stmt.named_child(0)?;
    if expr.kind() != "assignment_expression" {
        return None;
    }

    let left = expr.child_by_field_name("left")?;
    let right = expr.child_by_field_name("right")?;
    let (base, key) = subscript_assignment_base_and_key(left)?;
    if normalize_var_name(&source[base.byte_range()]) != var_name {
        return None;
    }

    let key_type =
        infer_array_key_expression_type(key, source, file_symbols, resolver, callable_resolver)
            .unwrap_or_else(|| TypeInfo::Simple("array-key".to_string()));
    let value_type =
        infer_expression_type_info(right, source, file_symbols, resolver, callable_resolver)
            .unwrap_or_else(|| {
                infer_literal_value_type_text(
                    &source[right.byte_range()],
                    right,
                    source,
                    file_symbols,
                    resolver,
                    callable_resolver,
                )
            });
    let type_info = TypeInfo::Generic {
        base: "array".to_string(),
        args: vec![key_type, value_type],
    };

    Some(VariableInference {
        type_display: Some(type_info.to_string()),
        resolved_type_fqn: None,
        phpdoc_comment: None,
        type_info: Some(type_info),
    })
}

fn subscript_assignment_base_and_key(left: Node) -> Option<(Node, Node)> {
    if left.kind() != "subscript_expression" {
        return None;
    }

    let base = left.named_child(0)?;
    if base.kind() != "variable_name" {
        return None;
    }
    let key = left.named_child(1)?;
    Some((base, key))
}

fn infer_array_key_expression_type(
    key: Node,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
) -> Option<TypeInfo> {
    infer_expression_type_info(key, source, file_symbols, resolver, callable_resolver)
        .or_else(|| infer_literal_expression_type_info(key, source))
        .map(|type_info| array_key_compatible_type_info(&type_info))
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
    let type_info = expand_file_type_aliases(&type_info, file_symbols, &mut Vec::new());
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

fn expand_file_type_aliases(
    type_info: &TypeInfo,
    file_symbols: &FileSymbols,
    visited: &mut Vec<String>,
) -> TypeInfo {
    match type_info {
        TypeInfo::Simple(name) => file_type_alias_for_name(name, file_symbols, visited)
            .unwrap_or_else(|| TypeInfo::Simple(name.clone())),
        TypeInfo::Generic { base, args } => {
            let base_type = file_type_alias_for_name(base, file_symbols, visited)
                .unwrap_or_else(|| TypeInfo::Simple(base.clone()));
            let args = args
                .iter()
                .map(|arg| expand_file_type_aliases(arg, file_symbols, visited))
                .collect();
            match base_type {
                TypeInfo::Simple(base) => TypeInfo::Generic { base, args },
                TypeInfo::Generic {
                    base,
                    args: mut base_args,
                } => {
                    base_args.extend(args);
                    TypeInfo::Generic {
                        base,
                        args: base_args,
                    }
                }
                other => other,
            }
        }
        TypeInfo::ArrayShape(items) => TypeInfo::ArrayShape(
            items
                .iter()
                .map(|item| php_lsp_types::ArrayShapeItem {
                    key: item.key.clone(),
                    optional: item.optional,
                    value: expand_file_type_aliases(&item.value, file_symbols, visited),
                })
                .collect(),
        ),
        TypeInfo::ObjectShape(items) => TypeInfo::ObjectShape(
            items
                .iter()
                .map(|item| php_lsp_types::ArrayShapeItem {
                    key: item.key.clone(),
                    optional: item.optional,
                    value: expand_file_type_aliases(&item.value, file_symbols, visited),
                })
                .collect(),
        ),
        TypeInfo::Callable {
            params,
            return_type,
        } => TypeInfo::Callable {
            params: params
                .iter()
                .map(|param| expand_file_type_aliases(param, file_symbols, visited))
                .collect(),
            return_type: return_type.as_ref().map(|return_type| {
                Box::new(expand_file_type_aliases(return_type, file_symbols, visited))
            }),
        },
        TypeInfo::ClassString(Some(inner)) => TypeInfo::ClassString(Some(Box::new(
            expand_file_type_aliases(inner, file_symbols, visited),
        ))),
        TypeInfo::ClassString(None) => TypeInfo::ClassString(None),
        TypeInfo::Conditional {
            subject,
            target,
            if_type,
            else_type,
        } => TypeInfo::Conditional {
            subject: subject.clone(),
            target: Box::new(expand_file_type_aliases(target, file_symbols, visited)),
            if_type: Box::new(expand_file_type_aliases(if_type, file_symbols, visited)),
            else_type: Box::new(expand_file_type_aliases(else_type, file_symbols, visited)),
        },
        TypeInfo::Union(types) => TypeInfo::Union(
            types
                .iter()
                .map(|type_info| expand_file_type_aliases(type_info, file_symbols, visited))
                .collect(),
        ),
        TypeInfo::Intersection(types) => TypeInfo::Intersection(
            types
                .iter()
                .map(|type_info| expand_file_type_aliases(type_info, file_symbols, visited))
                .collect(),
        ),
        TypeInfo::Nullable(inner) => TypeInfo::Nullable(Box::new(expand_file_type_aliases(
            inner,
            file_symbols,
            visited,
        ))),
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
        | TypeInfo::Parent_ => type_info.clone(),
    }
}

fn file_type_alias_for_name(
    name: &str,
    file_symbols: &FileSymbols,
    visited: &mut Vec<String>,
) -> Option<TypeInfo> {
    if visited.iter().any(|visited_name| visited_name == name) {
        return None;
    }
    let alias = file_symbols
        .type_aliases
        .iter()
        .find(|alias| alias.name == name)?;

    visited.push(name.to_string());
    let expanded = expand_file_type_aliases(&alias.type_info, file_symbols, visited);
    visited.pop();
    Some(expanded)
}

fn infer_variable_in_scope(
    scope_node: Node,
    var_name: &str,
    usage_start: usize,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
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
                                    inferred.type_info = Some(resolved_fqn_type_info(&resolved));
                                }
                            }
                            if inferred.type_info.is_none() {
                                if let Some(callable_info) = infer_callable_parameter_inference(
                                    scope_node,
                                    param,
                                    i,
                                    var_name,
                                    source,
                                    file_symbols,
                                    resolver,
                                    callable_resolver,
                                ) {
                                    inferred = callable_info;
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
        callable_resolver,
    ) {
        inferred = stmt_info;
    }

    inferred
}

struct CallableArgumentSite {
    target_fqn: String,
    argument_index: usize,
    argument_name: Option<String>,
    receiver_type: Option<TypeInfo>,
    argument_types: Vec<CallableArgumentType>,
}

#[derive(Debug, Clone)]
struct CallArgument<'tree> {
    value_node: Node<'tree>,
    name: Option<String>,
}

fn call_arguments<'tree>(call_node: Node<'tree>, source: &str) -> Vec<CallArgument<'tree>> {
    let arguments = if let Some(arguments) = call_node.child_by_field_name("arguments") {
        arguments
    } else {
        let mut cursor = call_node.walk();
        let found = call_node
            .children(&mut cursor)
            .find(|child| child.kind() == "arguments");
        let Some(arguments) = found else {
            return Vec::new();
        };
        arguments
    };
    if arguments.kind() != "arguments" {
        return Vec::new();
    }

    let mut result = Vec::new();
    let mut cursor = arguments.walk();
    for child in arguments.named_children(&mut cursor) {
        if child.kind() == "argument" {
            result.push(CallArgument {
                value_node: argument_value_node(child).unwrap_or(child),
                name: argument_name(child, source),
            });
        }
    }
    result
}

fn argument_value_node(argument: Node) -> Option<Node> {
    argument.child_by_field_name("value").or_else(|| {
        let mut cursor = argument.walk();
        argument.named_children(&mut cursor).last()
    })
}

#[allow(clippy::too_many_arguments)]
fn infer_callable_parameter_inference(
    scope_node: Node,
    param_node: Node,
    parameter_index: usize,
    var_name: &str,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
) -> Option<VariableInference> {
    if !is_closure_scope_node(scope_node) {
        return None;
    }
    let site = callable_argument_site_for_closure(
        scope_node,
        source,
        file_symbols,
        resolver,
        callable_resolver,
    )?;
    let parameter_name = var_name.trim_start_matches('$');

    let type_info = callable_param_type_from_local_signature(
        &site,
        parameter_index,
        source,
        file_symbols,
        resolver,
        callable_resolver,
    )
    .or_else(|| {
        let resolver = callable_resolver?;
        resolver(CallableParameterContext {
            target_fqn: &site.target_fqn,
            argument_index: site.argument_index,
            argument_name: site.argument_name.as_deref(),
            parameter_index,
            parameter_name,
            receiver_type: site.receiver_type.as_ref(),
            argument_types: &site.argument_types,
        })
    })?;

    let resolved_type_fqn = resolve_phpdoc_var_type(&type_info, param_node, source, file_symbols);
    Some(VariableInference {
        type_display: Some(type_info.to_string()),
        resolved_type_fqn,
        phpdoc_comment: None,
        type_info: Some(type_info),
    })
}

fn is_closure_scope_node(node: Node) -> bool {
    matches!(
        node.kind(),
        "arrow_function" | "anonymous_function" | "anonymous_function_creation_expression"
    )
}

fn callable_argument_site_for_closure<'tree>(
    closure_node: Node<'tree>,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
) -> Option<CallableArgumentSite> {
    let argument = enclosing_argument_for_closure(closure_node)?;
    let arguments = argument
        .parent()
        .filter(|parent| parent.kind() == "arguments")?;
    let call_node = arguments.parent()?;
    let argument_index = argument_index(arguments, argument)?;
    let argument_name = argument_name(argument, source);
    let (target_fqn, receiver_type) =
        callable_target_for_call(call_node, source, file_symbols, resolver, callable_resolver)?;
    let argument_types =
        callable_argument_types(call_node, source, file_symbols, resolver, callable_resolver);

    Some(CallableArgumentSite {
        target_fqn,
        argument_index,
        argument_name,
        receiver_type,
        argument_types,
    })
}

fn callable_argument_types(
    call_node: Node,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
) -> Vec<CallableArgumentType> {
    call_arguments(call_node, source)
        .into_iter()
        .enumerate()
        .filter_map(|(argument_index, arg)| {
            let type_info = infer_expression_type_info(
                arg.value_node,
                source,
                file_symbols,
                resolver,
                callable_resolver,
            )?;
            Some(CallableArgumentType {
                argument_index,
                argument_name: arg.name,
                type_info,
            })
        })
        .collect()
}

fn enclosing_argument_for_closure(mut node: Node) -> Option<Node> {
    while let Some(parent) = node.parent() {
        if parent.kind() == "argument" {
            return Some(parent);
        }
        if matches!(
            parent.kind(),
            "method_declaration" | "function_definition" | "program"
        ) {
            return None;
        }
        node = parent;
    }
    None
}

fn callable_target_for_call(
    call_node: Node,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
) -> Option<(String, Option<TypeInfo>)> {
    match call_node.kind() {
        "function_call_expression" => {
            let function = call_node
                .child_by_field_name("function")
                .or_else(|| call_node.named_child(0))?;
            Some((
                resolve_function_name(&source[function.byte_range()], file_symbols),
                None,
            ))
        }
        "member_call_expression" | "nullsafe_member_call_expression" => {
            let object = call_node.child_by_field_name("object")?;
            let name = call_node.child_by_field_name("name")?;
            let receiver_type = infer_expression_type_info(
                object,
                source,
                file_symbols,
                resolver,
                callable_resolver,
            );
            let class_fqn = receiver_type
                .as_ref()
                .and_then(|type_info| {
                    object_fqn_from_type_info(type_info, call_node, source, file_symbols)
                })
                .or_else(|| {
                    try_resolve_object_type(
                        object,
                        source,
                        file_symbols,
                        resolver,
                        callable_resolver,
                        None,
                    )
                })?;
            Some((
                format!("{}::{}", class_fqn, &source[name.byte_range()]),
                receiver_type,
            ))
        }
        "scoped_call_expression" => {
            let scope = call_node.child_by_field_name("scope")?;
            let name = call_node.child_by_field_name("name")?;
            let class_fqn = resolve_scope_class_name(
                &source[scope.byte_range()],
                call_node,
                source,
                file_symbols,
            );
            Some((
                format!("{}::{}", class_fqn, &source[name.byte_range()]),
                None,
            ))
        }
        _ => None,
    }
}

fn object_fqn_from_type_info(
    type_info: &TypeInfo,
    context_node: Node,
    source: &str,
    file_symbols: &FileSymbols,
) -> Option<String> {
    match type_info {
        TypeInfo::Simple(name) if !is_builtin_non_object_type(name) => {
            Some(resolve_class_name(name, file_symbols))
        }
        TypeInfo::Generic { base, .. } if !is_builtin_non_object_type(base) => {
            Some(resolve_class_name(base, file_symbols))
        }
        TypeInfo::Nullable(inner) => {
            object_fqn_from_type_info(inner, context_node, source, file_symbols)
        }
        TypeInfo::Union(types) | TypeInfo::Intersection(types) => {
            types.iter().find_map(|type_info| {
                object_fqn_from_type_info(type_info, context_node, source, file_symbols)
            })
        }
        TypeInfo::Self_ | TypeInfo::Static_ => {
            find_parent_class_fqn(context_node, source, file_symbols)
        }
        _ => None,
    }
}

fn callable_param_type_from_local_signature(
    site: &CallableArgumentSite,
    parameter_index: usize,
    _source: &str,
    file_symbols: &FileSymbols,
    _resolver: Option<MemberTypeResolver<'_>>,
    _callable_resolver: Option<CallableParamTypeResolver<'_>>,
) -> Option<TypeInfo> {
    let symbol = file_symbols
        .symbols
        .iter()
        .find(|symbol| symbol.fqn == site.target_fqn)?;
    let signature = symbol.signature.as_ref()?;
    callable_param_type_from_signature(signature, symbol, site, parameter_index, file_symbols)
}

fn callable_param_type_from_signature(
    signature: &Signature,
    symbol: &SymbolInfo,
    site: &CallableArgumentSite,
    parameter_index: usize,
    file_symbols: &FileSymbols,
) -> Option<TypeInfo> {
    let callable_param = signature_param_for_call_arg(
        signature,
        site.argument_index,
        site.argument_name.as_deref(),
    )?;
    let expected = callable_param.type_info.as_ref()?;
    let template_names = callable_template_names(symbol, &site.target_fqn, file_symbols);
    let mut substitutions = receiver_template_substitutions(
        &site.target_fqn,
        site.receiver_type.as_ref(),
        file_symbols,
    );

    for arg in &site.argument_types {
        if arg.argument_index == site.argument_index {
            continue;
        }
        let Some(param) = signature_param_for_call_arg(
            signature,
            arg.argument_index,
            arg.argument_name.as_deref(),
        ) else {
            continue;
        };
        let Some(param_type) = param.type_info.as_ref() else {
            continue;
        };
        bind_template_type_info(
            param_type,
            &arg.type_info,
            &template_names,
            &mut substitutions,
        );
    }

    let expected = substitute_type_info(expected, &substitutions);
    callable_param_type(&expected, parameter_index)
}

fn signature_param_for_call_arg<'a>(
    signature: &'a Signature,
    arg_index: usize,
    name: Option<&str>,
) -> Option<&'a php_lsp_types::ParamInfo> {
    if let Some(name) = name {
        return signature.params.iter().find(|param| {
            param
                .name
                .trim_start_matches('$')
                .eq_ignore_ascii_case(name)
        });
    }

    signature
        .params
        .get(arg_index)
        .or_else(|| signature.params.last().filter(|param| param.is_variadic))
}

fn callable_template_names(
    symbol: &SymbolInfo,
    target_fqn: &str,
    file_symbols: &FileSymbols,
) -> HashSet<String> {
    let mut names = symbol
        .templates
        .iter()
        .map(|template| template.name.clone())
        .collect::<HashSet<_>>();
    if let Some((class_fqn, _)) = target_fqn.rsplit_once("::") {
        if let Some(class_symbol) = file_symbols.symbols.iter().find(|sym| sym.fqn == class_fqn) {
            names.extend(
                class_symbol
                    .templates
                    .iter()
                    .map(|template| template.name.clone()),
            );
        }
    }
    names
}

fn receiver_template_substitutions(
    target_fqn: &str,
    receiver_type: Option<&TypeInfo>,
    file_symbols: &FileSymbols,
) -> HashMap<String, TypeInfo> {
    let mut substitutions = HashMap::new();
    let Some((class_fqn, _)) = target_fqn.rsplit_once("::") else {
        return substitutions;
    };
    let Some(TypeInfo::Generic { base, args }) = receiver_type else {
        return substitutions;
    };
    let resolved_base = resolve_class_name(base, file_symbols);
    if resolved_base.trim_start_matches('\\') != class_fqn.trim_start_matches('\\') {
        return substitutions;
    }
    let Some(class_symbol) = file_symbols.symbols.iter().find(|sym| sym.fqn == class_fqn) else {
        return substitutions;
    };
    for (template, arg) in class_symbol.templates.iter().zip(args.iter()) {
        substitutions.insert(template.name.clone(), arg.clone());
    }
    substitutions
}

fn bind_template_type_info(
    pattern: &TypeInfo,
    actual: &TypeInfo,
    template_names: &HashSet<String>,
    substitutions: &mut HashMap<String, TypeInfo>,
) {
    match (pattern, actual) {
        (TypeInfo::Simple(name), actual) if template_names.contains(name) => {
            substitutions
                .entry(name.clone())
                .or_insert_with(|| actual.clone());
        }
        (TypeInfo::Nullable(pattern), actual) => {
            bind_template_type_info(pattern, actual, template_names, substitutions);
        }
        (TypeInfo::Union(types), actual) | (TypeInfo::Intersection(types), actual) => {
            for ty in types {
                bind_template_type_info(ty, actual, template_names, substitutions);
            }
        }
        (
            TypeInfo::Generic {
                base: pattern_base,
                args: pattern_args,
            },
            TypeInfo::Generic {
                base: actual_base,
                args: actual_args,
            },
        ) if pattern_base.eq_ignore_ascii_case(actual_base) => {
            for (pattern_arg, actual_arg) in pattern_args.iter().zip(actual_args.iter()) {
                bind_template_type_info(pattern_arg, actual_arg, template_names, substitutions);
            }
        }
        (TypeInfo::ClassString(Some(pattern_inner)), TypeInfo::ClassString(Some(actual_inner))) => {
            bind_template_type_info(pattern_inner, actual_inner, template_names, substitutions)
        }
        (
            TypeInfo::Callable {
                params: pattern_params,
                return_type: pattern_return,
            },
            TypeInfo::Callable {
                params: actual_params,
                return_type: actual_return,
            },
        ) => {
            for (pattern_param, actual_param) in pattern_params.iter().zip(actual_params.iter()) {
                bind_template_type_info(pattern_param, actual_param, template_names, substitutions);
            }
            if let (Some(pattern_return), Some(actual_return)) = (pattern_return, actual_return) {
                bind_template_type_info(
                    pattern_return,
                    actual_return,
                    template_names,
                    substitutions,
                );
            }
        }
        _ => {}
    }
}

fn substitute_type_info(
    type_info: &TypeInfo,
    substitutions: &HashMap<String, TypeInfo>,
) -> TypeInfo {
    match type_info {
        TypeInfo::Simple(name) => substitutions
            .get(name)
            .cloned()
            .unwrap_or_else(|| TypeInfo::Simple(name.clone())),
        TypeInfo::Generic { base, args } => TypeInfo::Generic {
            base: base.clone(),
            args: args
                .iter()
                .map(|arg| substitute_type_info(arg, substitutions))
                .collect(),
        },
        TypeInfo::ArrayShape(items) => TypeInfo::ArrayShape(
            items
                .iter()
                .map(|item| php_lsp_types::ArrayShapeItem {
                    key: item.key.clone(),
                    optional: item.optional,
                    value: substitute_type_info(&item.value, substitutions),
                })
                .collect(),
        ),
        TypeInfo::ObjectShape(items) => TypeInfo::ObjectShape(
            items
                .iter()
                .map(|item| php_lsp_types::ArrayShapeItem {
                    key: item.key.clone(),
                    optional: item.optional,
                    value: substitute_type_info(&item.value, substitutions),
                })
                .collect(),
        ),
        TypeInfo::Callable {
            params,
            return_type,
        } => TypeInfo::Callable {
            params: params
                .iter()
                .map(|param| substitute_type_info(param, substitutions))
                .collect(),
            return_type: return_type
                .as_ref()
                .map(|return_type| Box::new(substitute_type_info(return_type, substitutions))),
        },
        TypeInfo::ClassString(Some(inner)) => {
            TypeInfo::ClassString(Some(Box::new(substitute_type_info(inner, substitutions))))
        }
        TypeInfo::Conditional {
            subject,
            target,
            if_type,
            else_type,
        } => TypeInfo::Conditional {
            subject: subject.clone(),
            target: Box::new(substitute_type_info(target, substitutions)),
            if_type: Box::new(substitute_type_info(if_type, substitutions)),
            else_type: Box::new(substitute_type_info(else_type, substitutions)),
        },
        TypeInfo::Union(types) => TypeInfo::Union(
            types
                .iter()
                .map(|ty| substitute_type_info(ty, substitutions))
                .collect(),
        ),
        TypeInfo::Intersection(types) => TypeInfo::Intersection(
            types
                .iter()
                .map(|ty| substitute_type_info(ty, substitutions))
                .collect(),
        ),
        TypeInfo::Nullable(inner) => {
            TypeInfo::Nullable(Box::new(substitute_type_info(inner, substitutions)))
        }
        TypeInfo::ClassString(None)
        | TypeInfo::LiteralString(_)
        | TypeInfo::LiteralInt(_)
        | TypeInfo::LiteralFloat(_)
        | TypeInfo::LiteralBool(_)
        | TypeInfo::LiteralNull
        | TypeInfo::Void
        | TypeInfo::Never
        | TypeInfo::Mixed
        | TypeInfo::Self_
        | TypeInfo::Static_
        | TypeInfo::Parent_ => type_info.clone(),
    }
}

fn callable_param_type(type_info: &TypeInfo, parameter_index: usize) -> Option<TypeInfo> {
    match type_info {
        TypeInfo::Callable { params, .. } => params.get(parameter_index).cloned(),
        TypeInfo::Nullable(inner) => callable_param_type(inner, parameter_index),
        TypeInfo::Union(types) | TypeInfo::Intersection(types) => types
            .iter()
            .find_map(|ty| callable_param_type(ty, parameter_index)),
        _ => None,
    }
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
        if let Some(rest) = strip_exact_tag(line, "@var") {
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
        TypeInfo::Conditional {
            if_type, else_type, ..
        } => resolve_phpdoc_var_type(if_type, context_node, source, file_symbols)
            .or_else(|| resolve_phpdoc_var_type(else_type, context_node, source, file_symbols)),
        TypeInfo::ClassString(_)
        | TypeInfo::ArrayShape(_)
        | TypeInfo::ObjectShape(_)
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

fn symbol_effective_type_info(symbol: &SymbolInfo, file_symbols: &FileSymbols) -> Option<TypeInfo> {
    let native = symbol
        .signature
        .as_ref()
        .and_then(|signature| signature.return_type.as_ref());
    let phpdoc = symbol.doc_comment.as_deref().and_then(|doc| {
        let parsed = parse_phpdoc(doc);
        if symbol.kind == php_lsp_types::PhpSymbolKind::Property {
            parsed.var_type
        } else {
            parsed.return_type
        }
    });

    match (native, phpdoc) {
        (Some(native), Some(phpdoc))
            if parser_type_info_specificity_score(&phpdoc)
                > parser_type_info_specificity_score(native) =>
        {
            Some(resolve_type_info_relative_to_symbol(
                &phpdoc,
                symbol,
                file_symbols,
            ))
        }
        (Some(native), _) => Some(resolve_type_info_relative_to_symbol(
            native,
            symbol,
            file_symbols,
        )),
        (None, Some(phpdoc)) => Some(resolve_type_info_relative_to_symbol(
            &phpdoc,
            symbol,
            file_symbols,
        )),
        (None, None) => None,
    }
}

fn resolve_type_info_relative_to_symbol(
    type_info: &TypeInfo,
    symbol: &SymbolInfo,
    file_symbols: &FileSymbols,
) -> TypeInfo {
    match type_info {
        TypeInfo::Simple(name) => TypeInfo::Simple(resolve_type_name_relative_to_symbol(
            name,
            symbol,
            file_symbols,
        )),
        TypeInfo::Generic { base, args } => TypeInfo::Generic {
            base: resolve_type_name_relative_to_symbol(base, symbol, file_symbols),
            args: args
                .iter()
                .map(|arg| resolve_type_info_relative_to_symbol(arg, symbol, file_symbols))
                .collect(),
        },
        TypeInfo::Nullable(inner) => TypeInfo::Nullable(Box::new(
            resolve_type_info_relative_to_symbol(inner, symbol, file_symbols),
        )),
        TypeInfo::Union(types) => TypeInfo::Union(
            types
                .iter()
                .map(|ty| resolve_type_info_relative_to_symbol(ty, symbol, file_symbols))
                .collect(),
        ),
        TypeInfo::Intersection(types) => TypeInfo::Intersection(
            types
                .iter()
                .map(|ty| resolve_type_info_relative_to_symbol(ty, symbol, file_symbols))
                .collect(),
        ),
        TypeInfo::ClassString(Some(inner)) => TypeInfo::ClassString(Some(Box::new(
            resolve_type_info_relative_to_symbol(inner, symbol, file_symbols),
        ))),
        TypeInfo::Conditional {
            subject,
            target,
            if_type,
            else_type,
        } => TypeInfo::Conditional {
            subject: subject.clone(),
            target: Box::new(resolve_type_info_relative_to_symbol(
                target,
                symbol,
                file_symbols,
            )),
            if_type: Box::new(resolve_type_info_relative_to_symbol(
                if_type,
                symbol,
                file_symbols,
            )),
            else_type: Box::new(resolve_type_info_relative_to_symbol(
                else_type,
                symbol,
                file_symbols,
            )),
        },
        TypeInfo::ArrayShape(items) => TypeInfo::ArrayShape(
            items
                .iter()
                .map(|item| php_lsp_types::ArrayShapeItem {
                    key: item.key.clone(),
                    optional: item.optional,
                    value: resolve_type_info_relative_to_symbol(&item.value, symbol, file_symbols),
                })
                .collect(),
        ),
        TypeInfo::ObjectShape(items) => TypeInfo::ObjectShape(
            items
                .iter()
                .map(|item| php_lsp_types::ArrayShapeItem {
                    key: item.key.clone(),
                    optional: item.optional,
                    value: resolve_type_info_relative_to_symbol(&item.value, symbol, file_symbols),
                })
                .collect(),
        ),
        TypeInfo::Callable {
            params,
            return_type,
        } => TypeInfo::Callable {
            params: params
                .iter()
                .map(|param| resolve_type_info_relative_to_symbol(param, symbol, file_symbols))
                .collect(),
            return_type: return_type.as_ref().map(|return_type| {
                Box::new(resolve_type_info_relative_to_symbol(
                    return_type,
                    symbol,
                    file_symbols,
                ))
            }),
        },
        TypeInfo::Self_ | TypeInfo::Static_ | TypeInfo::Parent_ => type_info.clone(),
        TypeInfo::ClassString(None)
        | TypeInfo::LiteralString(_)
        | TypeInfo::LiteralInt(_)
        | TypeInfo::LiteralFloat(_)
        | TypeInfo::LiteralBool(_)
        | TypeInfo::LiteralNull
        | TypeInfo::Void
        | TypeInfo::Never
        | TypeInfo::Mixed => type_info.clone(),
    }
}

fn resolve_type_name_relative_to_symbol(
    type_name: &str,
    symbol: &SymbolInfo,
    file_symbols: &FileSymbols,
) -> String {
    let type_name = type_name.trim();
    if type_name.is_empty()
        || type_name.starts_with('\\')
        || is_builtin_non_object_type(type_name)
        || matches!(type_name, "$this" | "self" | "static" | "parent")
    {
        return type_name.to_string();
    }
    let owner_fqn = symbol.parent_fqn.as_deref().unwrap_or(&symbol.fqn);
    let owner_namespace = owner_fqn.rsplit_once('\\').map(|(namespace, _)| namespace);
    let (first_part, rest) = type_name
        .split_once('\\')
        .map_or((type_name, None), |(first, rest)| (first, Some(rest)));
    if let Some(namespace) = owner_namespace {
        for use_stmt in &file_symbols.use_statements {
            if use_stmt.kind != UseKind::Class || use_stmt.namespace.as_deref() != Some(namespace) {
                continue;
            }
            let alias = use_stmt
                .alias
                .as_deref()
                .unwrap_or_else(|| use_stmt.fqn.rsplit('\\').next().unwrap_or(&use_stmt.fqn));
            if alias == first_part {
                let mut resolved = use_stmt.fqn.trim_start_matches('\\').to_string();
                if let Some(rest) = rest {
                    resolved.push('\\');
                    resolved.push_str(rest);
                }
                return format!("\\{resolved}");
            }
        }

        if type_name.contains('\\') {
            let namespace_root = namespace.split('\\').next().unwrap_or(namespace);
            if first_part == namespace_root {
                return format!("\\{}", type_name.trim_start_matches('\\'));
            }
        }

        return format!("\\{namespace}\\{type_name}");
    }

    if type_name.contains('\\') {
        format!("\\{}", type_name.trim_start_matches('\\'))
    } else {
        type_name.to_string()
    }
}

fn parser_type_info_specificity_score(type_info: &TypeInfo) -> usize {
    match type_info {
        TypeInfo::Mixed | TypeInfo::Void | TypeInfo::Never | TypeInfo::LiteralNull => 0,
        TypeInfo::Simple(name) => {
            if is_builtin_non_object_type(name) {
                1
            } else {
                3
            }
        }
        TypeInfo::Self_ | TypeInfo::Static_ | TypeInfo::Parent_ => 3,
        TypeInfo::Nullable(inner) => parser_type_info_specificity_score(inner),
        TypeInfo::Union(types) | TypeInfo::Intersection(types) => {
            types.iter().map(parser_type_info_specificity_score).sum()
        }
        TypeInfo::Generic { args, .. } => {
            4 + args
                .iter()
                .map(parser_type_info_specificity_score)
                .sum::<usize>()
        }
        TypeInfo::ArrayShape(items) => {
            5 + items
                .iter()
                .map(|item| parser_type_info_specificity_score(&item.value))
                .sum::<usize>()
        }
        TypeInfo::ObjectShape(items) => {
            5 + items
                .iter()
                .map(|item| parser_type_info_specificity_score(&item.value))
                .sum::<usize>()
        }
        TypeInfo::Callable {
            params,
            return_type,
        } => {
            3 + params
                .iter()
                .map(parser_type_info_specificity_score)
                .sum::<usize>()
                + return_type
                    .as_ref()
                    .map(|return_type| parser_type_info_specificity_score(return_type))
                    .unwrap_or_default()
        }
        TypeInfo::ClassString(inner) => {
            3 + inner
                .as_ref()
                .map(|inner| parser_type_info_specificity_score(inner))
                .unwrap_or_default()
        }
        TypeInfo::LiteralString(_)
        | TypeInfo::LiteralInt(_)
        | TypeInfo::LiteralFloat(_)
        | TypeInfo::LiteralBool(_) => 2,
        TypeInfo::Conditional {
            if_type, else_type, ..
        } => {
            3 + parser_type_info_specificity_score(if_type)
                + parser_type_info_specificity_score(else_type)
        }
    }
}

fn try_resolve_function_call_return_type(
    node: Node,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
    function_resolver: Option<FunctionTypeResolver<'_>>,
) -> Option<ResolvedFunctionType> {
    match node.kind() {
        "parenthesized_expression" => (0..node.named_child_count()).find_map(|i| {
            let child = node.named_child(i)?;
            try_resolve_function_call_return_type(
                child,
                source,
                file_symbols,
                resolver,
                function_resolver,
            )
        }),
        "function_call_expression" => {
            let function = node
                .child_by_field_name("function")
                .or_else(|| node.named_child(0))?;
            let raw_name = source[function.byte_range()].trim();
            if raw_name.is_empty() || function.kind() == "member_access_expression" {
                return None;
            }
            let resolved = resolve_function_name(raw_name, file_symbols);
            let resolved_type = |function_name: &str| {
                function_resolver
                    .and_then(|resolve_fn| resolve_fn(function_name))
                    .map(|mut resolved| {
                        resolved.type_text = resolver_type_text_for_parser(&resolved.type_text);
                        resolved
                    })
                    .or_else(|| {
                        resolver
                            .and_then(|resolve_fn| resolve_fn("", function_name))
                            .map(|type_text| {
                                ResolvedFunctionType::new(resolver_type_text_for_parser(&type_text))
                            })
                    })
            };
            resolved_type(&resolved).or_else(|| {
                (!raw_name.starts_with('\\') && !raw_name.contains('\\') && resolved != raw_name)
                    .then(|| resolved_type(raw_name))
                    .flatten()
            })
        }
        _ => None,
    }
}

fn try_resolve_function_call_return_type_info(
    node: Node,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
    function_resolver: Option<FunctionTypeResolver<'_>>,
) -> Option<TypeInfo> {
    let resolved = try_resolve_function_call_return_type(
        node,
        source,
        file_symbols,
        resolver,
        function_resolver,
    )?;
    let type_info = type_info_from_type_text(&resolved.type_text);
    Some(resolve_function_call_return_type_at_call_site(
        &type_info,
        resolved.signature.as_ref(),
        node,
        source,
        file_symbols,
        resolver,
        callable_resolver,
    ))
}

fn resolve_function_call_return_type_at_call_site(
    type_info: &TypeInfo,
    signature: Option<&Signature>,
    call_node: Node,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
) -> TypeInfo {
    match type_info {
        TypeInfo::Conditional {
            subject,
            target,
            if_type,
            else_type,
        } => {
            let Some(actual) = conditional_subject_argument_type(
                call_node,
                subject,
                signature,
                source,
                file_symbols,
                resolver,
                callable_resolver,
            ) else {
                return (**else_type).clone();
            };
            let template_names = conditional_template_names(type_info);
            let mut substitutions = HashMap::new();
            if type_pattern_matches_actual(target, &actual, &template_names, &mut substitutions) {
                substitute_type_info(if_type, &substitutions)
            } else {
                substitute_type_info(else_type, &substitutions)
            }
        }
        _ => type_info.clone(),
    }
}

fn conditional_subject_argument_type(
    call_node: Node,
    subject: &str,
    signature: Option<&Signature>,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
) -> Option<TypeInfo> {
    let subject_name = subject.trim().trim_start_matches('$');
    let arguments = call_arguments(call_node, source);
    if let Some(argument) = arguments
        .iter()
        .find(|arg| arg.name.as_deref() == Some(subject_name))
    {
        return infer_expression_type_info(
            argument.value_node,
            source,
            file_symbols,
            resolver,
            callable_resolver,
        );
    }

    let signature = signature?;
    let parameter_index = signature
        .params
        .iter()
        .position(|param| param.name == subject_name)?;
    if let Some(argument) = arguments
        .iter()
        .filter(|arg| arg.name.is_none())
        .nth(parameter_index)
    {
        return infer_expression_type_info(
            argument.value_node,
            source,
            file_symbols,
            resolver,
            callable_resolver,
        );
    }

    signature
        .params
        .get(parameter_index)?
        .default_value
        .as_deref()
        .and_then(infer_default_value_type_info)
}

fn infer_default_value_type_info(default_value: &str) -> Option<TypeInfo> {
    let text = default_value.trim();
    if text.is_empty() {
        return None;
    }
    if text.eq_ignore_ascii_case("null") {
        return Some(TypeInfo::LiteralNull);
    }
    if text.eq_ignore_ascii_case("true") {
        return Some(TypeInfo::LiteralBool(true));
    }
    if text.eq_ignore_ascii_case("false") {
        return Some(TypeInfo::LiteralBool(false));
    }
    if text.starts_with(['\'', '"']) {
        return Some(TypeInfo::Simple("string".to_string()));
    }
    if matches!(text, "[]" | "array()") {
        return Some(TypeInfo::Simple("array".to_string()));
    }
    if text.parse::<i64>().is_ok() {
        return Some(TypeInfo::Simple("int".to_string()));
    }
    if text.parse::<f64>().is_ok() {
        return Some(TypeInfo::Simple("float".to_string()));
    }
    None
}

fn conditional_template_names(type_info: &TypeInfo) -> HashSet<String> {
    let mut names = HashSet::new();
    collect_conditional_template_names(type_info, &mut names);
    names
}

fn collect_conditional_template_names(type_info: &TypeInfo, names: &mut HashSet<String>) {
    match type_info {
        TypeInfo::ClassString(Some(inner)) => collect_template_name_leaf(inner, names),
        TypeInfo::Conditional {
            target,
            if_type,
            else_type,
            ..
        } => {
            collect_conditional_template_names(target, names);
            collect_conditional_template_names(if_type, names);
            collect_conditional_template_names(else_type, names);
        }
        TypeInfo::Union(types) | TypeInfo::Intersection(types) => {
            for ty in types {
                collect_conditional_template_names(ty, names);
            }
        }
        TypeInfo::Nullable(inner) => collect_conditional_template_names(inner, names),
        _ => {}
    }
}

fn collect_template_name_leaf(type_info: &TypeInfo, names: &mut HashSet<String>) {
    if let TypeInfo::Simple(name) = type_info {
        if !is_builtin_non_object_type(name) && !name.contains('\\') {
            names.insert(name.clone());
        }
    }
}

fn type_pattern_matches_actual(
    pattern: &TypeInfo,
    actual: &TypeInfo,
    template_names: &HashSet<String>,
    substitutions: &mut HashMap<String, TypeInfo>,
) -> bool {
    match (pattern, actual) {
        (TypeInfo::Mixed, _) => true,
        (TypeInfo::Simple(name), actual) if template_names.contains(name) => {
            substitutions
                .entry(name.clone())
                .or_insert_with(|| actual.clone());
            true
        }
        (TypeInfo::Simple(expected), TypeInfo::Simple(actual)) => same_type_name(expected, actual),
        (TypeInfo::ClassString(Some(pattern_inner)), TypeInfo::ClassString(Some(actual_inner))) => {
            type_pattern_matches_actual(pattern_inner, actual_inner, template_names, substitutions)
        }
        (TypeInfo::ClassString(None), TypeInfo::ClassString(_)) => true,
        (
            TypeInfo::Generic {
                base: expected_base,
                args: expected_args,
            },
            TypeInfo::Generic {
                base: actual_base,
                args: actual_args,
            },
        ) if same_type_name(expected_base, actual_base)
            && expected_args.len() == actual_args.len() =>
        {
            expected_args
                .iter()
                .zip(actual_args.iter())
                .all(|(expected_arg, actual_arg)| {
                    type_pattern_matches_actual(
                        expected_arg,
                        actual_arg,
                        template_names,
                        substitutions,
                    )
                })
        }
        (TypeInfo::Union(types), actual) => types.iter().any(|type_info| {
            let mut branch_substitutions = substitutions.clone();
            let matches = type_pattern_matches_actual(
                type_info,
                actual,
                template_names,
                &mut branch_substitutions,
            );
            if matches {
                *substitutions = branch_substitutions;
            }
            matches
        }),
        (TypeInfo::Intersection(types), actual) => types.iter().all(|type_info| {
            type_pattern_matches_actual(type_info, actual, template_names, substitutions)
        }),
        (TypeInfo::Nullable(_), TypeInfo::LiteralNull) => true,
        (TypeInfo::Nullable(inner), actual) => {
            type_pattern_matches_actual(inner, actual, template_names, substitutions)
        }
        (TypeInfo::LiteralString(expected), TypeInfo::LiteralString(actual))
        | (TypeInfo::LiteralInt(expected), TypeInfo::LiteralInt(actual))
        | (TypeInfo::LiteralFloat(expected), TypeInfo::LiteralFloat(actual)) => expected == actual,
        (TypeInfo::LiteralBool(expected), TypeInfo::LiteralBool(actual)) => expected == actual,
        (TypeInfo::LiteralNull, TypeInfo::LiteralNull) => true,
        _ => false,
    }
}

fn same_type_name(left: &str, right: &str) -> bool {
    left.trim_start_matches('\\')
        .eq_ignore_ascii_case(right.trim_start_matches('\\'))
}

fn try_resolve_function_call_object_fqn(
    node: Node,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
    function_resolver: Option<FunctionTypeResolver<'_>>,
) -> Option<String> {
    let type_info = try_resolve_function_call_return_type_info(
        node,
        source,
        file_symbols,
        resolver,
        callable_resolver,
        function_resolver,
    )?;
    resolver_type_info_to_object_fqn(&type_info, node, source, file_symbols)
}

fn resolver_type_text_for_parser(type_text: &str) -> String {
    resolver_type_info_for_parser(&type_info_from_type_text(type_text)).to_string()
}

fn resolver_type_info_for_parser(type_info: &TypeInfo) -> TypeInfo {
    match type_info {
        TypeInfo::Simple(name) => TypeInfo::Simple(resolver_type_name_for_parser(name)),
        TypeInfo::Generic { base, args } => TypeInfo::Generic {
            base: resolver_type_name_for_parser(base),
            args: args.iter().map(resolver_type_info_for_parser).collect(),
        },
        TypeInfo::Nullable(inner) => {
            TypeInfo::Nullable(Box::new(resolver_type_info_for_parser(inner)))
        }
        TypeInfo::Union(types) => {
            TypeInfo::Union(types.iter().map(resolver_type_info_for_parser).collect())
        }
        TypeInfo::Intersection(types) => {
            TypeInfo::Intersection(types.iter().map(resolver_type_info_for_parser).collect())
        }
        TypeInfo::ClassString(Some(inner)) => {
            TypeInfo::ClassString(Some(Box::new(resolver_type_info_for_parser(inner))))
        }
        TypeInfo::Conditional {
            subject,
            target,
            if_type,
            else_type,
        } => TypeInfo::Conditional {
            subject: subject.clone(),
            target: Box::new(resolver_type_info_for_parser(target)),
            if_type: Box::new(resolver_type_info_for_parser(if_type)),
            else_type: Box::new(resolver_type_info_for_parser(else_type)),
        },
        TypeInfo::ArrayShape(items) => TypeInfo::ArrayShape(
            items
                .iter()
                .map(|item| php_lsp_types::ArrayShapeItem {
                    key: item.key.clone(),
                    optional: item.optional,
                    value: resolver_type_info_for_parser(&item.value),
                })
                .collect(),
        ),
        TypeInfo::ObjectShape(items) => TypeInfo::ObjectShape(
            items
                .iter()
                .map(|item| php_lsp_types::ArrayShapeItem {
                    key: item.key.clone(),
                    optional: item.optional,
                    value: resolver_type_info_for_parser(&item.value),
                })
                .collect(),
        ),
        TypeInfo::Callable {
            params,
            return_type,
        } => TypeInfo::Callable {
            params: params.iter().map(resolver_type_info_for_parser).collect(),
            return_type: return_type
                .as_ref()
                .map(|return_type| Box::new(resolver_type_info_for_parser(return_type))),
        },
        TypeInfo::Self_
        | TypeInfo::Static_
        | TypeInfo::Parent_
        | TypeInfo::ClassString(None)
        | TypeInfo::LiteralString(_)
        | TypeInfo::LiteralInt(_)
        | TypeInfo::LiteralFloat(_)
        | TypeInfo::LiteralBool(_)
        | TypeInfo::LiteralNull
        | TypeInfo::Void
        | TypeInfo::Never
        | TypeInfo::Mixed => type_info.clone(),
    }
}

fn resolver_type_name_for_parser(name: &str) -> String {
    let name = name.trim();
    if !name.starts_with('\\') && name.contains('\\') && !is_builtin_non_object_type(name) {
        format!("\\{name}")
    } else {
        name.to_string()
    }
}

fn resolver_type_info_to_object_fqn(
    type_info: &TypeInfo,
    context_node: Node,
    source: &str,
    file_symbols: &FileSymbols,
) -> Option<String> {
    match type_info {
        TypeInfo::Simple(name) => {
            if is_builtin_non_object_type(name) {
                None
            } else if name.starts_with('\\') || name.contains('\\') {
                Some(name.trim_start_matches('\\').to_string())
            } else {
                Some(resolve_class_name(name, file_symbols))
            }
        }
        TypeInfo::Nullable(inner) => {
            resolver_type_info_to_object_fqn(inner, context_node, source, file_symbols)
        }
        TypeInfo::Union(types) | TypeInfo::Intersection(types) => types.iter().find_map(|ty| {
            resolver_type_info_to_object_fqn(ty, context_node, source, file_symbols)
        }),
        TypeInfo::Self_ | TypeInfo::Static_ => {
            find_parent_class_fqn(context_node, source, file_symbols)
        }
        TypeInfo::Generic { base, .. } => {
            if is_builtin_non_object_type(base) {
                None
            } else if base.starts_with('\\') || base.contains('\\') {
                Some(base.trim_start_matches('\\').to_string())
            } else {
                Some(resolve_class_name(base, file_symbols))
            }
        }
        TypeInfo::Conditional {
            if_type, else_type, ..
        } => resolver_type_info_to_object_fqn(if_type, context_node, source, file_symbols).or_else(
            || resolver_type_info_to_object_fqn(else_type, context_node, source, file_symbols),
        ),
        TypeInfo::ClassString(_)
        | TypeInfo::ArrayShape(_)
        | TypeInfo::ObjectShape(_)
        | TypeInfo::Callable { .. }
        | TypeInfo::LiteralString(_)
        | TypeInfo::LiteralInt(_)
        | TypeInfo::LiteralFloat(_)
        | TypeInfo::LiteralBool(_)
        | TypeInfo::LiteralNull
        | TypeInfo::Void
        | TypeInfo::Never
        | TypeInfo::Mixed
        | TypeInfo::Parent_ => None,
    }
}

fn infer_function_call_type_info(
    node: Node,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
) -> Option<TypeInfo> {
    let short_name = function_call_short_name(node, source)?;
    match short_name.as_str() {
        "array_keys" => {
            let first_arg = call_arguments(node, source).first()?.value_node;
            let array_type = infer_expression_type_info(
                first_arg,
                source,
                file_symbols,
                resolver,
                callable_resolver,
            )?;
            let key_type = iterable_key_type_info(&array_type)
                .map(|type_info| array_key_compatible_type_info(&type_info))
                .unwrap_or_else(|| TypeInfo::Simple("array-key".to_string()));
            Some(TypeInfo::Generic {
                base: "list".to_string(),
                args: vec![key_type],
            })
        }
        "array_values" => {
            let first_arg = call_arguments(node, source).first()?.value_node;
            let array_type = infer_expression_type_info(
                first_arg,
                source,
                file_symbols,
                resolver,
                callable_resolver,
            )?;
            let value_type = iterable_value_type_info(&array_type, None)?;
            Some(TypeInfo::Generic {
                base: "list".to_string(),
                args: vec![value_type],
            })
        }
        _ => try_resolve_function_call_return_type_info(
            node,
            source,
            file_symbols,
            resolver,
            callable_resolver,
            None,
        ),
    }
}

fn function_call_short_name(node: Node, source: &str) -> Option<String> {
    if node.kind() != "function_call_expression" {
        return None;
    }

    let function = node
        .child_by_field_name("function")
        .or_else(|| node.named_child(0))?;
    if function.kind() == "member_access_expression" {
        return None;
    }

    let raw_name = source[function.byte_range()].trim();
    let short = raw_name
        .trim_start_matches('\\')
        .rsplit('\\')
        .next()
        .unwrap_or(raw_name)
        .to_ascii_lowercase();
    (!short.is_empty()).then_some(short)
}

fn infer_cast_expression_type_info(node: Node, source: &str) -> Option<TypeInfo> {
    let text = source[node.byte_range()].trim_start();
    let lower = text.to_ascii_lowercase();
    let cast_type = if lower.starts_with("(string)") {
        "string"
    } else if lower.starts_with("(int)") || lower.starts_with("(integer)") {
        "int"
    } else if lower.starts_with("(float)")
        || lower.starts_with("(double)")
        || lower.starts_with("(real)")
    {
        "float"
    } else if lower.starts_with("(bool)") || lower.starts_with("(boolean)") {
        "bool"
    } else if lower.starts_with("(array)") {
        "array"
    } else if lower.starts_with("(object)") {
        "object"
    } else {
        return None;
    };

    Some(TypeInfo::Simple(cast_type.to_string()))
}

fn infer_binary_expression_type_info(
    node: Node,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
) -> Option<TypeInfo> {
    let text = source[node.byte_range()].trim();
    find_top_level_text(text, "??")?;

    let left = node
        .child_by_field_name("left")
        .or_else(|| node.named_child(0));
    let right = node
        .child_by_field_name("right")
        .or_else(|| node.named_child(1));
    let left_type = left
        .and_then(|left| {
            infer_expression_type_info(left, source, file_symbols, resolver, callable_resolver)
        })
        .and_then(|type_info| type_info_without_null(&type_info));
    let right_type = right
        .and_then(|right| {
            infer_expression_type_info(right, source, file_symbols, resolver, callable_resolver)
        })
        .or_else(|| right.and_then(|right| infer_literal_expression_type_info(right, source)));

    merge_optional_type_infos(left_type, right_type)
}

fn infer_literal_expression_type_info(node: Node, source: &str) -> Option<TypeInfo> {
    let text = source[node.byte_range()].trim();
    if text.starts_with(['\'', '"']) {
        return Some(TypeInfo::Simple("string".to_string()));
    }
    if text.eq_ignore_ascii_case("true") {
        return Some(TypeInfo::LiteralBool(true));
    }
    if text.eq_ignore_ascii_case("false") {
        return Some(TypeInfo::LiteralBool(false));
    }
    if text.eq_ignore_ascii_case("null") {
        return Some(TypeInfo::LiteralNull);
    }
    if text.parse::<i64>().is_ok() {
        return Some(TypeInfo::Simple("int".to_string()));
    }
    if text.parse::<f64>().is_ok() {
        return Some(TypeInfo::Simple("float".to_string()));
    }
    None
}

fn type_info_from_type_text(type_text: &str) -> TypeInfo {
    let type_text = type_text.trim();
    if let Some(type_info) = parse_phpdoc(&format!("/** @var {type_text} */")).var_type {
        return type_info;
    }

    if let Some(inner) = type_text.strip_prefix('?') {
        return TypeInfo::Nullable(Box::new(type_info_from_type_text(inner)));
    }

    let union_parts = split_top_level_text(type_text, '|');
    if union_parts.len() > 1 {
        return merge_type_infos(
            union_parts
                .into_iter()
                .filter(|part| !part.trim().is_empty())
                .map(type_info_from_type_text)
                .collect(),
        );
    }

    let intersection_parts = split_top_level_text(type_text, '&');
    if intersection_parts.len() > 1 {
        return TypeInfo::Intersection(
            intersection_parts
                .into_iter()
                .filter(|part| !part.trim().is_empty())
                .map(type_info_from_type_text)
                .collect(),
        );
    }

    match type_text.to_ascii_lowercase().as_str() {
        "null" => TypeInfo::LiteralNull,
        "true" => TypeInfo::LiteralBool(true),
        "false" => TypeInfo::LiteralBool(false),
        _ => TypeInfo::Simple(type_text.to_string()),
    }
}

fn type_info_without_null(type_info: &TypeInfo) -> Option<TypeInfo> {
    match type_info {
        TypeInfo::LiteralNull => None,
        TypeInfo::Nullable(inner) => type_info_without_null(inner),
        TypeInfo::Union(types) => {
            let kept: Vec<TypeInfo> = types.iter().filter_map(type_info_without_null).collect();
            (!kept.is_empty()).then(|| merge_type_infos(kept))
        }
        other => Some(other.clone()),
    }
}

fn merge_optional_type_infos(left: Option<TypeInfo>, right: Option<TypeInfo>) -> Option<TypeInfo> {
    match (left, right) {
        (Some(left), Some(right)) => Some(merge_type_infos(vec![left, right])),
        (Some(only), None) | (None, Some(only)) => Some(only),
        (None, None) => None,
    }
}

fn merge_type_infos(types: Vec<TypeInfo>) -> TypeInfo {
    let mut merged = Vec::new();
    let mut seen = HashSet::new();
    for type_info in types {
        match type_info {
            TypeInfo::Union(inner) => {
                for inner_type in inner {
                    let key = inner_type.to_string();
                    if seen.insert(key) {
                        merged.push(inner_type);
                    }
                }
            }
            other => {
                let key = other.to_string();
                if seen.insert(key) {
                    merged.push(other);
                }
            }
        }
    }

    if merged.len() == 1 {
        merged.pop().unwrap()
    } else {
        TypeInfo::Union(merged)
    }
}

fn infer_expression_type_info(
    node: Node,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
) -> Option<TypeInfo> {
    if let Some(class_string) = class_string_type_info_from_expression(node, source, file_symbols) {
        return Some(class_string);
    }

    match node.kind() {
        "parenthesized_expression" => {
            for i in 0..node.named_child_count() {
                if let Some(child) = node.named_child(i) {
                    if let Some(type_info) = infer_expression_type_info(
                        child,
                        source,
                        file_symbols,
                        resolver,
                        callable_resolver,
                    ) {
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
                callable_resolver,
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
        "function_call_expression" => {
            infer_function_call_type_info(node, source, file_symbols, resolver, callable_resolver)
        }
        "cast_expression" => infer_cast_expression_type_info(node, source),
        "binary_expression" => infer_binary_expression_type_info(
            node,
            source,
            file_symbols,
            resolver,
            callable_resolver,
        ),
        "member_access_expression" | "nullsafe_member_access_expression" => {
            let object = node.child_by_field_name("object")?;
            let name = node.child_by_field_name("name")?;
            let class_fqn = try_resolve_object_type(
                object,
                source,
                file_symbols,
                resolver,
                callable_resolver,
                None,
            )?;
            let prop_fqn = format!("{}::${}", class_fqn, &source[name.byte_range()]);
            file_symbols
                .symbols
                .iter()
                .find_map(|sym| {
                    (sym.fqn == prop_fqn)
                        .then(|| symbol_effective_type_info(sym, file_symbols))
                        .flatten()
                })
                .or_else(|| {
                    resolver.and_then(|resolve_fn| {
                        let resolver_owner = resolver_owner_type_text_for_object(
                            object,
                            &class_fqn,
                            source,
                            file_symbols,
                            resolver,
                            callable_resolver,
                        )
                        .unwrap_or_else(|| class_fqn.trim_start_matches('\\').to_string());
                        resolve_fn(&resolver_owner, &format!("${}", &source[name.byte_range()]))
                            .map(|type_text| {
                                type_info_from_type_text(&resolver_type_text_for_parser(&type_text))
                            })
                    })
                })
        }
        "member_call_expression" | "nullsafe_member_call_expression" => {
            let object = node.child_by_field_name("object")?;
            let name = node.child_by_field_name("name")?;
            let class_fqn = try_resolve_object_type(
                object,
                source,
                file_symbols,
                resolver,
                callable_resolver,
                None,
            )?;
            let method_fqn = format!("{}::{}", class_fqn, &source[name.byte_range()]);
            file_symbols
                .symbols
                .iter()
                .find_map(|sym| {
                    (sym.fqn == method_fqn)
                        .then(|| symbol_effective_type_info(sym, file_symbols))
                        .flatten()
                })
                .or_else(|| {
                    resolver.and_then(|resolve_fn| {
                        let resolver_owner = resolver_owner_type_text_for_object(
                            object,
                            &class_fqn,
                            source,
                            file_symbols,
                            resolver,
                            callable_resolver,
                        )
                        .unwrap_or_else(|| class_fqn.trim_start_matches('\\').to_string());
                        resolve_fn(&resolver_owner, &source[name.byte_range()]).map(|type_text| {
                            type_info_from_type_text(&resolver_type_text_for_parser(&type_text))
                        })
                    })
                })
        }
        "subscript_expression" => {
            let base = node.named_child(0)?;
            let key_text = node
                .named_child(1)
                .map(|node| source[node.byte_range()].trim().to_string());
            let base_type = infer_expression_type_info(
                base,
                source,
                file_symbols,
                resolver,
                callable_resolver,
            )?;
            iterable_value_type_info(&base_type, key_text.as_deref())
        }
        _ => None,
    }
}

fn class_string_type_info_from_expression(
    node: Node,
    source: &str,
    file_symbols: &FileSymbols,
) -> Option<TypeInfo> {
    let raw = source.get(node.byte_range())?.trim();
    let class_name = raw.strip_suffix("::class")?.trim();
    if class_name.is_empty() {
        return None;
    }
    Some(TypeInfo::ClassString(Some(Box::new(TypeInfo::Simple(
        resolve_class_name(class_name, file_symbols)
            .trim_start_matches('\\')
            .to_string(),
    )))))
}

fn infer_literal_array_shape_type(
    node: Node,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
) -> Option<TypeInfo> {
    let text = source[node.byte_range()].trim();
    infer_literal_array_shape_text(
        text,
        node,
        source,
        file_symbols,
        resolver,
        callable_resolver,
    )
}

fn infer_literal_array_shape_text(
    text: &str,
    context_node: Node,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
) -> Option<TypeInfo> {
    let (body, _) = literal_array_body(text)?;
    let mut items = Vec::new();
    for part in split_top_level_text(body, ',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let arrow = find_top_level_text(part, "=>")?;
        let key_text = part[..arrow].trim();
        let value_text = part[arrow + 2..].trim();
        let key = normalize_array_access_key(key_text)?;
        let value = infer_literal_value_type_text(
            value_text,
            context_node,
            source,
            file_symbols,
            resolver,
            callable_resolver,
        );
        items.push(php_lsp_types::ArrayShapeItem {
            key: Some(key),
            optional: false,
            value,
        });
    }

    (!items.is_empty()).then_some(TypeInfo::ArrayShape(items))
}

fn infer_literal_value_type_text(
    text: &str,
    context_node: Node,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
) -> TypeInfo {
    let text = text.trim();
    if let Some(shape) = infer_literal_array_shape_text(
        text,
        context_node,
        source,
        file_symbols,
        resolver,
        callable_resolver,
    ) {
        return shape;
    }
    if let Some(class_name) = text
        .strip_prefix("new ")
        .and_then(|rest| rest.split(['(', ' ', '\n', '\t']).next())
        .filter(|name| !name.is_empty())
    {
        return TypeInfo::Simple(resolve_class_name(class_name, file_symbols));
    }
    if matches!(text, "true" | "false") {
        return TypeInfo::LiteralBool(text == "true");
    }
    if text == "null" {
        return TypeInfo::LiteralNull;
    }
    if text.starts_with(['\'', '"']) {
        return TypeInfo::Simple("string".to_string());
    }
    if text.parse::<i64>().is_ok() {
        return TypeInfo::Simple("int".to_string());
    }
    if text.parse::<f64>().is_ok() {
        return TypeInfo::Simple("float".to_string());
    }

    TypeInfo::Mixed
}

fn literal_array_body(text: &str) -> Option<(&str, char)> {
    let text = text.trim();
    if let Some(body) = text
        .strip_prefix('[')
        .and_then(|body| body.strip_suffix(']'))
    {
        return Some((body, ']'));
    }
    let body = text
        .strip_prefix("array(")
        .and_then(|body| body.strip_suffix(')'))?;
    Some((body, ')'))
}

fn split_top_level_text(s: &str, delimiter: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut paren_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for (idx, ch) in s.char_indices() {
        if let Some(quote_ch) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote_ch {
                quote = None;
            }
            continue;
        }

        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }

        match ch {
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '<' => angle_depth += 1,
            '>' => angle_depth = angle_depth.saturating_sub(1),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        if ch == delimiter
            && paren_depth == 0
            && angle_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
        {
            parts.push(s[start..idx].trim());
            start = idx + ch.len_utf8();
        }
    }

    parts.push(s[start..].trim());
    parts
}

fn find_top_level_text(s: &str, needle: &str) -> Option<usize> {
    let mut paren_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for (idx, ch) in s.char_indices() {
        if let Some(quote_ch) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote_ch {
                quote = None;
            }
            continue;
        }

        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }

        if paren_depth == 0
            && angle_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && s[idx..].starts_with(needle)
        {
            return Some(idx);
        }

        match ch {
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '<' => angle_depth += 1,
            '>' => angle_depth = angle_depth.saturating_sub(1),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }
    }

    None
}

fn foreach_variable_inference(
    stmt: Node,
    var_name: &str,
    usage_start: usize,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
) -> Option<VariableInference> {
    if stmt.kind() != "foreach_statement" || usage_start < stmt.start_byte() {
        return None;
    }

    let key_node = foreach_key_variable_node(stmt, source)
        .filter(|key_node| normalize_var_name(&source[key_node.byte_range()]) == var_name);
    let value_node = foreach_value_variable_node(stmt, source)
        .filter(|value_node| normalize_var_name(&source[value_node.byte_range()]) == var_name);
    if key_node.is_none() && value_node.is_none() {
        return None;
    }

    let iterable_node = foreach_iterable_node(stmt)?;
    let iterable_type = infer_expression_type_info(
        iterable_node,
        source,
        file_symbols,
        resolver,
        callable_resolver,
    )?;

    let (type_info, variable_node) = if let Some(key_node) = key_node {
        (iterable_key_type_info(&iterable_type)?, key_node)
    } else {
        let value_node = value_node?;
        (iterable_value_type_info(&iterable_type, None)?, value_node)
    };

    let resolved_type_fqn =
        resolve_phpdoc_var_type(&type_info, variable_node, source, file_symbols);
    Some(VariableInference {
        type_display: Some(type_info.to_string()),
        resolved_type_fqn,
        phpdoc_comment: None,
        type_info: Some(type_info),
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

fn foreach_key_variable_node<'a>(stmt: Node<'a>, source: &str) -> Option<Node<'a>> {
    let pair = stmt.named_child(1)?;
    if pair.kind() != "pair" {
        return None;
    }
    variable_node_in_foreach_part(pair.named_child(0)?, source)
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

fn infer_textual_expression_type_info(
    scope_node: Node,
    expr_text: &str,
    usage_start: usize,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
) -> Option<VariableInference> {
    let expr_text = expr_text.trim();
    let (base_var, keys) = parse_textual_array_access_chain(expr_text)
        .unwrap_or_else(|| (normalize_var_name(expr_text), Vec::new()));
    if !base_var.starts_with('$') {
        return None;
    }

    let mut inference = infer_variable_in_scope(
        scope_node,
        &base_var,
        usage_start,
        source,
        file_symbols,
        resolver,
        callable_resolver,
    );
    if keys.is_empty() {
        return inference.has_data().then_some(inference);
    }

    let mut value_type = inference.type_info.take()?;
    for key_text in &keys {
        value_type = iterable_value_type_info(&value_type, key_text.as_deref())?;
    }
    let resolved_type_fqn = resolve_phpdoc_var_type(&value_type, scope_node, source, file_symbols);
    Some(VariableInference {
        type_display: Some(value_type.to_string()),
        resolved_type_fqn,
        phpdoc_comment: None,
        type_info: Some(value_type),
    })
}

fn parse_textual_array_access_chain(expr_text: &str) -> Option<(String, Vec<Option<String>>)> {
    let bracket = expr_text.find('[')?;
    let base = expr_text[..bracket].trim();
    if !base.starts_with('$') || base.len() <= 1 {
        return None;
    }

    let mut keys = Vec::new();
    let mut idx = bracket;
    let bytes = expr_text.as_bytes();
    while idx < expr_text.len() {
        while idx < expr_text.len() && bytes[idx].is_ascii_whitespace() {
            idx += 1;
        }
        if idx >= expr_text.len() || bytes[idx] != b'[' {
            break;
        }
        let content_start = idx + 1;
        let content_end = find_matching_textual_bracket(expr_text, idx).unwrap_or(expr_text.len());
        let key = expr_text[content_start..content_end]
            .trim()
            .split(']')
            .next()
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(str::to_string);
        keys.push(key);
        if content_end >= expr_text.len() {
            break;
        }
        idx = content_end + 1;
    }

    (!keys.is_empty()).then(|| (normalize_var_name(base), keys))
}

fn find_matching_textual_bracket(text: &str, open: usize) -> Option<usize> {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut depth = 0usize;
    for (idx, ch) in text[open..].char_indices() {
        let idx = open + idx;
        if let Some(quote_ch) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote_ch {
                quote = None;
            }
            continue;
        }

        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }

        match ch {
            '[' => depth += 1,
            ']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }

    None
}

pub fn iterable_value_type_info(type_info: &TypeInfo, key_text: Option<&str>) -> Option<TypeInfo> {
    match type_info {
        TypeInfo::Nullable(inner) => iterable_value_type_info(inner, key_text),
        TypeInfo::Union(types) | TypeInfo::Intersection(types) => types
            .iter()
            .find_map(|ty| iterable_value_type_info(ty, key_text)),
        TypeInfo::Simple(name) if is_plain_iterable_type_name(name) => Some(TypeInfo::Mixed),
        TypeInfo::Generic { base, args } => generic_value_type_arg(base, args).cloned(),
        TypeInfo::ArrayShape(items) => array_shape_value_type(items, key_text).cloned(),
        TypeInfo::Conditional {
            if_type, else_type, ..
        } => iterable_value_type_info(if_type, key_text)
            .or_else(|| iterable_value_type_info(else_type, key_text)),
        _ => None,
    }
}

fn is_plain_iterable_type_name(name: &str) -> bool {
    matches!(
        name.trim_start_matches('\\').to_ascii_lowercase().as_str(),
        "array" | "iterable" | "traversable" | "iterator" | "iteratoraggregate" | "generator"
    )
}

fn iterable_key_type_info(type_info: &TypeInfo) -> Option<TypeInfo> {
    match type_info {
        TypeInfo::Nullable(inner) => iterable_key_type_info(inner),
        TypeInfo::Union(types) | TypeInfo::Intersection(types) => {
            types.iter().find_map(iterable_key_type_info)
        }
        TypeInfo::Generic { base, args } => generic_key_type_arg(base, args),
        TypeInfo::ArrayShape(items) => array_shape_key_type(items),
        TypeInfo::Conditional {
            if_type, else_type, ..
        } => iterable_key_type_info(if_type).or_else(|| iterable_key_type_info(else_type)),
        _ => None,
    }
}

fn array_key_compatible_type_info(type_info: &TypeInfo) -> TypeInfo {
    match type_info {
        TypeInfo::LiteralString(_) => TypeInfo::Simple("string".to_string()),
        TypeInfo::LiteralInt(_) => TypeInfo::Simple("int".to_string()),
        TypeInfo::Nullable(inner) => array_key_compatible_type_info(inner),
        TypeInfo::Union(types) | TypeInfo::Intersection(types) => {
            let compatible: Vec<TypeInfo> = types
                .iter()
                .filter_map(|type_info| {
                    let normalized = array_key_compatible_type_info(type_info);
                    is_array_key_type_info(&normalized).then_some(normalized)
                })
                .collect();
            if compatible.is_empty() {
                TypeInfo::Simple("array-key".to_string())
            } else {
                merge_type_infos(compatible)
            }
        }
        TypeInfo::Simple(name) => {
            let lower = name.trim_start_matches('\\').to_ascii_lowercase();
            match lower.as_str() {
                "string" | "non-empty-string" | "numeric-string" | "class-string" => {
                    TypeInfo::Simple("string".to_string())
                }
                "int" | "integer" | "positive-int" | "negative-int" | "non-negative-int" => {
                    TypeInfo::Simple("int".to_string())
                }
                "array-key" => TypeInfo::Simple("array-key".to_string()),
                _ => TypeInfo::Simple("array-key".to_string()),
            }
        }
        _ => TypeInfo::Simple("array-key".to_string()),
    }
}

fn is_array_key_type_info(type_info: &TypeInfo) -> bool {
    match type_info {
        TypeInfo::Simple(name) => {
            matches!(
                name.trim_start_matches('\\').to_ascii_lowercase().as_str(),
                "array-key" | "int" | "integer" | "string"
            )
        }
        TypeInfo::Union(types) | TypeInfo::Intersection(types) => {
            types.iter().all(is_array_key_type_info)
        }
        _ => false,
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

fn generic_key_type_arg(base: &str, args: &[TypeInfo]) -> Option<TypeInfo> {
    let base = base.trim_start_matches('\\').to_ascii_lowercase();
    match base.as_str() {
        "array" | "iterable" | "traversable" | "iterator" | "iteratoraggregate" | "generator"
            if args.len() > 1 =>
        {
            args.first().cloned()
        }
        "list" | "non-empty-list" => Some(TypeInfo::Simple("int".to_string())),
        _ if (base.ends_with("\\collection") || base.ends_with("collection")) && args.len() > 1 => {
            args.first().cloned()
        }
        _ => None,
    }
}

fn array_shape_key_type(items: &[php_lsp_types::ArrayShapeItem]) -> Option<TypeInfo> {
    let mut keys = items
        .iter()
        .filter_map(|item| item.key.as_deref())
        .map(|key| {
            if key.parse::<i64>().is_ok() {
                TypeInfo::LiteralInt(key.to_string())
            } else {
                TypeInfo::LiteralString(format!("'{}'", key))
            }
        })
        .collect::<Vec<_>>();
    match keys.len() {
        0 => None,
        1 => keys.pop(),
        _ => Some(TypeInfo::Union(keys)),
    }
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
    let trimmed = normalize_shape_key_text(raw);
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn position_to_byte(source: &str, line: u32, character: u32) -> usize {
    let mut offset = 0usize;
    let line_idx = line as usize;
    for (i, row) in source.split_inclusive('\n').enumerate() {
        let line_text = row.strip_suffix('\n').unwrap_or(row);
        if i == line_idx {
            let byte_col = utf16_col_to_byte(source, line, character) as usize;
            return offset + byte_col.min(line_text.len());
        }
        offset += row.len();
    }

    if line_idx == source.split_inclusive('\n').count() {
        return source.len();
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
            for var_node in [
                foreach_key_variable_node(node, source),
                foreach_value_variable_node(node, source),
            ]
            .into_iter()
            .flatten()
            {
                if normalize_var_name(&source[var_node.byte_range()]) == var_name {
                    let start = var_node.start_byte();
                    if start <= usage_start {
                        *best = Some((start, node_range(var_node)));
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
            for var_node in [
                foreach_key_variable_node(node, source),
                foreach_value_variable_node(node, source),
            ]
            .into_iter()
            .flatten()
            {
                collect_variable_node(var_node, usage_start, source, vars);
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

    fn test_param(name: &str) -> php_lsp_types::ParamInfo {
        php_lsp_types::ParamInfo {
            name: name.to_string(),
            type_info: None,
            default_value: None,
            is_variadic: false,
            is_by_ref: false,
            is_promoted: false,
        }
    }

    fn defaulted_test_param(name: &str, default_value: &str) -> php_lsp_types::ParamInfo {
        let mut param = test_param(name);
        param.default_value = Some(default_value.to_string());
        param
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

    fn parse_and_infer_var_type_info_at(
        code: &str,
        line: u32,
        col: u32,
        var_name: &str,
    ) -> Option<TypeInfo> {
        let mut parser = FileParser::new();
        parser.parse_full(code);
        let tree = parser.tree().unwrap();
        let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
        infer_variable_type_info_at_position(tree, code, &file_symbols, line, col, var_name)
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
    fn test_position_to_byte_handles_crlf() {
        let source = "<?php\r\n$first = 1;\r\n$second = $first;\r\n";
        let expected = source.find("$second").unwrap();

        assert_eq!(position_to_byte(source, 2, 0), expected);
    }

    #[test]
    fn test_position_to_byte_converts_utf16_character() {
        let source = "<?php\n$emoji = \"😀\";\n$result = $emoji;\n";
        let expected = source.find("$result").unwrap();

        assert_eq!(position_to_byte(source, 2, 0), expected);
    }

    struct ObjectTypeResolveDepthReset {
        previous: usize,
    }

    impl ObjectTypeResolveDepthReset {
        fn set(value: usize) -> Self {
            let previous = OBJECT_TYPE_RESOLVE_DEPTH.with(|depth| {
                let previous = depth.get();
                depth.set(value);
                previous
            });
            Self { previous }
        }
    }

    impl Drop for ObjectTypeResolveDepthReset {
        fn drop(&mut self) {
            OBJECT_TYPE_RESOLVE_DEPTH.with(|depth| depth.set(self.previous));
        }
    }

    fn object_type_resolve_depth() -> usize {
        OBJECT_TYPE_RESOLVE_DEPTH.with(|depth| depth.get())
    }

    #[test]
    fn test_object_type_resolve_depth_guard_restores_after_panic() {
        let _reset = ObjectTypeResolveDepthReset::set(MAX_OBJECT_TYPE_RESOLVE_DEPTH - 1);

        let result = std::panic::catch_unwind(|| {
            let _guard = ObjectTypeResolveDepthGuard::enter()
                .expect("guard should enter below max resolve depth");
            assert_eq!(object_type_resolve_depth(), MAX_OBJECT_TYPE_RESOLVE_DEPTH);
            std::panic::resume_unwind(Box::new("simulated object type resolver panic"));
        });

        assert!(result.is_err());
        assert_eq!(
            object_type_resolve_depth(),
            MAX_OBJECT_TYPE_RESOLVE_DEPTH - 1
        );

        let guard = ObjectTypeResolveDepthGuard::enter()
            .expect("next resolve attempt should start from the restored depth");
        assert_eq!(object_type_resolve_depth(), MAX_OBJECT_TYPE_RESOLVE_DEPTH);
        drop(guard);
        assert_eq!(
            object_type_resolve_depth(),
            MAX_OBJECT_TYPE_RESOLVE_DEPTH - 1
        );
    }

    #[test]
    fn test_object_type_resolve_depth_guard_respects_max_depth() {
        let _reset = ObjectTypeResolveDepthReset::set(MAX_OBJECT_TYPE_RESOLVE_DEPTH);

        assert!(ObjectTypeResolveDepthGuard::enter().is_none());
        assert_eq!(object_type_resolve_depth(), MAX_OBJECT_TYPE_RESOLVE_DEPTH);
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
    fn test_resolve_symbol_after_emoji_uses_utf16_position() {
        let code = "<?php\n$emoji = \"😀\"; strlen('hello');\n";
        let result = parse_and_resolve(code, 1, 17).expect("strlen should resolve after emoji");

        assert_eq!(result.ref_kind, RefKind::FunctionCall);
        assert_eq!(result.name, "strlen");
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
    fn test_conditional_return_uses_subject_parameter_position_for_positional_arguments() {
        let signature = Signature {
            params: vec![test_param("prefix"), test_param("abstract")],
            return_type: None,
        };
        let function_resolver = |function_name: &str| -> Option<ResolvedFunctionType> {
            (function_name == "App\\helper").then(|| {
                ResolvedFunctionType::with_signature(
                    "($abstract is class-string<TClass> ? TClass : mixed)",
                    Some(signature.clone()),
                )
            })
        };

        let unresolved_code = r#"<?php
namespace App;
class Backend {}
helper(Backend::class)->ping();
"#;
        let mut parser = FileParser::new();
        parser.parse_full(unresolved_code);
        let tree = parser.tree().unwrap();
        let file_symbols = extract_file_symbols(tree, unresolved_code, "file:///test.php");
        let (line, col) = find_line_col(unresolved_code, "ping");
        let unresolved = symbol_at_position_with_full_resolvers(
            tree,
            unresolved_code,
            line,
            col,
            &file_symbols,
            None,
            None,
            Some(&function_resolver),
        )
        .expect("method symbol should be produced even when receiver type is unresolved");

        assert_eq!(unresolved.ref_kind, RefKind::MethodCall);
        assert_eq!(unresolved.fqn, "ping");

        let resolved_code = r#"<?php
namespace App;
class Backend {}
helper('service', Backend::class)->ping();
"#;
        let mut parser = FileParser::new();
        parser.parse_full(resolved_code);
        let tree = parser.tree().unwrap();
        let file_symbols = extract_file_symbols(tree, resolved_code, "file:///test.php");
        let (line, col) = find_line_col(resolved_code, "ping");
        let resolved = symbol_at_position_with_full_resolvers(
            tree,
            resolved_code,
            line,
            col,
            &file_symbols,
            None,
            None,
            Some(&function_resolver),
        )
        .expect("second positional argument should match the conditional subject");

        assert_eq!(resolved.ref_kind, RefKind::MethodCall);
        assert_eq!(resolved.fqn, "App\\Backend::ping");
    }

    #[test]
    fn test_conditional_return_uses_defaulted_subject_argument_when_omitted() {
        let response_signature = Signature {
            params: vec![defaulted_test_param("content", "null")],
            return_type: None,
        };
        let redirect_signature = Signature {
            params: vec![defaulted_test_param("to", "null")],
            return_type: None,
        };
        let function_resolver = |function_name: &str| -> Option<ResolvedFunctionType> {
            match function_name {
                "App\\response" => Some(ResolvedFunctionType::with_signature(
                    "($content is null ? App\\ResponseFactory : App\\Response)",
                    Some(response_signature.clone()),
                )),
                "App\\redirect" => Some(ResolvedFunctionType::with_signature(
                    "($to is null ? App\\Redirector : App\\RedirectResponse)",
                    Some(redirect_signature.clone()),
                )),
                _ => None,
            }
        };

        let code = r#"<?php
namespace App;
class JsonResponse {}
class ResponseFactory { public function json(): JsonResponse {} }
class Response { public function setContent(string $content): void {} }
class RedirectResponse { public function with(string $key, mixed $value): self {} }
class Redirector { public function route(string $name): RedirectResponse {} }

response()->json();
response('ok')->setContent('ok');
redirect()->route('home');
redirect('/home')->with('status', 'ok');
"#;

        let mut parser = FileParser::new();
        parser.parse_full(code);
        let tree = parser.tree().unwrap();
        let file_symbols = extract_file_symbols(tree, code, "file:///test.php");

        let (line, col) = find_line_col(code, "json");
        let response_factory_method = symbol_at_position_with_full_resolvers(
            tree,
            code,
            line,
            col,
            &file_symbols,
            None,
            None,
            Some(&function_resolver),
        )
        .expect("response() should resolve through the default null conditional branch");
        assert_eq!(response_factory_method.fqn, "App\\ResponseFactory::json");

        let (line, col) = find_line_col(code, "setContent");
        let response_method = symbol_at_position_with_full_resolvers(
            tree,
            code,
            line,
            col,
            &file_symbols,
            None,
            None,
            Some(&function_resolver),
        )
        .expect("response('ok') should resolve through the non-null conditional branch");
        assert_eq!(response_method.fqn, "App\\Response::setContent");

        let (line, col) = find_line_col(code, "route");
        let redirector_method = symbol_at_position_with_full_resolvers(
            tree,
            code,
            line,
            col,
            &file_symbols,
            None,
            None,
            Some(&function_resolver),
        )
        .expect("redirect() should resolve through the default null conditional branch");
        assert_eq!(redirector_method.fqn, "App\\Redirector::route");

        let (line, col) = find_line_col(code, "with");
        let redirect_response_method = symbol_at_position_with_full_resolvers(
            tree,
            code,
            line,
            col,
            &file_symbols,
            None,
            None,
            Some(&function_resolver),
        )
        .expect("redirect('/home') should resolve through the non-null conditional branch");
        assert_eq!(redirect_response_method.fqn, "App\\RedirectResponse::with");
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
    fn test_resolve_method_call_on_function_return_with_nullable_resolver_type_text() {
        let code = r#"<?php
namespace App\Controller;

class Handler {
    public function run(): void {
        makeUser()->getName();
    }
}
"#;
        let mut parser = FileParser::new();
        parser.parse_full(code);
        let tree = parser.tree().unwrap();
        let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
        let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
            (class_fqn.is_empty() && member_name == "App\\Controller\\makeUser")
                .then(|| "?App\\Entity\\User".to_string())
        };
        let (line, col) = find_line_col(code, "getName");
        let result =
            symbol_at_position_with_resolver(tree, code, line, col, &file_symbols, Some(&resolver))
                .expect("method call should resolve through normalized resolver type text");
        assert_eq!(result.ref_kind, RefKind::MethodCall);
        assert_eq!(result.fqn, "App\\Entity\\User::getName");
    }

    #[test]
    fn test_infer_foreach_value_from_resolver_generic_function_return_preserves_args() {
        let code = r#"<?php
namespace App\Controller;

function run(): void {
    foreach (loadUsers() as $user) {
        $user;
    }
}
"#;
        let mut parser = FileParser::new();
        parser.parse_full(code);
        let tree = parser.tree().unwrap();
        let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
        let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
            (class_fqn.is_empty() && member_name == "App\\Controller\\loadUsers")
                .then(|| "App\\Support\\Collection<int, App\\Entity\\User>".to_string())
        };
        let (line, col) = find_line_col(code, "$user;");
        let inferred = infer_variable_type_at_position_with_resolver(
            tree,
            code,
            &file_symbols,
            line,
            col,
            "$user",
            &resolver,
        )
        .expect("foreach value type should preserve generic resolver return args");
        assert_eq!(inferred, "App\\Entity\\User");
    }

    #[test]
    fn test_resolve_foreach_value_from_resolver_generic_member_return_preserves_absolute_args() {
        let code = r#"<?php
namespace App\Soap\Inbound\Handler;

use App\Entity\ReverseRequest;

final class CompleteHandler {
    public function update(ReverseRequest $reverseRequest): void {
        foreach ($reverseRequest->getReversePortingNumbers() as $portingNumber) {
            $portingNumber->getPhoneNumber();
        }
    }
}
"#;
        let mut parser = FileParser::new();
        parser.parse_full(code);
        let tree = parser.tree().unwrap();
        let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
        let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
            (class_fqn == "App\\Entity\\ReverseRequest"
                && member_name == "getReversePortingNumbers")
                .then(|| {
                    "Doctrine\\Common\\Collections\\Collection<int, App\\Entity\\ReversePortingNumber>"
                        .to_string()
                })
        };
        let (line, col) = find_line_col(code, "$portingNumber->getPhoneNumber");
        let result = symbol_at_position_with_resolver(
            tree,
            code,
            line,
            col + "$portingNumber->".len() as u32,
            &file_symbols,
            Some(&resolver),
        )
        .expect("foreach value should resolve through normalized generic member return");
        assert_eq!(result.ref_kind, RefKind::MethodCall);
        assert_eq!(
            result.fqn,
            "App\\Entity\\ReversePortingNumber::getPhoneNumber"
        );
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
        let (line, col) = find_line_col(code, "$user->getName");
        let col = col + "$user->".len() as u32;
        let result = parse_and_resolve(code, line, col).expect("foreach value should resolve");
        assert_eq!(result.fqn, "App\\Entity\\User::getName");
        assert_eq!(result.ref_kind, RefKind::MethodCall);
    }

    #[test]
    fn test_resolve_foreach_value_from_phpdoc_generic_namespace_relative_method_return() {
        let code = r#"<?php
namespace App;

class Repository {
    /** @return array<int, Entity\User> */
    public function users(): array { return []; }
}

function run(Repository $repository): void {
    foreach ($repository->users() as $user) {
        $user;
    }
}
"#;
        let (line, col) = find_line_col(code, "$user;");
        let inferred = parse_and_infer_var_type_at(code, line, col, "$user")
            .expect("foreach value type should be inferred");
        assert_eq!(inferred, "App\\Entity\\User");
    }

    #[test]
    fn test_resolve_foreach_value_from_phpdoc_generic_alias_qualified_method_return() {
        let code = r#"<?php
namespace App\Models {
    class User {
        public function getName(): string { return ''; }
    }
}

namespace App {
    use App\Models as Model;

    class Repository {
        /** @return array<int, Model\User> */
        public function users(): array { return []; }
    }

    function run(Repository $repository): void {
        foreach ($repository->users() as $user) {
            $user->getName();
        }
    }
}
"#;
        let (line, col) = find_line_col(code, "$user->getName");
        let col = col + "$user->".len() as u32;
        let result = parse_and_resolve(code, line, col).expect("foreach value should resolve");
        assert_eq!(result.fqn, "App\\Models\\User::getName");
        assert_eq!(result.ref_kind, RefKind::MethodCall);
    }

    #[test]
    fn test_infer_foreach_value_from_member_assigned_collection() {
        let code = r#"<?php
namespace App;

/**
 * @var array<int, \App\Entity\DataRequest> $pagination
 */
$pagination = [];
foreach ($pagination as $dr):
    $shown = $dr->numbers;
    foreach ($shown as $num):
        $num;
    endforeach;
endforeach;
"#;
        let mut parser = FileParser::new();
        parser.parse_full(code);
        let tree = parser.tree().unwrap();
        let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
        let (line, col) = find_line_col(code, "$num;");
        let node = find_node_at_point(tree.root_node(), Point::new(line as usize, col as usize))
            .expect("variable node");
        let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
            (class_fqn == "App\\Entity\\DataRequest" && member_name == "$numbers")
                .then(|| "array<int, string>".to_string())
        };
        let info = infer_variable_hover_info_at_node_with_resolvers(
            node,
            code,
            &file_symbols,
            node.start_byte(),
            "$num",
            Some(&resolver),
            None,
        )
        .expect("foreach value should infer from assigned member collection");

        assert_eq!(info.type_display.as_deref(), Some("string"));
    }

    #[test]
    fn test_infer_foreach_value_from_member_assigned_plain_array() {
        let code = r#"<?php
namespace App;

/**
 * @var array<int, \App\Entity\DataRequest> $pagination
 */
$pagination = [];
foreach ($pagination as $dr):
    $shown = $dr->numbers;
    foreach ($shown as $num):
        $num;
    endforeach;
endforeach;
"#;
        let mut parser = FileParser::new();
        parser.parse_full(code);
        let tree = parser.tree().unwrap();
        let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
        let (line, col) = find_line_col(code, "$num;");
        let node = find_node_at_point(tree.root_node(), Point::new(line as usize, col as usize))
            .expect("variable node");
        let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
            (class_fqn == "App\\Entity\\DataRequest" && member_name == "$numbers")
                .then(|| "array".to_string())
        };
        let info = infer_variable_hover_info_at_node_with_resolvers(
            node,
            code,
            &file_symbols,
            node.start_byte(),
            "$num",
            Some(&resolver),
            None,
        )
        .expect("foreach value should infer mixed from plain array");

        assert_eq!(info.type_display.as_deref(), Some("mixed"));
    }

    #[test]
    fn test_infer_foreach_value_from_array_keys_after_array_write() {
        let code = r#"<?php
function run(array $numbers): void {
    $normalizedNumbers = [];
    foreach ($numbers as $number) {
        $normalizedNumber = preg_replace('/\D+/', '', is_scalar($number) ? (string)$number : '') ?? '';
        if ('' !== $normalizedNumber) {
            $normalizedNumbers[$normalizedNumber] = true;
        }
    }
    $numbers = array_keys($normalizedNumbers);
    foreach ($numbers as $phoneNumber) {
        $phoneNumber;
    }
}
"#;
        let (line, col) = find_line_col(code, "$phoneNumber;");
        let info =
            parse_and_variable_hover_info(code, line, col + 2).expect("foreach value should infer");

        assert_eq!(info.variable_name, "$phoneNumber");
        assert_eq!(info.type_display.as_deref(), Some("string"));
    }

    #[test]
    fn test_resolve_foreach_key_and_value_from_phpdoc_generator() {
        let code = r#"<?php
namespace App;

class User {
    public function getName(): string { return ''; }
}

function run(): void {
    /** @var \Generator<string, User, mixed, void> $users */
    $users = loadUsers();
    foreach ($users as $id => $user) {
        $id;
        $user->getName();
    }
}
"#;
        let (user_line, user_col) = find_line_col(code, "getName");
        let result =
            parse_and_resolve(code, user_line, user_col).expect("generator value should resolve");
        assert_eq!(result.fqn, "App\\User::getName");
        assert_eq!(result.ref_kind, RefKind::MethodCall);

        let (id_line, id_col) = find_line_col(code, "$id;");
        let inferred = parse_and_infer_var_type_info_at(code, id_line, id_col + 2, "$id")
            .expect("generator key should infer");
        assert_eq!(inferred, TypeInfo::Simple("string".to_string()));
    }

    #[test]
    fn test_resolve_array_map_style_callback_parameter_from_callable_signature() {
        let code = r#"<?php
namespace App;

class User {
    public function getName(): string { return ''; }
}

/**
 * @template TItem
 * @template TResult
 * @param callable(TItem): TResult $callback
 * @param array<int, TItem> $items
 * @return array<int, TResult>
 */
function map_values(callable $callback, array $items): array { return []; }

function run(): void {
    /** @var array<int, User> $users */
    $users = [];
    map_values(fn($user) => $user->getName(), $users);
    $user->getName();
}
"#;
        let (line, col) = find_line_col(code, "$user->getName(),");
        let result = parse_and_resolve(code, line, col + "$user->".len() as u32)
            .expect("callback parameter method should resolve");
        assert_eq!(result.fqn, "App\\User::getName");
        assert_eq!(result.ref_kind, RefKind::MethodCall);

        let (outside_line, outside_col) = find_line_col(code, "$user->getName();");
        let outside = parse_and_resolve(code, outside_line, outside_col + "$user->".len() as u32)
            .expect("outside method still has a syntactic symbol");
        assert_ne!(
            outside.fqn, "App\\User::getName",
            "closure parameter type must not leak into outer scope"
        );
    }

    #[test]
    fn test_resolve_collection_callback_parameter_from_receiver_generic_signature() {
        let code = r#"<?php
namespace App;

class User {
    public function getName(): string { return ''; }
}

/**
 * @template TItem
 */
class Collection {
    /**
     * @template TResult
     * @param callable(TItem): TResult $callback
     * @return Collection<TResult>
     */
    public function map(callable $callback): self { return $this; }

    /**
     * @param callable(TItem): bool $callback
     * @return Collection<TItem>
     */
    public function filter(callable $callback): self { return $this; }
}

function run(): void {
    /** @var Collection<User> $users */
    $users = loadUsers();
    $users->map(fn($user) => $user->getName());
    $users->filter(function ($user): bool {
        return '' !== $user->getName();
    });
}
"#;
        let (map_line, map_col) = find_line_col(code, "$user->getName());");
        let map_result = parse_and_resolve(code, map_line, map_col + "$user->".len() as u32)
            .expect("map callback parameter method should resolve");
        assert_eq!(map_result.fqn, "App\\User::getName");

        let (filter_line, filter_col) = find_line_col(code, "$user->getName();");
        let filter_result =
            parse_and_resolve(code, filter_line, filter_col + "$user->".len() as u32)
                .expect("filter callback parameter method should resolve");
        assert_eq!(filter_result.fqn, "App\\User::getName");
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
    /** @var array{'user': User} $row */
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
    fn test_infer_nested_array_shape_type_info_from_phpdoc_access_text() {
        let code = r#"<?php
function run(): void {
    /** @var array{meta: array{city: string, zip?: int}} $row */
    $row = [];
    $row['meta']['
}
"#;
        let inferred = parse_and_infer_var_type_info_at(code, 4, 18, "$row['meta']")
            .expect("nested array shape should be inferred");
        let TypeInfo::ArrayShape(items) = inferred else {
            panic!("expected nested array shape");
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].key.as_deref(), Some("city"));
        assert_eq!(items[1].key.as_deref(), Some("zip"));
        assert!(items[1].optional);
    }

    #[test]
    fn test_infer_array_shape_from_multiline_file_type_alias() {
        let code = r#"<?php
namespace App;

/**
 * @phpstan-type RowShape array{
 *   'user-id': int,
 *   meta: array{
 *     city: string,
 *   },
 * }
 */
use App\Entity\User;

function run(): void {
    /** @var RowShape $row */
    $row = [];
    $row['meta']['
}
"#;
        let (line, col) = find_line_col(code, "$row['meta']['");
        let inferred = parse_and_infer_var_type_info_at(
            code,
            line,
            col + "$row['meta']".len() as u32,
            "$row['meta']",
        )
        .expect("type alias array shape should be expanded for local inference");
        let TypeInfo::ArrayShape(items) = inferred else {
            panic!("expected nested alias array shape");
        };
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].key.as_deref(), Some("city"));
    }

    #[test]
    fn test_infer_literal_array_shape_type_info() {
        let code = r#"<?php
function run(): void {
    $row = ['foo' => 1, 'meta' => ['city' => 'Paris']];
    $row['meta']['
}
"#;
        let inferred = parse_and_infer_var_type_info_at(code, 3, 18, "$row['meta']")
            .expect("literal nested array shape should be inferred");
        let TypeInfo::ArrayShape(items) = inferred else {
            panic!("expected literal nested array shape");
        };
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].key.as_deref(), Some("city"));
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
    fn test_find_variable_definition_foreach_value_usage() {
        let code = r#"<?php
function demo(array $items): void {
    foreach ($items as $item) {
        echo $item;
    }
}
"#;
        let (line, col) = find_line_col(code, "echo $item");
        let def = parse_and_find_var_def(code, line, col + "echo ".len() as u32 + 2)
            .expect("foreach value variable definition should be found");
        let (def_line, def_col) = find_line_col(code, "$item) {");
        assert_eq!(def.0, def_line);
        assert_eq!(def.1, def_col);
    }

    #[test]
    fn test_find_variable_definition_foreach_value_declaration_points_to_itself() {
        let code = r#"<?php
function demo(array $items): void {
    foreach ($items as $item) {
        echo $item;
    }
}
"#;
        let (line, col) = find_line_col(code, "$item) {");
        let def = parse_and_find_var_def(code, line, col + 2)
            .expect("foreach value declaration should be its own definition");
        assert_eq!(def.0, line);
        assert_eq!(def.1, col);
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
