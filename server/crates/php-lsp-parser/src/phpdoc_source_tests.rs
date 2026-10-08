use super::*;

fn symbols(source: &str) -> FileSymbols {
    let mut parser = crate::parser::FileParser::new();
    parser.parse_full(source);
    extract_file_symbols(parser.tree().unwrap(), source, "file:///phpdoc-source.php")
}

#[test]
fn phpdoc_definition_extracted_property_does_not_select_longer_name() {
    let source = "<?php\n/**\n * @property string $slugLong mentions $slug\n * @property string $slug actual\n */\nclass Owned {}";
    let symbols = symbols(source);
    let property = symbols
        .symbols
        .iter()
        .find(|symbol| symbol.name == "slug")
        .unwrap();
    assert_eq!(property.selection_range.0, 3);
}

#[test]
fn phpdoc_definition_extracted_method_does_not_select_suffix_or_description() {
    let source = "<?php\n/**\n * @method string longFetch() mentions fetch()\n * @method string fetch () actual\n */\nclass Owned {}";
    let symbols = symbols(source);
    let method = symbols
        .symbols
        .iter()
        .find(|symbol| symbol.name == "fetch")
        .unwrap();
    assert_eq!(method.selection_range.0, 3);
}

#[test]
fn phpdoc_definition_attached_ranges_survive_attributes_unicode_and_crlf() {
    let source = "<?php\r\n/* 😀 */ /** @property string $slug */\r\n#[Marker]\r\nclass Owned {\r\n    /** @return array{key: int} */\r\n    #[Marker]\r\n    public function rows() {}\r\n    /** @var string */\r\n    public $value;\r\n}\r\n/** @return string */\r\n#[Marker]\r\nfunction read() {}";
    let symbols = symbols(source);
    for symbol in &symbols.symbols {
        if let Some(comment) = &symbol.doc_comment {
            let offset = source.find(comment).unwrap();
            let start = byte_offset_to_point(source, offset);
            let end = byte_offset_to_point(source, offset + comment.len());
            let expected =
                crate::utf16::range_byte_to_utf16(source, (start.0, start.1, end.0, end.1));
            assert_eq!(symbol.doc_comment_range, Some(expected), "{}", symbol.fqn);
        }
    }
    assert_eq!(
        symbols
            .symbols
            .iter()
            .filter(|symbol| symbol.doc_comment_range.is_some())
            .count(),
        5
    );
}

#[test]
fn phpdoc_definition_member_spans_share_the_multiline_tag_grammar() {
    for (comment, name, kind, declaration) in [
        ("/** mentions @property string $slug\n * @property-read array{nested: string}\n * $slug actual\n */", "slug", PhpSymbolKind::Property, "$slug actual"),
        ("/**\r\n * @method\r\n * static\tcallable(int): string\r\n * Fetch (int $fetch) mentions fetch()\r\n */", "fetch", PhpSymbolKind::Method, "Fetch ("),
        ("/**\n * @method string other() fetch()\n * @method string fetch()\n */", "fetch", PhpSymbolKind::Method, "fetch()\n"),
    ] {
        let span = crate::phpdoc::phpdoc_member_name_span(comment, name, kind).unwrap();
        let expected = comment.rfind(declaration).unwrap() + usize::from(kind == PhpSymbolKind::Property);
        assert_eq!(span, (expected, expected + name.len()), "{comment}");
    }
    assert!(crate::phpdoc::phpdoc_member_name_span(
        "/** @propertyish string $slug */",
        "slug",
        PhpSymbolKind::Property
    )
    .is_none());
}

#[test]
fn phpdoc_definition_comment_barrier_prevents_attachment_through_attributes() {
    for source in [
        "<?php /** @property string $slug */ #[Marker] /* barrier */ class Owned {}",
        "<?php /** @property string $slug */ class Foreign {} #[Marker] class Owned {}",
    ] {
        let symbols = symbols(source);
        let owner = symbols
            .symbols
            .iter()
            .find(|symbol| symbol.name == "Owned")
            .unwrap();
        assert!(owner.doc_comment.is_none(), "{source}");
        assert!(owner.doc_comment_range.is_none());
        assert!(!symbols
            .symbols
            .iter()
            .any(|symbol| symbol.fqn == "Owned::$slug"));
    }
}
