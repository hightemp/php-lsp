//! Helpers for return-type code actions.

use php_lsp_types::TypeInfo;
use tree_sitter::{Node, Tree};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingReturnTypeCandidate {
    pub name: String,
    pub declaration_range: (u32, u32, u32, u32),
    pub insert_position: (u32, u32),
    pub return_type: TypeInfo,
}

pub fn find_missing_return_type_candidates(
    tree: &Tree,
    source: &str,
    range: (u32, u32, u32, u32),
) -> Vec<MissingReturnTypeCandidate> {
    let mut candidates = Vec::new();
    walk_for_missing_return_types(tree.root_node(), source, range, &mut candidates);
    candidates
}

fn walk_for_missing_return_types(
    node: Node,
    source: &str,
    range: (u32, u32, u32, u32),
    candidates: &mut Vec<MissingReturnTypeCandidate>,
) {
    if matches!(node.kind(), "function_definition" | "method_declaration") {
        if let Some(candidate) = missing_return_type_candidate(node, source, range) {
            candidates.push(candidate);
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_for_missing_return_types(child, source, range, candidates);
    }
}

fn missing_return_type_candidate(
    node: Node,
    source: &str,
    range: (u32, u32, u32, u32),
) -> Option<MissingReturnTypeCandidate> {
    if node.child_by_field_name("return_type").is_some() {
        return None;
    }

    let name_node = node.child_by_field_name("name")?;
    let name = node_text(name_node, source).to_string();
    if matches!(name.as_str(), "__construct" | "__destruct") {
        return None;
    }

    let doc_node = find_doc_comment_node(node, source)?;
    let action_range = union_ranges(node_range(doc_node), node_range(node));
    if !ranges_overlap(action_range, range) {
        return None;
    }

    let phpdoc = crate::phpdoc::parse_phpdoc(node_text(doc_node, source));
    let return_type = phpdoc.return_type?;
    let parameters = node.child_by_field_name("parameters")?;
    let end = parameters.end_position();

    Some(MissingReturnTypeCandidate {
        name,
        declaration_range: action_range,
        insert_position: (end.row as u32, end.column as u32),
        return_type,
    })
}

fn find_doc_comment_node<'tree>(node: Node<'tree>, source: &str) -> Option<Node<'tree>> {
    let mut prev = node.prev_sibling();
    while let Some(p) = prev {
        if p.kind() == "comment" {
            let text = node_text(p, source);
            if text.starts_with("/**") {
                return Some(p);
            }
            return None;
        }
        prev = p.prev_sibling();
    }
    None
}

fn node_text<'a>(node: Node, source: &'a str) -> &'a str {
    &source[node.byte_range()]
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

fn union_ranges(left: (u32, u32, u32, u32), right: (u32, u32, u32, u32)) -> (u32, u32, u32, u32) {
    let start = if (left.0, left.1) <= (right.0, right.1) {
        (left.0, left.1)
    } else {
        (right.0, right.1)
    };
    let end = if (left.2, left.3) >= (right.2, right.3) {
        (left.2, left.3)
    } else {
        (right.2, right.3)
    };
    (start.0, start.1, end.0, end.1)
}

fn ranges_overlap(left: (u32, u32, u32, u32), right: (u32, u32, u32, u32)) -> bool {
    (left.0, left.1) <= (right.2, right.3) && (right.0, right.1) <= (left.2, left.3)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::FileParser;

    fn parse_candidates(
        source: &str,
        range: (u32, u32, u32, u32),
    ) -> Vec<MissingReturnTypeCandidate> {
        let mut parser = FileParser::new();
        parser.parse_full(source);
        find_missing_return_type_candidates(parser.tree().unwrap(), source, range)
    }

    #[test]
    fn finds_function_and_method_return_type_insertions() {
        let source = r#"<?php
/**
 * @return string|null
 */
function label($value) { return $value; }

class Demo {
    /**
     * @return static
     */
    public function fluent() { return $this; }
}
"#;

        let candidates = parse_candidates(source, (0, 0, 12, 0));
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].name, "label");
        assert_eq!(candidates[0].insert_position, (4, 22));
        assert_eq!(candidates[0].return_type.to_string(), "string|null");
        assert_eq!(candidates[1].name, "fluent");
        assert_eq!(candidates[1].return_type.to_string(), "static");
    }

    #[test]
    fn skips_native_return_types_and_constructors() {
        let source = r#"<?php
class Demo {
    /** @return int */
    public function already(): int { return 1; }

    /** @return string */
    public function __construct() {}
}
"#;

        let candidates = parse_candidates(source, (0, 0, 8, 0));
        assert!(candidates.is_empty());
    }
}
