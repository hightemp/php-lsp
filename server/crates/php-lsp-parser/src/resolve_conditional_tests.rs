use super::*;
use crate::{parser::FileParser, symbols::extract_file_symbols};

fn at_call(type_text: &str, source: &str) -> TypeInfo {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let tree = parser.tree().unwrap();
    assert!(!tree.root_node().has_error(), "{source}");
    let symbols = extract_file_symbols(tree, source, "file:///condition.php");
    let offset = source.rfind("helper(").unwrap();
    let mut call = tree
        .root_node()
        .named_descendant_for_byte_range(offset, offset + 1)
        .unwrap();
    while call.kind() != "function_call_expression" {
        call = call.parent().unwrap();
    }
    let signature = symbols
        .symbols
        .iter()
        .find(|s| s.name == "helper")
        .unwrap()
        .signature
        .as_ref()
        .unwrap();
    resolve_function_call_return_type_at_call_site(
        &crate::phpdoc::parse_type_string(type_text),
        Some(signature),
        &HashSet::new(),
        call,
        source,
        &symbols,
        None,
        None,
        None,
    )
}

#[test]
fn unknown_parser_subject_keeps_both_branches_instead_of_selecting_else() {
    let source = "<?php function helper($value){} helper($unknown);";
    assert_eq!(
        at_call("($value is string ? int : bool)", source),
        TypeInfo::Union(vec![
            TypeInfo::Simple("int".into()),
            TypeInfo::Simple("bool".into())
        ])
    );
}

#[test]
fn ordinary_class_names_are_not_invented_conditional_templates() {
    let source = "<?php class Foo {} class Bar {} function helper($value){} helper(Bar::class);";
    let result = at_call("($value is class-string<Foo> ? string : int)", source);
    assert_ne!(
        result,
        TypeInfo::Simple("string".into()),
        "Foo must not become an inferred template bound to Bar"
    );
}

#[test]
fn parser_default_literal_keeps_its_value_for_conditional_matching() {
    let source = "<?php function helper($value='ready'){} helper();";
    assert_eq!(
        at_call("($value is 'ready' ? int : bool)", source),
        TypeInfo::Simple("int".into())
    );
}

#[test]
fn nested_parser_conditionals_resolve_after_the_outer_branch() {
    let source = "<?php function helper($value,$flag){} helper('ready',true);";
    assert_eq!(
        at_call(
            "($value is string ? ($flag is bool ? int : float) : string)",
            source
        ),
        TypeInfo::Simple("int".into())
    );
}

#[test]
fn an_unresolved_conditional_is_not_projected_to_its_first_object_branch() {
    let source = "<?php $value;";
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let symbols = extract_file_symbols(
        parser.tree().unwrap(),
        source,
        "file:///unknown-conditional.php",
    );
    let ty = crate::phpdoc::parse_type_string("($x is string ? Hit : Miss)");
    assert_eq!(
        object_fqn_from_resolved_member_type_info(
            &ty,
            parser.tree().unwrap().root_node(),
            source,
            &symbols
        ),
        None
    );
    assert_eq!(
        resolver_type_info_to_object_fqn(&ty, parser.tree().unwrap().root_node(), source, &symbols),
        None
    );
    assert_eq!(
        resolve_phpdoc_var_type(&ty, parser.tree().unwrap().root_node(), source, &symbols),
        None
    );
}

#[test]
fn unresolved_conditional_iterables_merge_both_key_and_value_branches() {
    let ty =
        crate::phpdoc::parse_type_string("($x is string ? array<int, Hit> : array<string, Miss>)");
    assert_eq!(
        iterable_value_type_info(&ty, None),
        Some(TypeInfo::Union(vec![
            TypeInfo::Simple("Hit".into()),
            TypeInfo::Simple("Miss".into())
        ]))
    );
    assert_eq!(
        iterable_key_type_info(&ty),
        Some(TypeInfo::Union(vec![
            TypeInfo::Simple("int".into()),
            TypeInfo::Simple("string".into())
        ]))
    );
}

#[test]
fn local_method_calls_specialize_before_resolving_a_member_receiver() {
    for (argument, expected) in [("42", "Miss::common"), ("'value'", "Hit::common")] {
        let source=format!("<?php class Hit {{function common(){{}}}} class Miss {{function common(){{}}}} class Choice {{/** @return ($value is string ? Hit : Miss) */ function choose($value){{}}}} $result=(new Choice())->choose({argument}); $result->common();");
        let mut parser = FileParser::new();
        parser.parse_full(&source);
        let symbols = extract_file_symbols(
            parser.tree().unwrap(),
            &source,
            "file:///method-conditional.php",
        );
        let offset = source.rfind("common").unwrap();
        let symbol = symbol_at_position(
            parser.tree().unwrap(),
            &source,
            0,
            offset as u32 + 1,
            &symbols,
        )
        .unwrap();
        assert_eq!(symbol.fqn, expected);
    }
}

#[test]
fn local_method_returns_specialize_nested_container_conditionals() {
    let source="<?php class Hit {} class Miss {} class Choice {/** @return array<int, ($value is string ? Hit : Miss)> */ function choose($value){}} (new Choice())->choose(42);";
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let symbols = extract_file_symbols(parser.tree().unwrap(), source, "file:///nested-method.php");
    let offset = source.rfind("choose(").unwrap();
    let mut call = parser
        .tree()
        .unwrap()
        .root_node()
        .named_descendant_for_byte_range(offset, offset + 1)
        .unwrap();
    while call.kind() != "member_call_expression" {
        call = call.parent().unwrap();
    }
    let ty = infer_expression_type_info(call, source, &symbols, None, None).unwrap();
    assert_eq!(
        iterable_value_type_info(&ty, None),
        Some(TypeInfo::Simple("Miss".into()))
    );
}

#[test]
fn local_method_conditions_use_proven_declaring_class_relations() {
    let source="<?php class Hit {} class Miss {} class Base {} class Child extends Base {} class Choice {/** @return ($value is Base ? Hit : Miss) */ static function choose($value){}} Choice::choose(new Child);";
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let symbols =
        extract_file_symbols(parser.tree().unwrap(), source, "file:///subtype-method.php");
    let offset = source.rfind("choose(").unwrap();
    let mut call = parser
        .tree()
        .unwrap()
        .root_node()
        .named_descendant_for_byte_range(offset, offset + 1)
        .unwrap();
    while call.kind() != "scoped_call_expression" {
        call = call.parent().unwrap();
    }
    assert_eq!(
        infer_expression_type_info(call, source, &symbols, None, None),
        Some(TypeInfo::Simple("Hit".into()))
    );
}
