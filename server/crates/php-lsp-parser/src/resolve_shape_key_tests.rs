use super::*;
use crate::{parser::FileParser, symbols::extract_file_symbols};

fn shape(text: &str) -> TypeInfo {
    crate::phpdoc::parse_type_string(text)
}

#[test]
fn foreach_key_types_preserve_noncanonical_numeric_strings_and_escaped_literals() {
    let ty = shape(r#"array{'01': Foo, '1': Bar, '-0': Baz, 'it\'s?': Quoted}"#);
    assert_eq!(
        iterable_key_type_info(&ty),
        Some(TypeInfo::Union(vec![
            TypeInfo::LiteralString("'01'".into()),
            TypeInfo::LiteralInt("1".into()),
            TypeInfo::LiteralString("'-0'".into()),
            TypeInfo::LiteralString(r#"'it\'s?'"#.into()),
        ]))
    );
    assert_eq!(
        iterable_key_type_info(&shape("array{first: Foo, ...<int, Bar>}")),
        Some(TypeInfo::Union(vec![
            TypeInfo::LiteralString("'first'".into()),
            simple("int")
        ]))
    );
    assert_eq!(
        iterable_key_type_info(&shape("array{first: Foo, ...}")),
        Some(simple("array-key"))
    );
    assert_eq!(
        iterable_key_type_info(&shape("array{first: Foo, ...<Bar>}")),
        Some(simple("array-key"))
    );
    assert_eq!(
        iterable_key_type_info(&shape("array{Foo, Bar}")),
        Some(TypeInfo::Union(vec![
            TypeInfo::LiteralInt("0".into()),
            TypeInfo::LiteralInt("1".into())
        ]))
    );
}

#[test]
fn differently_escaped_keys_do_not_alias_or_borrow_a_neighbor_type() {
    assert_eq!(value(r#"array{'\n': Foo}"#, Some(r#""\n""#)), None);
    let text = r#"array{'\n': Foo, "\n": Bar, 'it\'s?': Quoted}"#;
    assert_eq!(value(text, Some(r#"'\n'"#)), Some(simple("Foo")));
    assert_eq!(value(text, Some(r#""\n""#)), Some(simple("Bar")));
    assert_eq!(value(text, Some(r#""it's?""#)), Some(simple("Quoted")));
    let original = shape(text);
    let restored = crate::phpdoc::parse_phpdoc(&format!("/** @return {original} */")).return_type;
    assert_eq!(
        restored,
        Some(original),
        "key values must survive the resolver's PHPDoc text boundary"
    );
}

#[test]
fn over_budget_shape_values_remain_unknown_instead_of_emitting_a_truncated_type() {
    let items = (0..5000)
        .map(|n| php_lsp_types::ArrayShapeItem {
            key: Some(n.to_string()),
            optional: false,
            value: simple("Foo"),
        })
        .collect();
    assert_eq!(
        iterable_value_type_info(&TypeInfo::ArrayShape(items), None),
        None
    );
    let mut deep = simple("Foo");
    for _ in 0..100 {
        deep = TypeInfo::Generic {
            base: "array".into(),
            args: vec![deep],
        };
    }
    let ty = TypeInfo::ArrayShape(vec![php_lsp_types::ArrayShapeItem {
        key: Some("deep".into()),
        optional: false,
        value: deep,
    }]);
    assert_eq!(iterable_value_type_info(&ty, Some("'deep'")), None);
}
fn value(text: &str, key: Option<&str>) -> Option<TypeInfo> {
    iterable_value_type_info(&shape(text), key)
}
fn simple(name: &str) -> TypeInfo {
    TypeInfo::Simple(name.into())
}

#[test]
fn a_known_missing_sealed_shape_key_has_no_value_type() {
    for key in ["'missing'", "\"missing\"", "2", "''", "'READY'"] {
        assert_eq!(
            value("array{ready: Foo, other: Bar}", Some(key)),
            None,
            "{key}"
        );
    }
}

#[test]
fn named_keys_keep_exact_identity_including_literal_question_marks() {
    let text = "array{ready: Plain, 'ready?': Question, 'ready??'?: OptionalQuestion}";
    for (key, expected) in [
        ("'ready'", "Plain"),
        ("'ready?'", "Question"),
        ("'ready??'", "OptionalQuestion"),
    ] {
        assert_eq!(value(text, Some(key)), Some(simple(expected)), "{key}");
    }
}

#[test]
fn optional_markers_are_removed_only_from_phpdoc_key_syntax() {
    let TypeInfo::ArrayShape(items) = shape("array{ready?: Foo, 'ready?'?: Bar, \"ready??\": Baz}")
    else {
        panic!()
    };
    assert_eq!(
        items
            .iter()
            .map(|item| (item.key.as_deref(), item.optional))
            .collect::<Vec<_>>(),
        vec![
            (Some("ready"), true),
            (Some("ready?"), true),
            (Some("ready??"), false)
        ]
    );
}

#[test]
fn quoted_empty_and_whitespace_keys_remain_distinct_from_missing_keys() {
    let text = "array{'': Empty, ' ready? ': Spaced, ready: Plain}";
    assert_eq!(value(text, Some("''")), Some(simple("Empty")));
    assert_eq!(value(text, Some("' ready? '")), Some(simple("Spaced")));
    assert_eq!(value(text, Some("'ready?'")), None);
}

#[test]
fn an_unresolved_key_cannot_borrow_the_first_named_field() {
    for key in ["$key", "KEY", "$object->key", "'rea' . 'dy'"] {
        assert_eq!(
            value("array{ready: Foo, KEY: Decoy}", Some(key)),
            None,
            "{key}"
        );
    }
    assert_eq!(value("array{ready: Foo, ...<Bar>}", Some("$key")), None);
}

#[test]
fn malformed_empty_key_syntax_does_not_create_a_catch_all_value() {
    for text in ["array{: Foo}", "array{?: Foo}"] {
        assert_eq!(value(text, Some("0")), None, "{text}");
    }
}

#[test]
fn iterating_a_shape_combines_all_possible_values_instead_of_picking_the_first() {
    assert_eq!(
        value("array{first: Foo, second: Bar}", None),
        Some(TypeInfo::Union(vec![simple("Foo"), simple("Bar")]))
    );
    assert_eq!(
        value("array{first: Foo, second: Foo}", None),
        Some(simple("Foo"))
    );
    assert_eq!(value("array{}", None), None);
}

#[test]
fn numeric_tuple_keys_select_their_own_slot_and_do_not_become_catch_all_items() {
    assert_eq!(value("array{Foo, Bar}", Some("0")), Some(simple("Foo")));
    assert_eq!(value("array{Foo, Bar}", Some("1")), Some(simple("Bar")));
    for key in ["2", "-1", "'missing'"] {
        assert_eq!(value("array{Foo, Bar}", Some(key)), None, "{key}");
    }
}

#[test]
fn only_an_explicit_open_tail_can_supply_a_missing_named_key() {
    assert_eq!(
        value("array{first: Foo, ...<Bar>}", Some("'missing'")),
        Some(simple("Bar"))
    );
    assert_eq!(
        value("array{first: Foo, ...}", Some("'missing'")),
        Some(TypeInfo::Mixed)
    );
    assert_eq!(
        value("array{first: Foo, ...<string, Bar>}", Some("0")),
        None
    );
    assert_eq!(
        value("array{first: Foo, ...<string, Bar>}", Some("'other'")),
        Some(simple("Bar"))
    );
}

#[test]
fn nested_missing_keys_stop_inference_at_the_failed_segment() {
    let outer = shape("array{outer: array{ready: Foo, 'ready?': Bar}}");
    let inner = iterable_value_type_info(&outer, Some("'outer'")).unwrap();
    assert_eq!(iterable_value_type_info(&inner, Some("'missing'")), None);
    assert_eq!(
        iterable_value_type_info(&inner, Some("'ready?'")),
        Some(simple("Bar"))
    );
}

#[test]
fn missing_shape_receivers_do_not_create_exact_method_references() {
    let source="<?php class Foo {function onlyFoo(){}} function run(){/** @var array{ready: Foo} $shape */ $shape=[]; $shape['missing']->onlyFoo();}";
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let tree = parser.tree().unwrap();
    let symbols = extract_file_symbols(tree, source, "file:///shape.php");
    let references = crate::references::collect_symbol_references_in_file(tree, source, &symbols);
    assert!(
        !references
            .iter()
            .any(|reference| !reference.is_declaration && reference.target_fqn == "Foo::onlyFoo"),
        "{references:?}"
    );
}
