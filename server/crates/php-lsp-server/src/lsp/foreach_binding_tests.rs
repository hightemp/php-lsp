use super::*;

fn has_foreach_binding(source: &str) -> bool {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let offset = source.find("/*USE*/").unwrap() + 7;
    let mut node = parser
        .tree()
        .unwrap()
        .root_node()
        .named_descendant_for_byte_range(offset, offset + 1)
        .unwrap();
    while node.kind() != "variable_name" {
        node = node.parent().unwrap();
    }
    enclosing_foreach_statement_for_variable(source, node).is_some()
}

#[test]
fn foreach_binding_respects_independent_callable_parameters_and_missing_captures() {
    for body in [
        "function separate(Other $value){/*USE*/$value;}",
        "$fn=function(Other $value){/*USE*/$value;};",
        "$fn=fn(Other $value)=>/*USE*/$value;",
        "$fn=function(){/*USE*/$value;};",
        "$obj=new class{function run(Other $value){/*USE*/$value;}};",
    ] {
        let source = format!("<?php foreach($rows as $value){{{body}}}");
        assert!(
            !has_foreach_binding(&source),
            "foreign foreach binding: {body}"
        );
    }
}

#[test]
fn foreach_binding_preserves_explicit_and_implicit_by_value_captures() {
    for body in [
        "$fn=function()use($value){/*USE*/$value;};",
        "$fn=fn()=>/*USE*/$value;",
    ] {
        let source = format!("<?php foreach($rows as $value){{{body}}}");
        assert!(has_foreach_binding(&source), "lost capture: {body}");
    }
}

#[test]
fn foreach_binding_does_not_override_later_writes_or_reference_captures() {
    for body in [
        "$value=new Other;/*USE*/$value;",
        "$value=$unknown;/*USE*/$value;",
        "$value=&$unknown;/*USE*/$value;",
        "unset($value);/*USE*/$value;",
        "$fn=function()use(&$value){/*USE*/$value;};",
    ] {
        let source = format!("<?php foreach($rows as $value){{{body}}}");
        assert!(
            !has_foreach_binding(&source),
            "stale foreach binding: {body}"
        );
    }
}

#[test]
fn foreach_binding_does_not_rebind_property_or_offset_owners() {
    for target in ["$value->child", "$value[0]"] {
        let source = format!("<?php foreach($rows as {target}){{/*USE*/$value;}}");
        assert!(
            !has_foreach_binding(&source),
            "owner treated as binding: {target}"
        );
    }
}

#[test]
fn aborted_statement_scan_does_not_publish_an_earlier_assignment() {
    for body in [
        format!("{}echo 0;{}", "if($flag){".repeat(128), "}".repeat(128)),
        "echo 0;".repeat(5000),
    ] {
        let source=format!("<?php function walk($flag){{$value=new Known;{body}$value=new NewType;/*USE*/$value;}}");
        let mut parser = FileParser::new();
        parser.parse_full(&source);
        let tree = parser.tree().unwrap();
        let usage = source.find("/*USE*/").unwrap() + 7;
        let mut node = tree
            .root_node()
            .named_descendant_for_byte_range(usage, usage + 1)
            .unwrap();
        while node.kind() != "function_definition" {
            node = node.parent().unwrap();
        }
        assert!(
            latest_assignment_rhs_before_usage(node, "$value", usage, &source).is_none(),
            "partial old type survived aborted scan"
        );
    }
    let source = "<?php function walk(){$value=new Current;/*USE*/$value;}";
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let tree = parser.tree().unwrap();
    let usage = source.find("/*USE*/").unwrap() + 7;
    let mut node = tree
        .root_node()
        .named_descendant_for_byte_range(usage, usage + 1)
        .unwrap();
    while node.kind() != "function_definition" {
        node = node.parent().unwrap();
    }
    let (_, rhs) = latest_assignment_rhs_before_usage(node, "$value", usage, source).unwrap();
    assert_eq!(&source[rhs.byte_range()], "new Current");
}

#[test]
fn foreach_value_binding_does_not_cover_the_earlier_iterable_receiver() {
    let source = "<?php function walk($node){foreach(/*ITER*/$node->children as /*BIND*/$node){/*BODY*/$node->run();}}";
    let mut parser = FileParser::new();
    parser.parse_full(source);
    for (marker, expected) in [("/*ITER*/", false), ("/*BIND*/", true), ("/*BODY*/", true)] {
        let offset = source.find(marker).unwrap() + marker.len();
        let mut node = parser
            .tree()
            .unwrap()
            .root_node()
            .named_descendant_for_byte_range(offset, offset + 1)
            .unwrap();
        while node.kind() != "variable_name" {
            node = node.parent().unwrap();
        }
        assert_eq!(
            enclosing_foreach_statement_for_variable(source, node).is_some(),
            expected,
            "{marker}"
        );
    }
}
