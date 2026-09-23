use super::*;
use crate::{parser::FileParser, symbols::extract_file_symbols};

#[test]
fn scalar_exit_guard_rejects_reference_exposing_callables() {
    for source in [
        "<?php function &test(A|false $value) { if ($value === false) return; yield $value; $value->method(); }",
        "<?php class C { function __construct(public A|false &$value) { if ($value === false) return; $this->value = false; $value->method(); } }",
    ] {
        let mut parser = FileParser::new();
        parser.parse_full(source);
        let tree = parser.tree().unwrap();
        assert!(!tree.root_node().has_error());
        let symbols = extract_file_symbols(tree, source, "file:///references.php");
        let mut node = tree.root_node().descendant_for_byte_range(source.find("$value->").unwrap(), source.find("$value->").unwrap()).unwrap();
        while !matches!(node.kind(), "function_definition" | "method_declaration") {
            node = node.parent().unwrap();
        }
        let ty = parse_phpdoc("/** @var A|false */").var_type.unwrap();
        assert!(narrow_type_after_exit_guards(&ty, node, "$value", source.find("$value->").unwrap(), source, &symbols).is_none(), "an external reference can mutate the local binding: {source}");
    }
}

#[test]
fn scalar_exit_guard_rejects_aliases_dynamic_bindings_and_back_edges() {
    let mut failures = Vec::new();
    for body in [
        "if ($value === false) return; $name = 'value'; $$name = false; $value->method();",
        "if ($value === false) return; ${'value'} = false; $value->method();",
        "if ($value === false) return; eval('$value = false;'); $value->method();",
        "if ($value === false) return; while (true) { $value->method(); $value = false; }",
        "if ($value === false) return; again: $value->method(); $value = false; goto again;",
        "$aliases = [&$value]; if ($value === false) return; $aliases[0] = false; $value->method();",
        "include 'alias.php'; if ($value === false) return; $alias = false; $value->method();",
        "extract($bindings, EXTR_REFS); if ($value === false) return; $bindings['value'] = false; $value->method();",
        "if ($value === false) return; assert('$value = false'); $value->method();",
        "if ($value === false) return; call_user_func('extract', ['value' => false]); $value->method();",
        "if ($value === false) return; try { throw new \\Exception(); } catch (\\Exception $value) {} $value->method();",
        "try { throw new \\Exception(); } catch (\\Exception $value) {} if ($value === false) return; $value->method();",
    ] {
        let source = format!("<?php function test(A|false $value) {{ {body} }}");
        let mut parser = FileParser::new();
        parser.parse_full(&source);
        let tree = parser.tree().unwrap();
        assert!(!tree.root_node().has_error(), "{body}");
        let symbols = extract_file_symbols(tree, &source, "file:///guards.php");
        let mut cursor = tree.root_node().walk();
        let scope = tree.root_node().named_children(&mut cursor)
            .find(|node| node.kind() == "function_definition").unwrap();
        let ty = parse_phpdoc("/** @var A|false */").var_type.unwrap();
        if narrow_type_after_exit_guards(&ty, scope, "$value", source.find("$value->").unwrap(), &source, &symbols).is_some() {
            failures.push(body);
        }
    }
    assert!(failures.is_empty(), "unsafe narrowing: {failures:#?}");
}

#[test]
fn composite_callback_serialization_preserves_grouping_inside_every_container() {
    for text in [
        "(A|B)&(A|C)",
        "array{item:(A|B)&(A|C)}",
        "object{item:(A|B)&C}",
        "Box<(A|B)&C>",
        "class-string<(A|B)&C>",
        "callable((A|B)&C):(A|B)&D",
    ] {
        let ty = parse_phpdoc(&format!("/** @var {text} */"))
            .var_type
            .unwrap();
        let encoded = receiver_type_text(&ty);
        assert_eq!(
            parse_phpdoc(&format!("/** @var {encoded} */")).var_type,
            Some(ty),
            "lost grouping in {text}: {encoded}"
        );
    }
}

#[test]
fn composite_receivers_preserve_type_info_without_claiming_a_single_class() {
    for native in [false, true] {
        for text in ["A&B", "B&A", "A|B", "B|A", "(A&B)|C", "A|int"] {
            let declaration = if native {
                format!("function test({text} $value) {{")
            } else {
                format!("function test($value) {{\n/** @var {text} $value */")
            };
            let source = format!("<?php\nnamespace Test;\n{declaration}\n$value->method();\n}}\n");
            let mut parser = FileParser::new();
            parser.parse_full(&source);
            let tree = parser.tree().unwrap();
            let symbols = extract_file_symbols(tree, &source, "file:///composite.php");
            let offset = source.rfind("$value->").unwrap();
            let line = source[..offset]
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count() as u32;
            let full =
                infer_variable_type_info_at_position(tree, &source, &symbols, line, 2, "$value");
            assert!(
                matches!(full, Some(TypeInfo::Union(_) | TypeInfo::Intersection(_))),
                "{text} native={native}: {full:?}"
            );
            let single =
                infer_variable_type_at_position(tree, &source, &symbols, line, 2, "$value");
            assert!(
                single.is_none(),
                "composite {text} native={native} falsely narrowed to {single:?}, full={full:?}"
            );
        }
    }
}
