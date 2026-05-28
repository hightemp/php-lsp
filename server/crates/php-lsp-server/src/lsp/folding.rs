//! Folding LSP handlers extracted from `server.rs`.

use super::super::*;
use std::collections::HashSet;

fn is_folding_declaration_node(kind: &str) -> bool {
    matches!(
        kind,
        "namespace_definition"
            | "class_declaration"
            | "interface_declaration"
            | "trait_declaration"
            | "enum_declaration"
            | "function_definition"
            | "method_declaration"
            | "anonymous_function_creation_expression"
    )
}

fn is_declaration_parent_for_block(node: tree_sitter::Node) -> bool {
    if node.kind() != "compound_statement" {
        return false;
    }

    node.parent()
        .is_some_and(|parent| is_folding_declaration_node(parent.kind()))
}

fn folding_range_for_node(node: tree_sitter::Node, source: &str) -> Option<FoldingRange> {
    let kind = match node.kind() {
        "comment" => {
            let text = node_text(source, node).trim_start();
            if !text.starts_with("/**") {
                return None;
            }
            Some(FoldingRangeKind::Comment)
        }
        "array_creation_expression" => Some(FoldingRangeKind::Region),
        "compound_statement" if !is_declaration_parent_for_block(node) => {
            Some(FoldingRangeKind::Region)
        }
        kind if is_folding_declaration_node(kind) => None,
        _ => return None,
    };

    let start = node.start_position();
    let end = node.end_position();
    let start_line = start.row as u32;
    let end_line = end.row as u32;
    if end_line <= start_line {
        return None;
    }

    Some(FoldingRange {
        start_line,
        start_character: Some(start.column as u32),
        end_line,
        end_character: Some(end.column as u32),
        kind,
        collapsed_text: None,
    })
}

fn collect_folding_ranges(
    node: tree_sitter::Node,
    source: &str,
    ranges: &mut Vec<FoldingRange>,
    seen: &mut HashSet<(u32, Option<u32>, u32, Option<u32>)>,
) {
    if let Some(range) = folding_range_for_node(node, source) {
        let key = (
            range.start_line,
            range.start_character,
            range.end_line,
            range.end_character,
        );
        if seen.insert(key) {
            ranges.push(range);
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_folding_ranges(child, source, ranges, seen);
    }
}

fn folding_ranges(tree: &tree_sitter::Tree, source: &str) -> Vec<FoldingRange> {
    let mut ranges = Vec::new();
    let mut seen = HashSet::new();
    collect_folding_ranges(tree.root_node(), source, &mut ranges, &mut seen);
    ranges.sort_by_key(|range| {
        (
            range.start_line,
            range.start_character.unwrap_or_default(),
            range.end_line,
            range.end_character.unwrap_or_default(),
        )
    });
    ranges
}

impl PhpLspBackend {
    pub(crate) async fn lsp_folding_range(
        &self,
        params: FoldingRangeParams,
    ) -> Result<Option<Vec<FoldingRange>>> {
        let uri_str = params.text_document.uri.as_str().to_string();

        let ranges = if let Some(parser) = self.open_files.get(&uri_str) {
            let Some(tree) = parser.tree() else {
                return Ok(None);
            };
            folding_ranges(tree, &parser.source())
        } else {
            let Some(path) = uri_to_path(&uri_str) else {
                return Ok(None);
            };
            let Ok(source) = read_file_to_string_blocking(path, "foldingRange source read").await
            else {
                return Ok(None);
            };
            let mut parser = FileParser::new();
            parser.parse_full(&source);
            let Some(tree) = parser.tree() else {
                return Ok(None);
            };
            folding_ranges(tree, &source)
        };

        if ranges.is_empty() {
            Ok(None)
        } else {
            Ok(Some(ranges))
        }
    }
}
