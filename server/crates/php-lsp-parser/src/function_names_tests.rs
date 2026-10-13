use super::*;
use crate::{parser::FileParser, symbols::extract_file_symbols};

fn call(root: Node) -> Node {
    let mut cursor = root.walk();
    loop {
        if cursor.node().kind() == "function_call_expression" {
            return cursor.node();
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            assert!(cursor.goto_parent());
        }
    }
}

#[test]
fn p232_namespaced_fallback_without_index_evidence_is_unknown() {
    let source = "<?php namespace App; array_values($items);";
    let mut parser = FileParser::new();
    parser.parse_full(source);
    assert_eq!(
        global_builtin_name(call(parser.tree().unwrap().root_node()), source),
        None,
        "another source may declare App\\array_values"
    );
}

#[test]
fn p232_function_name_facts_are_reused_for_a_batch() {
    let source = "<?php namespace App; preg_match('/x/',$text,$matches);";
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let root = parser.tree().unwrap().root_node();
    let symbols = extract_file_symbols(parser.tree().unwrap(), source, "file:///name-batch.php");
    let before = BUILDS.with(|count| count.get());
    let absent = |fqn: &str| !fqn.contains('\\');
    let _scope = FunctionNameScope::enter(root, source, Some(&symbols), Some(&absent));
    for _ in 0..1000 {
        assert_eq!(
            global_builtin_name(call(root), source).as_deref(),
            Some("preg_match")
        );
    }
    assert_eq!(BUILDS.with(|count| count.get()) - before, 1);
}

#[test]
fn p232_nested_name_context_and_unwind_restore_the_parent() {
    let outer = "<?php namespace App; preg_match('/x/',$text,$matches);";
    let inner =
        "<?php namespace App; function preg_match($a,$b,$c){} preg_match('/x/',$text,$matches);";
    let mut p = FileParser::new();
    p.parse_full(outer);
    let mut q = FileParser::new();
    q.parse_full(inner);
    let absent = |fqn: &str| !fqn.contains('\\');
    let _scope =
        FunctionNameScope::enter(p.tree().unwrap().root_node(), outer, None, Some(&absent));
    assert_eq!(
        global_builtin_name(call(p.tree().unwrap().root_node()), outer).as_deref(),
        Some("preg_match")
    );
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _nested = FunctionNameScope::enter(q.tree().unwrap().root_node(), inner, None, None);
        assert_eq!(
            global_builtin_name(call(q.tree().unwrap().root_node()), inner),
            None
        );
        panic!("forced unwind");
    }));
    assert!(result.is_err());
    assert_eq!(
        global_builtin_name(call(p.tree().unwrap().root_node()), outer).as_deref(),
        Some("preg_match")
    );
}

#[test]
fn p232_external_function_presence_invalidates_fallback_even_without_a_return_type() {
    let source = "<?php namespace App; preg_match('/x/',$text,$matches);";
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let root = parser.tree().unwrap().root_node();
    {
        let exists = |fqn: &str| fqn.eq_ignore_ascii_case("App\\preg_match");
        let _scope = FunctionNameScope::enter(root, source, None, Some(&exists));
        assert_eq!(global_builtin_name(call(root), source), None);
    }
    assert_eq!(global_builtin_name(call(root), source), None);
    {
        let absent = |fqn: &str| !fqn.contains('\\');
        let _scope = FunctionNameScope::enter(root, source, None, Some(&absent));
        assert_eq!(
            global_builtin_name(call(root), source).as_deref(),
            Some("preg_match")
        );
    }
    assert!(ACTIVE.with(|active| active.borrow().is_none()));
}

#[test]
fn p232_parallel_name_facts_do_not_cross_threads() {
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = [true, false]
        .into_iter()
        .map(|shadow| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let source = "<?php namespace App; preg_match('/x/',$text,$matches);";
                let mut parser = FileParser::new();
                parser.parse_full(source);
                let root = parser.tree().unwrap().root_node();
                let exists = |fqn: &str| !fqn.contains('\\') || shadow;
                let _scope = FunctionNameScope::enter(root, source, None, Some(&exists));
                barrier.wait();
                for _ in 0..64 {
                    assert_eq!(global_builtin_name(call(root), source).is_none(), shadow);
                }
                barrier.wait();
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
}
