use super::*;
use crate::{
    analysis_metrics::{Probe, Work},
    parser::{FileParser, SourceEdit},
};

fn parsed(source: &str) -> Arc<ParsedSnapshot> {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    parser.snapshot()
}
#[test]
fn analysis_cache_keeps_logical_uri_and_php_version_separate() {
    let parsed=parsed("<?php #[PhpStormStubsElementAvailable(from: '8.3')] function future() {} function stable() {}");
    let old = Some(PhpSymbolExtractionVersion { major: 8, minor: 2 });
    let new = Some(PhpSymbolExtractionVersion { major: 8, minor: 3 });
    let a = parsed.analysis("file:///one.php", old).unwrap();
    assert!(!a.symbols.symbols.iter().any(|s| s.name == "future"));
    let b = parsed.analysis("file:///one.php", new).unwrap();
    assert!(b.symbols.symbols.iter().any(|s| s.name == "future"));
    assert!(Arc::ptr_eq(
        &a,
        &parsed.analysis("file:///one.php", old).unwrap()
    ));
    let c = parsed.analysis("file:///two.php", new).unwrap();
    assert!(c.symbols.symbols.iter().all(|s| s.uri == "file:///two.php"));
    assert!(a.symbols.symbols.iter().all(|s| s.uri == "file:///one.php"));
    assert!(parsed
        .analysis("file:///one.php", None)
        .unwrap()
        .symbols
        .symbols
        .iter()
        .any(|s| s.name == "future"));
    for i in 0..32 {
        parsed
            .analysis(&format!("file:///alias{i}.php"), new)
            .unwrap();
    }
    assert_eq!(parsed.analyses.lock().unwrap().len(), MAX_ANALYSIS_VARIANTS);
}
#[test]
fn analysis_cancelled_symbol_or_reference_fill_is_not_cached() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let source = format!(
        "<?php {}",
        (0..100)
            .map(|i| format!("function f{i}() {{}} "))
            .collect::<String>()
    );
    let parsed = parsed(&source);
    let checks = Arc::new(AtomicUsize::new(0));
    let n = checks.clone();
    crate::control::run_cancellable(
        Arc::new(move || n.fetch_add(1, Ordering::SeqCst) >= 8),
        || assert!(parsed.analysis("file:///cancel-facts.php", None).is_none()),
    );
    assert!(parsed.analyses.lock().unwrap().is_empty());
    let facts = parsed.analysis("file:///cancel-facts.php", None).unwrap();
    assert_eq!(facts.symbols.symbols.len(), 100);
    let checks = Arc::new(AtomicUsize::new(0));
    let n = checks.clone();
    crate::control::run_cancellable(
        Arc::new(move || n.fetch_add(1, Ordering::SeqCst) >= 8),
        || assert!(facts.references().is_none()),
    );
    assert!(facts.references.lock().unwrap().is_none());
    assert_eq!(facts.references().unwrap().len(), 100);
}
#[test]
fn analysis_panicking_fill_restores_scope_and_recovers_poisoned_cache() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let parsed = parsed("<?php function safe() {}");
    let n = Arc::new(AtomicUsize::new(0));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::control::run_cancellable(
            Arc::new(move || {
                assert!(
                    n.fetch_add(1, Ordering::SeqCst) < 4,
                    "controlled fill panic"
                );
                false
            }),
            || parsed.analysis("file:///panic-facts.php", None),
        )
    }));
    assert!(result.is_err());
    assert!(!crate::control::is_cancelled());
    assert_eq!(
        parsed
            .analysis("file:///panic-facts.php", None)
            .unwrap()
            .symbols
            .symbols[0]
            .name,
        "safe"
    );
}
#[test]
fn analysis_concurrent_readers_fill_symbols_and_references_once() {
    let uri = "file:///concurrent-facts.php";
    let parsed = parsed(&format!("<?php /* {uri} */ function shared() {{}}"));
    let probe = Probe::new(uri);
    let barrier = std::sync::Barrier::new(4);
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..4)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    let facts = parsed.analysis(uri, None).unwrap();
                    let refs = facts.references().unwrap();
                    (facts, refs)
                })
            })
            .collect();
        let results: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
        for result in &results[1..] {
            assert!(Arc::ptr_eq(&results[0].0, &result.0));
            assert!(Arc::ptr_eq(&results[0].1, &result.1));
        }
    });
    assert_eq!(
        (probe.count(Work::Symbols), probe.count(Work::References)),
        (1, 1)
    );
}
#[test]
fn analysis_reference_cache_does_not_inherit_external_function_proofs() {
    let parsed=parsed("<?php namespace App; class Box {function work(){}} /** @var array<string,Box> $objects */ $objects=[]; $values=array_values($objects); foreach ($values as $value) {$value->work();}");
    let facts = parsed.analysis("file:///ambient.php", None).unwrap();
    let tree = parsed.tree().unwrap();
    let collect = || {
        crate::references::collect_symbol_references_in_file(tree, &parsed.source, &facts.symbols)
    };
    let plain = collect();
    let exists = |fqn: &str| !fqn.contains('\\');
    let _external = FunctionNameScope::enter(
        tree.root_node(),
        &parsed.source,
        Some(&facts.symbols),
        Some(&exists),
    );
    let contextual = collect();
    assert_ne!(
        plain, contextual,
        "fixture must observe ambient external proofs"
    );
    assert_eq!(*facts.references().unwrap(), plain);
    assert_eq!(
        collect(),
        contextual,
        "cache isolation must restore caller context"
    );
}
#[test]
fn analysis_batch_edits_preserve_old_snapshot_and_match_a_fresh_parse() {
    for ending in ["\n", "\r\n", "\r"] {
        let uri = "file:///batch-facts.php";
        let old = format!("<?php /* {uri} 😀\u{2028} */{ending}function foo() {{}}{ending}");
        let mut parser = FileParser::new();
        parser.parse_full(&old);
        let snapshot = parser.snapshot();
        let sexp = snapshot.tree().unwrap().root_node().to_sexp();
        let probe = Probe::new(uri);
        parser
            .apply_edits(&[
                SourceEdit {
                    range: Some(((1, 9), (1, 12))),
                    text: "longer",
                },
                SourceEdit {
                    range: Some(((1, 9), (1, 15))),
                    text: "bar",
                },
                SourceEdit {
                    range: Some(((1, 999), (1, 999))),
                    text: " // 😀",
                },
            ])
            .unwrap();
        assert_eq!(probe.count(Work::Parse), 1);
        assert_eq!(probe.count(Work::Source), 1);
        let expected = old.replace("function foo() {}", "function bar() {} // 😀");
        assert_eq!(parser.source(), expected);
        let fresh = parsed(&expected);
        assert_eq!(
            parser.tree().unwrap().root_node().to_sexp(),
            fresh.tree().unwrap().root_node().to_sexp()
        );
        assert_eq!(
            parser.tree().unwrap().root_node().end_position(),
            fresh.tree().unwrap().root_node().end_position()
        );
        assert_eq!(&*snapshot.source, &old);
        assert_eq!(snapshot.tree().unwrap().root_node().to_sexp(), sexp);
        assert_eq!(snapshot.tree().unwrap().root_node().end_byte(), old.len());
        assert_eq!(
            snapshot.lines.position_to_offset(1, 9),
            Some(old.find("foo").unwrap())
        );
    }
}
#[test]
fn analysis_mixed_full_and_ranged_batch_parses_once_and_reversed_batch_is_atomic() {
    let mut parser = FileParser::new();
    parser.parse_full("<?php function original() {}");
    let uri = "file:///full-batch.php";
    let source = format!("<?php /* {uri} */\r\nfunction foo() {{}}");
    let probe = Probe::new(uri);
    parser
        .apply_edits(&[
            SourceEdit {
                range: None,
                text: &source,
            },
            SourceEdit {
                range: Some(((1, 9), (1, 12))),
                text: "bar",
            },
        ])
        .unwrap();
    assert_eq!(parser.source(), source.replace("foo", "bar"));
    assert_eq!(probe.count(Work::Parse), 1);
    let before = parser.snapshot();
    assert!(parser
        .apply_edits(&[
            SourceEdit {
                range: Some(((1, 9), (1, 12))),
                text: "baz"
            },
            SourceEdit {
                range: Some(((1, 4), (0, 0))),
                text: "bad"
            }
        ])
        .is_err());
    assert!(Arc::ptr_eq(&before, &parser.snapshot()));
    parser.apply_edits(&[]).unwrap();
    assert!(Arc::ptr_eq(&before, &parser.snapshot()));
}
