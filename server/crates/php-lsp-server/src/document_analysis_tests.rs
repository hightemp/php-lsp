use super::*;
use futures::StreamExt;
use php_lsp_parser::analysis_metrics::{Probe, Work};
use tower::{Service, ServiceExt};
use tower_lsp::LspService;

async fn backend(uri: &str) -> LspService<PhpLspBackend> {
    backend_with_messages(uri).await.0
}
async fn backend_with_messages(
    uri: &str,
) -> (
    LspService<PhpLspBackend>,
    mpsc::UnboundedReceiver<tower_lsp::jsonrpc::Request>,
) {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(message) = socket.next().await {
            let _ = tx.send(message);
        }
    });
    let initialized = service.ready().await.unwrap().call(tower_lsp::jsonrpc::Request::build("initialize").id(1).params(serde_json::json!({"capabilities":{},"initializationOptions":{"stubExtensions":[],"indexVendor":false,"diagnosticsMode":"syntax-only"}})).finish()).await.unwrap().unwrap();
    assert!(initialized.error().is_none());
    let backend = service.inner();
    backend.did_open(serde_json::from_value(serde_json::json!({"textDocument":{"uri":uri,"languageId":"php","version":1,"text":format!("<?php /* {uri} */\nfunction foo() {{}}\n")}})).unwrap()).await;
    (service, rx)
}

#[tokio::test(flavor = "current_thread")]
async fn analysis_repeated_snapshots_reuse_source_and_symbols() {
    let uri = "file:///analysis-snapshots.php";
    let service = backend(uri).await;
    service.inner().open_document_snapshot(uri).unwrap();
    let probe = Probe::new(uri);
    for _ in 0..5 {
        let snapshot = service.inner().open_document_snapshot(uri).unwrap();
        assert_eq!(snapshot.file_symbols.symbols[0].name, "foo");
    }
    assert_eq!(
        probe.count(Work::Symbols),
        0,
        "repeated analysis: {:?}",
        probe.all()
    );
    assert_eq!(
        probe.count(Work::Source),
        0,
        "repeated source copies: {:?}",
        probe.all()
    );
    service.inner().shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn analysis_change_batches_parses_and_publishes_each_index_once() {
    let uri = "file:///analysis-change.php";
    let service = backend(uri).await;
    let probe = Probe::new(uri);
    service.inner().did_change(serde_json::from_value(serde_json::json!({"textDocument":{"uri":uri,"version":7},"contentChanges":[{"range":{"start":{"line":1,"character":9},"end":{"line":1,"character":12}},"text":"bar"},{"range":{"start":{"line":1,"character":9},"end":{"line":1,"character":12}},"text":"baz"}]})).unwrap()).await;
    service.inner().cancel_debounced_diagnostics(uri).await;
    let counts = probe.all();
    assert_eq!(
        (counts[0], counts[2], counts[3], counts[4]),
        (1, 1, 1, 2),
        "parse/symbols/refs/index writes: {counts:?}"
    );
    assert!(service.inner().index.resolve_fqn("baz").is_some());
    assert!(service.inner().index.resolve_fqn("foo").is_none());
    service.inner().shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn analysis_diagnostics_reuse_the_current_parse() {
    let uri = "file:///analysis-diagnostics.php";
    let service = backend(uri).await;
    let probe = Probe::new(uri);
    service
        .inner()
        .publish_diagnostics(&uri.parse().unwrap())
        .await;
    assert_eq!(
        probe.count(Work::Parse),
        0,
        "diagnostics reparsed an existing tree: {:?}",
        probe.all()
    );
    service.inner().shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn analysis_currentness_checks_do_not_extract_symbols_or_copy_source() {
    let uri = "file:///analysis-current.php";
    let service = backend(uri).await;
    let backend = service.inner();
    let context = backend.request_context_for_uri(uri).await;
    let request = DiagnosticPublishRequest {
        uri: uri.parse().unwrap(),
        diagnostics: vec![],
        version: Some(1),
        expected_state: backend.current_document_state(uri),
        expected_template: None,
        expected_analyzer_source: None,
        require_idle_index: false,
        expected_runtime_generation: context.state.generation,
        indexing_workspace_folder: None,
        expected_indexing_run: None,
        expected_runtime_config: context.runtime_config().clone(),
        expected_index: context.index(&backend.index),
        computation_sequence: 0,
    };
    let probe = Probe::new(uri);
    for _ in 0..5 {
        assert!(diagnostic_publish_request_is_current(
            &request,
            &backend.open_files,
            &backend.document_versions,
            &backend.template_documents
        ));
    }
    assert_eq!(
        (probe.count(Work::Symbols), probe.count(Work::Source)),
        (0, 0),
        "currentness did analysis: {:?}",
        probe.all()
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn analysis_change_removes_excluded_root_entries_instead_of_leaving_old_symbols() {
    let uri = "file:///analysis-root/src/Subject.php";
    let service = backend(uri).await;
    let backend = service.inner();
    let old = backend.open_document_snapshot(uri).unwrap();
    let root_index = Arc::new(WorkspaceIndex::new());
    root_index.update_file_with_references(uri, old.file_symbols.as_ref().clone(), vec![]);
    let mut state = backend.runtime_state.lock().await;
    *state = Arc::new(WorkspaceRuntimeState {
        fallback: state.fallback.clone(),
        fallback_index: state.fallback_index.clone(),
        generation: state.generation + 1,
        configs: vec![WorkspaceRootConfig {
            workspace_folder: PathBuf::from("/analysis-root"),
            root: PathBuf::from("/analysis-root"),
            namespace_map: None,
            runtime_config: ResolvedRuntimeConfiguration {
                exclude_paths: vec![PathBuf::from("src")],
                ..state.fallback.clone()
            },
            index: root_index.clone(),
            vendor_file_lru: Arc::new(Mutex::new(VendorFileLru::default())),
        }],
    });
    drop(state);
    backend.did_change(serde_json::from_value(serde_json::json!({"textDocument":{"uri":uri,"version":2},"contentChanges":[{"text":"<?php function fresh() {}"}]})).unwrap()).await;
    backend.cancel_debounced_diagnostics(uri).await;
    assert!(
        root_index.resolve_fqn("foo").is_none(),
        "excluded root retained old symbols"
    );
    assert!(root_index.resolve_fqn("fresh").is_none());
    assert!(backend.index.resolve_fqn("foo").is_none());
    backend.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn analysis_debounced_publication_does_not_rebuild_a_generic_snapshot() {
    let uri = "file:///analysis-debounced.php";
    let (service, mut messages) = backend_with_messages(uri).await;
    let probe = Probe::new(uri);
    service.inner().did_change(serde_json::from_value(serde_json::json!({"textDocument":{"uri":uri,"version":7},"contentChanges":[{"range":{"start":{"line":1,"character":9},"end":{"line":1,"character":12}},"text":"bar"}]})).unwrap()).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(message) = messages.recv().await {
            if message.method() == "textDocument/publishDiagnostics"
                && message
                    .params()
                    .is_some_and(|p| p["uri"] == uri && p["version"] == 7)
            {
                return;
            }
        }
        panic!("publisher stopped");
    })
    .await
    .unwrap();
    let doc = serde_json::json!({"textDocument":{"uri":uri}});
    let pos = serde_json::json!({"textDocument":{"uri":uri},"position":{"line":1,"character":10}});
    assert!(service
        .inner()
        .document_symbol(serde_json::from_value(doc).unwrap())
        .await
        .unwrap()
        .is_some());
    assert!(service
        .inner()
        .hover(serde_json::from_value(pos.clone()).unwrap())
        .await
        .unwrap()
        .is_some());
    assert!(service
        .inner()
        .document_highlight(serde_json::from_value(pos).unwrap())
        .await
        .unwrap()
        .is_some());
    let counts = probe.all();
    assert_eq!(
        (counts[0], counts[2], counts[3], counts[4]),
        (1, 1, 1, 2),
        "debounce/publication repeated source work: {counts:?}"
    );
    service.inner().shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn analysis_hot_lsp_readers_reuse_prepared_versioned_facts() {
    let uri = "file:///analysis-hot-readers.php";
    let service = backend(uri).await;
    service.inner().open_document_snapshot(uri).unwrap();
    service
        .inner()
        .open_document_snapshot_for_php_version(uri, PhpVersion::DEFAULT)
        .unwrap();
    let probe = Probe::new(uri);
    let doc = serde_json::json!({"textDocument":{"uri":uri}});
    let pos = serde_json::json!({"textDocument":{"uri":uri},"position":{"line":1,"character":10}});
    assert!(service
        .inner()
        .document_symbol(serde_json::from_value(doc).unwrap())
        .await
        .unwrap()
        .is_some());
    assert!(service
        .inner()
        .hover(serde_json::from_value(pos.clone()).unwrap())
        .await
        .unwrap()
        .is_some());
    assert!(service
        .inner()
        .document_highlight(serde_json::from_value(pos).unwrap())
        .await
        .unwrap()
        .is_some());
    assert_eq!(
        (probe.count(Work::Symbols), probe.count(Work::Source)),
        (0, 0),
        "hot readers bypassed the document cache: {:?}",
        probe.all()
    );
    service.inner().shutdown().await.unwrap();
}

#[derive(Default)]
struct AnalysisGate {
    entered: tokio::sync::Notify,
    once: AtomicBool,
    released: StdMutex<bool>,
    cv: std::sync::Condvar,
}
impl AnalysisGate {
    fn block(&self, work: Work) {
        if work != Work::Symbols || self.once.swap(true, Ordering::SeqCst) {
            return;
        }
        self.entered.notify_one();
        let (released, _) = self
            .cv
            .wait_timeout_while(
                self.released.lock().unwrap(),
                Duration::from_secs(3),
                |released| !*released,
            )
            .unwrap();
        assert!(*released, "analysis gate watchdog expired");
    }
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.cv.notify_all();
    }
}
struct ReleaseAnalysis(Arc<AnalysisGate>);
impl Drop for ReleaseAnalysis {
    fn drop(&mut self) {
        self.0.release();
    }
}

fn root_config(
    root: &str,
    index: Arc<WorkspaceIndex>,
    php_version: PhpVersion,
    excluded: bool,
) -> WorkspaceRootConfig {
    WorkspaceRootConfig {
        workspace_folder: PathBuf::from(root),
        root: PathBuf::from(root),
        namespace_map: None,
        runtime_config: ResolvedRuntimeConfiguration {
            php_version,
            exclude_paths: if excluded {
                vec![PathBuf::from("src")]
            } else {
                vec![]
            },
            diagnostics_mode: DiagnosticsMode::SyntaxOnly,
            index_vendor: false,
            ..Default::default()
        },
        index,
        vendor_file_lru: Arc::new(Mutex::new(VendorFileLru::default())),
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn analysis_runtime_replacement_during_preparation_uses_latest_targets_and_version() {
    let uri = "file:///analysis-race/src/Subject.php";
    let service = Arc::new(backend(uri).await);
    let old_index = Arc::new(WorkspaceIndex::new());
    {
        let mut state = service.inner().runtime_state.lock().await;
        *state = Arc::new(WorkspaceRuntimeState {
            fallback: state.fallback.clone(),
            fallback_index: state.fallback_index.clone(),
            generation: state.generation + 1,
            configs: vec![root_config(
                "/analysis-race",
                old_index.clone(),
                PhpVersion { major: 8, minor: 2 },
                false,
            )],
        });
    }
    let gate = Arc::new(AnalysisGate::default());
    let release = ReleaseAnalysis(gate.clone());
    let g = gate.clone();
    let _probe = Probe::with_hook(uri, move |work| g.block(work));
    let task = {
        let service = service.clone();
        tokio::spawn(async move {
            service.inner().did_change(serde_json::from_value(serde_json::json!({"textDocument":{"uri":uri,"version":7},"contentChanges":[{"text":format!("<?php /* {uri} */ #[PhpStormStubsElementAvailable(from: '8.4')] function future() {{}} function current() {{}}") }]})).unwrap()).await;
        })
    };
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    // Both the document and runtime must remain accessible while source facts build.
    assert!(service.inner().open_files.try_get_mut(uri).is_present());
    let new_index = Arc::new(WorkspaceIndex::new());
    {
        let mut state =
            tokio::time::timeout(Duration::from_secs(1), service.inner().runtime_state.lock())
                .await
                .unwrap();
        *state = Arc::new(WorkspaceRuntimeState {
            fallback: state.fallback.clone(),
            fallback_index: state.fallback_index.clone(),
            generation: state.generation + 1,
            configs: vec![root_config(
                "/analysis-race",
                new_index.clone(),
                PhpVersion { major: 8, minor: 4 },
                false,
            )],
        });
    }
    drop(release);
    task.await.unwrap();
    service.inner().cancel_debounced_diagnostics(uri).await;
    assert!(old_index.resolve_fqn("current").is_none());
    assert!(new_index.resolve_fqn("future").is_some());
    assert!(service.inner().index.resolve_fqn("future").is_some());
    service.inner().shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn analysis_close_reopen_during_preparation_cannot_publish_the_old_lifetime() {
    let uri = "file:///analysis-reopen.php";
    let service = Arc::new(backend(uri).await);
    let old = service.inner().open_document_snapshot(uri).unwrap();
    let gate = Arc::new(AnalysisGate::default());
    let release = ReleaseAnalysis(gate.clone());
    let g = gate.clone();
    let _probe = Probe::with_hook(uri, move |work| g.block(work));
    let task = {
        let service = service.clone();
        tokio::spawn(async move {
            service.inner().did_change(serde_json::from_value(serde_json::json!({"textDocument":{"uri":uri,"version":7},"contentChanges":[{"text":format!("<?php /* {uri} */ function stale() {{}}") }]})).unwrap()).await;
        })
    };
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    service
        .inner()
        .did_close(serde_json::from_value(serde_json::json!({"textDocument":{"uri":uri}})).unwrap())
        .await;
    service.inner().did_open(serde_json::from_value(serde_json::json!({"textDocument":{"uri":uri,"languageId":"php","version":1,"text":"<?php function reopened() {}"}})).unwrap()).await;
    drop(release);
    task.await.unwrap();
    let current = service.inner().open_document_snapshot(uri).unwrap();
    assert_ne!(old.document_state, current.document_state);
    assert!(!Arc::ptr_eq(&old.parsed, &current.parsed));
    assert_eq!(old.file_symbols.symbols[0].name, "foo");
    assert!(service.inner().index.resolve_fqn("stale").is_none());
    assert!(service.inner().index.resolve_fqn("reopened").is_some());
    service.inner().shutdown().await.unwrap();
}
#[tokio::test(flavor = "current_thread")]
async fn analysis_overlapping_roots_keep_versions_separate_and_clear_excluded_targets() {
    let uri = "file:///analysis-overlap/src/Subject.php";
    let service = backend(uri).await;
    let backend = service.inner();
    let old = backend.open_document_snapshot(uri).unwrap();
    let a = Arc::new(WorkspaceIndex::new());
    let b = Arc::new(WorkspaceIndex::new());
    let excluded = Arc::new(WorkspaceIndex::new());
    excluded.update_file_with_references(uri, old.file_symbols.as_ref().clone(), vec![]);
    {
        let mut state = backend.runtime_state.lock().await;
        *state = Arc::new(WorkspaceRuntimeState {
            fallback: state.fallback.clone(),
            fallback_index: state.fallback_index.clone(),
            generation: state.generation + 1,
            configs: vec![
                root_config(
                    "/analysis-overlap",
                    a.clone(),
                    PhpVersion { major: 8, minor: 2 },
                    false,
                ),
                root_config(
                    "/analysis-overlap",
                    b.clone(),
                    PhpVersion { major: 8, minor: 4 },
                    false,
                ),
                root_config(
                    "/analysis-overlap",
                    excluded.clone(),
                    PhpVersion { major: 8, minor: 4 },
                    true,
                ),
            ],
        });
    }
    backend.did_change(serde_json::from_value(serde_json::json!({"textDocument":{"uri":uri,"version":2},"contentChanges":[{"text":"<?php #[PhpStormStubsElementAvailable(from: '8.4')] function future() {} function shared() {}"}]})).unwrap()).await;
    backend.cancel_debounced_diagnostics(uri).await;
    assert!(a.resolve_fqn("future").is_none());
    assert!(b.resolve_fqn("future").is_some());
    assert!(a.resolve_fqn("shared").is_some());
    assert!(b.resolve_fqn("shared").is_some());
    assert!(
        excluded.resolve_fqn("foo").is_none(),
        "excluded overlapping target retained old source"
    );
    backend.shutdown().await.unwrap();
}
#[tokio::test(flavor = "current_thread")]
async fn analysis_diagnostics_use_snapshot_aliases_after_index_source_replacement() {
    let uri = "file:///analysis-diagnostic-aliases.php";
    let mut parser = FileParser::new();
    parser.parse_full(
        "<?php namespace Old; use Vendor\\MissingOld as Alias; function run(Alias $value) {}",
    );
    let parsed = parser.snapshot();
    let index = Arc::new(WorkspaceIndex::new());
    let config = DiagnosticsRuntimeConfig {
        mode: DiagnosticsMode::BasicSemantic,
        ..Default::default()
    };
    let expected = compute_snapshot_diagnostics_blocking(
        uri.into(),
        parsed.clone(),
        index.clone(),
        config,
        Some(1),
    )
    .await;
    let mut replacement = FileParser::new();
    replacement.parse_full(
        "<?php namespace Newer; use Vendor\\MissingNew as Alias; function run(Alias $value) {}",
    );
    let facts = replacement.snapshot().analysis(uri, None).unwrap();
    index.update_file_with_references(uri, facts.symbols().as_ref().clone(), vec![]);
    let actual =
        compute_snapshot_diagnostics_blocking(uri.into(), parsed, index, config, Some(1)).await;
    assert_eq!(actual, expected);
    assert!(actual
        .iter()
        .any(|diagnostic| diagnostic.message.contains("MissingOld")));
    assert!(actual
        .iter()
        .all(|diagnostic| !diagnostic.message.contains("MissingNew")));
}

#[tokio::test(flavor = "current_thread")]
async fn analysis_publication_identity_rejects_context_only_template_refresh() {
    let uri = "file:///analysis-context.twig";
    let service = backend(uri).await;
    let backend = service.inner();
    let original = preprocess_twig_template(
        "{{ value }}",
        &[TemplateVariableType {
            name: "value".into(),
            type_text: "OldType".into(),
            shape_definitions: vec![],
        }],
    );
    {
        let mut parser = backend.open_files.get_mut(uri).unwrap();
        parser.parse_full(original.virtual_source());
        backend
            .template_documents
            .insert(uri.into(), original.clone());
    }
    let context = backend.request_context_for_uri(uri).await;
    let request = DiagnosticPublishRequest {
        uri: uri.parse().unwrap(),
        diagnostics: vec![],
        version: Some(1),
        expected_state: backend.current_document_state(uri),
        expected_template: Some(original.clone()),
        expected_analyzer_source: None,
        require_idle_index: false,
        expected_runtime_generation: context.state.generation,
        indexing_workspace_folder: None,
        expected_indexing_run: None,
        expected_runtime_config: context.runtime_config().clone(),
        expected_index: context.index(&backend.index),
        computation_sequence: 0,
    };
    let probe = Probe::new(uri);
    assert!(diagnostic_publish_request_is_current(
        &request,
        &backend.open_files,
        &backend.document_versions,
        &backend.template_documents
    ));
    let refreshed = original.with_twig_variable_types(&[TemplateVariableType {
        name: "value".into(),
        type_text: "NewType".into(),
        shape_definitions: vec![],
    }]);
    {
        let mut parser = backend.open_files.get_mut(uri).unwrap();
        parser.parse_full(refreshed.virtual_source());
        backend.template_documents.insert(uri.into(), refreshed);
    }
    assert_eq!(backend.current_document_state(uri), request.expected_state);
    assert!(!diagnostic_publish_request_is_current(
        &request,
        &backend.open_files,
        &backend.document_versions,
        &backend.template_documents
    ));
    assert_eq!(
        (probe.count(Work::Symbols), probe.count(Work::Source)),
        (0, 0)
    );
    backend.shutdown().await.unwrap();
}
