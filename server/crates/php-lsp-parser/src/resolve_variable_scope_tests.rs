use super::*;
use crate::parser::FileParser;

fn position(source: &str, marker: &str) -> (u32, u32, usize) {
    let offset = source.find(marker).unwrap() + marker.len();
    let prefix = &source[..offset];
    (
        prefix.bytes().filter(|byte| *byte == b'\n').count() as u32,
        prefix.rsplit('\n').next().unwrap().len() as u32,
        offset,
    )
}

fn definition(source: &str) -> Option<(u32, u32, u32, u32)> {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    assert!(!parser.tree().unwrap().root_node().has_error(), "{source}");
    let (line, col, _) = position(source, "/*USE*/");
    variable_definition_at_position(parser.tree().unwrap(), source, line, col)
}

fn expected_definition(source: &str) -> (u32, u32, u32, u32) {
    let (line, col, offset) = position(source, "/*DEF*/");
    let name = &source[offset..];
    let length = name
        .chars()
        .take_while(|ch| ch.is_alphanumeric() || matches!(ch, '$' | '_'))
        .map(char::len_utf8)
        .sum::<usize>() as u32;
    (line, col, line, col + length)
}

fn inferred(source: &str) -> Option<String> {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let tree = parser.tree().unwrap();
    let file = crate::symbols::extract_file_symbols(tree, source, "file:///scope.php");
    let (line, col, _) = position(source, "/*USE*/");
    infer_variable_type_at_position(tree, source, &file, line, col, "$value")
}

#[test]
fn outer_definition_never_uses_a_nested_callable_assignment() {
    for nested in [
        "$fn=function() {$value=new Wrong;};",
        "function nested() {$value=new Wrong;}",
        "$fn=fn($value)=>($value=new Wrong);",
    ] {
        let source=format!("<?php class Right {{}} class Wrong {{}} function outer() {{ /*DEF*/$value=new Right; {nested} echo /*USE*/$value; }}");
        assert_eq!(
            definition(&source),
            Some(expected_definition(&source)),
            "{nested}"
        );
        assert_eq!(inferred(&source).as_deref(), Some("Right"), "{nested}");
    }
}

#[test]
fn uncaptured_closures_and_named_functions_do_not_borrow_outer_bindings() {
    for nested in [
        "$fn=function() {echo /*USE*/$value;};",
        "function nested() {echo /*USE*/$value;}",
    ] {
        let source =
            format!("<?php class Right {{}} function outer() {{$value=new Right; {nested}}}");
        assert_eq!(definition(&source), None, "{nested}");
        assert_eq!(inferred(&source), None, "{nested}");
    }
}

#[test]
fn closure_and_arrow_capture_resolve_parent_definition_and_type() {
    for nested in [
        "$fn=function() use($value) {echo /*USE*/$value;};",
        "$fn=static function() use($value) {echo /*USE*/$value;};",
        "$fn=static fn()=> /*USE*/$value;",
        "$fn=function() use(&$value) {echo /*USE*/$value;};",
        "$fn=fn()=> /*USE*/$value;",
    ] {
        let source = format!(
            "<?php class Right {{}} function outer() {{/*DEF*/$value=new Right; {nested}}}"
        );
        assert_eq!(
            definition(&source),
            Some(expected_definition(&source)),
            "{nested}"
        );
        assert_eq!(
            inferred(&source).as_deref(),
            if nested.contains("use(&") {
                None
            } else {
                Some("Right")
            },
            "{nested}"
        );
    }
}

#[test]
fn capture_creation_never_uses_later_parent_assignments() {
    for nested in [
        "$fn=function() use($value) {echo /*USE*/$value;};",
        "$fn=fn()=> /*USE*/$value;",
    ] {
        let source=format!("<?php class Right {{}} class Wrong {{}} function outer() {{/*DEF*/$value=new Right; {nested} $value=new Wrong;}}");
        assert_eq!(
            definition(&source),
            Some(expected_definition(&source)),
            "{nested}"
        );
        assert_eq!(inferred(&source).as_deref(), Some("Right"), "{nested}");
    }
}

#[test]
fn shadowing_parameter_wins_over_parent_capture() {
    for nested in [
        "$fn=fn(Right /*DEF*/$value)=> /*USE*/$value;",
        "$fn=function(Right /*DEF*/$value) {echo /*USE*/$value;};",
    ] {
        let source=format!("<?php class Right {{}} class Wrong {{}} function outer() {{$value=new Wrong; {nested}}}");
        assert_eq!(
            definition(&source),
            Some(expected_definition(&source)),
            "{nested}"
        );
        assert_eq!(inferred(&source).as_deref(), Some("Right"), "{nested}");
    }
}

#[test]
fn nested_capture_chain_stops_at_an_uncaptured_closure() {
    let connected="<?php class Right {} function outer() {/*DEF*/$value=new Right; $a=function() use($value) {$b=fn()=> /*USE*/$value;};}";
    assert_eq!(definition(connected), Some(expected_definition(connected)));
    assert_eq!(inferred(connected).as_deref(), Some("Right"));
    let disconnected = connected.replace("use($value)", "");
    assert_eq!(definition(&disconnected), None);
    assert_eq!(inferred(&disconnected), None);
}

#[test]
fn captured_local_reassignment_is_visible_only_inside_its_callable() {
    for capture in ["$value", "&$value"] {
        let source=format!("<?php class Right {{}} class Wrong {{}} function outer() {{$value=new Wrong; $fn=function() use({capture}) {{/*DEF*/$value=new Right; echo /*USE*/$value;}};}}");
        assert_eq!(definition(&source), Some(expected_definition(&source)));
        assert_eq!(inferred(&source).as_deref(), Some("Right"));
    }
}

#[test]
fn by_reference_assignment_and_destructuring_define_the_written_variable() {
    for assignment in [
        "/*DEF*/$value =& $other;",
        "[/*DEF*/$value]=$items;",
        "list(/*DEF*/$value)=$items;",
        "['key'=>[/*DEF*/$value]]=$items;",
        "foreach($items as [/*DEF*/$value]) {}",
        "foreach($items as &/*DEF*/$value) {}",
    ] {
        let source =
            format!("<?php function outer($other,$items) {{{assignment} echo /*USE*/$value;}}");
        assert_eq!(
            definition(&source),
            Some(expected_definition(&source)),
            "{assignment}"
        );
    }
}

#[test]
fn destructuring_dynamic_keys_are_reads_instead_of_variable_definitions() {
    let source="<?php function outer($items) {/*DEF*/$value='key'; [$value=>$other]=$items; echo /*USE*/$value;}";
    assert_eq!(definition(source), Some(expected_definition(source)));
}

#[test]
fn arrow_local_rhs_is_visible_inside_expression_without_leaking_outward() {
    let source = "<?php class Right {} $fn=fn()=> (/*DEF*/$value=new Right) && /*USE*/$value;";
    assert_eq!(definition(source), Some(expected_definition(source)));
    assert_eq!(inferred(source).as_deref(), Some("Right"));
    let outer = "<?php class Wrong {} $fn=fn()=> ($value=new Wrong); echo /*USE*/$value;";
    assert_eq!(definition(outer), None);
    assert_eq!(inferred(outer), None);
}

#[test]
fn anonymous_class_constructor_arguments_execute_in_the_outer_variable_scope() {
    let source="<?php class Right {} class Wrong {} function outer() {$object=new class(/*DEF*/$value=new Right) {function run() {$value=new Wrong;}}; echo /*USE*/$value;}";
    assert_eq!(definition(source), Some(expected_definition(source)));
    assert_eq!(inferred(source).as_deref(), Some("Right"));
}

#[test]
fn destructuring_values_and_reference_aliases_infer_the_written_type() {
    for assignment in [
        "$value =& $other;",
        "[$value]=[new Right];",
        "list($value)=[new Right];",
        "[, $value]=[new Wrong,new Right];",
        "[2=>$value]=[new Wrong,new Wrong,new Right];",
        "[&$value]=[new Right];",
        "['key'=>[$value]]=['key'=>[new Right]];",
    ] {
        let source=format!("<?php class Right {{}} class Wrong {{}} function outer(Right $other) {{{assignment} echo /*USE*/$value;}}");
        assert_eq!(inferred(&source).as_deref(), Some("Right"), "{assignment}");
    }
}

#[test]
fn unknown_destructuring_write_does_not_keep_an_earlier_object_type() {
    let source="<?php class Wrong {} function outer($items) {$value=new Wrong; [$value]=$items; echo /*USE*/$value;}";
    assert_eq!(inferred(source), None);
}

#[test]
fn cached_capture_call_metadata_keeps_the_outer_receiver_type() {
    let source="<?php class Right {function run(){}} function outer() {$value=new Right; $fn=function() use($value) {$value->run();};}";
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let tree = parser.tree().unwrap();
    let file = crate::symbols::extract_file_symbols(tree, source, "file:///scope.php");
    let refs = crate::references::collect_symbol_references_in_file(tree, source, &file);
    assert!(
        refs.iter()
            .any(|r| !r.is_declaration && r.target_fqn == "Right::run"),
        "capture receiver lost: {refs:?}"
    );
}

#[test]
fn reference_capture_does_not_freeze_a_parent_value_before_later_writes() {
    let source="<?php class Wrong {} class Right {} function outer() {/*DEF*/$value=new Wrong; $fn=function() use(&$value) {echo /*USE*/$value;}; $value=new Right; $fn();}";
    assert_eq!(definition(source), Some(expected_definition(source)));
    assert_eq!(
        inferred(source),
        None,
        "reference capture is not a value snapshot"
    );
    let by_value = source.replace("use(&$value)", "use($value)");
    assert_eq!(inferred(&by_value).as_deref(), Some("Wrong"));
}

#[test]
fn capture_definition_excludes_the_unfinished_containing_assignment() {
    for callable in [
        "fn()=> /*USE*/$value",
        "function() use($value) {return /*USE*/$value;}",
    ] {
        let source = format!("<?php class Right {{}} /*DEF*/$value=new Right; $value={callable};");
        assert_eq!(
            definition(&source),
            Some(expected_definition(&source)),
            "{callable}"
        );
        assert_eq!(inferred(&source).as_deref(), Some("Right"), "{callable}");
        let undefined = format!("<?php $value={callable};");
        assert_eq!(
            definition(&undefined),
            None,
            "unfinished assignment: {callable}"
        );
        assert_eq!(
            inferred(&undefined),
            None,
            "unfinished assignment: {callable}"
        );
    }
}

#[test]
fn captures_keep_completed_writes_in_earlier_rhs_elements() {
    for callable in [
        "fn()=> /*USE*/$value",
        "function() use($value) {return /*USE*/$value;}",
    ] {
        for target in ["$bundle", "$value"] {
            let source=format!("<?php class Right {{}} class Wrong {{}} $value=new Wrong; {target}=[(/*DEF*/$value=new Right),{callable}];");
            assert_eq!(
                definition(&source),
                Some(expected_definition(&source)),
                "{target}: {callable}"
            );
            assert_eq!(
                inferred(&source).as_deref(),
                Some("Right"),
                "{target}: {callable}"
            );
            let mut parser = FileParser::new();
            parser.parse_full(&source);
            let (line, col, _) = position(&source, "/*USE*/");
            let variables =
                local_variable_names_at_position(parser.tree().unwrap(), &source, line, col);
            assert!(
                variables.iter().any(|name| name == "$value"),
                "completed binding absent: {variables:?}"
            );
            assert!(
                !variables.iter().any(|name| name == "$bundle"),
                "unfinished outer binding: {variables:?}"
            );
        }
    }
}
