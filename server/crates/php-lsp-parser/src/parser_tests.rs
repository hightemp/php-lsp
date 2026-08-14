use super::*;

fn utf16_position_at(source: &str, needle: &str) -> (u32, u32) {
    let offset = source
        .find(needle)
        .unwrap_or_else(|| panic!("needle `{needle}` not found"));
    let prefix = &source[..offset];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let line_start = prefix.rfind('\n').map_or(0, |idx| idx + 1);
    let character = prefix[line_start..].encode_utf16().count() as u32;
    (line, character)
}

fn utf16_position_after(source: &str, needle: &str) -> (u32, u32) {
    let offset = source
        .find(needle)
        .unwrap_or_else(|| panic!("needle `{needle}` not found"))
        + needle.len();
    let prefix = &source[..offset];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let line_start = prefix.rfind('\n').map_or(0, |idx| idx + 1);
    let character = prefix[line_start..].encode_utf16().count() as u32;
    (line, character)
}

#[test]
fn test_parse_full_simple_class() {
    let mut parser = FileParser::new();
    parser.parse_full("<?php\nclass Foo {\n    public function bar(): void {}\n}\n");

    let tree = parser.tree().expect("Should have a tree");
    let root = tree.root_node();
    assert_eq!(root.kind(), "program");
    assert!(!root.has_error());
}

#[test]
fn test_parse_full_with_error() {
    let mut parser = FileParser::new();
    parser.parse_full("<?php\nfunction foo( {\n}\n");

    let tree = parser.tree().expect("Should have a tree");
    let root = tree.root_node();
    assert_eq!(root.kind(), "program");
    // Tree should have an error node but still parse
    assert!(root.has_error());
}

#[test]
fn test_incremental_edit() {
    let mut parser = FileParser::new();
    parser.parse_full("<?php\nclass Foo {}\n");

    // Change "Foo" to "Bar" (line 1, chars 6-9)
    parser
        .apply_edit(1, 6, 1, 9, "Bar")
        .expect("valid incremental edit");

    let source = parser.source();
    assert!(source.contains("class Bar {}"));

    let tree = parser.tree().expect("Should have a tree after edit");
    assert!(!tree.root_node().has_error());
}

#[test]
fn test_incremental_edit_after_emoji_uses_utf16_positions() {
    let mut parser = FileParser::new();
    parser.parse_full("<?php\n$emoji = \"😀\"; $name = 1;\n");

    parser
        .apply_edit(1, 15, 1, 20, "$value")
        .expect("valid incremental edit after emoji");

    let source = parser.source();
    assert!(source.contains("$emoji = \"😀\"; $value = 1;"));

    let tree = parser.tree().expect("Should have a tree after edit");
    assert!(!tree.root_node().has_error());
}

#[test]
fn test_incremental_edit_after_complex_emoji_uses_utf16_positions() {
    let mut parser = FileParser::new();
    let source = "<?php\n$emoji = \"🇺🇸 👨‍👩‍👧‍👦 👍🏽 ❤️ e\u{0301}\"; $name = 1;\n";
    parser.parse_full(source);

    let start = utf16_position_at(source, "$name");
    let end = utf16_position_after(source, "$name");
    parser
        .apply_edit(start.0, start.1, end.0, end.1, "$value")
        .expect("valid incremental edit after complex emoji");

    let source = parser.source();
    assert!(source.contains("\"🇺🇸 👨‍👩‍👧‍👦 👍🏽 ❤️ e\u{0301}\"; $value = 1;"));

    let tree = parser.tree().expect("Should have a tree after edit");
    assert!(!tree.root_node().has_error());
}

#[test]
fn test_incremental_delete_emoji_string_uses_utf16_positions() {
    let mut parser = FileParser::new();
    parser.parse_full("<?php\n$value = \"😀\";\n");

    parser
        .apply_edit(1, 9, 1, 13, "\"ok\"")
        .expect("valid emoji replacement");

    let source = parser.source();
    assert!(source.contains("$value = \"ok\";"));

    let tree = parser.tree().expect("Should have a tree after edit");
    assert!(!tree.root_node().has_error());
}

#[test]
fn test_incremental_collapsed_range_inserts_text() {
    let mut parser = FileParser::new();
    parser.parse_full("<?php\n$value = 1;\n");

    parser
        .apply_edit(1, 0, 1, 0, "$prefix = 0;\n")
        .expect("collapsed incremental range should be a valid insertion");

    assert_eq!(parser.source(), "<?php\n$prefix = 0;\n$value = 1;\n");
    assert!(!parser
        .tree()
        .expect("Should have a tree after insertion")
        .root_node()
        .has_error());
}

#[test]
fn test_incremental_reversed_same_line_range_preserves_state() {
    let mut parser = FileParser::new();
    parser.parse_full("<?php\nclass Demo {}\n");
    let original_source = parser.source();
    let original_tree = parser
        .tree()
        .expect("Should have an original tree")
        .root_node()
        .to_sexp();

    let error = parser
        .apply_edit(1, 10, 1, 6, "Broken")
        .expect_err("reversed same-line range should be rejected");

    assert_eq!(
        error,
        ApplyEditError::ReversedRange {
            start: (1, 10),
            end: (1, 6),
        }
    );
    assert_eq!(parser.source(), original_source);
    assert_eq!(
        parser
            .tree()
            .expect("Tree should remain available")
            .root_node()
            .to_sexp(),
        original_tree
    );
}

#[test]
fn test_incremental_reversed_cross_line_range_preserves_state() {
    let mut parser = FileParser::new();
    parser.parse_full("<?php\nclass First {}\nclass Second {}\n");
    let original_source = parser.source();
    let original_tree = parser
        .tree()
        .expect("Should have an original tree")
        .root_node()
        .to_sexp();

    let error = parser
        .apply_edit(2, 5, 1, 5, "Broken")
        .expect_err("reversed cross-line range should be rejected");

    assert_eq!(
        error,
        ApplyEditError::ReversedRange {
            start: (2, 5),
            end: (1, 5),
        }
    );
    assert_eq!(parser.source(), original_source);
    assert_eq!(
        parser
            .tree()
            .expect("Tree should remain available")
            .root_node()
            .to_sexp(),
        original_tree
    );
}

#[test]
fn test_incremental_reversed_utf16_range_preserves_multibyte_text() {
    let mut parser = FileParser::new();
    let original_source = "<?php\n$emoji = \"😀\"; $name = 1;\n";
    parser.parse_full(original_source);
    let original_tree = parser
        .tree()
        .expect("Should have an original tree")
        .root_node()
        .to_sexp();

    let error = parser
        .apply_edit(1, 20, 1, 10, "$broken")
        .expect_err("reversed UTF-16 range should be rejected");

    assert_eq!(
        error,
        ApplyEditError::ReversedRange {
            start: (1, 20),
            end: (1, 10),
        }
    );
    assert_eq!(parser.source(), original_source);
    assert_eq!(
        parser
            .tree()
            .expect("Tree should remain available")
            .root_node()
            .to_sexp(),
        original_tree
    );
}

#[test]
fn test_parse_empty_php() {
    let mut parser = FileParser::new();
    parser.parse_full("<?php\n");

    let tree = parser.tree().expect("Should have a tree");
    assert!(!tree.root_node().has_error());
}

#[test]
fn test_parse_mixed_html_php() {
    let mut parser = FileParser::new();
    parser.parse_full("<html><body><?php echo 'hello'; ?></body></html>");

    let tree = parser.tree().expect("Should have a tree");
    assert_eq!(tree.root_node().kind(), "program");
    // Mixed PHP/HTML should parse without errors
    assert!(!tree.root_node().has_error());
}

#[test]
fn test_parse_self_param_and_static_return_type() {
    let mut parser = FileParser::new();
    parser.parse_full(
            "<?php\nclass Demo {\n    public function withSelf(self $arg): static\n    {\n        return $this;\n    }\n}\n",
        );

    let tree = parser.tree().expect("Should have a tree");
    assert!(
        !tree.root_node().has_error(),
        "Valid self/static type-hint syntax should parse without errors"
    );
}
