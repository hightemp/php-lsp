//! Shared CST helpers for parser-side analyses.

use tree_sitter::Node;

fn previous_token(mut node: Node<'_>) -> Option<Node<'_>> {
    loop {
        node = node.prev_sibling()?;
        if !node.is_extra() {
            return Some(node);
        }
    }
}

fn next_token(mut node: Node<'_>) -> Option<Node<'_>> {
    loop {
        node = node.next_sibling()?;
        if !node.is_extra() {
            return Some(node);
        }
    }
}

/// Braced member names evaluate an expression rather than naming a static member.
pub(crate) fn is_dynamic_member_name(node: Node<'_>) -> bool {
    previous_token(node).is_some_and(|n| n.kind() == "{")
        && next_token(node).is_some_and(|n| n.kind() == "}")
        && node.parent().is_some_and(|parent| {
            matches!(
                parent.kind(),
                "member_access_expression"
                    | "nullsafe_member_access_expression"
                    | "member_call_expression"
                    | "nullsafe_member_call_expression"
                    | "scoped_call_expression"
                    | "class_constant_access_expression"
            )
        })
}

pub(crate) fn class_constant_parts(node: Node<'_>) -> Option<(Node<'_>, Node<'_>)> {
    let mut cursor = node.walk();
    let mut children = node.named_children(&mut cursor).filter(|n| !n.is_extra());
    Some((children.next()?, children.next()?))
}

/// Only identifiers in proven expression slots are global constant reads.
/// Unknown/declaration/type/member-label roles deliberately fail closed.
pub(crate) fn is_constant_reference(node: Node<'_>) -> bool {
    if node.is_error()
        || node.is_missing()
        || node.has_error()
        || !(node.kind() == "qualified_name"
            || (node.kind() == "name" && node.named_child_count() == 0))
    {
        return false;
    }
    let Some(parent) = node.parent().filter(|p| !p.is_error()) else {
        return false;
    };
    let field = |name| {
        parent
            .child_by_field_name(name)
            .is_some_and(|n| n.id() == node.id())
    };
    match parent.kind() {
        // The grammar wraps dynamic class-constant expressions in an aliased name.
        "name" => {
            is_dynamic_member_name(parent)
                && parent.start_byte() == node.start_byte()
                && parent.end_byte() == node.end_byte()
        }
        "assignment_expression"
        | "reference_assignment_expression"
        | "augmented_assignment_expression" => field("right"),
        "binary_expression" => {
            field("left")
                || (field("right")
                    && parent
                        .child_by_field_name("operator")
                        .is_none_or(|n| n.kind() != "instanceof"))
        }
        "argument" => !field("name"),
        "const_element" => previous_token(node).is_some_and(|n| n.kind() == "="),
        "enum_case" | "static_variable_declaration" | "cast_expression" | "case_statement" => {
            field("value")
        }
        "simple_parameter" | "property_promotion_parameter" | "property_element" => {
            field("default_value")
        }
        "arrow_function" | "property_hook" => field("body"),
        "conditional_expression" => field("condition") || field("body") || field("alternative"),
        "for_statement" => field("initialize") || field("condition") || field("update"),
        "foreach_statement" => {
            let mut cursor = parent.walk();
            let first = parent.named_children(&mut cursor).find(|n| !n.is_extra());
            first.is_some_and(|n| n.id() == node.id())
        }
        "match_conditional_expression" | "match_default_expression" => field("return_expression"),
        "unary_op_expression" => field("argument"),
        "member_access_expression"
        | "nullsafe_member_access_expression"
        | "member_call_expression"
        | "nullsafe_member_call_expression" => field("object") || is_dynamic_member_name(node),
        "scoped_call_expression" | "class_constant_access_expression" => {
            is_dynamic_member_name(node)
        }
        "dynamic_variable_name" => previous_token(node).is_some_and(|n| n.kind() == "{"),
        "list_literal" => next_token(node).is_some_and(|n| n.kind() == "=>"),
        "subscript_expression" => {
            // "$items[KEY]" uses a literal key; "{$items[KEY]}" evaluates KEY.
            !parent.parent().is_some_and(|outer| {
                matches!(
                    outer.kind(),
                    "encapsed_string" | "heredoc_body" | "shell_command_expression"
                ) && previous_token(parent).is_none_or(|n| n.kind() != "{")
            })
        }
        "expression_statement"
        | "return_statement"
        | "echo_statement"
        | "parenthesized_expression"
        | "array_element_initializer"
        | "sequence_expression"
        | "match_condition_list"
        | "throw_expression"
        | "yield_expression"
        | "include_expression"
        | "include_once_expression"
        | "require_expression"
        | "require_once_expression"
        | "print_intrinsic"
        | "exit_statement"
        | "clone_expression"
        | "error_suppression_expression"
        | "variadic_unpacking"
        | "break_statement"
        | "continue_statement" => true,
        _ => false,
    }
}

/// Promote cursor positions within a qualified name to its single expression.
pub(crate) fn constant_name_at(mut node: Node<'_>) -> Option<Node<'_>> {
    while let Some(parent) = node.parent().filter(|p| {
        matches!(
            p.kind(),
            "qualified_name" | "namespace_name" | "namespace_name_as_prefix"
        )
    }) {
        node = parent;
    }
    is_constant_reference(node).then_some(node)
}

pub(crate) fn is_foreach_header_declared_variable(node: Node, source: &str) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == "foreach_statement" {
            let foreach_text = &source[parent.byte_range()];
            let node_start = node.start_byte().saturating_sub(parent.start_byte());
            let header_end = foreach_text
                .find('{')
                .or_else(|| foreach_text.find(':'))
                .unwrap_or(foreach_text.len());

            return find_keyword(foreach_text, "as")
                .is_some_and(|as_pos| node_start > as_pos + "as".len() && node_start < header_end);
        }
        current = parent.parent();
    }
    false
}

fn find_keyword(text: &str, keyword: &str) -> Option<usize> {
    text.match_indices(keyword).find_map(|(index, _)| {
        let before = text[..index].chars().next_back();
        let after = text[index + keyword.len()..].chars().next();
        let before_boundary = before.is_none_or(|c| !is_identifier_char(c));
        let after_boundary = after.is_none_or(|c| !is_identifier_char(c));
        (before_boundary && after_boundary).then_some(index)
    })
}

fn is_identifier_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

pub(crate) fn ancestor_field_contains(node: Node, ancestor_kind: &str, fields: &[&str]) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == ancestor_kind {
            return fields.iter().any(|field| {
                parent.child_by_field_name(field).is_some_and(|field_node| {
                    field_node.id() == node.id() || node_contains(field_node, node)
                })
            });
        }
        current = parent.parent();
    }
    false
}

pub(crate) fn node_contains(parent: Node, child: Node) -> bool {
    parent.start_byte() <= child.start_byte() && parent.end_byte() >= child.end_byte()
}

pub(crate) fn has_ancestor_before_scope(node: Node, ancestor_kind: &str) -> bool {
    ancestor_before_scope(node, ancestor_kind).is_some()
}

pub(crate) fn is_by_ref_output_argument_variable(node: Node, source: &str) -> bool {
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

pub(crate) fn ancestor_before_scope<'tree>(
    node: Node<'tree>,
    ancestor_kind: &str,
) -> Option<Node<'tree>> {
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

pub(crate) fn argument_index(arguments: Node, argument: Node) -> Option<usize> {
    let mut cursor = arguments.walk();
    let index = arguments
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "argument")
        .position(|child| child.id() == argument.id());
    index
}

pub(crate) fn argument_name(argument: Node, source: &str) -> Option<String> {
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

#[cfg(test)]
#[path = "cst_tests.rs"]
mod tests;
