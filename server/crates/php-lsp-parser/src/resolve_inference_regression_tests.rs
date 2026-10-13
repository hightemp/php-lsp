use crate::{
    parser::FileParser, references::collect_symbol_references_in_file,
    resolve::infer_variable_type_info_at_position, symbols::extract_file_symbols,
};

fn inferred(source: &str, variable: &str) -> Option<String> {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let tree = parser.tree().unwrap();
    assert!(!tree.root_node().has_error(), "invalid fixture");
    let symbols = extract_file_symbols(tree, source, "file:///review.php");
    let offset = source.find("/*USE*/").unwrap() + 7;
    let before = &source[..offset];
    let line = before.bytes().filter(|b| *b == b'\n').count() as u32;
    let column = before.rsplit('\n').next().unwrap().len() as u32;
    infer_variable_type_info_at_position(tree, source, &symbols, line, column, variable)
        .map(|ty| ty.to_string().trim_start_matches('\\').to_string())
}

#[test]
fn control_normal_foreach_value() {
    let s="<?php class Item{} /** @param Item[] $rows */ function walk(array $rows){foreach($rows as $value){/*USE*/$value->run();}}";
    assert_eq!(inferred(s, "$value").as_deref(), Some("Item"));
}

#[test]
fn nested_and_positional_foreach_patterns_resolve_each_static_path() {
    let nested="<?php class Item{} class Other{} /** @param array<int,array{key:array{0:Item,1:Other}}> $rows */ function walk(array $rows){foreach($rows as ['key'=>[$first,$second]]){/*USE*/$second;}}";
    assert_eq!(inferred(nested, "$first").as_deref(), Some("Item"));
    assert_eq!(inferred(nested, "$second").as_deref(), Some("Other"));
    let holes="<?php class Item{} class Other{} /** @param array<int,array{0:Item,1:Other}> $rows */ function walk(array $rows){foreach($rows as list(,$second)){/*USE*/$second;}}";
    assert_eq!(inferred(holes, "$second").as_deref(), Some("Other"));
}

#[test]
fn repeated_destructuring_variable_uses_the_last_completed_field() {
    let foreach="<?php class Item{} class Other{} /** @param array<int,array{0:Item,1:Other}> $rows */ function walk(array $rows){foreach($rows as [$value,$value]){/*USE*/$value;}}";
    assert_eq!(inferred(foreach, "$value").as_deref(), Some("Other"));
    let assignment="<?php class Item{} class Other{} function walk(){/** @var array{0:Item,1:Other} $row */$row=[];[$value,$value]=$row;/*USE*/$value;}";
    assert_eq!(inferred(assignment, "$value").as_deref(), Some("Other"));
}

#[test]
fn repeated_foreach_variable_definition_selects_the_last_written_node() {
    let source="<?php function walk($rows){foreach($rows as [/*FIRST*/$value,/*LAST*/$value]){/*USE*/$value;}}";
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let usage = source.find("/*USE*/").unwrap() + 7;
    let location = crate::resolve::variable_definition_at_position(
        parser.tree().unwrap(),
        source,
        0,
        usage as u32 + 1,
    )
    .unwrap();
    assert_eq!(location.1, source.find("/*LAST*/").unwrap() as u32 + 8);
}

#[test]
fn unknown_pattern_key_and_body_write_clear_old_types() {
    let dynamic="<?php class Known{} /** @param array<int,array{a:Known,b:Known}> $rows */ function walk(array $rows,$key){$value=new Known;foreach($rows as [$key=>$value]){/*USE*/$value;}}";
    assert_eq!(inferred(dynamic, "$value"), None);
    let body="<?php class Known{} /** @param Known[] $rows */ function walk(array $rows,$unknown){foreach($rows as $value){$value=$unknown;/*USE*/$value;}}";
    assert_eq!(inferred(body, "$value"), None);
}

#[test]
fn interpolated_pattern_keys_are_unknown_but_escaped_literal_keys_resolve() {
    let dynamic = r#"<?php class Item{} /** @param array<int,array{'$key':Item}> $rows */ function walk(array $rows,$key){foreach($rows as ["$key"=>$value]){/*USE*/$value;}}"#;
    assert_eq!(
        inferred(dynamic, "$value"),
        None,
        "interpolation must not become a literal field key"
    );
    let literal = dynamic.replace("[\"$key\"", "[\"\\$key\"");
    assert_eq!(inferred(&literal, "$value").as_deref(), Some("Item"));
}

#[test]
fn by_reference_foreach_keeps_the_element_type_and_pending_output_call_keeps_old_type() {
    let reference="<?php class Item{} /** @param Item[] $rows */ function walk(array $rows){foreach($rows as &/*note*/$value){/*USE*/$value;}}";
    assert_eq!(inferred(reference, "$value").as_deref(), Some("Item"));
    let pending="<?php class Known{} function walk($text){$value=new Known;preg_match('/x/',$text,/*USE*/$value);}";
    assert_eq!(inferred(pending, "$value").as_deref(), Some("Known"));
}

#[test]
fn foreach_owner_reads_do_not_become_local_definitions() {
    for target in ["$holder->child", "$holder[0]"] {
        let source=format!("<?php function walk($rows){{/*DEF*/$holder=new Holder;foreach($rows as {target}){{/*USE*/$holder;}}}}");
        let mut parser = FileParser::new();
        parser.parse_full(&source);
        let usage = source.find("/*USE*/").unwrap() + 7;
        let location = crate::resolve::variable_definition_at_position(
            parser.tree().unwrap(),
            &source,
            0,
            usage as u32 + 1,
        )
        .unwrap();
        assert_eq!(
            location.1,
            source.find("/*DEF*/").unwrap() as u32 + 7,
            "owner became a new foreach local: {target}"
        );
    }
}

#[test]
fn completed_loop_body_write_is_not_a_definite_post_loop_assignment() {
    let source="<?php class Item{} class Other{} /** @param Item[] $rows */ function walk(array $rows){foreach($rows as $value){$value=new Other;}/*USE*/$value;}";
    assert_ne!(inferred(source, "$value").as_deref(), Some("Other"));
}

#[test]
fn many_independent_functions_keep_their_simple_native_receivers() {
    let mut source =
        String::from("<?php class Known{function run(){}} function expensive(Known $value){");
    source.push_str(&"echo 0;".repeat(1000));
    source.push_str(&"$value->run();".repeat(100));
    source.push('}');
    let mut offsets = Vec::new();
    for index in 0..64 {
        source.push_str(&format!("function healthy{index}(Known $ok){{"));
        offsets.push(source.len() + "$ok->".len());
        source.push_str("$ok->run();}");
    }
    let mut parser = FileParser::new();
    parser.parse_full(&source);
    let tree = parser.tree().unwrap();
    let symbols = extract_file_symbols(tree, &source, "file:///many-callables.php");
    let refs = collect_symbol_references_in_file(tree, &source, &symbols);
    for offset in offsets {
        assert!(
            refs.iter().any(|r| !r.is_declaration
                && r.range.1 == offset as u32
                && r.target_fqn == "Known::run"),
            "missing exact native receiver at{offset}"
        );
    }
}
#[test]
fn foreach_body_reassignment_replaces_element_type() {
    let s="<?php class Item{} class Other{} /** @param Item[] $rows */ function walk(array $rows){foreach($rows as $value){$value=new Other;/*USE*/$value->run();}}";
    assert_eq!(inferred(s, "$value").as_deref(), Some("Other"));
}
#[test]
fn unknown_assignment_invalidates_old_exact_type() {
    let s="<?php class Known{} function walk($unknown){$value=new Known; $value=$unknown;/*USE*/$value->run();}";
    assert_eq!(inferred(s, "$value"), None);
}

#[test]
fn completed_output_argument_write_invalidates_the_old_type() {
    let source="<?php class Known{} function walk($text){$value=new Known;preg_match('/x/',$text,$value);/*USE*/$value;}";
    assert_eq!(inferred(source, "$value"), None);
}

#[test]
fn wide_pattern_lookups_share_the_inference_work_budget() {
    let mut source = String::from("<?php foreach($rows as [");
    source.push_str(
        &(0..1000)
            .map(|index| format!("$v{index}"))
            .collect::<Vec<_>>()
            .join(","),
    );
    source.push_str("]){ }");
    let mut parser = FileParser::new();
    parser.parse_full(&source);
    let tree = parser.tree().unwrap();
    let mut cursor = tree.root_node().walk();
    let statement = tree
        .root_node()
        .named_children(&mut cursor)
        .find(|node| node.kind() == "foreach_statement")
        .unwrap();
    let _guard = super::ObjectTypeResolveDepthGuard::enter().unwrap();
    for _ in 0..20 {
        let _ = super::foreach_binding(statement, "$v999", usize::MAX, &source);
    }
    assert_eq!(
        super::budget::remaining(),
        0,
        "pattern traversal escaped the inference budget"
    );
}
#[test]
fn unknown_foreach_value_invalidates_old_exact_type() {
    let s="<?php class Known{} function walk($unknown){$value=new Known;foreach($unknown as $value){/*USE*/$value->run();}}";
    assert_eq!(inferred(s, "$value"), None);
}
#[test]
fn scalar_assignment_invalidates_old_object_type() {
    let s = "<?php class Known{} function walk(){$value=new Known; $value=42;/*USE*/$value;}";
    assert!(inferred(s, "$value").is_none_or(|ty| ty != "Known"));
}
#[test]
fn control_same_key_and_value_name_matches_php_key_assignment() {
    let s="<?php class Item{} /** @param array<string,Item> $rows */ function walk(array $rows){foreach($rows as $entry=>$entry){/*USE*/$entry->run();}}";
    assert_eq!(inferred(s, "$entry").as_deref(), Some("string"));
}
#[test]
fn destructured_foreach_first_value_gets_its_field_type() {
    let s="<?php class Item{} class Other{} /** @param array<int,array{first:Item,second:Other}> $rows */ function walk(array $rows){foreach($rows as ['first'=>$first,'second'=>$second]){/*USE*/$first->run();}}";
    assert_eq!(inferred(s, "$first").as_deref(), Some("Item"));
}
#[test]
fn destructured_foreach_second_value_gets_its_field_type() {
    let s="<?php class Item{} class Other{} /** @param array<int,array{first:Item,second:Other}> $rows */ function walk(array $rows){foreach($rows as ['first'=>$first,'second'=>$second]){/*USE*/$second->run();}}";
    assert_eq!(inferred(s, "$second").as_deref(), Some("Other"));
}
#[test]
fn foreach_property_target_does_not_rebind_its_object() {
    let s="<?php class Item{} class Holder{public $child;} /** @param Item[] $rows */ function walk(array $rows){$holder=new Holder;foreach($rows as $holder->child){/*USE*/$holder->run();}}";
    assert_eq!(inferred(s, "$holder").as_deref(), Some("Holder"));
}
#[test]
fn foreach_subscript_target_does_not_rebind_its_base() {
    let s="<?php class Item{} class Holder extends ArrayObject{} /** @param Item[] $rows */ function walk(array $rows,Holder $holder){foreach($rows as $holder[0]){/*USE*/$holder->run();}}";
    assert_eq!(inferred(s, "$holder").as_deref(), Some("Holder"));
}
#[test]
fn nested_foreach_independent_function_keeps_its_native_parameter() {
    let s="<?php class Item{} class Other{} /** @param Item[] $rows */ function walk(array $rows){foreach($rows as $value){function separate(Other $value){/*USE*/$value->run();}}}";
    assert_eq!(inferred(s, "$value").as_deref(), Some("Other"));
}
#[test]
fn control_independent_function_without_shared_file_pressure() {
    let source = "<?php class Known{function run(){}} function healthy(Known $ok){$ok->run();}";
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let tree = parser.tree().unwrap();
    let symbols = extract_file_symbols(tree, source, "file:///budget-control.php");
    let refs = collect_symbol_references_in_file(tree, source, &symbols);
    assert!(refs
        .iter()
        .any(|r| !r.is_declaration && r.target_fqn == "Known::run"));
}
fn known_chain(length: usize) -> Option<String> {
    let mut source =
        String::from("<?php class Node{public Node $child;} function walk(Node $node){");
    for index in 0..length {
        let previous = if index == 0 {
            "$node".to_string()
        } else {
            format!("$node{}", index - 1)
        };
        source.push_str(&format!("$node{index}={previous}->child;"));
    }
    let variable = format!("$node{}", length - 1);
    source.push_str(&format!("/*USE*/{variable};}}"));
    inferred(&source, &variable)
}
#[test]
fn control_short_fully_typed_acyclic_chain() {
    assert_eq!(known_chain(1).as_deref(), Some("Node"));
}
#[test]
fn fully_typed_acyclic_chain_keeps_its_known_result() {
    assert_eq!(known_chain(7).as_deref(), Some("Node"));
}
#[test]
fn unrelated_function_keeps_exact_references_after_expensive_function() {
    let mut source =
        String::from("<?php class Known{function run(){}} function expensive(Known $value){");
    source.push_str(&"echo 0;".repeat(1000));
    source.push_str(&"$value->run();".repeat(100));
    source.push_str("} function healthy(Known $ok){$ok->run();}");
    let mut parser = FileParser::new();
    parser.parse_full(&source);
    let tree = parser.tree().unwrap();
    let symbols = extract_file_symbols(tree, &source, "file:///budget-file.php");
    let refs = collect_symbol_references_in_file(tree, &source, &symbols);
    let offset = source.rfind("run();").unwrap() as u32;
    let actual = refs
        .iter()
        .find(|r| !r.is_declaration && r.range.1 == offset)
        .unwrap();
    assert_eq!(
        actual.target_fqn, "Known::run",
        "healthy function lost exact target: {actual:?}"
    );
}

#[test]
fn reference_walk_keeps_original_aliases_across_repeated_namespace_sections() {
    let source = r#"<?php
namespace Model { class First { function run() {} } class Second { function run() {} } }
namespace App {
    /** @phpstan-type Rows array<int, Item> */
    use Model\First as Item;
    // Local inference must retain this section's file-level alias.
    function first() { /** @var Rows $rows */ $rows=[]; foreach ($rows as $value) { $value->run(); } }
}
namespace App {
    /** @phpstan-type Rows array<int, Item> */
    use Model\Second as Item;
    // The repeated namespace imports a different Item.
    function second() { /** @var Rows $rows */ $rows=[]; foreach ($rows as $value) { $value->run(); } }
}
namespace { function global_call(\Model\First $value) { $value->run(); } }
"#;
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let tree = parser.tree().unwrap();
    assert!(!tree.root_node().has_error());
    let file = extract_file_symbols(tree, source, "file:///section-references.php");
    assert_eq!(file.type_aliases.len(), 2);
    let refs = collect_symbol_references_in_file(tree, source, &file);
    let targets: Vec<_> = refs
        .iter()
        .filter(|reference| {
            !reference.is_declaration
                && reference.target_kind == php_lsp_types::PhpSymbolKind::Method
        })
        .map(|reference| (reference.range.0, reference.target_fqn.as_str()))
        .collect();
    assert_eq!(
        targets,
        vec![
            (6, "Model\\First::run"),
            (14, "Model\\First::run"),
            (12, "Model\\Second::run")
        ]
    );
}
