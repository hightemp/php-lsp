use super::*;

#[test]
fn p237_completion_converter_preserves_plain_markdown_and_plaintext_documentation() {
    for documentation in [
        lsp_types::Documentation::String("Plain docs 😀\r\nsecond line".into()),
        lsp_types::Documentation::MarkupContent(lsp_types::MarkupContent {
            kind: lsp_types::MarkupKind::Markdown,
            value: "**Markdown**\n```php\nexample();\n```".into(),
        }),
        lsp_types::Documentation::MarkupContent(lsp_types::MarkupContent {
            kind: lsp_types::MarkupKind::PlainText,
            value: "Literal **not bold**".into(),
        }),
    ] {
        let expected = serde_json::to_value(&documentation).unwrap();
        let item = lsp_types::CompletionItem {
            label: "example".into(),
            documentation: Some(documentation),
            data: Some(serde_json::json!({"kind":"framework-string-key","key":"example"})),
            ..Default::default()
        };
        let item = framework_string_key_completion_item_to_ls(item);
        assert_eq!(
            serde_json::to_value(item.documentation.unwrap()).unwrap(),
            expected
        );
    }
}
#[test]
fn p237_completion_converter_keeps_absent_documentation_absent() {
    let item = lsp_types::CompletionItem {
        label: "example".into(),
        ..Default::default()
    };
    assert!(framework_string_key_completion_item_to_ls(item)
        .documentation
        .is_none());
}
