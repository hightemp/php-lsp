use super::*;

#[test]
fn server_assignment_lookup_excludes_a_proven_exiting_then_arm() {
    for (body, expected) in [
        ("$value=new Wrong; return;", None),
        ("$value=new Right; work();", Some("new Right")),
        ("$value=new Right; goto end; return;", Some("new Right")),
        (
            "$value=new Right; while($flag){goto end;} return;",
            Some("new Right"),
        ),
    ] {
        let source = format!(
            "<?php function run($value,$flag){{if($flag){{{body}}} end: echo /*USE*/$value;}}"
        );
        let mut parser = FileParser::new();
        parser.parse_full(&source);
        let tree = parser.tree().unwrap();
        let offset = source.find("/*USE*/").unwrap();
        let mut node = tree
            .root_node()
            .named_descendant_for_byte_range(offset, offset + 1)
            .unwrap();
        while node.kind() != "function_definition" {
            node = node.parent().unwrap();
        }
        let found = latest_assignment_rhs_before_usage(node, "$value", offset, &source);
        assert_eq!(
            found.map(|(_, rhs)| &source[rhs.byte_range()]),
            expected,
            "{source}"
        );
    }
}

#[test]
fn server_variable_scope_finds_the_nearest_arrow_or_closure() {
    for expression in [
        "fn()=>($value=new Model) && $value",
        "function() {$value=new Model; return $value;}",
    ] {
        let source = format!("<?php function outer() {{$fn={expression};}}");
        let mut parser = FileParser::new();
        parser.parse_full(&source);
        let tree = parser.tree().unwrap();
        let offset = source.rfind("$value").unwrap();
        let mut node = tree
            .root_node()
            .named_descendant_for_byte_range(offset, offset + 1)
            .unwrap();
        while node.kind() != "variable_name" {
            node = node.parent().unwrap();
        }
        let expected = if expression.starts_with("fn") {
            "arrow_function"
        } else {
            "anonymous_function"
        };
        assert_eq!(
            local_variable_scope_node(node).kind(),
            expected,
            "{expression}"
        );
        assert!(
            latest_assignment_rhs_before_usage(
                local_variable_scope_node(node),
                "$value",
                offset,
                &source
            )
            .is_some(),
            "own RHS invisible: {expression}"
        );
    }
}
