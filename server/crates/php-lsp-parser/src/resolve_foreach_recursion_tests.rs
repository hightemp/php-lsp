use super::*;
use crate::{parser::FileParser, symbols::extract_file_symbols};

fn infer(source: &str, marker: &str, name: &str) -> (Option<TypeInfo>, usize) {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let tree = parser.tree().unwrap();
    assert!(!tree.root_node().has_error());
    let symbols = extract_file_symbols(tree, source, "file:///foreach-recursion.php");
    let offset = source.find(marker).unwrap() + marker.len();
    let prefix = &source[..offset];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let col = prefix.rsplit('\n').next().unwrap().len() as u32;
    // Bound the RED run using the existing guard: the regression must never
    // execute the original exponential traversal at its normal depth of 64.
    let _request = ObjectTypeResolveDepthGuard::enter().unwrap();
    let previous =
        OBJECT_TYPE_RESOLVE_DEPTH.with(|depth| depth.replace(MAX_OBJECT_TYPE_RESOLVE_DEPTH - 16));
    let restore = ObjectTypeResolveDepthGuard { previous };
    TYPE_RESOLUTION_TEST_ENTRIES.with(|entries| entries.set(0));
    let result = infer_variable_type_info_at_position(tree, source, &symbols, line, col, name);
    let entries = TYPE_RESOLUTION_TEST_ENTRIES.with(Cell::get);
    drop(restore);
    (result, entries)
}

#[test]
fn foreach_iterable_keeps_its_pre_binding_type_when_the_value_reuses_its_name() {
    let source="<?php class Item {} /** @param Item[] $items */ function run(array $items){foreach(/*ITER*/$items as $items){echo $items;}}";
    let (result, entries) = infer(source, "/*ITER*/", "$items");
    assert!(
        entries < 100,
        "self-binding expanded recursively {entries} times"
    );
    assert_eq!(result, Some(crate::phpdoc::parse_type_string("Item[]")));
}

#[test]
fn foreach_body_reused_variable_resolves_the_element_without_exponential_work() {
    let source="<?php class Item {function run(){}} /** @param Item[] $items */ function useItems(array $items){foreach($items as $items){/*USE*/$items->run();}}";
    let (result, entries) = infer(source, "/*USE*/", "$items");
    assert!(
        entries < 100,
        "element lookup expanded recursively {entries} times: {result:?}"
    );
    assert_eq!(result, Some(TypeInfo::Simple("Item".into())));
}

#[test]
fn foreach_header_comments_preserve_value_and_key_types() {
    let source="<?php class Item{} /** @param array<string,Item> $items */ function walk(array $items){foreach(/*ITER*/$items /*A*/ as /*B*/ $key /*C*/ => /*D*/ $item){/*USE*/$item->run();}}";
    assert_eq!(
        infer(source, "/*USE*/", "$item").0,
        Some(TypeInfo::Simple("Item".into()))
    );
    assert_eq!(
        infer(source, "/*USE*/", "$key").0,
        Some(TypeInfo::Simple("string".into()))
    );
}

#[test]
fn foreach_key_binding_cannot_replace_the_earlier_iterable_expression() {
    let source="<?php class Item {} /** @param array<string,Item> $items */ function run(array $items){foreach(/*ITER*/$items as $items=>$value){echo $value;}}";
    let (result, entries) = infer(source, "/*ITER*/", "$items");
    assert!(
        entries < 100,
        "key lookup expanded recursively {entries} times"
    );
    assert_eq!(
        result,
        Some(TypeInfo::Generic {
            base: "array".into(),
            args: vec![
                TypeInfo::Simple("string".into()),
                TypeInfo::Simple("Item".into())
            ]
        })
    );
}

#[test]
fn unknown_reused_foreach_receiver_has_bounded_work_without_a_guessed_type() {
    let source = "<?php function run($items){foreach($items as $items){/*USE*/$items->run();}}";
    let (result, entries) = infer(source, "/*USE*/", "$items");
    assert!(
        entries < 100,
        "unknown foreach receiver expanded recursively {entries} times"
    );
    assert_eq!(result, None);
}

#[test]
fn branching_iterable_with_a_reused_receiver_cannot_expand_exponentially() {
    let source="<?php function run($items){foreach(is_array($items)?$items:[$items] as $items){/*USE*/$items->run();}}";
    let (result, entries) = infer(source, "/*USE*/", "$items");
    assert!(
        entries < 100,
        "branching foreach receiver expanded recursively {entries} times: {result:?}"
    );
}

#[test]
fn member_iterable_reusing_its_receiver_has_bounded_lookup_work() {
    let source =
        "<?php function run($items){foreach($items->children as $items){/*USE*/$items->run();}}";
    let (result, entries) = infer(source, "/*USE*/", "$items");
    assert!(
        entries < 100,
        "member foreach receiver expanded recursively {entries} times: {result:?}"
    );
}

#[test]
fn full_depth_reference_collection_keeps_the_element_method_target() {
    let source="<?php class Item{function run(){}} class Container{/** @var Item[] */ public array $children; function run(){}} function walk(Container $node){foreach($node->children as $node){$node->run();}}";
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let tree = parser.tree().unwrap();
    let symbols = extract_file_symbols(tree, source, "file:///foreach-reference.php");
    TYPE_RESOLUTION_TEST_ENTRIES.with(|entries| entries.set(0));
    let refs = crate::references::collect_symbol_references_in_file(tree, source, &symbols);
    assert!(TYPE_RESOLUTION_TEST_ENTRIES.with(Cell::get) < 1000);
    assert!(
        refs.iter()
            .any(|reference| !reference.is_declaration && reference.target_fqn == "Item::run"),
        "{refs:?}"
    );
    assert!(
        !refs
            .iter()
            .any(|reference| !reference.is_declaration && reference.target_fqn == "Container::run"),
        "{refs:?}"
    );
}

#[test]
fn unknown_member_assignment_chain_does_not_repeat_all_preceding_inference() {
    let mut source = String::from("<?php function walk($node){");
    for index in 0..7 {
        let previous = if index == 0 {
            "$node".to_string()
        } else {
            format!("$node{}", index - 1)
        };
        source.push_str(&format!("$node{index}={previous}->child;"));
    }
    source.push_str("/*USE*/$node6->run();}");
    let (result, entries) = infer(&source, "/*USE*/", "$node6");
    assert_eq!(result, None);
    assert!(
        entries < budget::MAX_INFERENCE_WORK,
        "unknown assignment chain repeated inference {entries} times"
    );
}

#[test]
fn exhausted_scan_cannot_publish_an_earlier_exact_assignment() {
    let mut source = String::from(
        "<?php class OldType{} class NewType{} function walk($node){$target=new OldType;",
    );
    for index in 0..7 {
        let previous = if index == 0 {
            "$node".to_string()
        } else {
            format!("$node{}", index - 1)
        };
        source.push_str(&format!("$node{index}={previous}->child;"));
    }
    source.push_str("$target=$node6->child;$target=new NewType;/*USE*/$target->run();}");
    let (result, entries) = infer(&source, "/*USE*/", "$target");
    assert_eq!(
        result, None,
        "exhausted scan published a partial exact type"
    );
    assert!(
        entries < budget::MAX_INFERENCE_WORK,
        "exhausted scan kept computing {entries} times"
    );
}

#[test]
fn flat_statement_scan_exhaustion_discards_the_earlier_native_type() {
    let mut source =
        String::from("<?php class OldType{} class NewType{} function walk(OldType $target){");
    source.push_str(&"echo 0;".repeat(budget::MAX_INFERENCE_WORK));
    source.push_str("$target=new NewType;/*USE*/$target->run();}");
    let mut parser = FileParser::new();
    parser.parse_full(&source);
    let symbols = extract_file_symbols(parser.tree().unwrap(), &source, "file:///flat-budget.php");
    let usage = source.find("/*USE*/").unwrap() + "/*USE*/".len();
    let result = infer_variable_type_info_at_position(
        parser.tree().unwrap(),
        &source,
        &symbols,
        0,
        usage as u32,
        "$target",
    );
    assert_eq!(
        budget::remaining(),
        0,
        "test must exhaust work rather than depth"
    );
    assert_eq!(
        result, None,
        "budget exhaustion retained the original parameter type"
    );
}
