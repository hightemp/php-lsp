//! Extract diagnostics (syntax errors) from tree-sitter CST.

use crate::utf16::Utf16LineIndex;
use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};
use tree_sitter::Node;

/// Extract syntax error diagnostics from a tree-sitter tree.
pub fn extract_syntax_errors(tree: &tree_sitter::Tree, source: &str) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let utf16_index = Utf16LineIndex::new(source);
    collect_errors(tree.root_node(), &utf16_index, &mut diagnostics);
    diagnostics
}

fn collect_errors(node: Node, utf16_index: &Utf16LineIndex, diagnostics: &mut Vec<Diagnostic>) {
    if let Some(message) = tree_sitter_error_message(node) {
        let start = node.start_position();
        let end = node.end_position();
        diagnostics.push(Diagnostic {
            range: Range {
                start: Position::new(
                    start.row as u32,
                    utf16_index.byte_col_to_utf16(start.row as u32, start.column as u32),
                ),
                end: Position::new(
                    end.row as u32,
                    utf16_index.byte_col_to_utf16(end.row as u32, end.column as u32),
                ),
            },
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("php-lsp".to_string()),
            message,
            ..Default::default()
        });
    }

    // Recurse into children
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_errors(child, utf16_index, diagnostics);
    }
}

fn tree_sitter_error_message(node: Node) -> Option<String> {
    if node.is_error() {
        Some("Syntax error".to_string())
    } else if node.is_missing() {
        Some(format!("Missing {}", node.kind()))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::FileParser;

    #[test]
    fn test_no_errors_on_valid_php() {
        let mut parser = FileParser::new();
        parser.parse_full("<?php\nclass Foo {\n    public function bar(): void {}\n}\n");

        let tree = parser.tree().unwrap();
        let diags = extract_syntax_errors(tree, &parser.source());
        assert!(diags.is_empty());
    }

    #[test]
    fn test_errors_on_invalid_php() {
        let mut parser = FileParser::new();
        parser.parse_full("<?php\nfunction foo( {\n}\n");

        let tree = parser.tree().unwrap();
        let diags = extract_syntax_errors(tree, &parser.source());
        assert!(!diags.is_empty());
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diags[0].source.as_deref(), Some("php-lsp"));
    }

    #[test]
    fn test_dangling_member_access_is_tree_sitter_syntax_error() {
        let mut parser = FileParser::new();
        parser.parse_full("<?php\nfunction demo(object $item): void {\n    $item->\n}\n");

        let tree = parser.tree().unwrap();
        let diags = extract_syntax_errors(tree, &parser.source());
        assert!(
            diags
                .iter()
                .any(|diag| diag.message == "Syntax error" || diag.message.starts_with("Missing ")),
            "dangling member access should be reported from tree-sitter errors: {diags:?}"
        );
    }

    #[test]
    fn test_multiple_errors() {
        let mut parser = FileParser::new();
        parser.parse_full("<?php\nclass { }\nfunction ( {}\n");

        let tree = parser.tree().unwrap();
        let diags = extract_syntax_errors(tree, &parser.source());
        assert!(
            diags.len() >= 2,
            "Expected multiple errors, got {}",
            diags.len()
        );
    }

    #[test]
    fn test_error_ranges_use_utf16_after_emoji_comment() {
        let mut parser = FileParser::new();
        parser.parse_full("<?php\n// 😀😀😀\nfunction foo( {\n}\n");

        let tree = parser.tree().unwrap();
        let diags = extract_syntax_errors(tree, &parser.source());
        let diag = diags.first().expect("expected syntax diagnostic");

        assert_eq!(diag.range.start.line, 2);
        assert!(
            diag.range.start.character <= 14,
            "diagnostic range should use UTF-16 columns, got {:?}",
            diag.range
        );
    }
}
