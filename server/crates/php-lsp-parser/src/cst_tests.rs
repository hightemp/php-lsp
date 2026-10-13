use super::*;
use crate::parser::FileParser;

fn find_variable_nodes<'tree>(
    node: Node<'tree>,
    source: &str,
    name: &str,
    out: &mut Vec<Node<'tree>>,
) {
    if node.kind() == "variable_name" && &source[node.byte_range()] == name {
        out.push(node);
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        find_variable_nodes(child, source, name, out);
    }
}

fn variable_nodes<'tree>(root: Node<'tree>, source: &str, name: &str) -> Vec<Node<'tree>> {
    let mut nodes = Vec::new();
    find_variable_nodes(root, source, name, &mut nodes);
    nodes
}

#[test]
fn by_ref_output_argument_detection_covers_positional_and_named_preg_match_calls() {
    let source = r#"<?php
preg_match('/x/', $text, $matches);
preg_match_all('/x/', $text, matches: $allMatches);
other($notOutput);
"#;
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let root = parser.tree().unwrap().root_node();

    let matches = variable_nodes(root, source, "$matches");
    assert_eq!(matches.len(), 1);
    assert!(is_by_ref_output_argument_variable(matches[0], source));

    let all_matches = variable_nodes(root, source, "$allMatches");
    assert_eq!(all_matches.len(), 1);
    assert!(is_by_ref_output_argument_variable(all_matches[0], source));

    let not_output = variable_nodes(root, source, "$notOutput");
    assert_eq!(not_output.len(), 1);
    assert!(!is_by_ref_output_argument_variable(not_output[0], source));
}

#[test]
fn argument_helpers_parse_named_and_positional_arguments() {
    let source = "<?php\npreg_match_all(pattern: '/x/', subject: $text, matches: $matches);\n";
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let root = parser.tree().unwrap().root_node();
    let matches = variable_nodes(root, source, "$matches");
    assert_eq!(matches.len(), 1);

    let argument = ancestor_before_scope(matches[0], "argument").unwrap();
    let arguments = argument.parent().unwrap();

    assert_eq!(argument_index(arguments, argument), Some(2));
    assert_eq!(argument_name(argument, source).as_deref(), Some("matches"));
}

#[test]
fn ancestor_before_scope_stops_at_function_boundaries() {
    let source = r#"<?php
$outer = function () use ($captured) {
    return $captured;
};
"#;
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let root = parser.tree().unwrap().root_node();
    let captured = variable_nodes(root, source, "$captured");
    assert_eq!(captured.len(), 2);

    assert!(has_ancestor_before_scope(
        captured[0],
        "anonymous_function_use_clause"
    ));
    assert!(!has_ancestor_before_scope(
        captured[1],
        "anonymous_function_use_clause"
    ));
}

#[test]
fn p232_foreign_preg_match_calls_do_not_write_output_arguments() {
    for (declaration, call) in [
        ("", "\\Vendor\\preg_match"),
        ("", "namespace\\preg_match"),
        ("function preg_match($a,$b,$c) {}", "preg_match"),
        ("use function Vendor\\preg_match;", "preg_match"),
        ("use function Vendor\\preg_match as rx;", "rx"),
    ] {
        let source=format!("<?php namespace App; {declaration} function run($text){{{call}('/x/',$text,$matches);}}");
        let mut parser = FileParser::new();
        parser.parse_full(&source);
        let nodes = variable_nodes(parser.tree().unwrap().root_node(), &source, "$matches");
        assert_eq!(nodes.len(), 1);
        assert!(
            !is_by_ref_output_argument_variable(nodes[0], &source),
            "foreign output {call}"
        );
    }
}

#[test]
fn p232_imported_global_preg_match_alias_keeps_output_binding() {
    let source="<?php namespace App; use function preg_match as rx; function run($text){RX('/x/',$text,matches:$matches);}";
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let nodes = variable_nodes(parser.tree().unwrap().root_node(), source, "$matches");
    assert!(is_by_ref_output_argument_variable(nodes[0], source));
}

#[test]
fn p232_named_non_output_parameter_is_not_positional_output() {
    let source = "<?php preg_match('/x/',$text,flags:$flags);";
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let nodes = variable_nodes(parser.tree().unwrap().root_node(), source, "$flags");
    assert!(!is_by_ref_output_argument_variable(nodes[0], source));
}
