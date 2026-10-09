use super::*;
use php_lsp_parser::{parser::FileParser, symbols::extract_file_symbols};

fn symbols(source: &str) -> FileSymbols {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    assert!(!parser.tree().unwrap().root_node().has_error(), "{source}");
    extract_file_symbols(
        parser.tree().unwrap(),
        source,
        "file:///generic-bindings.php",
    )
}
fn indexed(source: &str) -> WorkspaceIndex {
    let index = WorkspaceIndex::new();
    index.update_file("file:///generic-bindings.php", symbols(source));
    index
}

#[test]
fn receiver_template_bindings_follow_declared_inheritance_edges() {
    let index=indexed("<?php /** @template T */ class Base {} /** @extends Base<int> */ class Child extends Base {}");
    let found = index
        .receiver_template_bindings("Child", "Base", &HashMap::new())
        .unwrap();
    assert_eq!(found.get("T"), Some(&TypeInfo::Simple("int".into())));
    let seed = HashMap::from([("T".into(), TypeInfo::Simple("string".into()))]);
    assert_eq!(
        index.receiver_template_bindings("Base", "Base", &seed),
        Some(seed)
    );
}

#[test]
fn conflicting_receiver_bindings_remain_unresolved() {
    let index=indexed("<?php /** @template T */ interface I {} /** @extends I<int> */ interface A extends I {} /** @extends I<string> */ interface B extends I {} interface C extends A,B {}");
    let found = index
        .receiver_template_bindings("C", "I", &HashMap::new())
        .unwrap();
    assert!(
        !found.contains_key("T"),
        "ambiguous owner binding: {found:?}"
    );
}

#[test]
fn incomplete_or_cyclic_owner_paths_do_not_invent_bindings() {
    let index =
        indexed("<?php class A extends B {} class B extends A {} class C extends Missing {}");
    assert!(index
        .receiver_template_bindings("A", "Other", &HashMap::new())
        .is_none());
    assert!(index
        .receiver_template_bindings("C", "Other", &HashMap::new())
        .is_none());
}

#[test]
fn a_pinned_reader_keeps_old_bindings_until_an_atomic_update_finishes() {
    use std::sync::{mpsc, Arc};
    use std::time::Duration;
    let old="<?php /** @template T */ class Base {} /** @extends Base<int> */ class Child extends Base {}";
    let new="<?php /** @template T */ class Base {} /** @extends Base<string> */ class Child extends Base {}";
    let index = Arc::new(indexed(old));
    let lease = index.read();
    let (started_tx, started_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let writer = index.clone();
    let new = symbols(new);
    let thread = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        writer.update_file("file:///generic-bindings.php", new);
        done_tx.send(()).unwrap();
    });
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let found = index
        .receiver_template_bindings("Child", "Base", &HashMap::new())
        .unwrap();
    assert_eq!(found.get("T"), Some(&TypeInfo::Simple("int".into())));
    assert!(
        done_rx.try_recv().is_err(),
        "writer bypassed the publication lease"
    );
    drop(lease);
    done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    thread.join().unwrap();
    let found = index
        .receiver_template_bindings("Child", "Base", &HashMap::new())
        .unwrap();
    assert_eq!(found.get("T"), Some(&TypeInfo::Simple("string".into())));
}
