//! Shared lexical variable scopes and capture-connected binding components.
use crate::cst::{ancestor_field_contains, is_by_ref_output_argument_variable, node_contains};
use tree_sitter::Node;

/// Static element path written by a destructuring target. Dynamic keys remain unknown.
pub(crate) fn destructuring_path(
    target: Node<'_>,
    variable: &str,
    source: &str,
) -> Option<Vec<Option<String>>> {
    if target.kind() == "variable_name" {
        return (normalize_var_name(&source[target.byte_range()]) == variable).then(Vec::new);
    }
    if target.kind() == "by_ref" {
        return target
            .named_child(0)
            .and_then(|child| destructuring_path(child, variable, source));
    }
    if target.kind() != "list_literal" {
        return None;
    }
    let mut segments = vec![Vec::new()];
    let mut cursor = target.walk();
    for child in target.children(&mut cursor) {
        if child.kind() == "," {
            segments.push(Vec::new());
        } else if !child.is_extra() {
            segments.last_mut().unwrap().push(child);
        }
    }
    for (index, segment) in segments.into_iter().enumerate() {
        let nodes = segment
            .iter()
            .copied()
            .filter(|node| node.is_named())
            .collect::<Vec<_>>();
        let Some(value) = nodes.last().copied() else {
            continue;
        };
        let Some(mut rest) = destructuring_path(value, variable, source) else {
            continue;
        };
        let key = if segment.iter().any(|node| node.kind() == "=>") {
            nodes
                .first()
                .filter(|node| matches!(node.kind(), "string" | "encapsed_string" | "integer"))
                .map(|node| php_lsp_types::normalize_shape_key_text(&source[node.byte_range()]))
        } else {
            Some(index.to_string())
        };
        rest.insert(0, key);
        return Some(rest);
    }
    None
}

/// Closest callable, class body, or program containing this node.
pub fn lexical_scope(mut node: Node<'_>) -> Node<'_> {
    loop {
        if is_variable_scope_node(node)
            || is_class_member_body_kind(node.kind())
            || node.kind() == "program"
        {
            return node;
        }
        let Some(parent) = node.parent() else {
            return node;
        };
        node = parent;
    }
}

/// Stop traversal into a nested callable or class body, but keep constructor arguments.
pub fn is_scope_boundary(node: Node<'_>) -> bool {
    is_variable_scope_node(node)
        || is_class_member_body_kind(node.kind())
        || matches!(
            node.kind(),
            "class_declaration"
                | "interface_declaration"
                | "trait_declaration"
                | "enum_declaration"
        )
}

pub(crate) fn captured_parent<'tree>(
    scope: Node<'tree>,
    source: &str,
    variable: &str,
) -> Option<(Node<'tree>, usize)> {
    if scope_parameter_declares(scope, source, variable) {
        return None;
    }
    let captures = match scope.kind() {
        "arrow_function" => true,
        "anonymous_function" | "anonymous_function_creation_expression" => {
            closure_use_clause_contains(scope, source, variable)
        }
        _ => false,
    };
    if !captures {
        return None;
    }
    let parent = parent_variable_scope(scope)?;
    Some((parent, scope.start_byte()))
}

/// Reference capture is an alias whose future value cannot be frozen at creation.
pub(crate) fn capture_is_by_reference(scope: Node<'_>, variable: &str, source: &str) -> bool {
    let mut cursor = scope.walk();
    for clause in scope
        .named_children(&mut cursor)
        .filter(|node| node.kind() == "anonymous_function_use_clause")
    {
        let mut pending = vec![clause];
        while let Some(node) = pending.pop() {
            if node.kind() == "variable_name"
                && normalize_var_name(&source[node.byte_range()]) == variable
            {
                let mut parent = node.parent();
                while let Some(node) = parent {
                    if node.id() == clause.id() {
                        break;
                    }
                    if matches!(node.kind(), "by_ref" | "reference_modifier") {
                        return true;
                    }
                    parent = node.parent();
                }
            }
            let mut cursor = node.walk();
            pending.extend(node.named_children(&mut cursor));
        }
    }
    false
}

pub(crate) fn variable_binding_root<'tree>(
    mut scope: Node<'tree>,
    root: Node<'tree>,
    source: &str,
    var_name: &str,
) -> Node<'tree> {
    while scope.id() != root.id() {
        if scope_parameter_declares(scope, source, var_name) {
            break;
        }
        let captures_parent = match scope.kind() {
            "arrow_function" => true,
            "anonymous_function" | "anonymous_function_creation_expression" => {
                closure_use_clause_contains(scope, source, var_name)
            }
            _ => false,
        };
        if !captures_parent {
            break;
        }
        let parent = parent_variable_scope(scope).unwrap_or(root);
        if parent.id() == scope.id() {
            break;
        }
        scope = parent;
    }
    scope
}

pub(crate) fn nested_scope_captures_binding(
    nested_scope: Node,
    current_scope: Node,
    source: &str,
    var_name: &str,
) -> bool {
    if scope_parameter_declares(nested_scope, source, var_name) {
        return false;
    }
    let captures_parent = match nested_scope.kind() {
        "arrow_function" => true,
        "anonymous_function" | "anonymous_function_creation_expression" => {
            closure_use_clause_contains(nested_scope, source, var_name)
        }
        _ => false,
    };
    captures_parent
        && parent_variable_scope(nested_scope)
            .is_some_and(|parent| parent.id() == current_scope.id())
}

pub(crate) fn scope_parameter_declares(scope: Node, source: &str, var_name: &str) -> bool {
    scope
        .child_by_field_name("parameters")
        .is_some_and(|parameters| node_contains_variable(parameters, source, var_name))
}

pub(crate) fn closure_use_clause_contains(scope: Node, source: &str, var_name: &str) -> bool {
    let mut cursor = scope.walk();
    let contains = scope.named_children(&mut cursor).any(|child| {
        child.kind() == "anonymous_function_use_clause"
            && node_contains_variable(child, source, var_name)
    });
    contains
}

pub(crate) fn node_contains_variable(node: Node, source: &str, var_name: &str) -> bool {
    if node.kind() == "variable_name" && normalize_var_name(&source[node.byte_range()]) == var_name
    {
        return true;
    }
    let mut cursor = node.walk();
    let contains = node
        .named_children(&mut cursor)
        .any(|child| node_contains_variable(child, source, var_name));
    contains
}

pub(crate) fn is_closure_capture_token(node: Node) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "anonymous_function_use_clause" => return true,
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

pub(crate) fn is_variable_declaration(node: Node, source: &str, var_name: &str) -> bool {
    if is_foreach_binding_variable(node)
        || is_by_ref_output_binding_variable(node, source)
        || is_assignment_binding_variable(node)
        || ancestor_field_contains(node, "catch_clause", &["name", "variable"])
    {
        return true;
    }

    let parent = match node.parent() {
        Some(p) => p,
        None => return false,
    };

    match parent.kind() {
        "simple_parameter" | "variadic_parameter" | "property_promotion_parameter" => parent
            .child_by_field_name("name")
            .map(|n| n.id() == node.id())
            .unwrap_or(false),
        "assignment_expression"
        | "by_ref_assignment_expression"
        | "reference_assignment_expression" => parent
            .child_by_field_name("left")
            .map(|n| normalize_var_name(&source[n.byte_range()]) == var_name)
            .unwrap_or(false),
        "catch_clause" => ["name", "variable"].iter().any(|field| {
            parent
                .child_by_field_name(field)
                .map(|n| n.id() == node.id())
                .unwrap_or(false)
        }),
        "global_declaration" | "static_variable_declaration" => true,
        _ => false,
    }
}

pub(crate) fn is_foreach_binding_variable(node: Node) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == "foreach_statement" {
            return foreach_binding_target(parent)
                .filter(|target| node_contains(*target, node))
                .is_some_and(|target| is_writable_target_variable(node, target));
        }
        if is_variable_scope_node(parent) || parent.kind() == "program" {
            return false;
        }
        current = parent.parent();
    }
    false
}

pub(crate) fn foreach_binding_target(statement: Node) -> Option<Node> {
    let mut after_as = false;
    let mut cursor = statement.walk();
    for child in statement.children(&mut cursor) {
        if !after_as {
            after_as = child.kind() == "as";
            continue;
        }
        if child.is_named() && !child.is_extra() && !child.is_error() && !child.is_missing() {
            return Some(child);
        }
    }
    None
}

pub(crate) fn is_assignment_binding_variable(node: Node) -> bool {
    assignment_binding_expression(node).is_some()
}

/// An enclosing assignment has not written its target while evaluating its RHS.
pub(crate) fn is_pending_assignment_binding(node: Node, before: usize) -> bool {
    assignment_binding_expression(node)
        .and_then(|assignment| assignment.child_by_field_name("right"))
        .is_some_and(|right| right.end_byte() > before)
}

fn assignment_binding_expression(node: Node<'_>) -> Option<Node<'_>> {
    let mut current = node.parent();
    while let Some(parent) = current {
        if matches!(
            parent.kind(),
            "assignment_expression"
                | "by_ref_assignment_expression"
                | "reference_assignment_expression"
        ) {
            return parent
                .child_by_field_name("left")
                .filter(|left| node_contains(*left, node))
                .filter(|left| is_writable_target_variable(node, *left))
                .map(|_| parent);
        }
        if is_variable_scope_node(parent) || parent.kind() == "program" {
            return None;
        }
        current = parent.parent();
    }
    None
}

pub(crate) fn is_by_ref_output_binding_variable(node: Node, source: &str) -> bool {
    if !is_by_ref_output_argument_variable(node, source) {
        return false;
    }
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == "argument" {
            return argument_value_node(parent)
                .filter(|value| node_contains(*value, node))
                .is_some_and(|value| is_writable_target_variable(node, value));
        }
        if is_variable_scope_node(parent) || parent.kind() == "program" {
            return false;
        }
        current = parent.parent();
    }
    false
}

pub(crate) fn argument_value_node(argument: Node) -> Option<Node> {
    let name_id = argument.child_by_field_name("name").map(|node| node.id());
    let reference_modifier_id = argument
        .child_by_field_name("reference_modifier")
        .map(|node| node.id());
    let mut cursor = argument.walk();
    let value = argument.named_children(&mut cursor).find(|child| {
        !child.is_extra()
            && !child.is_error()
            && !child.is_missing()
            && Some(child.id()) != name_id
            && Some(child.id()) != reference_modifier_id
    });
    value
}

pub(crate) fn is_writable_target_variable(node: Node, target: Node) -> bool {
    let mut branch = node;
    while branch.id() != target.id() {
        let Some(parent) = branch.parent() else {
            return false;
        };
        if !node_contains(target, parent) {
            return false;
        }
        match parent.kind() {
            "dynamic_variable_name" => return false,
            "member_access_expression"
            | "nullsafe_member_access_expression"
            | "scoped_property_access_expression" => return false,
            "subscript_expression" => {
                if parent
                    .child_by_field_name("index")
                    .is_some_and(|index| node_contains(index, node))
                {
                    return false;
                }
                let object = parent
                    .child_by_field_name("object")
                    .or_else(|| parent.named_child(0));
                if !object.is_some_and(|object| node_contains(object, node)) {
                    return false;
                }
            }
            "array_element_initializer" => {
                if !array_element_initializer_has_writable_variable(parent, node) {
                    return false;
                }
            }
            "list_literal" => {
                if !list_literal_has_writable_variable(parent, node) {
                    return false;
                }
            }
            _ => {}
        }
        branch = parent;
    }
    true
}

pub(crate) fn array_element_initializer_has_writable_variable(element: Node, node: Node) -> bool {
    let mut after_arrow = false;
    let mut cursor = element.walk();
    for child in element.children(&mut cursor) {
        if child.kind() == "=>" {
            after_arrow = true;
            continue;
        }
        if !after_arrow
            || !child.is_named()
            || child.is_extra()
            || child.is_error()
            || child.is_missing()
        {
            continue;
        }
        return node_contains(child, node);
    }
    !after_arrow
}

pub(crate) fn list_literal_has_writable_variable(list: Node, node: Node) -> bool {
    let mut segment_start = list.start_byte();
    let mut segment_end = list.end_byte();
    let mut cursor = list.walk();
    for child in list.children(&mut cursor) {
        if child.kind() != "," {
            continue;
        }
        if child.end_byte() <= node.start_byte() {
            segment_start = child.end_byte();
        } else if child.start_byte() >= node.end_byte() {
            segment_end = child.start_byte();
            break;
        }
    }

    let mut cursor = list.walk();
    let arrow = list.children(&mut cursor).find(|child| {
        child.kind() == "=>"
            && child.start_byte() >= segment_start
            && child.end_byte() <= segment_end
    });
    arrow.is_none_or(|arrow| node.start_byte() >= arrow.end_byte())
}

pub(crate) fn find_variable_scope(node: Node<'_>) -> Option<Node<'_>> {
    node.parent().map(lexical_scope)
}

pub(crate) fn parent_variable_scope(node: Node<'_>) -> Option<Node<'_>> {
    node.parent().map(lexical_scope)
}

pub(crate) fn is_variable_scope_node(node: Node) -> bool {
    is_variable_scope_kind(node.kind())
}

pub(crate) fn is_variable_scope_kind(kind: &str) -> bool {
    matches!(
        kind,
        "method_declaration"
            | "function_definition"
            | "arrow_function"
            | "anonymous_function"
            | "anonymous_function_creation_expression"
    )
}

pub(crate) fn is_class_member_body_kind(kind: &str) -> bool {
    matches!(
        kind,
        "declaration_list" | "enum_declaration_list" | "class_body"
    )
}

pub(crate) fn normalize_var_name(text: &str) -> String {
    if text.starts_with('$') {
        text.to_string()
    } else {
        format!("${}", text)
    }
}

pub(crate) fn node_range(node: Node) -> (u32, u32, u32, u32) {
    let start = node.start_position();
    let end = node.end_position();
    (
        start.row as u32,
        start.column as u32,
        end.row as u32,
        end.column as u32,
    )
}
