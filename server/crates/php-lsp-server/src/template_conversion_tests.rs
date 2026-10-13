use super::*;
use php_lsp_parser::parser::FileParser;

fn assert_valid_virtual_php(doc: &TemplateDocument) {
    let mut parser = FileParser::new();
    parser.parse_full(doc.virtual_source());
    assert!(
        !parser.tree().unwrap().root_node().has_error(),
        "{}",
        doc.virtual_source()
    );
}

#[test]
fn p233_blade_escaped_echoes_are_unmapped_and_do_not_consume_adjacent_php() {
    for escaped in [
        "@{{ ClientOnly }}",
        "@@{{ ClientOnly }}",
        "@{!! ClientOnly !!}",
    ] {
        for newline in ["\n", "\r\n"] {
            let source = format!("😀 {escaped}{newline}{{{{ $server->name }}}}");
            let doc = preprocess_blade_template(&source);
            assert!(
                !doc.virtual_source().contains("ClientOnly"),
                "escaped PHP: {}",
                doc.virtual_source()
            );
            let offset = source.find("ClientOnly").unwrap();
            assert!(doc
                .map_original_position_to_virtual(position_for_byte_offset(&source, offset))
                .is_none());
            assert!(doc.virtual_source().contains("$server->name"));
            assert_valid_virtual_php(&doc);
        }
    }
}

#[test]
fn p233_twig_numbers_remain_complete_tokens() {
    for number in [
        "1.5",
        "-0.25",
        "1e3",
        "2.5E+2",
        "4.25e-3",
        "10_000.25",
        "1.25e1_0",
        "42",
    ] {
        let source = format!("{{{{ {number} }}}}");
        let doc = preprocess_twig_template(&source, &[]);
        assert_eq!(doc.virtual_source(), format!("<?php echo {number}; ?>\n"));
        assert_valid_virtual_php(&doc);
    }
}

#[test]
fn p233_twig_decimal_integers_are_not_reinterpreted_as_php_octal() {
    for (number, expected) in [("08", "8"), ("012", "12"), ("00_42", "42"), ("000", "0")] {
        let doc = preprocess_twig_template(&format!("{{{{ {number} }}}}"), &[]);
        assert_eq!(doc.virtual_source(), format!("<?php echo {expected}; ?>\n"));
        assert_valid_virtual_php(&doc);
    }
}

#[test]
fn p233_twig_numeric_arguments_and_member_dots_have_exact_ranges() {
    let source = "😀 {{ user.price(1.25, -2.5E+2) }}\r\n";
    let doc = preprocess_twig_template(source, &[]);
    assert!(
        doc.virtual_source().contains("$user->price(1.25, -2.5E+2)"),
        "{}",
        doc.virtual_source()
    );
    assert_valid_virtual_php(&doc);
    for needle in ["price", "1.25", "2.5E+2"] {
        let start = source.find(needle).unwrap();
        let end = start + needle.len();
        let mapped_start = doc
            .map_original_position_to_virtual(position_for_byte_offset(source, start))
            .unwrap();
        let mapped_end = doc
            .map_original_position_to_virtual(position_for_byte_offset(source, end))
            .unwrap();
        assert_eq!(
            doc.map_virtual_range_to_original_exact(Range::new(mapped_start, mapped_end)),
            Some(range_for_byte_offsets(source, start, end)),
            "{needle}"
        );
    }
}

#[test]
fn p233_twig_numbers_in_unsupported_expressions_do_not_create_phantom_variables() {
    let doc = preprocess_twig_template(
        "{{ 1e3|number_format }} {{ user.price(2.5E+2)|format }}",
        &[],
    );
    assert!(
        !doc.virtual_source().contains("$e3"),
        "{}",
        doc.virtual_source()
    );
    assert!(
        !doc.virtual_source().contains("$E"),
        "{}",
        doc.virtual_source()
    );
    assert!(
        doc.virtual_source().contains("$user->price(2.5E+2)"),
        "{}",
        doc.virtual_source()
    );
    assert_valid_virtual_php(&doc);
}

#[test]
fn p233_twig_key_value_for_maps_both_declarations() {
    for header in [
        "for key, value in items",
        "for key,value in items",
        "for key ,\n value in items",
        "for key, value in items|slice(0, 2)",
    ] {
        let source =
            format!("😀 {{%- {header} -%}}\r\n{{{{ key }}}} {{{{ value.name }}}} {{% endfor %}}");
        let doc = preprocess_twig_template(&source, &[]);
        assert!(
            doc.virtual_source()
                .contains("foreach ($items as $key => $value)"),
            "{}",
            doc.virtual_source()
        );
        assert_valid_virtual_php(&doc);
        for variable in ["key", "value"] {
            let start = source.find(variable).unwrap();
            let end = start + variable.len();
            let virtual_start = doc
                .map_original_position_to_virtual(position_for_byte_offset(&source, start))
                .unwrap();
            let virtual_end = doc
                .map_original_position_to_virtual(position_for_byte_offset(&source, end))
                .unwrap();
            assert_eq!(
                doc.map_virtual_range_to_original_exact(Range::new(virtual_start, virtual_end)),
                Some(range_for_byte_offsets(&source, start, end))
            );
        }
    }
}

#[test]
fn p233_template_reversed_edit_does_not_modify_source() {
    let mut source = "😀 alpha\r\nbeta".to_string();
    let before = source.clone();
    assert!(matches!(
        apply_text_change(
            &mut source,
            Some(Range::new(Position::new(1, 3), Position::new(0, 2))),
            "bad"
        ),
        Err(TemplateEditError::ReversedRange(_))
    ));
    assert_eq!(source, before);
}

#[test]
fn p233_template_edit_clamps_columns_but_keeps_newlines_and_utf16_boundaries() {
    for newline in ["\n", "\r\n"] {
        let mut source = format!("😀ab{newline}tail");
        apply_text_change(
            &mut source,
            Some(Range::new(Position::new(0, 2), Position::new(0, u32::MAX))),
            "X",
        )
        .unwrap();
        assert_eq!(source, format!("😀X{newline}tail"));
    }
}

#[test]
fn p233_template_invalid_start_and_end_lines_are_explicit_errors() {
    for original in ["", "😀", "one\r\ntwo", "one\n"] {
        for range in [
            Range::new(Position::new(99, 0), Position::new(99, 1)),
            Range::new(Position::new(0, 0), Position::new(u32::MAX, 0)),
        ] {
            let mut source = original.to_string();
            assert!(matches!(
                apply_text_change(&mut source, Some(range), "bad"),
                Err(TemplateEditError::InvalidLine(_))
            ));
            assert_eq!(source, original);
        }
    }
}

#[test]
fn p233_template_eof_empty_full_and_surrogate_edits_follow_shared_utf16_contract() {
    let mut source = String::new();
    apply_text_change(
        &mut source,
        Some(Range::new(Position::new(0, 0), Position::new(0, 0))),
        "😀\r\n",
    )
    .unwrap();
    apply_text_change(
        &mut source,
        Some(Range::new(Position::new(1, 0), Position::new(1, u32::MAX))),
        "tail",
    )
    .unwrap();
    assert_eq!(source, "😀\r\ntail");
    // Shared position conversion resolves a half-surrogate to the scalar start.
    apply_text_change(
        &mut source,
        Some(Range::new(Position::new(0, 1), Position::new(0, 2))),
        "X",
    )
    .unwrap();
    assert_eq!(source, "X\r\ntail");
    apply_text_change(&mut source, None, "").unwrap();
    assert_eq!(source, "");
}

#[test]
fn p233_twig_number_scanning_preserves_strings_names_comments_and_range_boundaries() {
    for (source, expected) in [
        (
            "{{ '1.5 and user.name' }}",
            "<?php echo '1.5 and user.name'; ?>\n",
        ),
        (
            "{{ E + e3 + price1 }}",
            "<?php echo $E + $e3 + $price1; ?>\n",
        ),
        ("{# 1e3 user.price(1.5) #}{{ 2.5 }}", "<?php echo 2.5; ?>\n"),
        ("{{ 1.5..2.5 }}", "<?php echo null; ?>\n"),
    ] {
        let doc = preprocess_twig_template(source, &[]);
        assert_eq!(doc.virtual_source(), expected);
        assert_valid_virtual_php(&doc);
    }
}

#[test]
fn p233_nested_twig_loops_keep_value_only_and_key_value_bindings() {
    let doc = preprocess_twig_template("{% for group in groups %}{% for key, item in group.items %}{{ key }}{{ item.name }}{% endfor %}{% endfor %}", &[]);
    assert!(doc.virtual_source().contains("foreach ($groups as $group)"));
    assert!(doc
        .virtual_source()
        .contains("foreach ($group->items as $key => $item)"));
    assert_valid_virtual_php(&doc);
}

#[test]
fn p233_blade_escaped_echo_body_does_not_publish_semantic_tokens() {
    let source = "@{{ $ignored + '@if($ignored)' }}\r\n@if ($live){{ $live }}@endif";
    let doc = preprocess_blade_template(source);
    assert!(!doc.virtual_source().contains("ignored"));
    let tokens = decode_semantic_tokens(&doc.map_semantic_tokens_to_original(Vec::new()));
    assert!(!tokens.is_empty());
    assert!(tokens.iter().all(|token| token.line == 1));
    assert_valid_virtual_php(&doc);
}

#[test]
fn p233_unterminated_escaped_blade_marker_does_not_hide_later_directives() {
    for open in ["@{{", "@{!!"] {
        let doc =
            preprocess_blade_template(&format!("{open} client text\r\n@if ($live) text @endif"));
        assert!(
            doc.virtual_source().contains("if ($live)"),
            "{}",
            doc.virtual_source()
        );
        assert_valid_virtual_php(&doc);
    }
}
