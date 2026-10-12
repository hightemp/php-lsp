use super::*;

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
