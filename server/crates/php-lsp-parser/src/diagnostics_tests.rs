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

#[test]
fn typed_syntax_diagnostics_keep_utf16_ranges_and_explicit_kind() {
    for source in [
        "<?php /* 😀 é 中文 */ function broken( {}",
        "<?php\r\n/* 😀é */ function broken( {}\r\n",
        "<?php class { } function ( {}",
    ] {
        let mut parser = FileParser::new();
        parser.parse_full(source);
        let typed = extract_syntax_errors_utf16(parser.tree().unwrap(), source)
            .into_iter()
            .map(Utf16SyntaxDiagnostic::into_lsp_diagnostic)
            .collect::<Vec<_>>();
        assert!(!typed.is_empty());
        assert_eq!(typed, extract_syntax_errors(parser.tree().unwrap(), source));
        assert!(typed
            .iter()
            .all(|diagnostic| diagnostic.code
                == Some(NumberOrString::String(SYNTAX_ERROR_CODE.into()))));
    }
}

#[test]
fn syntax_error_spans_match_cst_offsets_after_non_ascii_and_crlf() {
    fn position(source: &str, offset: usize) -> Position {
        let prefix = &source[..offset];
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
        let start = prefix.rfind('\n').map_or(0, |newline| newline + 1);
        Position::new(line, prefix[start..].encode_utf16().count() as u32)
    }
    fn spans(node: tree_sitter::Node, source: &str, out: &mut Vec<Range>) {
        if node.is_error() || node.is_missing() {
            out.push(Range::new(
                position(source, node.start_byte()),
                position(source, node.end_byte()),
            ));
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            spans(child, source, out);
        }
    }
    for source in [
        "<?php /* 😀é中文 */ function broken( {}",
        "<?php\r\n/* 😀😀 é */ class { } function ( {}\r\n",
    ] {
        let mut parser = FileParser::new();
        parser.parse_full(source);
        let mut expected = Vec::new();
        spans(parser.tree().unwrap().root_node(), source, &mut expected);
        assert!(!expected.is_empty());
        let actual = extract_syntax_errors(parser.tree().unwrap(), source);
        assert_eq!(
            actual
                .into_iter()
                .map(|diagnostic| diagnostic.range)
                .collect::<Vec<_>>(),
            expected
        );
    }
}
