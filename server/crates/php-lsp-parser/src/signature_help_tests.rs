use super::*;
use crate::parser::FileParser;
use crate::symbols::extract_file_symbols;

fn context_for(source: &str, line: u32, character: u32) -> SignatureHelpContext {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let tree = parser.tree().expect("tree");
    let file_symbols = extract_file_symbols(tree, source, "file:///test.php");
    signature_help_context_at_position(tree, source, line, character, &file_symbols, None)
        .expect("signature help context")
}

#[test]
fn detects_function_call_active_parameter() {
    let source = "<?php\nfunction foo($a, $b) {}\nfoo(1, 2);\n";
    let ctx = context_for(source, 2, 7);
    assert_eq!(ctx.symbol.fqn, "foo");
    assert_eq!(ctx.active_parameter, 1);
}

#[test]
fn detects_active_parameter_after_emoji_byte_column() {
    let source = "<?php\nfunction foo($a, $b) {}\n$emoji = \"😀\"; foo(1, 2);\n";
    let byte_col_inside_second_arg = source
        .lines()
        .nth(2)
        .and_then(|line| line.find('2'))
        .expect("second argument byte column") as u32;
    let ctx = context_for(source, 2, byte_col_inside_second_arg);

    assert_eq!(ctx.symbol.fqn, "foo");
    assert_eq!(ctx.active_parameter, 1);
}

#[test]
fn detects_constructor_call() {
    let source = "<?php\nclass Foo { public function __construct($a) {} }\nnew Foo(1);\n";
    let ctx = context_for(source, 2, 9);
    assert_eq!(ctx.symbol.fqn, "Foo::__construct");
    assert_eq!(ctx.active_parameter, 0);
}

#[test]
fn keeps_nested_call_context() {
    let source = "<?php\nfunction outer($a) {}\nfunction inner($a, $b) {}\nouter(inner(1, 2));\n";
    let ctx = context_for(source, 3, 15);
    assert_eq!(ctx.symbol.fqn, "inner");
    assert_eq!(ctx.active_parameter, 1);
}

const DECLARATIONS: &str = "<?php\nfunction foo($first, $second, $third = null) {}\nfunction outer($first, $second) {}\nfunction inner($first, $second) {}\nclass Service { public function run($first, $second, $third = null) {} public function __construct($first, $second) {} }\n";

fn marked_source(text: &str) -> (String, u32, u32) {
    let cursor = text.find("<cursor>").expect("one cursor");
    let before = &text[..cursor];
    let line = before.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let col = before.rsplit('\n').next().unwrap().len() as u32;
    (text.replacen("<cursor>", "", 1), line, col)
}

fn marked_context(body: &str) -> SignatureHelpContext {
    let (source, line, col) = marked_source(&format!("{DECLARATIONS}{body}"));
    let mut parser = FileParser::new();
    parser.parse_full(&source);
    let tree = parser.tree().unwrap();
    let symbols = extract_file_symbols(tree, &source, "file:///signature.php");
    signature_help_context_at_position(tree, &source, line, col, &symbols, None).unwrap_or_else(
        || {
            panic!(
                "no signature help for {body:?}: {}",
                tree.root_node().to_sexp()
            )
        },
    )
}

fn assert_marked(body: &str, fqn: &str, active: usize) {
    let context = marked_context(body);
    assert_eq!(context.symbol.fqn, fqn, "{body}");
    assert_eq!(context.active_parameter, active, "{body}");
}

#[test]
fn nullsafe_method_calls_resolve_the_same_target_and_parameter_as_ordinary_calls() {
    for operator in ["->", "?->"] {
        assert_marked(
            &format!("function demo(?Service $service) {{ $service{operator}run(1, <cursor>2); }}"),
            "Service::run",
            1,
        );
    }
}

#[test]
fn block_comments_are_not_argument_separators_or_string_delimiters() {
    for body in [
        "foo(1 /* , , */, <cursor>2);",
        "foo(1 /* (, [, { */, <cursor>2);",
        "foo(1 /* ' unclosed quote */, <cursor>2);",
        "foo(1 /* \" unclosed quote */, <cursor>2);",
        "foo(/* , <cursor> , */ 1, 2);",
    ] {
        let expected = if body.contains("<cursor> , */") { 0 } else { 1 };
        assert_marked(body, "foo", expected);
    }
}

#[test]
fn line_comments_do_not_shift_active_parameter() {
    for comment in ["// , , (, [", "# , , ), ]"] {
        assert_marked(&format!("foo(1, {comment}\n<cursor>2);"), "foo", 1);
    }
}

#[test]
fn heredoc_and_nowdoc_punctuation_does_not_change_argument_position() {
    for opener in ["<<<TEXT", "<<<\"TEXT\"", "<<<'TEXT'"] {
        assert_marked(
            &format!("foo({opener}\na, b, ( [ {{ ' \"\nTEXT\n, <cursor>2);"),
            "foo",
            1,
        );
        assert_marked(
            &format!("foo({opener}\na, <cursor>b\nTEXT\n, 2);"),
            "foo",
            0,
        );
    }
}

#[test]
fn shell_strings_and_interpolated_strings_are_single_arguments() {
    assert_marked("foo(`printf a,b`, <cursor>2);", "foo", 1);
    assert_marked("foo(\"value {$row['a,b']}\", <cursor>2);", "foo", 1);
    assert_marked("foo('escaped \\' a,b', <cursor>2);", "foo", 1);
}

#[test]
fn nested_arrays_closures_and_calls_preserve_the_owning_call() {
    assert_marked(
        "foo([1, 2, [3, 4]], function($x, $y) { return [$x, $y]; }, <cursor>3);",
        "foo",
        2,
    );
    assert_marked("outer(inner(1, <cursor>2), 3);", "inner", 1);
    assert_marked("outer(inner(1, 2)<cursor>, 3);", "outer", 0);
    assert_marked("outer(inner(1, 2), <cursor>3);", "outer", 1);
}

#[test]
fn separator_and_parenthesis_cursor_boundaries_select_the_expected_argument() {
    for (body, active) in [
        ("foo(<cursor>);", 0),
        ("foo(1<cursor>, 2);", 0),
        ("foo(1,<cursor> 2);", 1),
        ("foo(1, 2<cursor>);", 1),
        ("foo(1, 2,<cursor>);", 2),
    ] {
        assert_marked(body, "foo", active);
    }
}

#[test]
fn incomplete_function_calls_keep_signature_help_at_eof_and_after_comments() {
    for (body, active) in [
        ("foo(<cursor>", 0),
        ("foo(1,<cursor>", 1),
        ("foo(1, /* , ( */ <cursor>", 1),
        ("outer(inner(1, <cursor>", 1),
    ] {
        assert_marked(
            body,
            if body.starts_with("outer") {
                "inner"
            } else {
                "foo"
            },
            active,
        );
    }
}

#[test]
fn incomplete_constructor_and_nullsafe_calls_keep_their_targets() {
    assert_marked("new Service(1, <cursor>", "Service::__construct", 1);
    assert_marked(
        "$service = new Service(1, 2); $service?->run(1, <cursor>",
        "Service::run",
        1,
    );
}

#[test]
fn complete_calls_do_not_own_cursors_before_open_or_after_close() {
    for body in [
        "foo<cursor>(1, 2);",
        "foo(1, 2)<cursor>;",
        "foo(1, 2); <cursor>",
        "function other($x, <cursor>$y) {}",
        "$array = [1, <cursor>2];",
    ] {
        let (source, line, col) = marked_source(&format!("{DECLARATIONS}{body}"));
        let mut parser = FileParser::new();
        parser.parse_full(&source);
        let tree = parser.tree().unwrap();
        let symbols = extract_file_symbols(tree, &source, "file:///signature.php");
        assert!(
            signature_help_context_at_position(tree, &source, line, col, &symbols, None).is_none(),
            "{body}"
        );
    }
}

#[test]
fn incomplete_call_recovery_preserves_namespace_import_kinds_and_qualified_names() {
    for (body, fqn) in [
        (
            "use function Vendor\\foo as alias; alias(1, <cursor>",
            "Vendor\\foo",
        ),
        (
            "use Service as Alias; new Alias(1, <cursor>",
            "Service::__construct",
        ),
        (
            "use Service as Alias; Alias::run(1, <cursor>",
            "Service::run",
        ),
    ] {
        let (source, line, col) = marked_source(&format!(
            "<?php\nnamespace {{\n{}\n}}\nnamespace Client {{\n{body}",
            DECLARATIONS.strip_prefix("<?php\n").unwrap()
        ));
        let context = context_for(&source, line, col);
        assert_eq!(context.symbol.fqn, fqn);
        assert_eq!(context.active_parameter, 1);
    }
    assert_marked("\\foo(1, <cursor>", "foo", 1);
}

#[test]
fn incomplete_nested_arrays_and_comment_trivia_keep_outer_argument_positions() {
    assert_marked("foo([1, 2, <cursor>", "foo", 0);
    assert_marked("foo([1, 2], 3, // , fake(\n<cursor>", "foo", 2);
    assert_marked("foo(1, /* inner( , <cursor> , ) */", "foo", 1);
    assert_marked("foo(1, /* first , */ /* second ( , */ <cursor>", "foo", 1);
}

#[test]
fn error_recovery_does_not_invent_calls_in_declarations_comments_or_unknown_inner_calls() {
    for body in [
        "function unfinished($first, <cursor>",
        "/* foo(1, <cursor> */",
        "outer($callback(1, <cursor>",
        "foo(1, /* trivia */ function later<cursor>() {}",
    ] {
        let (source, line, col) = marked_source(&format!("{DECLARATIONS}{body}"));
        let mut parser = FileParser::new();
        parser.parse_full(&source);
        let tree = parser.tree().unwrap();
        let symbols = extract_file_symbols(tree, &source, "file:///signature.php");
        assert!(
            signature_help_context_at_position(tree, &source, line, col, &symbols, None).is_none(),
            "{body}: {}",
            tree.root_node().to_sexp()
        );
    }
}
