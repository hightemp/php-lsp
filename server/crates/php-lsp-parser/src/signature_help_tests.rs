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
