use super::*;
use crate::util::uri::path_to_uri;
use tower_lsp::LspService;

fn indexed(index: &WorkspaceIndex, uri: &str, source: &str) {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let symbols = extract_file_symbols(parser.tree().unwrap(), source, uri);
    let references = collect_symbol_references_in_file(parser.tree().unwrap(), source, &symbols);
    index.update_file_with_references(uri, symbols, references);
}
#[tokio::test(flavor = "current_thread")]
async fn p238_code_lens_scans_each_reference_source_once_for_all_symbols() {
    let (service, _socket) = LspService::new(PhpLspBackend::new);
    let backend = service.inner();
    let uri = "file:///p238-small/Types.php";
    let caller = "file:///p238-small/Caller.php";
    let source = "<?php class First { function one() {} } class Second { function two() {} }";
    let request = backend.request_context_for_uri(uri).await;
    let index = request.index(&backend.index);
    indexed(&index, uri, source);
    indexed(
        &index,
        caller,
        "<?php function f(First $a,Second $b){ $a->one(); $b->two(); }",
    );
    let mut parser = FileParser::new();
    parser.parse_full(source);
    backend.open_files.insert(uri.into(), parser);
    REFERENCE_SCAN_WORK
        .get_or_init(StdMutex::default)
        .lock()
        .unwrap()
        .clear();
    let lenses = backend
        .lsp_code_lens(CodeLensParams {
            text_document: TextDocumentIdentifier {
                uri: uri.parse().unwrap(),
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(lenses.len(), 4);
    assert!(lenses
        .iter()
        .all(|lens| lens.command.as_ref().unwrap().title == "1 reference"));
    let counts = REFERENCE_SCAN_WORK.get().unwrap().lock().unwrap();
    assert_eq!(
        counts.get(uri),
        Some(&1),
        "target source scanned repeatedly: {counts:?}"
    );
    assert_eq!(
        counts.get(caller),
        Some(&1),
        "caller source scanned repeatedly: {counts:?}"
    );
}

fn params(uri: &str) -> CodeLensParams {
    CodeLensParams {
        text_document: TextDocumentIdentifier {
            uri: uri.parse().unwrap(),
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    }
}
fn open(backend: &PhpLspBackend, uri: &str, source: &str, version: i32, generation: u64) {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    backend.open_files.insert(uri.into(), parser);
    backend.document_versions.insert(
        uri.into(),
        OpenDocumentState {
            version,
            generation,
        },
    );
}

#[tokio::test(flavor = "current_thread")]
async fn p238_two_thousand_lenses_visit_references_once_and_expand_each_binding_once() {
    let (service, _socket) = LspService::new(PhpLspBackend::new);
    let backend = service.inner();
    let uri = "file:///p238-scale/Types.php";
    let caller = "file:///p238-scale/Caller.php";
    let request = backend.request_context_for_uri(uri).await;
    let index = request.index(&backend.index);
    let mut source = String::from("<?php\n");
    let mut calls = String::from("<?php function call_all(){\n");
    for n in 0..1000 {
        source.push_str(&format!(
            "class C{n} {{ public static function run() {{}} }}\n"
        ));
        calls.push_str(&format!("C{n}::run();\n"));
    }
    calls.push_str("}\n");
    indexed(&index, uri, &source);
    indexed(&index, caller, &calls);
    open(backend, uri, &source, 1, 1);
    let expected_visits = index
        .read()
        .file_references()
        .iter()
        .map(|entry| entry.value().len())
        .sum::<usize>();
    REFERENCE_SCAN_WORK
        .get_or_init(StdMutex::default)
        .lock()
        .unwrap()
        .clear();
    let lenses = backend.lsp_code_lens(params(uri)).await.unwrap().unwrap();
    assert_eq!(lenses.len(), 2000);
    assert!(lenses
        .iter()
        .all(|lens| lens.data.as_ref().unwrap()["references"] == 1));
    let hooks = &backend.code_lens_control.hooks;
    assert_eq!(hooks.references.load(Ordering::SeqCst), expected_visits);
    assert_eq!(hooks.expansions.load(Ordering::SeqCst), 2000);
    let scans = REFERENCE_SCAN_WORK.get().unwrap().lock().unwrap();
    assert_eq!(scans.get(uri), Some(&1));
    assert_eq!(scans.get(caller), Some(&1));
}

#[tokio::test(flavor = "current_thread")]
async fn p238_grouped_lenses_match_reference_queries_for_kinds_casing_inheritance_and_traits() {
    let (service, _socket) = LspService::new(PhpLspBackend::new);
    let backend = service.inner();
    let uri = "file:///p238-oracle/Types.php";
    let caller = "file:///p238-oracle/Caller.php";
    let source="<?php namespace N; interface Contract {function Run();} trait Shared {function log(){}} class Base {function Run(){}} class Child extends Base implements Contract {use Shared;function Run(){}} enum Choice implements Contract {use Shared;case One;function Run(){}}";
    let calls="<?php namespace UseSite; use N\\Child as Alias; function f(\\N\\Base $b, Alias $c, \\N\\Choice $e, $unknown) { $b->RUN();$c->rUn();$c->log();$e->Run();$e->log();$unknown->run();new Alias(); }";
    let request = backend.request_context_for_uri(uri).await;
    let index = request.index(&backend.index);
    indexed(&index, uri, source);
    indexed(&index, caller, calls);
    open(backend, uri, source, 1, 1);
    let symbols = index
        .read()
        .file_symbols()
        .get(uri)
        .unwrap()
        .value()
        .clone();
    let lenses = backend.lsp_code_lens(params(uri)).await.unwrap().unwrap();
    for symbol in symbols
        .symbols
        .iter()
        .filter(|symbol| is_code_lens_symbol_kind(symbol.kind))
    {
        let mut expected = backend.reference_locations_for_symbol(
            &index,
            Some(&request),
            &symbol.fqn,
            symbol.kind,
            false,
        );
        expected.sort_by(|a, b| {
            a.uri
                .as_str()
                .cmp(b.uri.as_str())
                .then(a.range.start.cmp(&b.range.start))
                .then(a.range.end.cmp(&b.range.end))
        });
        let lens = lenses
            .iter()
            .find(|lens| lens.data.as_ref().unwrap()["fqn"] == symbol.fqn)
            .unwrap();
        assert_eq!(
            lens.command.as_ref().unwrap().arguments.as_ref().unwrap()[2],
            serde_json::to_value(expected).unwrap(),
            "{}",
            symbol.fqn
        );
    }
}

fn pause(
    backend: &PhpLspBackend,
    after: bool,
) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (reached, rx) = std::sync::mpsc::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let hooks = &backend.code_lens_control.hooks;
    *if after {
        hooks.after.lock().unwrap()
    } else {
        hooks.before.lock().unwrap()
    } = Some((reached, wait));
    (rx, release)
}
async fn reached(rx: std::sync::mpsc::Receiver<()>) {
    tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(5)).unwrap())
        .await
        .unwrap();
}
async fn fixture(uri: &str) -> (Arc<LspService<PhpLspBackend>>, Arc<WorkspaceIndex>) {
    let (service, _socket) = LspService::new(PhpLspBackend::new);
    let service = Arc::new(service);
    let backend = service.inner();
    let request = backend.request_context_for_uri(uri).await;
    let index = request.index(&backend.index);
    let source = "<?php class Box {function run(){}} function f(Box $b){$b->run();}";
    indexed(&index, uri, source);
    open(backend, uri, source, 1, 1);
    (service, index)
}

#[tokio::test(flavor = "current_thread")]
async fn p238_captured_lenses_reject_new_buffers_templates_reopen_and_index_replacement() {
    for change in 0..7 {
        let uri = format!("file:///p238-race-{change}/Types.php");
        let (service, index) = fixture(&uri).await;
        let (rx, release) = pause(service.inner(), true);
        let other = service.clone();
        let target = uri.clone();
        let job =
            tokio::spawn(
                async move { other.inner().lsp_code_lens(params(&target)).await.unwrap() },
            );
        reached(rx).await;
        match change {
            0 => open(
                service.inner(),
                "file:///p238-new/Other.php",
                "<?php new Box();",
                1,
                1,
            ),
            1 => {
                let extra = "file:///p238-new/Template.blade.php";
                open(service.inner(), extra, "<?php class Fake {}", 1, 1);
                service
                    .inner()
                    .template_documents
                    .insert(extra.into(), preprocess_blade_template("{{ 1 }}"));
            }
            2 => {
                service.inner().document_versions.insert(
                    uri.clone(),
                    OpenDocumentState {
                        version: 1,
                        generation: 2,
                    },
                );
            }
            3 => {
                index.remove_file(&uri);
            }
            4 => {
                let mut current = service.inner().runtime_state.lock().await;
                *current = Arc::new(WorkspaceRuntimeState {
                    generation: current.generation + 1,
                    configs: current.configs.clone(),
                    fallback: current.fallback.clone(),
                    fallback_index: current.fallback_index.clone(),
                });
            }
            5 => {
                service.inner().open_files.remove(&uri);
            }
            _ => open(service.inner(), &uri, "<?php class Changed {}", 2, 2),
        }
        let _ = release.send(());
        assert!(job.await.unwrap().is_none(), "stale case {change}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn p238_cancellation_and_shutdown_stop_the_worker_without_blocking_the_runtime() {
    for shutdown in [false, true] {
        let uri = "file:///p238-cancel/Types.php";
        let (service, _) = fixture(uri).await;
        let (rx, _release) = pause(service.inner(), false);
        let other = service.clone();
        let job =
            tokio::spawn(async move { other.inner().lsp_code_lens(params(uri)).await.unwrap() });
        reached(rx).await;
        // The current-thread runtime remains responsive while the worker is held.
        tokio::time::timeout(Duration::from_secs(1), tokio::task::yield_now())
            .await
            .unwrap();
        assert_eq!(
            service
                .inner()
                .code_lens_control
                .hooks
                .active
                .load(Ordering::SeqCst),
            1
        );
        if shutdown {
            service.inner().lsp_shutdown().await.unwrap();
            assert!(job.await.unwrap().is_none());
        } else {
            job.abort();
            assert!(job.await.unwrap_err().is_cancelled());
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while service
                .inner()
                .code_lens_control
                .hooks
                .active
                .load(Ordering::SeqCst)
                != 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            service
                .inner()
                .code_lens_control
                .hooks
                .references
                .load(Ordering::SeqCst),
            0
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn p238_closed_target_ranges_require_matching_source_provenance() {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("php-lsp-p238-{nonce}.php"));
    let uri = path_to_uri(&path).unwrap();
    let (service, _socket) = LspService::new(PhpLspBackend::new);
    let service = Arc::new(service);
    let backend = service.inner();
    let request = backend.request_context_for_uri(&uri).await;
    let index = request.index(&backend.index);
    let source = "<?php\r\n/* 😀 */ class Box {}";
    std::fs::write(&path, source).unwrap();
    indexed(&index, &uri, source);
    assert!(
        backend.lsp_code_lens(params(&uri)).await.unwrap().is_none(),
        "unverified range snapshot"
    );
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let symbols = extract_file_symbols(parser.tree().unwrap(), source, &uri);
    let references = collect_symbol_references_in_file(parser.tree().unwrap(), source, &symbols);
    index.update_file_with_references_from_source(
        &uri,
        symbols,
        references,
        SourceFingerprint::from_bytes(source.as_bytes()),
    );
    let lenses = backend.lsp_code_lens(params(&uri)).await.unwrap().unwrap();
    assert_eq!(lenses.len(), 1);
    assert_eq!(lenses[0].range.start.character, 15);
    let (rx, release) = pause(backend, true);
    let target = uri.clone();
    let other = service.clone();
    let job =
        tokio::spawn(async move { other.inner().lsp_code_lens(params(&target)).await.unwrap() });
    reached(rx).await;
    std::fs::write(&path, "<?php\n\nclass Changed {}").unwrap();
    release.send(()).unwrap();
    assert!(job.await.unwrap().is_none());
    std::fs::remove_file(path).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn p238_cached_reference_locations_survive_replay_without_reading_callers() {
    use php_lsp_index::cache::{
        build_cache_from_index, load_valid_cached_files, save_cache_atomic, CacheNamespace,
        IndexCacheConfig,
    };
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("php-lsp-p238-cache-{nonce}"));
    std::fs::create_dir_all(&root).unwrap();
    let paths = [root.join("Types.php"), root.join("Caller Ж.php")];
    let uris = paths.each_ref().map(|path| path_to_uri(path).unwrap());
    let sources = [
        "<?php class Box {static function run(){}}",
        "<?php /* 😀 */ Box::run();",
    ];
    let original = WorkspaceIndex::new();
    for ((path, uri), source) in paths.iter().zip(&uris).zip(sources) {
        std::fs::write(path, source).unwrap();
        let mut parser = FileParser::new();
        parser.parse_full(source);
        let symbols = extract_file_symbols(parser.tree().unwrap(), source, uri);
        let references =
            collect_symbol_references_in_file(parser.tree().unwrap(), source, &symbols);
        original.update_file_with_references_from_source(
            uri,
            symbols,
            references,
            SourceFingerprint::from_bytes(source.as_bytes()),
        );
    }
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
    let cache = build_cache_from_index(&original, &root, &paths, &config);
    assert_eq!(cache.files.len(), 2);
    let cache_path = root.join("index.bin");
    save_cache_atomic(&cache_path, &cache).unwrap();
    let (service, _socket) = LspService::new(PhpLspBackend::new);
    let backend = service.inner();
    let request = backend.request_context_for_uri(&uris[0]).await;
    let index = request.index(&backend.index);
    let report = load_valid_cached_files(&index, &cache_path, &root, &paths, &config);
    assert_eq!(report.loaded_files, 2);
    assert!(report.parse_files.is_empty());
    // This is indexed occurrence consumption, not a second disk/parser scan.
    // A real watcher deletion would remove the entry and invalidate its revision.
    std::fs::remove_file(&paths[1]).unwrap();
    let lenses = backend
        .lsp_code_lens(params(&uris[0]))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(lenses.len(), 2);
    for lens in lenses {
        assert_eq!(lens.data.as_ref().unwrap()["references"], 1);
        let locations = &lens.command.unwrap().arguments.unwrap()[2];
        assert_eq!(locations[0]["uri"], uris[1]);
        let column = if lens.data.unwrap()["fqn"] == "Box::run" {
            20
        } else {
            15
        };
        assert_eq!(locations[0]["range"]["start"]["character"], column);
    }
    std::fs::remove_dir_all(root).unwrap();
}
