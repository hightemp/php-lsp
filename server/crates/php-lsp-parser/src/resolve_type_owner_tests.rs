use super::*;

fn variable_info(source: &str, needle: &str) -> VariableHoverInfo {
    let mut parser = crate::parser::FileParser::new();
    parser.parse_full(source);
    let tree = parser.tree().unwrap();
    let symbols = crate::symbols::extract_file_symbols(tree, source, "file:///parser-owner.php");
    let offset = source.find(needle).unwrap();
    let mut node = tree
        .root_node()
        .named_descendant_for_byte_range(offset, offset + 1)
        .unwrap();
    while node.kind() != "variable_name" {
        node = node.parent().unwrap();
    }
    infer_variable_hover_info_at_node_with_resolvers(
        node,
        source,
        &symbols,
        node.start_byte(),
        &source[node.byte_range()],
        None,
        None,
    )
    .unwrap()
}

#[test]
fn anonymous_foreach_phpdoc_self_has_no_named_outer_type_target() {
    for creation in ["new class", "new /* trivia */ class", "new #[Marker] class"] {
        let source = format!("<?php class Outer {{ function run() {{ $object = {creation} {{ function work() {{ /** @var array<int, self> $items */ $items = []; foreach ($items as $item) {{ $item; }} }} }}; }} }}");
        let info = variable_info(&source, "$item)");
        assert_ne!(
            info.resolved_type_fqn.as_deref(),
            Some("Outer"),
            "{creation}: {info:?}"
        );
    }
}

#[test]
fn anonymous_constructor_arguments_keep_the_named_outer_self_target() {
    let source = "<?php class Outer { function run() { /** @var self $value */ $value = $unknown; $object = new class('{', $value) {}; } }";
    let info = variable_info(source, "$value)");
    assert_eq!(info.resolved_type_fqn.as_deref(), Some("Outer"));
}

#[test]
fn anonymous_constructor_arguments_keep_the_named_outer_parent_target() {
    let source = "<?php class Grand {} class Other {} class Outer extends Grand { function run() { $object = new class(parent::class) extends Other {}; } }";
    assert_eq!(parent_scope_fqn(source).as_deref(), Some("Grand"));
}

fn parent_scope_fqn(source: &str) -> Option<String> {
    let mut parser = crate::parser::FileParser::new();
    parser.parse_full(source);
    let tree = parser.tree().unwrap();
    assert!(!tree.root_node().has_error(), "invalid fixture: {source}");
    let symbols = crate::symbols::extract_file_symbols(tree, source, "file:///parser-owner.php");
    symbol_at_position(
        tree,
        source,
        0,
        source.find("parent::").unwrap() as u32,
        &symbols,
    )
    .map(|symbol| symbol.fqn)
}

#[test]
fn anonymous_body_parent_target_survives_comments_and_attributes() {
    for creation in ["new class", "new /* trivia */ class", "new #[Marker] class"] {
        let source = format!("<?php class Grand {{}} class Other {{}} class Outer extends Grand {{ function run() {{ $object = {creation} extends Other {{ function work() {{ parent::class; }} }}; }} }}");
        assert_eq!(
            parent_scope_fqn(&source).as_deref(),
            Some("Other"),
            "{creation}"
        );
    }
}

#[test]
fn nearest_class_boundary_prevents_borrowing_an_outer_anonymous_parent() {
    // Named-class rows exercise a malformed editor CST; PHP rejects nested classes.
    for inner in [
        "$inner = new class { function work() { parent::class; } };",
        "class Inner { function work() { parent::class; } }",
        "class Inner extends Right { function work() { parent::class; } }",
    ] {
        let source = format!("<?php class Other {{}} class Right {{}} $outer = new class extends Other {{ function run() {{ {inner} }} }};");
        let fqn = parent_scope_fqn(&source);
        if inner.contains("extends Right") {
            assert_eq!(fqn.as_deref(), Some("Right"));
        } else {
            assert_ne!(fqn.as_deref(), Some("Other"));
        }
    }
}
