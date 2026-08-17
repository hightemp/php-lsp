use super::*;

#[test]
fn strip_range_formatter_wrapper_preserves_unwrapped_output() {
    let formatted = "<?php\necho 'selected with tag';\n".to_string();

    assert_eq!(
        strip_range_formatter_wrapper(formatted.clone(), false),
        Some(formatted)
    );
}

#[test]
fn strip_range_formatter_wrapper_accepts_lf_and_crlf_prefixes() {
    for (formatted, expected) in [
        ("<?php\necho 'lf';\n", "echo 'lf';\n"),
        ("<?php\r\necho 'crlf';\r\n", "echo 'crlf';\r\n"),
    ] {
        assert_eq!(
            strip_range_formatter_wrapper(formatted.to_string(), true).as_deref(),
            Some(expected)
        );
    }
}

#[test]
fn strip_range_formatter_wrapper_rejects_missing_or_changed_prefix() {
    for formatted in [
        "echo 'missing';\n",
        "\n<?php\necho 'shifted';\n",
        "<?phpecho 'changed';\n",
        "",
    ] {
        assert_eq!(
            strip_range_formatter_wrapper(formatted.to_string(), true),
            None,
            "unexpectedly accepted formatter output: {formatted:?}"
        );
    }
}
