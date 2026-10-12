use super::*;
use crate::{parser::FileParser, symbols::extract_file_symbols};

fn inferred(body: &str) -> Option<String> {
    let source=format!("<?php interface Input {{}} class Foo implements Input {{function onlyFoo(){{}}}} class Other implements Input {{}} function run(Input $value, bool $flag, bool $other) {{{body} echo /*USE*/$value;}}");
    infer_source(&source)
}

fn infer_source(source: &str) -> Option<String> {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let tree = parser.tree().unwrap();
    assert!(
        !tree.root_node().has_error(),
        "{source}\n{}",
        tree.root_node().to_sexp()
    );
    let symbols = extract_file_symbols(tree, source, "file:///guards.php");
    let offset = source.find("/*USE*/").unwrap() + "/*USE*/".len();
    let prefix = &source[..offset];
    let line = prefix.bytes().filter(|c| *c == b'\n').count() as u32;
    let col = prefix.rsplit('\n').next().unwrap().len() as u32;
    infer_variable_type_at_position(tree, source, &symbols, line, col, "$value")
}

#[test]
fn post_guard_conjunction_does_not_prove_the_instanceof_operand() {
    for condition in [
        "!($value instanceof Foo) && $flag",
        "!$value instanceof Foo and $flag",
        "!($value instanceof Foo) && ($flag || $other)",
        "!($value instanceof Foo) xor $flag",
        "!($value instanceof Foo) & $flag",
    ] {
        let body = format!("if ({condition}) return;");
        assert_eq!(inferred(&body).as_deref(), Some("Input"), "{condition}");
    }
}

#[test]
fn post_guard_disjunction_and_demorgan_conditions_prove_the_remaining_type() {
    for condition in [
        "!($value instanceof Foo) || $flag",
        "$flag || !($value instanceof Foo)",
        "!$value instanceof Foo or $flag",
        "!(($value instanceof Foo) && $flag)",
        "!($value instanceof Foo) && (!($value instanceof Foo) || $flag)",
    ] {
        let body = format!("if ({condition}) return;");
        assert_eq!(inferred(&body).as_deref(), Some("Foo"), "{condition}");
    }
}

#[test]
fn post_guard_parentheses_comments_and_actual_keyword_tokens_are_supported() {
    for condition in [
        "((!($value instanceof Foo)))",
        "!((($value)) instanceof Foo)",
        "/* before */ !($value /* left */ instanceof /* right */ Foo)",
        "!($value INSTANCEOF Foo)",
        "!($value instanceof Foo) /* operator */ || $flag",
    ] {
        assert_eq!(
            inferred(&format!("if ({condition}) return;")).as_deref(),
            Some("Foo"),
            "{condition}"
        );
    }
}

#[test]
fn post_guard_trivia_and_nested_callable_exits_are_not_real_exits() {
    for body in [
        "/* return; */ work();",
        "// throw new Error;\n work();",
        "echo 'return throw exit die(';",
        "$fn=function(){return;};",
        "$fn=fn()=>throw new Error;",
        "function nested(){return;}",
        "$obj=new class {function nested(){throw new Error;}};",
        "$returnValue=1;",
    ] {
        assert_eq!(
            inferred(&format!("if (!($value instanceof Foo)) {{{body}}}")).as_deref(),
            Some("Input"),
            "{body}"
        );
    }
}

#[test]
fn post_guard_conditional_nested_exits_do_not_prove_an_unconditional_exit() {
    for body in [
        "if($flag) return;",
        "while($flag){return;}",
        "foreach([] as $item){throw new Error;}",
        "try {throw new Error;} catch (Throwable $e) {}",
    ] {
        assert_eq!(
            inferred(&format!("if (!($value instanceof Foo)) {{{body}}}")).as_deref(),
            Some("Input"),
            "{body}"
        );
    }
}

#[test]
fn post_guard_exits_belong_to_the_then_branch() {
    for body in [
        "if (!($value instanceof Foo)) {work();} else {return;}",
        "if (!($value instanceof Foo)) {work();} elseif($flag) {throw new Error;}",
    ] {
        assert_eq!(inferred(body).as_deref(), Some("Input"), "{body}");
    }
}

#[test]
fn post_guard_unconditional_structural_exits_keep_ordinary_narrowing() {
    for body in [
        "return;",
        "throw new Error;",
        "exit;",
        "exit(1);",
        "die('stop');",
        "work(); /* else return */ return;",
        "if($flag){return;}else{throw new Error;}",
        "if($flag){return;}elseif($other){exit;}else{return;}",
    ] {
        assert_eq!(
            inferred(&format!("if (!($value instanceof Foo)) {{{body}}}")).as_deref(),
            Some("Foo"),
            "{body}"
        );
    }
}

#[test]
fn post_guard_else_and_intervening_writes_invalidate_the_proof() {
    for body in [
        "if(!($value instanceof Foo)){return;}else{$value=unknown();}",
        "if(!($value instanceof Foo))return; $value=unknown();",
        "if(!($value instanceof Foo))return; [$value]=unknown();",
        "if(!($value instanceof Foo))return; unset($value);",
        "if(!($value instanceof Foo))return; change($value);",
    ] {
        assert_ne!(inferred(body).as_deref(), Some("Foo"), "{body}");
    }
}

#[test]
fn post_guard_condition_effects_cannot_change_the_tested_binding() {
    for condition in [
        "!($value instanceof Foo) || ($value=unknown())",
        "!($value instanceof Foo) || change($value)",
        "!($value instanceof Foo) || extract($data)",
    ] {
        assert_ne!(
            inferred(&format!("if({condition})return;")).as_deref(),
            Some("Foo"),
            "{condition}"
        );
    }
}

#[test]
fn post_guard_dynamic_class_operands_do_not_become_static_class_names() {
    for condition in [
        "!($value instanceof $class)",
        "!($value instanceof $factory->type)",
        "!($other instanceof Foo)",
    ] {
        assert_eq!(
            inferred(&format!("if({condition})return;")).as_deref(),
            Some("Input"),
            "{condition}"
        );
    }
}

#[test]
fn post_guard_loop_and_unconnected_scope_uses_do_not_borrow_the_guard() {
    let source="<?php interface Input {} class Foo implements Input {} function run(Input $value, bool $flag) {if(!($value instanceof Foo))return; while($flag) {echo /*USE*/$value; $value=unknown();}}";
    assert_ne!(infer_source(source).as_deref(), Some("Foo"));
    let source="<?php class Foo {} function run($value) {if(!($value instanceof Foo))return; $fn=function(){echo /*USE*/$value;};}";
    assert_ne!(infer_source(source).as_deref(), Some("Foo"));
}

#[test]
fn post_guard_aliases_references_and_backward_jumps_remain_conservative() {
    for body in [
        "$alias=&$value; if(!($value instanceof Foo))return; work();",
        "$fn=function()use(&$value){}; if(!($value instanceof Foo))return; $fn();",
        "global $value; if(!($value instanceof Foo))return;",
        "label: if(!($value instanceof Foo))return; if($flag)goto label;",
    ] {
        assert_ne!(inferred(body).as_deref(), Some("Foo"), "{body}");
    }
}

#[test]
fn post_guard_false_and_true_boolean_implications_match_truth_tables() {
    type Formula = (&'static str, fn(bool, bool, bool) -> bool);
    let formulas: &[Formula] = &[
        ("!($value instanceof Foo) && $flag", |is_foo, a, _| {
            !is_foo && a
        }),
        ("!($value instanceof Foo) || $flag", |is_foo, a, _| {
            !is_foo || a
        }),
        (
            "!(($value instanceof Foo) && ($flag || $other))",
            |is_foo, a, b| !(is_foo && (a || b)),
        ),
        (
            "!(($value instanceof Foo) || ($flag && $other))",
            |is_foo, a, b| !(is_foo || (a && b)),
        ),
        (
            "(!($value instanceof Foo) && $flag) || (!($value instanceof Foo) && !$flag)",
            |is_foo, _, _| !is_foo,
        ),
    ];
    for (text, evaluate) in formulas {
        let mut false_non_foo = false;
        for a in [false, true] {
            for b in [false, true] {
                false_non_foo |= !evaluate(false, a, b);
            }
        }
        let actual = inferred(&format!("if({text})return;"));
        // Incomplete proofs are allowed, but a false fact is never allowed.
        if false_non_foo {
            assert_ne!(actual.as_deref(), Some("Foo"), "{text}");
        }
    }
}

#[test]
fn post_guard_deep_or_wide_proofs_stop_conservatively() {
    let condition = format!(
        "{}!($value instanceof Foo){}",
        "(".repeat(100),
        ")".repeat(100)
    );
    assert_ne!(
        inferred(&format!("if({condition})return;")).as_deref(),
        Some("Foo")
    );
    let body = format!(
        "{} if(!($value instanceof Foo))return;",
        "$flag=true;".repeat(5000)
    );
    assert_ne!(inferred(&body).as_deref(), Some("Foo"));
}

#[test]
fn post_guard_die_callable_creation_and_named_methods_do_not_exit() {
    for body in ["die(...);", "$obj->die();", "Other::die();", "$fn='die';"] {
        assert_eq!(
            inferred(&format!("if(!($value instanceof Foo)){{{body}}}")).as_deref(),
            Some("Input"),
            "{body}"
        );
    }
}

#[test]
fn post_guard_writes_in_an_exiting_then_arm_never_reach_the_continuation() {
    assert_eq!(
        inferred("if(!($value instanceof Foo) && $flag){$value=new Foo;return;}").as_deref(),
        Some("Input")
    );
    assert_eq!(
        inferred("if(!($value instanceof Foo)){$value=new Other;return;}").as_deref(),
        Some("Foo")
    );
}

#[test]
fn post_guard_constructor_arguments_can_escape_or_replace_the_binding() {
    for call in [
        "new Mutator($value)",
        "new class($value) {function __construct(&$v){$v=new Other;}}",
    ] {
        let body = format!("if(!($value instanceof Foo))return; $obj={call};");
        assert_ne!(inferred(&body).as_deref(), Some("Foo"), "{call}");
    }
}

#[test]
fn post_guard_static_class_names_resolve_in_the_active_namespace_section() {
    let source="<?php namespace First {class Foo {}} namespace Second {use First\\Foo as Target; function run($value){if(!($value instanceof Target))return; echo /*USE*/$value;}}";
    assert_eq!(infer_source(source).as_deref(), Some("First\\Foo"));
}
