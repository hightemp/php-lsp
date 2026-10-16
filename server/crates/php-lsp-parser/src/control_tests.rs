use super::*;
use crate::{parser::FileParser, symbols::extract_file_symbols};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[test]
fn dispatch_cancellation_stops_each_cst_producer_before_scanning_the_whole_file() {
    let mut source = "<?php\n".to_string();
    for i in 0..1000 {
        source.push_str(&format!("function f{i}():int {{return {i};}}\n"));
    }
    let mut parser = FileParser::new();
    parser.parse_full(&source);
    let tree = parser.tree().unwrap();
    let file = extract_file_symbols(tree, &source, "file:///cancel.php");
    let mut failures = Vec::new();
    for producer in ["symbols", "references", "tokens"] {
        let checks = Arc::new(AtomicUsize::new(0));
        let counter = checks.clone();
        let result = run_cancellable(
            Arc::new(move || counter.fetch_add(1, Ordering::SeqCst) >= 8),
            || match producer {
                "symbols" => extract_file_symbols(tree, &source, "file:///cancel.php")
                    .symbols
                    .len(),
                "references" => {
                    crate::references::collect_symbol_references_in_file(tree, &source, &file).len()
                }
                "tokens" => crate::semantic_tokens::extract_semantic_tokens(tree, &source).len(),
                _ => unreachable!(),
            },
        );
        if result.is_some() || checks.load(Ordering::SeqCst) > 32 {
            failures.push((producer, result, checks.load(Ordering::SeqCst)));
        }
    }
    assert!(
        failures.is_empty(),
        "uninterruptible producers: {failures:?}"
    );
    assert_eq!(
        extract_file_symbols(tree, &source, "file:///cancel.php")
            .symbols
            .len(),
        1000
    );
}

#[test]
fn dispatch_cancellation_scopes_restore_after_nested_work_and_unwind() {
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = cancelled.clone();
    assert_eq!(
        run_cancellable(Arc::new(move || flag.load(Ordering::SeqCst)), || {
            assert_eq!(run_cancellable(Arc::new(|| true), || 7), None);
            assert!(!is_cancelled());
            let _ = std::panic::catch_unwind(|| {
                run_cancellable(Arc::new(|| false), || panic!("scope unwind"))
            });
            assert!(!is_cancelled());
            cancelled.store(true, Ordering::SeqCst);
            42
        }),
        None
    );
    assert!(!is_cancelled());
}

#[test]
fn dispatch_cancellation_does_not_leak_to_another_worker_thread() {
    let gate = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            run_cancellable(Arc::new(|| false), || {
                gate.wait();
                gate.wait();
            });
        });
        gate.wait();
        assert_eq!(run_cancellable(Arc::new(|| true), || 0), None);
        assert!(!is_cancelled());
        gate.wait();
    });
}
