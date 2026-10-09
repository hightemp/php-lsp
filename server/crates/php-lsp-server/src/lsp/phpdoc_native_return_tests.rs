use super::*;

fn extracted_subject(source: &str) -> php_lsp_types::SymbolInfo {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    extract_file_symbols(parser.tree().unwrap(), source, "file:///native-return.php")
        .symbols
        .into_iter()
        .find(|symbol| symbol.name == "subject")
        .unwrap()
}

#[test]
fn incompatible_phpdoc_return_does_not_beat_native_by_specificity() {
    for (native, doc) in [
        ("int", "Wrong"),
        ("int", "array<int, Wrong>"),
        ("string", "array{name: string}"),
        ("bool", "Wrong|false"),
        ("void", "Wrong"),
        ("never", "Wrong"),
    ] {
        let source = format!(
            "<?php class Wrong {{}} /** @return {doc} */ function subject(): {native} {{}}"
        );
        let symbol = extracted_subject(&source);
        assert_eq!(
            symbol_effective_return_type(&symbol).unwrap().to_string(),
            native,
            "{doc} beat native {native}"
        );
    }
}

#[test]
fn compatible_return_refinements_and_fallbacks_are_preserved() {
    for (native, doc) in [
        ("array", "array<int, Wrong>"),
        ("array", "array{name: string}"),
        ("mixed", "Wrong"),
        ("bool", "true"),
        ("string", "class-string<Wrong>"),
    ] {
        let source = format!(
            "<?php class Wrong {{}} /** @return {doc} */ function subject(): {native} {{}}"
        );
        assert_eq!(
            symbol_effective_return_type(&extracted_subject(&source))
                .unwrap()
                .to_string(),
            doc
        );
    }
}
