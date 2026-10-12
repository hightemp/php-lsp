use super::*;

#[test]
fn escaped_key_definitions_compare_decoded_values_but_keep_written_ranges() {
    let source = r#"<?php /** @var array{'\n': Foo, "\n": Bar} $shape */ $shape=[]; $shape["\n"];"#;
    let access = ShapeDefinitionAccess {
        root_var: "$shape".into(),
        segments: vec![ShapePathSegment {
            key: "\n".into(),
            kind: ShapeDefinitionKind::Array,
        }],
    };
    let (start, end) = phpdoc_shape_key_range_before_usage(source, &access, source.len()).unwrap();
    assert_eq!(&source[start..end], r#"\n"#);
    assert_eq!(start, source.find(r#""\n": Bar"#).unwrap() + 1);
}

#[test]
fn empty_quoted_key_definition_retains_its_zero_width_source_selection() {
    let source = "<?php /** @var array{'': Foo, other: Bar} $shape */ $shape=[]; $shape[''];";
    let access = ShapeDefinitionAccess {
        root_var: "$shape".into(),
        segments: vec![ShapePathSegment {
            key: String::new(),
            kind: ShapeDefinitionKind::Array,
        }],
    };
    let (start, end) = phpdoc_shape_key_range_before_usage(source, &access, source.len()).unwrap();
    assert_eq!(start, source.find("''").unwrap() + 1);
    assert_eq!(start, end);
}

#[test]
fn shape_completion_does_not_deduplicate_literal_question_mark_keys() {
    let items = vec![
        php_lsp_types::ArrayShapeItem {
            key: Some("ready".into()),
            optional: false,
            value: php_lsp_types::TypeInfo::Mixed,
        },
        php_lsp_types::ArrayShapeItem {
            key: Some("ready?".into()),
            optional: false,
            value: php_lsp_types::TypeInfo::Mixed,
        },
    ];
    let mut found = Vec::new();
    collect_named_shape_items(&items, &mut HashSet::new(), &mut found);
    assert_eq!(found.len(), 2);
}

#[test]
fn phpdoc_key_definition_selects_literal_punctuation_and_omits_optional_syntax() {
    let source =
        "<?php /** @var array{ready: Foo, 'ready?'?: Bar} $shape */ $shape=[]; $shape['ready?'];";
    let access = ShapeDefinitionAccess {
        root_var: "$shape".into(),
        segments: vec![ShapePathSegment {
            key: "ready?".into(),
            kind: ShapeDefinitionKind::Array,
        }],
    };
    let (start, end) = phpdoc_shape_key_range_before_usage(source, &access, source.len()).unwrap();
    assert_eq!(&source[start..end], "ready?");
    assert_eq!(start, source.find("ready?'?").unwrap());
}

#[test]
fn literal_key_definition_does_not_navigate_to_a_neighbor_without_question_mark() {
    let source = "<?php $shape=['ready'=>1,'ready?'=>2]; $shape['ready?'];";
    let access = ShapeDefinitionAccess {
        root_var: "$shape".into(),
        segments: vec![ShapePathSegment {
            key: "ready?".into(),
            kind: ShapeDefinitionKind::Array,
        }],
    };
    let (start, end) =
        literal_array_shape_key_range_before_usage(source, &access, source.len()).unwrap();
    assert_eq!(&source[start..end], "ready?");
}
