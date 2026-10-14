use super::*;

#[tokio::test(flavor = "current_thread")]
async fn p236_disk_cache_roundtrip_preserves_nested_owners_without_reparsing_closed_sources() {
    use php_lsp_index::cache::{
        build_cache_from_index, load_valid_cached_files, save_cache_atomic, CacheNamespace,
        IndexCacheConfig,
    };
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root =
        std::env::temp_dir().join(format!("php-lsp-p236-cache-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("Calls Ж.php");
    let uri = php_lsp_types::uri::path_to_uri(&path).unwrap();
    let source="<?php\r\nfunction target() {}\r\nclass Peer { function ping() {} }\r\nfunction outer(Peer $peer) { /* 😀 */ target(); $a=function() use($peer) { target(); $peer?->ping(); }; $b=fn()=>target(); function nested(){ target(); } $o=new class(target()){function hidden(){ target(); }}; $peer?->ping(); target(...); target(); }";
    std::fs::write(&path, source).unwrap();
    let original = WorkspaceIndex::new();
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let symbols = extract_file_symbols(parser.tree().unwrap(), source, &uri);
    let references = collect_symbol_references_in_file(parser.tree().unwrap(), source, &symbols);
    let outer_range = symbols
        .symbols
        .iter()
        .find(|s| s.fqn == "outer")
        .unwrap()
        .range;
    let named = references
        .iter()
        .filter(|r| {
            r.call_site
                .is_some_and(|call| call.caller_range == Some(outer_range))
        })
        .count();
    assert_eq!(named, 4, "nested bodies leaked into outer metadata");
    assert!(references
        .iter()
        .any(|r| r.call_site.is_some_and(|call| call.caller_range.is_none())));
    original.update_file_with_references_from_source(
        &uri,
        symbols,
        references,
        SourceFingerprint::from_bytes(source.as_bytes()),
    );
    let config = IndexCacheConfig {
        namespace: CacheNamespace::Workspace,
        php_lsp_version: env!("CARGO_PKG_VERSION").into(),
        php_version: "8.2".into(),
        include_paths: vec![],
        exclude_paths: vec![],
        traversal_max_files: None,
        traversal_max_entries: None,
        stub_extensions: vec![],
        stubs_hash: 0,
    };
    let cache = build_cache_from_index(&original, &root, std::slice::from_ref(&path), &config);
    assert_eq!(cache.files.len(), 1);
    let cache_path = root.join("index.bin");
    save_cache_atomic(&cache_path, &cache).unwrap();
    let (service, _socket) = LspService::new(PhpLspBackend::new);
    let backend = service.inner();
    let request = backend.request_context_for_uri(&uri).await;
    let index = request.index(&backend.index);
    let report = load_valid_cached_files(
        &index,
        &cache_path,
        &root,
        std::slice::from_ref(&path),
        &config,
    );
    assert_eq!(report.loaded_files, 1);
    assert!(report.parse_files.is_empty());
    // All receivers are cached: graph construction must not need the source.
    std::fs::remove_file(&path).unwrap();
    let graph = backend
        .call_graph(&request, index, None, None)
        .await
        .unwrap();
    assert_eq!(graph.edges.len(), 4);
    assert!(graph
        .edges
        .iter()
        .all(|e| e.caller.fqn == "outer" && e.caller.range == outer_range));
    assert_eq!(
        graph
            .edges
            .iter()
            .filter(|e| e.target.fqn == "target")
            .count(),
        3
    );
    assert_eq!(
        graph
            .edges
            .iter()
            .filter(|e| e.target.fqn == "Peer::ping")
            .count(),
        1
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn p236_open_scope_changes_and_reopen_invalidate_old_call_graph_snapshots() {
    let (service, _socket) = LspService::new(PhpLspBackend::new);
    let backend = service.inner();
    let uri = "file:///p236/Scope.php";
    let request = backend.request_context_for_uri(uri).await;
    let index = request.index(&backend.index);
    let direct = "<?php function target() {} function outer(){ target(); }";
    let nested = "<?php function target() {} function outer(){ $f=function(){ target(); }; }";
    for (version, source) in [(1, direct), (2, nested)] {
        indexed(&index, uri, source);
        let mut parser = FileParser::new();
        parser.parse_full(source);
        backend.open_files.insert(uri.into(), parser);
        backend.document_versions.insert(
            uri.into(),
            OpenDocumentState {
                version,
                generation: 1,
            },
        );
    }
    let nested_graph = backend
        .call_graph(&request, index.clone(), None, None)
        .await
        .unwrap();
    assert!(nested_graph.edges.is_empty());
    backend.open_files.get_mut(uri).unwrap().parse_full(direct);
    backend.document_versions.insert(
        uri.into(),
        OpenDocumentState {
            version: 3,
            generation: 1,
        },
    );
    indexed(&index, uri, direct);
    assert!(!nested_graph.is_current(backend, &index));
    let direct_graph = backend
        .call_graph(&request, index.clone(), None, None)
        .await
        .unwrap();
    assert_eq!(direct_graph.edges.len(), 1);
    backend.open_files.remove(uri);
    backend.document_versions.remove(uri);
    assert!(!direct_graph.is_current(backend, &index));
    let mut parser = FileParser::new();
    parser.parse_full(direct);
    backend.open_files.insert(uri.into(), parser);
    backend.document_versions.insert(
        uri.into(),
        OpenDocumentState {
            version: 3,
            generation: 2,
        },
    );
    assert!(
        !direct_graph.is_current(backend, &index),
        "reused version must not revive the old graph"
    );
    let reopened = backend
        .call_graph(&request, index.clone(), None, None)
        .await
        .unwrap();
    assert_eq!(reopened.edges.len(), 1);
    indexed(&index, uri, nested);
    assert!(
        !reopened.is_current(backend, &index),
        "reindex must reject old graph"
    );
}
