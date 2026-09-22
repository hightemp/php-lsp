use super::*;

#[test]
fn scoped_read_dependencies_track_inherited_sources_misses_and_index_isolation() {
    let index = WorkspaceIndex::new();
    let other = WorkspaceIndex::new();
    let uri = "file:///dependency.php";
    let source =
        "<?php class Base { public function value(): string {} } class Child extends Base {}";
    let mut parser = php_lsp_parser::parser::FileParser::new();
    parser.parse_full(source);
    index.update_file(
        uri,
        php_lsp_parser::symbols::extract_file_symbols(parser.tree().unwrap(), source, uri),
    );
    let (result, dependencies) = index.trace_read_dependencies(|| {
        assert!(index.resolve_fqn("Missing").is_none());
        assert!(other.resolve_fqn("OtherRoot").is_none());
        index.resolve_member("Child::value")
    });
    assert!(result.is_some());
    assert!(dependencies.files.contains(uri));
    for name in ["child", "base", "missing"] {
        assert!(dependencies.symbols.contains(name), "{name}");
    }
    assert!(!dependencies.symbols.contains("otherroot"));
    assert!(!dependencies.whole_index);
    let (_, inventory) = index.trace_read_dependencies(|| index.observe_type_inventory());
    assert!(inventory.whole_index);
}

#[test]
fn dependency_observer_is_restored_after_unwind() {
    let index = WorkspaceIndex::new();
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        index.trace_read_dependencies(|| {
            index.resolve_fqn("BeforePanic");
            panic!("injected dependency computation failure");
        });
    }));
    assert!(failure.is_err());
    let (_, dependencies) = index.trace_read_dependencies(|| index.resolve_fqn("AfterPanic"));
    assert!(dependencies.symbols.contains("afterpanic"));
    assert!(!dependencies.symbols.contains("beforepanic"));
}
