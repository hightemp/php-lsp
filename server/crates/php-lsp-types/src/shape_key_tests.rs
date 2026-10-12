use super::*;

#[test]
fn noncanonical_numeric_string_keys_are_quoted_in_phpdoc_output() {
    for key in ["01", "-0", "9223372036854775808"] {
        let item = ArrayShapeItem {
            key: Some(key.into()),
            optional: false,
            value: TypeInfo::Mixed,
        };
        assert_eq!(item.to_string(), format!("'{key}': mixed"));
    }
}

#[test]
fn written_keys_follow_single_and_double_quote_escape_semantics() {
    assert_eq!(normalize_shape_key_text(r#"'\n'"#), r#"\n"#);
    assert_eq!(normalize_shape_key_text(r#""\n""#), "\n");
    assert_ne!(
        normalize_shape_key_text(r#"'\n'"#),
        normalize_shape_key_text(r#""\n""#)
    );
    assert_eq!(normalize_shape_key_text(r#"'it\'s?'"#), "it's?");
    assert_eq!(normalize_shape_key_text(r#""a\\b?""#), r#"a\b?"#);
    assert_eq!(
        decode_shape_key_token(r#""\101\x41\u{41}""#),
        Some("AAA".into())
    );
    assert_eq!(decode_shape_key_token(r#""\400""#), Some("\0".into()));
    assert_eq!(decode_shape_key_token(r#""\q""#), Some(r#"\q"#.into()));
    assert_eq!(
        decode_shape_key_token(r#""\$ready?""#),
        Some("$ready?".into())
    );
    assert_eq!(decode_shape_key_token(r#""\xff""#), None);
    assert_eq!(decode_shape_key_token(r#""\u{D800}""#), None);
}

#[test]
fn literal_question_marks_are_key_data_rather_than_optional_markers() {
    for key in ["ready?", "ready??", "?", "ключ?", "0?"] {
        assert_eq!(normalize_shape_key_text(key), key);
        assert_ne!(normalize_shape_key_text(key), key.trim_end_matches('?'));
    }
}

#[test]
fn written_quoted_keys_preserve_question_marks_and_inner_whitespace() {
    for (token, key) in [
        ("'ready?'", "ready?"),
        ("\"ready??\"", "ready??"),
        (" ' ready? ' ", " ready? "),
        ("''", ""),
        ("'\"ready?\"'", "\"ready?\""),
    ] {
        assert_eq!(normalize_shape_key_text(token), key);
    }
}

#[test]
fn shape_display_distinguishes_optional_syntax_from_a_literal_question_mark() {
    let item = ArrayShapeItem {
        key: Some("ready?".into()),
        optional: true,
        value: TypeInfo::Simple("Foo".into()),
    };
    assert_eq!(item.to_string(), "'ready?'?: Foo");
    let required = ArrayShapeItem {
        optional: false,
        ..item
    };
    assert_eq!(required.to_string(), "'ready?': Foo");
}
