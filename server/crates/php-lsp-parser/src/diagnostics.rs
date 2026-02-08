//! Extract diagnostics (syntax errors) from tree-sitter CST.

use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};
use tree_sitter::Node;

/// Extract syntax error diagnostics from a tree-sitter tree.
pub fn extract_syntax_errors(tree: &tree_sitter::Tree, _source: &str) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    collect_errors(tree.root_node(), &mut diagnostics);
    diagnostics
}

fn collect_errors(node: Node, diagnostics: &mut Vec<Diagnostic>) {
    if node.is_error() {
        let start = node.start_position();
        let end = node.end_position();
        diagnostics.push(Diagnostic {
            range: Range {
                start: Position::new(start.row as u32, start.column as u32),
                end: Position::new(end.row as u32, end.column as u32),
            },
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("php-lsp".to_string()),
            message: "Syntax error".to_string(),
            ..Default::default()
        });
    } else if node.is_missing() {
        let start = node.start_position();
        let end = node.end_position();
        let kind = node.kind();
        diagnostics.push(Diagnostic {
            range: Range {
                start: Position::new(start.row as u32, start.column as u32),
                end: Position::new(end.row as u32, end.column as u32),
            },
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("php-lsp".to_string()),
            message: format!("Missing {}", kind),
            ..Default::default()
        });
    }

    // Recurse into children
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_errors(child, diagnostics);
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
}
