use super::*;

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
