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
