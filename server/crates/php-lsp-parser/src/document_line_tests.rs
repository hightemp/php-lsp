use super::*;

// Independent client oracle: only LF, CRLF and bare CR advance an LSP line.
fn client_position(source: &str, offset: usize) -> (u32, u32) {
    let mut i = 0;
    let mut row = 0;
    let mut start = 0;
    while i < offset {
        match source.as_bytes()[i] {
            b'\r' => {
                i += 1;
                if source.as_bytes().get(i) == Some(&b'\n') {
                    i += 1;
                }
                row += 1;
                start = i;
            }
            b'\n' => {
                i += 1;
                row += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    (row, source[start..offset].encode_utf16().count() as u32)
}
fn tree_facts(parser: &FileParser) -> Vec<(String, usize, usize, Point, Point)> {
    let mut facts = Vec::new();
    let mut cursor = parser.tree().unwrap().walk();
    loop {
        let n = cursor.node();
        facts.push((
            n.kind().into(),
            n.start_byte(),
            n.end_byte(),
            n.start_position(),
            n.end_position(),
        ));
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return facts;
            }
        }
    }
}
fn assert_fresh(parser: &FileParser, expected: &str) {
    assert_eq!(
        parser.source(),
        expected,
        "server source diverged from client"
    );
    let mut fresh = FileParser::new();
    fresh.parse_full(expected);
    assert_eq!(
        tree_facts(parser),
        tree_facts(&fresh),
        "incremental CST points/spans diverged from fresh parse"
    );
}
fn replace(parser: &mut FileParser, expected: &mut String, needle: &str, replacement: &str) {
    let start = expected.find(needle).unwrap();
    let end = start + needle.len();
    let a = client_position(expected, start);
    let b = client_position(expected, end);
    parser.apply_edit(a.0, a.1, b.0, b.1, replacement).unwrap();
    expected.replace_range(start..end, replacement);
    assert_fresh(parser, expected);
}

#[test]
fn line_model_unicode_separators_do_not_redirect_incremental_edits() {
    for separator in ['\u{2028}', '\u{2029}', '\u{85}', '\u{b}', '\u{c}'] {
        for ending in ["\n", "\r\n", "\r"] {
            let mut source=format!("<?php{ending}// a{separator}b{ending}function foo() {{}}{ending}function tail() {{}}{ending}");
            let mut parser = FileParser::new();
            parser.parse_full(&source);
            replace(&mut parser, &mut source, "foo", "bar");
            replace(&mut parser, &mut source, "tail", "after");
        }
    }
}

#[test]
fn line_model_inserted_separators_and_mixed_endings_keep_exact_tree_points() {
    for inserted in [
        "\u{2028}",
        "\u{2029}",
        "\u{85}",
        "\u{b}",
        "\u{c}",
        "\r",
        "\n",
        "\r\n",
        "\r\n\r\n\r",
    ] {
        let mut source = "<?php\n/* 😀 x */\rfunction foo() {}\r\nfunction tail() {}\n".to_string();
        let mut parser = FileParser::new();
        parser.parse_full(&source);
        replace(&mut parser, &mut source, "x", inserted);
        replace(&mut parser, &mut source, "foo", "bar");
        replace(&mut parser, &mut source, "tail", "last");
    }
}

#[test]
fn line_model_crlf_creation_and_whole_break_replacement_match_fresh_parse() {
    let mut source = "<?php\rfunction foo() {}\r".to_string();
    let mut parser = FileParser::new();
    parser.parse_full(&source);
    // Insert LF after an existing CR: LSP line count stays unchanged, TS gains a row.
    parser.apply_edit(1, 0, 1, 0, "\n").unwrap();
    source.insert(6, '\n');
    assert_fresh(&parser, &source);
    replace(&mut parser, &mut source, "\r\n", "\r");
    replace(&mut parser, &mut source, "foo", "bar");
    parser
        .apply_edit(2, 0, 2, 0, "function tail() {}\r\n")
        .unwrap();
    source.push_str("function tail() {}\r\n");
    assert_fresh(&parser, &source);
}

#[test]
fn line_model_full_lsp_range_mapping_accounts_for_bare_cr_rows_and_utf16() {
    let source = "<?php\r/* 😀 */ function foo() {}\r\nfunction tail() {}\r";
    let mut parser = FileParser::new();
    parser.parse_full(source);
    for (kind, start, end, a, b) in tree_facts(&parser) {
        if kind != "name" {
            continue;
        }
        let expected_a = client_position(source, start);
        let expected_b = client_position(source, end);
        let actual = crate::utf16::range_byte_to_utf16(
            source,
            (a.row as u32, a.column as u32, b.row as u32, b.column as u32),
        );
        assert_eq!(
            actual,
            (expected_a.0, expected_a.1, expected_b.0, expected_b.1)
        );
    }
}

#[test]
fn line_model_phpdoc_var_name_keeps_its_binding_with_every_line_ending() {
    for ending in ["\n", "\r\n", "\r"] {
        for (tagged, expected) in [("$other", "Real"), ("$actual", "Wrong")] {
            let source = format!("<?php{ending}class Wrong {{}}{ending}class Real {{}}{ending}function run() {{{ending}/**{ending} * Description.{ending} * @var Wrong {tagged}{ending} */{ending}$actual = new Real();{ending}$actual;{ending}}}");
            let mut parser = FileParser::new();
            parser.parse_full(&source);
            let tree = parser.tree().unwrap();
            let symbols = crate::symbols::extract_file_symbols(tree, &source, "file:///var.php");
            let point = parser
                .line_index()
                .offset_to_point(source.rfind("$actual").unwrap());
            crate::line_index::counters::BUILDS.with(|c| c.set(0));
            let ty = crate::resolve::infer_variable_type_at_position(
                tree, &source, &symbols, point.0, point.1, "$actual",
            );
            assert_eq!(
                crate::line_index::counters::BUILDS.with(|c| c.get()),
                0,
                "CST-byte inference must not build a whole-document UTF-16 map"
            );
            assert_eq!(
                ty.as_deref(),
                Some(expected),
                "ending={ending:?}, tag={tagged}"
            );
        }
    }
}
