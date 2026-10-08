use super::*;
use crate::logging::tests::{captured_filter, Capture};
use std::future::Future;
use tracing::instrument::WithSubscriber;

fn settings(level: Option<serde_json::Value>) -> serde_json::Value {
    let mut settings = serde_json::json!({"stubExtensions":[], "diagnosticsMode":"off", "composerEnabled":false, "indexVendor":false});
    if let Some(level) = level {
        settings["logLevel"] = level;
    }
    settings
}

fn service(
    startup: &str,
) -> (
    tower_lsp::LspService<PhpLspBackend>,
    tracing::Dispatch,
    Capture,
) {
    let (filter, dispatch, capture) = captured_filter(startup);
    let (service, mut socket) = tower_lsp::LspService::new(move |client| {
        PhpLspBackend::with_log_filter(client, filter.clone())
    });
    use futures::StreamExt;
    tokio::spawn(async move { while socket.next().await.is_some() {} });
    (service, dispatch, capture)
}

async fn initialize(
    backend: &PhpLspBackend,
    settings: serde_json::Value,
    dispatch: &tracing::Dispatch,
) {
    backend.lsp_initialize(serde_json::from_value(serde_json::json!({"capabilities":{}, "rootUri":null, "initializationOptions":settings})).unwrap()).with_subscriber(dispatch.clone()).await.unwrap();
}

async fn change(
    backend: &PhpLspBackend,
    settings: serde_json::Value,
    dispatch: &tracing::Dispatch,
) {
    backend
        .lsp_did_change_configuration(DidChangeConfigurationParams { settings })
        .with_subscriber(dispatch.clone())
        .await;
}

async fn assert_debug_probe(
    backend: &PhpLspBackend,
    dispatch: &tracing::Dispatch,
    capture: &Capture,
    expected: bool,
) {
    capture.take();
    backend.lsp_goto_declaration(serde_json::from_value(serde_json::json!({"textDocument":{"uri":"file:///runtime-log-probe.php"}, "position":{"line":0,"character":0}})).unwrap()).with_subscriber(dispatch.clone()).await.unwrap();
    let logs = capture.take();
    assert_eq!(
        logs.contains("runtime-log-probe.php"),
        expected,
        "unexpected DEBUG emission: {logs}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn runtime_changes_enable_and_suppress_existing_log_callsites() {
    use tower::{Service, ServiceExt};
    use tower_lsp::jsonrpc::Request;
    let (mut service, dispatch, capture) = service("error");
    let request = Request::build("initialize")
        .id(1)
        .params(serde_json::json!({
            "capabilities":{}, "rootUri":null, "initializationOptions":settings(None)
        }))
        .finish();
    async { service.ready().await.unwrap().call(request).await.unwrap() }
        .with_subscriber(dispatch.clone())
        .await
        .unwrap();
    for (level, debug) in [
        ("debug", true),
        ("error", false),
        (" TRACE ", true),
        ("warn", false),
        ("info", false),
        ("debug", true),
    ] {
        let change = Request::build("workspace/didChangeConfiguration")
            .params(serde_json::json!({
                "settings": settings(Some(serde_json::json!(level)))
            }))
            .finish();
        async { service.ready().await.unwrap().call(change).await.unwrap() }
            .with_subscriber(dispatch.clone())
            .await;
        capture.take();
        let probe = Request::build("textDocument/declaration").id(2).params(serde_json::json!({
            "textDocument":{"uri":"file:///runtime-log-probe.php"}, "position":{"line":0,"character":0}
        })).finish();
        let response = async { service.ready().await.unwrap().call(probe).await.unwrap() }
            .with_subscriber(dispatch.clone())
            .await
            .unwrap();
        assert!(response.error().is_none(), "{response:?}");
        let logs = capture.take();
        assert_eq!(
            logs.contains("runtime-log-probe.php"),
            debug,
            "{level}: {logs}"
        );
    }
    service.inner().lsp_shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn removed_override_restores_startup_directives_and_invalid_values_preserve_active_filter() {
    let (service, dispatch, capture) =
        service("error,php_lsp_server::server::lsp::definition=debug");
    let backend = service.inner();
    initialize(
        backend,
        settings(Some(serde_json::json!("error"))),
        &dispatch,
    )
    .await;
    assert_debug_probe(backend, &dispatch, &capture, false).await;
    for value in [
        serde_json::json!(""),
        serde_json::json!("invalid"),
        serde_json::json!("debug,other=trace"),
        serde_json::json!(17),
        serde_json::Value::Null,
    ] {
        change(backend, settings(Some(value)), &dispatch).await;
        assert_debug_probe(backend, &dispatch, &capture, false).await;
    }
    change(backend, settings(None), &dispatch).await;
    assert_debug_probe(backend, &dispatch, &capture, true).await;
    tracing::dispatcher::with_default(
        &dispatch,
        || tracing::debug!(target: "unrelated", "unrelated-debug"),
    );
    assert!(
        capture.take().is_empty(),
        "restoration must keep target-specific directives"
    );
    backend.lsp_shutdown().await.unwrap();
}

struct Roots(PathBuf);
impl Roots {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "php-lsp-runtime-log-roots-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        Self(root)
    }
}
impl Drop for Roots {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn only_the_global_window_level_controls_multi_root_logs_and_preserves_indexes() {
    let roots = Roots::new();
    let uris = [
        php_lsp_types::uri::path_to_uri(&roots.0.join("a")).unwrap(),
        php_lsp_types::uri::path_to_uri(&roots.0.join("b")).unwrap(),
    ];
    let (service, dispatch, capture) = service("debug");
    let backend = service.inner();
    let mut snapshot = serde_json::json!({
        "configurationVersion":2,
        "global":settings(Some(serde_json::json!("error"))),
        "workspaceFolders":[
            {"uri":uris[0], "settings":{"logLevel":"trace"}},
            {"uri":uris[1], "settings":{"logLevel":"debug"}}
        ]
    });
    backend
        .lsp_initialize(
            serde_json::from_value(serde_json::json!({
                "capabilities":{}, "rootUri":null,
                "workspaceFolders":[{"uri":uris[0], "name":"a"}, {"uri":uris[1], "name":"b"}],
                "initializationOptions":snapshot
            }))
            .unwrap(),
        )
        .with_subscriber(dispatch.clone())
        .await
        .unwrap();
    assert_debug_probe(backend, &dispatch, &capture, false).await;
    let before = backend.runtime_state_snapshot().await;
    assert_eq!(before.configs.len(), 2);
    snapshot["global"] = settings(Some(serde_json::json!("debug")));
    change(backend, snapshot.clone(), &dispatch).await;
    assert_debug_probe(backend, &dispatch, &capture, true).await;
    let after = backend.runtime_state_snapshot().await;
    assert!(Arc::ptr_eq(&before.fallback_index, &after.fallback_index));
    for (before, after) in before.configs.iter().zip(&after.configs) {
        assert!(
            Arc::ptr_eq(&before.index, &after.index),
            "log-only changes replaced an index"
        );
    }
    snapshot["global"] = settings(None);
    snapshot["workspaceFolders"][0]["settings"]["logLevel"] = serde_json::json!("error");
    change(backend, snapshot, &dispatch).await;
    assert_debug_probe(backend, &dispatch, &capture, true).await;
    backend
        .lsp_did_change_workspace_folders(DidChangeWorkspaceFoldersParams {
            event: WorkspaceFoldersChangeEvent {
                added: vec![],
                removed: vec![WorkspaceFolder {
                    uri: uris[0].parse().unwrap(),
                    name: "a".to_string(),
                }],
            },
        })
        .with_subscriber(dispatch.clone())
        .await;
    assert_debug_probe(backend, &dispatch, &capture, true).await;
    assert_eq!(backend.runtime_state_snapshot().await.configs.len(), 1);
    backend.lsp_shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_waiting_configuration_change_wins_after_an_older_locked_publication() {
    let (service, dispatch, capture) = service("error");
    let backend = service.inner();
    initialize(backend, settings(None), &dispatch).await;
    let before = backend.runtime_state_snapshot().await;
    let gate = backend.configuration_reload.lock().await;
    let mut newer = Box::pin(
        backend
            .lsp_did_change_configuration(DidChangeConfigurationParams {
                settings: settings(Some(serde_json::json!("error"))),
            })
            .with_subscriber(dispatch.clone()),
    );
    // Poll to Pending under a held publication gate, rather than relying on sleep.
    std::future::poll_fn(|cx| match newer.as_mut().poll(cx) {
        std::task::Poll::Pending => std::task::Poll::Ready(()),
        std::task::Poll::Ready(()) => panic!("newer configuration bypassed the held gate"),
    })
    .await;
    assert_debug_probe(backend, &dispatch, &capture, false).await;
    *backend.client_settings.lock().await = settings(Some(serde_json::json!("trace")));
    backend
        .reload_effective_configuration_under_lock()
        .with_subscriber(dispatch.clone())
        .await;
    assert_debug_probe(backend, &dispatch, &capture, true).await;
    let intermediate = backend.runtime_state_snapshot().await;
    assert!(intermediate.generation > before.generation);
    drop(gate);
    tokio::time::timeout(Duration::from_secs(10), newer)
        .await
        .expect("queued configuration progress");
    let latest = backend.runtime_state_snapshot().await;
    assert!(latest.generation > intermediate.generation);
    assert_eq!(
        latest.fallback.log_level,
        LogLevelSetting::Override(tracing::Level::ERROR)
    );
    assert_debug_probe(backend, &dispatch, &capture, false).await;
    assert!(Arc::ptr_eq(&before.fallback_index, &latest.fallback_index));
    backend.lsp_shutdown().await.unwrap();
}
