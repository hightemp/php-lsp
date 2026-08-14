//! Shared CST helpers for parser-side analyses.

use tree_sitter::Node;

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
