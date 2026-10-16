//! Extract diagnostics (syntax errors) from tree-sitter CST.

use crate::utf16::Utf16LineIndex;
use lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range};
use tree_sitter::Node;

pub const SYNTAX_ERROR_CODE: &str = "syntax-error";

/// A syntax diagnostic whose range has already been converted to LSP UTF-16.
pub struct Utf16SyntaxDiagnostic(Diagnostic);

impl Utf16SyntaxDiagnostic {
    pub fn into_lsp_diagnostic(self) -> Diagnostic {
        self.0
    }
}

/// Extract syntax diagnostics in the LSP UTF-16 coordinate domain.
pub fn extract_syntax_errors(tree: &tree_sitter::Tree, source: &str) -> Vec<Diagnostic> {
    extract_syntax_errors_utf16(tree, source)
        .into_iter()
        .map(Utf16SyntaxDiagnostic::into_lsp_diagnostic)
        .collect()
}

/// Typed API for consumers that also handle byte-backed semantic diagnostics.
pub fn extract_syntax_errors_utf16(
    tree: &tree_sitter::Tree,
    source: &str,
) -> Vec<Utf16SyntaxDiagnostic> {
    extract_syntax_errors_with_positions(tree, &Utf16LineIndex::new(source))
}

/// Reuse the coordinate index belonging to the same immutable source snapshot.
pub fn extract_syntax_errors_with_positions(
    tree: &tree_sitter::Tree,
    positions: &Utf16LineIndex,
) -> Vec<Utf16SyntaxDiagnostic> {
    let mut diagnostics = Vec::new();
    collect_errors(tree.root_node(), positions, &mut diagnostics);
    diagnostics
}

fn collect_errors(
    node: Node,
    utf16_index: &Utf16LineIndex,
    diagnostics: &mut Vec<Utf16SyntaxDiagnostic>,
) {
    if let Some(message) = tree_sitter_error_message(node) {
        let start = node.start_position();
        let end = node.end_position();
        let (start_line, start_column) =
            utf16_index.point_to_position(start.row as u32, start.column as u32);
        let (end_line, end_column) =
            utf16_index.point_to_position(end.row as u32, end.column as u32);
        diagnostics.push(Utf16SyntaxDiagnostic(Diagnostic {
            range: Range {
                start: Position::new(start_line, start_column),
                end: Position::new(end_line, end_column),
            },
            severity: Some(DiagnosticSeverity::ERROR),
            code: Some(NumberOrString::String(SYNTAX_ERROR_CODE.into())),
            source: Some("php-lsp".to_string()),
            message,
            ..Default::default()
        }));
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
#[path = "diagnostics_tests.rs"]
mod tests;
