use super::*;

fn inferred_variable(source: &str, needle: &str) -> IndexedExpressionTypeInfo {
    let uri = "file:///type-owner.php";
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let tree = parser.tree().unwrap();
    let symbols = extract_file_symbols(tree, source, uri);
    let index = WorkspaceIndex::new();
    index.update_file(uri, symbols.clone());
    let offset = source.find(needle).unwrap();
    let mut node = tree
        .root_node()
        .named_descendant_for_byte_range(offset, offset + 1)
        .unwrap();
    while node.kind() != "variable_name" {
        node = node.parent().unwrap();
    }
    let cache = RequestTypeCache::new(uri, Some(1));
    let utf16 = Utf16LineIndex::new(source);
    let ctx = InlayHintContext {
        tree,
        source,
        file_symbols: &symbols,
        index: &index,
        type_cache: &cache,
        utf16_index: &utf16,
        requested_range: (0, 0, u32::MAX, u32::MAX),
        allow_twig_property_accessors: false,
        allow_blocking_file_io: false,
        cancellation: None,
    };
    server_variable_type_info(&ctx, node).expect("PHPDoc variable type")
}

#[test]
fn phpdoc_variable_type_owner_uses_containing_class_instead_of_first_class() {
    for ty in ["self", "static", "parent"] {
        let source = format!("<?php namespace App; class First {{}} class Actual {{ function run() {{ /** @var {ty} $value */ $value = $unknown; $value; }} }}");
        let info = inferred_variable(&source, "$value;");
        assert_eq!(info.owner_fqn, "App\\Actual", "{ty}");
    }
}

#[test]
fn phpdoc_variable_type_owner_is_empty_outside_a_class() {
    let source = "<?php namespace App; class First {} function run() { /** @var self $value */ $value = $unknown; $value; }";
    assert_eq!(inferred_variable(source, "$value;").owner_fqn, "");
}

#[test]
fn phpdoc_variable_type_owner_supports_trait_enum_and_closure_positions() {
    for declaration in ["trait Actual", "enum Actual", "class Actual"] {
        let source = format!("<?php class First {{}} {declaration} {{ function run() {{ $closure = function() {{ /** @var static $value */ $value = $unknown; $value; }}; }} }}");
        assert_eq!(
            inferred_variable(&source, "$value;").owner_fqn,
            "Actual",
            "{declaration}"
        );
    }
}

#[test]
fn phpdoc_foreach_value_keeps_the_collection_lexical_owner() {
    let source = "<?php namespace App; class First {} class Actual { function run() { /** @var array<int, self> $items */ $items = []; foreach ($items as $value) { $value; } } }";
    assert_eq!(
        inferred_variable(source, "$value;").owner_fqn,
        "App\\Actual"
    );
}

#[test]
fn phpdoc_variable_owner_and_imports_follow_the_active_namespace_section() {
    let source = "<?php namespace First { use Vendor\\Left as Alias; class Decoy {} } namespace Second { use Vendor\\Right as Alias; class Actual { function run() { /** @var array<int, Alias> $items */ $items = []; foreach ($items as $value) { $value; } } } }";
    let info = inferred_variable(source, "$value;");
    assert_eq!(info.owner_fqn, "Second\\Actual");
    assert_eq!(info.type_info.to_string(), "Vendor\\Right");
}

#[test]
fn phpdoc_variable_owner_uses_byte_ranges_after_non_ascii_and_crlf() {
    let source = "<?php /* 😀 Ж */ class First {} /* 😀 */ class Actual { function run() { /** @var self $value */ $value = $unknown; /* 😀 */ $value; } }\n";
    for source in [source.to_string(), source.replace('\n', "\r\n")] {
        assert_eq!(inferred_variable(&source, "$value;").owner_fqn, "Actual");
    }
}

#[test]
fn variable_assignment_from_method_keeps_declaring_owner_instead_of_caller() {
    let source = "<?php class First {} class Supplier { /** @return array<int, self> */ function rows() { return []; } } class Actual { function run(Supplier $supplier) { $items = $supplier->rows(); $items; } }";
    let info = inferred_variable(source, "$items;");
    assert_eq!(info.owner_fqn, "Supplier");
}

#[test]
fn phpdoc_variable_type_owner_does_not_borrow_outer_class_inside_anonymous_class() {
    for creation in ["new class", "new /* trivia */ class", "new #[Marker] class"] {
        let source = format!("<?php class Outer {{ function run() {{ $object = {creation} {{ function work() {{ /** @var self $value */ $value = $unknown; $value; }} }}; }} }}");
        assert_eq!(
            inferred_variable(&source, "$value;").owner_fqn,
            "",
            "{creation}"
        );
    }
}

#[test]
fn phpdoc_variable_owner_in_anonymous_constructor_arguments_stays_in_outer_scope() {
    let source = "<?php class Outer { function run() { /** @var self $value */ $value = $unknown; $object = new class('{', $value) {}; } }";
    assert_eq!(inferred_variable(source, "$value)").owner_fqn, "Outer");
}

#[test]
fn phpdoc_variable_owner_in_a_named_class_nested_inside_anonymous_body_is_named() {
    // PHP rejects nested named classes, but editor CST must respect their boundary.
    let source = "<?php $outer = new class { function run() { class Inner { function work() { /** @var self $value */ $value = $unknown; $value; } } } };";
    assert_eq!(inferred_variable(source, "$value;").owner_fqn, "Inner");
}

#[test]
fn phpdoc_parent_link_uses_the_actual_owner_parent() {
    let source = "<?php\nclass WrongBase {}\nclass RightBase {}\nclass Decoy extends WrongBase {}\nclass Actual extends RightBase { function run() { /** @var parent $value */ $value = $unknown; $value; } }";
    let info = inferred_variable(source, "$value;");
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let symbols = extract_file_symbols(parser.tree().unwrap(), source, "file:///type-owner.php");
    let index = WorkspaceIndex::new();
    index.update_file("file:///type-owner.php", symbols.clone());
    let markdown = markdown_type_info_class_links(
        &index,
        &symbols,
        &info.owner_fqn,
        &info.uri,
        &info.type_info,
    )
    .unwrap();
    assert!(
        markdown.contains("type-owner.php#L3"),
        "wrong parent location: {markdown}"
    );
    assert!(!markdown.contains("#L2"), "foreign parent: {markdown}");
}
