use super::*;
use tower_lsp::LspService;

fn indexed(index: &WorkspaceIndex, uri: &str, source: &str) {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let symbols = extract_file_symbols(parser.tree().unwrap(), source, uri);
    let references = collect_symbol_references_in_file(parser.tree().unwrap(), source, &symbols);
    index.update_file_with_references(uri, symbols, references);
}

#[tokio::test(flavor = "current_thread")]
async fn call_graph_uses_closed_precomputed_occurrences_without_reading_or_parsing_disk() {
    let (service, _socket) = LspService::new(PhpLspBackend::new);
    let backend = service.inner();
    let request = backend
        .request_context_for_uri("file:///never-created/Calls.php")
        .await;
    let index = request.index(&backend.index);
    indexed(&index,"file:///never-created/Calls.php","<?php class Target { function run() {} } class Other { function run() {} } function good(Target $obj) { $obj->run(); } function bad(Other $obj) { $obj->run(); }");
    let graph = backend
        .call_graph(&request, index, None, None)
        .await
        .unwrap();
    assert_eq!(graph.edges.len(), 2);
    let target = graph
        .edges
        .iter()
        .find(|edge| edge.target.fqn == "Target::run")
        .unwrap();
    assert_eq!(target.caller.fqn, "good");
}

#[tokio::test(flavor = "current_thread")]
async fn call_graph_open_overlay_replaces_stale_index_relationships_and_validates_edits() {
    let (service, _socket) = LspService::new(PhpLspBackend::new);
    let backend = service.inner();
    let uri = "file:///overlay/Calls.php";
    let request = backend.request_context_for_uri(uri).await;
    let index = request.index(&backend.index);
    let original="<?php class Target { function run() {} } class Other { function run() {} } class Receiver extends Target { function caller(){ $this->run(); } }";
    indexed(&index, uri, original);
    let mut parser = FileParser::new();
    parser.parse_full(&original.replace("extends Target", "extends Other"));
    backend.open_files.insert(uri.to_string(), parser);
    let graph = backend
        .call_graph(&request, index.clone(), None, None)
        .await
        .unwrap();
    assert_eq!(graph.edges.len(), 1);
    assert_eq!(graph.edges[0].target.fqn, "Other::run");
    assert!(graph.is_current(backend, &index));
    backend
        .open_files
        .get_mut(uri)
        .unwrap()
        .parse_full(original);
    assert!(!graph.is_current(backend, &index));
}

#[tokio::test(flavor = "current_thread")]
async fn call_graph_resolves_closed_cross_file_chains_with_current_index_dependencies() {
    let (service, _socket) = LspService::new(PhpLspBackend::new);
    let backend = service.inner();
    let path = std::env::temp_dir().join(format!(
        "php-lsp-hierarchy-chain-{}.php",
        std::process::id()
    ));
    let uri = php_lsp_types::uri::path_to_uri(&path).unwrap();
    let request = backend.request_context_for_uri(&uri).await;
    let index = request.index(&backend.index);
    indexed(&index,"file:///library/Lib.php","<?php namespace Lib; class Target { function run() {} } class Factory { function create(): Target {} }");
    let source = "<?php function caller(\\Lib\\Factory $factory) { $factory->create()->run(); }";
    std::fs::write(&path, source).unwrap();
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let symbols = extract_file_symbols(parser.tree().unwrap(), source, &uri);
    let refs = collect_symbol_references_in_file(parser.tree().unwrap(), source, &symbols);
    index.update_file_with_references_from_source(
        &uri,
        symbols,
        refs,
        SourceFingerprint::from_bytes(source.as_bytes()),
    );
    let graph = backend
        .call_graph(&request, index, None, None)
        .await
        .unwrap();
    assert!(graph
        .edges
        .iter()
        .any(|edge| edge.target.fqn == "Lib\\Target::run"));
    std::fs::remove_file(path).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn call_graph_rejects_closed_to_open_transition_and_superseded_index_before_publication() {
    let (service, _socket) = LspService::new(PhpLspBackend::new);
    let backend = service.inner();
    let uri = "file:///closed/Calls.php";
    let request = backend.request_context_for_uri(uri).await;
    let index = request.index(&backend.index);
    let source = "<?php function target() {} function caller(){ target(); }";
    indexed(&index, uri, source);
    let graph = backend
        .call_graph(&request, index.clone(), None, None)
        .await
        .unwrap();
    assert!(graph.is_current(backend, &index));
    let mut parser = FileParser::new();
    parser.parse_full(source);
    backend.open_files.insert(uri.to_string(), parser);
    assert!(
        !graph.is_current(backend, &index),
        "opening before index commit must invalidate closed inputs"
    );
    backend.open_files.remove(uri);
    index.remove_file(uri);
    assert!(!graph.is_current(backend, &index));
}
