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
