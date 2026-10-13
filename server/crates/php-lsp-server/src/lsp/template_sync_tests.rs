use super::*;
use futures::StreamExt;
use serde_json::json;
use tower_lsp::LspService;

async fn service() -> LspService<PhpLspBackend> {
    let (service, mut socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move { while socket.next().await.is_some() {} });
    service.inner().lsp_initialize(serde_json::from_value(json!({
        "capabilities": {},
        "initializationOptions": {"stubExtensions": [], "indexVendor": false, "diagnosticsMode": "off"}
    })).unwrap()).await.unwrap();
    service
}

async fn open(backend: &PhpLspBackend, uri: &str, language: &str, text: &str) {
    backend
        .lsp_did_open(
            serde_json::from_value(json!({"textDocument": {
                "uri": uri, "languageId": language, "version": 1, "text": text
            }}))
            .unwrap(),
        )
        .await;
}

async fn change(backend: &PhpLspBackend, uri: &str, version: i32, changes: serde_json::Value) {
    backend
        .lsp_did_change(
            serde_json::from_value(json!({
                "textDocument": {"uri": uri, "version": version}, "contentChanges": changes
            }))
            .unwrap(),
        )
        .await;
}

fn edit(start: (u32, u32), end: (u32, u32), text: &str) -> serde_json::Value {
    json!({"range": {"start": {"line": start.0, "character": start.1},
                     "end": {"line": end.0, "character": end.1}}, "text": text})
}

#[tokio::test(flavor = "current_thread")]
async fn p233_invalid_template_line_preserves_version_and_requires_full_sync() {
    for (language, uri, source) in [
        (
            "blade",
            "file:///test/p233-invalid.blade.php",
            "{{ $before }}",
        ),
        ("twig", "file:///test/p233-invalid.twig", "{{ before }}"),
    ] {
        let service = service().await;
        let backend = service.inner();
        open(backend, uri, language, source).await;
        let before = backend.open_document_snapshot(uri).unwrap();
        change(backend, uri, 2, json!([edit((99, 0), (99, 1), "bad")])).await;
        let after = backend.open_document_snapshot(uri).unwrap();
        assert_eq!(
            after.document_state, before.document_state,
            "invalid edit accepted new version"
        );
        assert_eq!(after.source, before.source);
        assert_eq!(after.template_document.unwrap().original_source(), source);
        assert!(backend.documents_requiring_full_sync.contains_key(uri));
        change(backend, uri, 3, json!([edit((0, 0), (0, 0), "ignored")])).await;
        assert_eq!(backend.current_document_state(uri), before.document_state);
        change(backend, uri, 4, json!([{"text": "{{ 1.5 }}"}])).await;
        assert_eq!(backend.current_document_state(uri).unwrap().version, 4);
        assert!(!backend.documents_requiring_full_sync.contains_key(uri));
        backend.lsp_shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn p233_template_edit_batch_rolls_back_when_a_later_range_is_invalid() {
    for (language, uri, source) in [
        (
            "blade",
            "file:///test/p233-batch.blade.php",
            "{{ $before }}",
        ),
        ("twig", "file:///test/p233-batch.twig", "{{ before }}"),
    ] {
        let service = service().await;
        let backend = service.inner();
        open(backend, uri, language, source).await;
        let before = backend.open_document_snapshot(uri).unwrap();
        change(
            backend,
            uri,
            2,
            json!([
                edit((0, 0), (0, 0), "prefix\n"),
                edit((90, 0), (90, 1), "bad")
            ]),
        )
        .await;
        let after = backend.open_document_snapshot(uri).unwrap();
        assert_eq!(after.source, before.source, "partial virtual PHP committed");
        assert_eq!(
            after.template_document.unwrap().original_source(),
            source,
            "partial original source committed"
        );
        assert_eq!(after.document_state, before.document_state);
        assert!(backend.documents_requiring_full_sync.contains_key(uri));
        backend.lsp_shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn p233_full_text_then_invalid_range_rolls_back_the_entire_batch() {
    for (language, uri) in [
        ("blade", "file:///test/p233-full-batch.blade.php"),
        ("twig", "file:///test/p233-full-batch.twig"),
    ] {
        let service = service().await;
        let backend = service.inner();
        open(backend, uri, language, "first\n{{ 2.5 }}").await;
        let before = backend.open_document_snapshot(uri).unwrap();
        change(
            backend,
            uri,
            2,
            json!([{"text": "short"}, edit((1, 0), (1, 2), "invalid on new source")]),
        )
        .await;
        let after = backend.open_document_snapshot(uri).unwrap();
        assert_eq!(after.document_state, before.document_state);
        assert_eq!(after.source, before.source);
        assert_eq!(
            after.template_document.unwrap().original_source(),
            "first\n{{ 2.5 }}"
        );
        assert!(backend.documents_requiring_full_sync.contains_key(uri));
        backend.lsp_shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn p233_valid_template_batch_uses_each_preceding_edit_as_its_base() {
    for (language, uri) in [
        ("blade", "file:///test/p233-layout.blade.php"),
        ("twig", "file:///test/p233-layout.twig"),
    ] {
        let service = service().await;
        let backend = service.inner();
        open(backend, uri, language, "{{ 1.5 }}").await;
        change(
            backend,
            uri,
            2,
            json!([edit((0, 0), (0, 0), "😀\r\n"), edit((1, 3), (1, 6), "2.75")]),
        )
        .await;
        let after = backend.open_document_snapshot(uri).unwrap();
        let template = after.template_document.unwrap();
        assert_eq!(after.document_state.unwrap().version, 2);
        assert_eq!(template.original_source(), "😀\r\n{{ 2.75 }}");
        assert_eq!(template.virtual_source(), after.source);
        assert!(!backend.documents_requiring_full_sync.contains_key(uri));
        backend.lsp_shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn p233_invalid_recovery_batch_keeps_desync_until_a_successful_full_sync() {
    let service = service().await;
    let backend = service.inner();
    let uri = "file:///test/p233-recovery.twig";
    open(backend, uri, "twig", "{{ 1.5 }}").await;
    for (version, batch) in [
        (2, json!([edit((99, 0), (99, 1), "bad")])),
        (
            3,
            json!([{"text": "{{ 2.5 }}"}, edit((99, 0), (99, 1), "bad")]),
        ),
    ] {
        change(backend, uri, version, batch).await;
        assert_eq!(backend.current_document_state(uri).unwrap().version, 1);
        assert!(backend.documents_requiring_full_sync.contains_key(uri));
    }
    change(
        backend,
        uri,
        4,
        json!([{"text": "{{ 2.5 }}"}, edit((0, 3), (0, 6), "3.5")]),
    )
    .await;
    assert_eq!(backend.current_document_state(uri).unwrap().version, 4);
    assert!(!backend.documents_requiring_full_sync.contains_key(uri));
    change(backend, uri, 2, json!([edit((99, 0), (99, 1), "stale")])).await;
    assert_eq!(backend.current_document_state(uri).unwrap().version, 4);
    assert!(!backend.documents_requiring_full_sync.contains_key(uri));
    assert_eq!(
        backend
            .open_document_snapshot(uri)
            .unwrap()
            .template_document
            .unwrap()
            .original_source(),
        "{{ 3.5 }}"
    );
    backend.lsp_shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn p233_template_desync_is_isolated_by_uri_and_cleared_on_reopen() {
    let service = service().await;
    let backend = service.inner();
    let bad = "file:///test/p233-close.twig";
    let good = "file:///test/p233-other.blade.php";
    open(backend, bad, "twig", "{{ 1.5 }}").await;
    open(backend, good, "blade", "{{ 1.5 }}").await;
    let old = backend.current_document_state(bad).unwrap();
    change(backend, bad, 2, json!([edit((99, 0), (99, 1), "bad")])).await;
    change(backend, good, 2, json!([edit((0, 3), (0, 6), "2.5")])).await;
    assert_eq!(backend.current_document_state(good).unwrap().version, 2);
    assert!(!backend.documents_requiring_full_sync.contains_key(good));
    backend
        .lsp_did_close(serde_json::from_value(json!({"textDocument": {"uri": bad}})).unwrap())
        .await;
    assert!(!backend.documents_requiring_full_sync.contains_key(bad));
    open(backend, bad, "twig", "{{ 3.5 }}").await;
    assert_ne!(
        backend.current_document_state(bad).unwrap().generation,
        old.generation
    );
    change(backend, bad, 2, json!([edit((0, 3), (0, 6), "4.5")])).await;
    assert_eq!(
        backend
            .open_document_snapshot(bad)
            .unwrap()
            .template_document
            .unwrap()
            .original_source(),
        "{{ 4.5 }}"
    );
    backend.lsp_shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p233_concurrent_invalid_edits_and_full_sync_keep_coherent_snapshots() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let service = std::sync::Arc::new(service().await);
        for (language, uri) in [
            ("blade", "file:///test/p233-race.blade.php"),
            ("twig", "file:///test/p233-race.twig"),
        ] {
            open(service.inner(), uri, language, "{{ 1.5 }}").await;
            for round in 0..20 {
                let before = service.inner().open_document_snapshot(uri).unwrap();
                let version = before.document_state.unwrap().version + 1;
                let next_source = format!("😀 round {round}\r\n{{{{ {}.5 }}}}", round + 2);
                let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(3));
                let first_committed = std::sync::Arc::new(tokio::sync::Notify::new());
                let bad_service = service.clone();
                let bad_barrier = barrier.clone();
                let bad_order = first_committed.clone();
                let invalid = tokio::spawn(async move {
                    bad_barrier.wait().await;
                    if round % 2 == 1 {
                        bad_order.notified().await;
                    }
                    change(
                        bad_service.inner(),
                        uri,
                        version,
                        json!([edit((99, 0), (99, 1), "bad")]),
                    )
                    .await;
                    if round % 2 == 0 {
                        bad_order.notify_one();
                    }
                });
                let full_service = service.clone();
                let full_barrier = barrier.clone();
                let full_source = next_source.clone();
                let full = tokio::spawn(async move {
                    full_barrier.wait().await;
                    if round % 2 == 0 {
                        first_committed.notified().await;
                    }
                    change(
                        full_service.inner(),
                        uri,
                        version + 1,
                        json!([{"text": full_source}]),
                    )
                    .await;
                    if round % 2 == 1 {
                        first_committed.notify_one();
                    }
                });
                barrier.wait().await;
                for _ in 0..8 {
                    let snapshot = service.inner().open_document_snapshot(uri).unwrap();
                    let template = snapshot.template_document.unwrap();
                    assert_eq!(snapshot.source, template.virtual_source());
                    if snapshot.document_state == before.document_state {
                        assert_eq!(snapshot.source, before.source);
                        assert_eq!(
                            template.original_source(),
                            before.template_document.as_ref().unwrap().original_source()
                        );
                    } else {
                        assert_eq!(snapshot.document_state.unwrap().version, version + 1);
                        assert_eq!(template.original_source(), next_source);
                    }
                    tokio::task::yield_now().await;
                }
                invalid.await.unwrap();
                full.await.unwrap();
                assert_eq!(
                    service.inner().current_document_state(uri).unwrap().version,
                    version + 1
                );
                assert!(!service
                    .inner()
                    .documents_requiring_full_sync
                    .contains_key(uri));
            }
        }
        service.inner().lsp_shutdown().await.unwrap();
    })
    .await
    .expect("template edit concurrency watchdog");
}
