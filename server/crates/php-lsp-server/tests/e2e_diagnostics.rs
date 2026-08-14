mod support;

use php_lsp_types::uri::path_to_uri;
use support::*;
use tower_lsp::ls_types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    InitializedParams, Position, Range, TextDocumentContentChangeEvent, TextDocumentIdentifier,
    TextDocumentItem, Uri, VersionedTextDocumentIdentifier,
};
use tower_lsp::LanguageServer;

fn open_document_params(uri: &Uri, version: i32, text: &str) -> DidOpenTextDocumentParams {
    DidOpenTextDocumentParams {
        text_document: TextDocumentItem::new(
            uri.clone(),
            "php".to_string(),
            version,
            text.to_string(),
        ),
    }
}

fn incremental_change_params(
    uri: &Uri,
    version: i32,
    range: Range,
    text: &str,
) -> DidChangeTextDocumentParams {
    DidChangeTextDocumentParams {
        text_document: VersionedTextDocumentIdentifier::new(uri.clone(), version),
        content_changes: vec![TextDocumentContentChangeEvent {
            range: Some(range),
            range_length: None,
            text: text.to_string(),
        }],
    }
}

async fn next_publish_diagnostics_for_version(
    notifications: &mut UnboundedReceiver<Request>,
    uri: &str,
    version: i64,
    timeout: Duration,
) -> serde_json::Value {
    let started = std::time::Instant::now();
    loop {
        let remaining = timeout
            .checked_sub(started.elapsed())
            .unwrap_or_else(|| panic!("timed out waiting for diagnostics version {version}"));
        let params = next_publish_diagnostics(notifications, uri, remaining).await;
        if params.get("version").and_then(|value| value.as_i64()) == Some(version) {
            return params;
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn test_open_file_diagnostics_are_syntax_only_while_workspace_indexing_runs() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-indexing-diagnostics-{}-{}",
        std::process::id(),
        nanos
    ));
    let _ = fs::remove_dir_all(&tmp_root);
    let src_dir = tmp_root.join("src");
    fs::create_dir_all(&src_dir).unwrap();
    fs::write(
        tmp_root.join("composer.json"),
        r#"{"autoload":{"psr-4":{"App\\":"src/"}}}"#,
    )
    .unwrap();

    let app_path = src_dir.join("ActiveDiagnostics.php");
    let app_uri = path_to_uri(&app_path).unwrap();
    let app_code = r#"<?php
namespace App;

use Vendor\Pkg\Service;

final class ActiveDiagnostics
{
    public function handle(Service $service): void {}
}
"#;
    fs::write(&app_path, app_code).unwrap();

    for file_index in 0..480 {
        let mut code = format!("<?php\nnamespace App\\Generated{};\n", file_index);
        for class_index in 0..12 {
            code.push_str(&format!(
                "final class Generated{}_{class_index} {{ public function method{class_index}(): void {{}} }}\n",
                file_index
            ));
        }
        fs::write(src_dir.join(format!("Generated{file_index}.php")), code).unwrap();
    }

    let root_uri = path_to_uri(&tmp_root).unwrap();
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(1, Some(&root_uri), None))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();
    wait_for_indexing_phase(&mut notifications, "indexing", Duration::from_secs(3)).await;

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&app_uri, app_code))
        .await
        .unwrap();

    let during_indexing =
        next_publish_diagnostics(&mut notifications, &app_uri, Duration::from_secs(3)).await;
    let during_indexing_messages = published_diagnostic_messages(&during_indexing);
    assert!(
        !during_indexing_messages
            .iter()
            .any(|message| message.contains("Vendor\\Pkg\\Service")),
        "diagnostics published during workspace indexing should not report unresolved symbols, got: {:?}",
        during_indexing_messages
    );

    wait_for_indexing_phase(&mut notifications, "ready", Duration::from_secs(10)).await;
    let after_indexing =
        next_publish_diagnostics(&mut notifications, &app_uri, Duration::from_secs(3)).await;
    let after_indexing_messages = published_diagnostic_messages(&after_indexing);
    assert!(
        after_indexing_messages
            .iter()
            .any(|message| message.contains("Unresolved use statement: Vendor\\Pkg\\Service")),
        "semantic diagnostics should resume after indexing is ready, got: {:?}",
        after_indexing_messages
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
    let _ = fs::remove_dir_all(&tmp_root);
}

#[tokio::test(flavor = "current_thread")]
async fn test_immediate_did_open_diagnostics_are_guarded_during_initialized_setup() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-initialized-early-diagnostics-{}-{}",
        std::process::id(),
        nanos
    ));
    let _ = fs::remove_dir_all(&tmp_root);
    let src_dir = tmp_root.join("src");
    fs::create_dir_all(&src_dir).unwrap();
    fs::write(
        tmp_root.join("composer.json"),
        r#"{"autoload":{"psr-4":{"App\\":"src/"}}}"#,
    )
    .unwrap();

    let app_path = src_dir.join("ImmediateOpen.php");
    let app_uri = path_to_uri(&app_path).unwrap();
    let app_code = r#"<?php
namespace App;

use Vendor\Pkg\Service;

final class ImmediateOpen
{
    public function handle(Service $service): void {}
}
"#;
    fs::write(&app_path, app_code).unwrap();

    let root_uri = path_to_uri(&tmp_root).unwrap();
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(1, Some(&root_uri), None))
        .await
        .unwrap();

    let backend = service.inner();
    let open_params = DidOpenTextDocumentParams {
        text_document: TextDocumentItem {
            uri: app_uri.parse::<Uri>().unwrap(),
            language_id: "php".to_string(),
            version: 1,
            text: app_code.to_string(),
        },
    };
    let (_, _) = tokio::join!(
        backend.initialized(InitializedParams {}),
        backend.did_open(open_params)
    );

    let early_diagnostics =
        next_publish_diagnostics(&mut notifications, &app_uri, Duration::from_secs(3)).await;
    let early_messages = published_diagnostic_messages(&early_diagnostics);
    assert!(
        !early_messages
            .iter()
            .any(|message| message.contains("Vendor\\Pkg\\Service")),
        "diagnostics published during initialized setup should not report unresolved symbols, got: {:?}",
        early_messages
    );

    wait_for_indexing_phase(&mut notifications, "ready", Duration::from_secs(10)).await;

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
    let _ = fs::remove_dir_all(&tmp_root);
}

#[tokio::test(flavor = "current_thread")]
async fn test_post_index_diagnostics_preresolve_vendor_imports_for_open_files() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-post-index-vendor-diagnostics-{}-{}",
        std::process::id(),
        nanos
    ));
    let _ = fs::remove_dir_all(&tmp_root);
    let src_dir = tmp_root.join("src");
    fs::create_dir_all(&src_dir).unwrap();
    fs::write(
        tmp_root.join("composer.json"),
        r#"{"autoload":{"psr-4":{"App\\":"src/"}}}"#,
    )
    .unwrap();

    let service_path = tmp_root.join("vendor/acme/pkg/src/Service.php");
    fs::create_dir_all(service_path.parent().unwrap()).unwrap();
    fs::write(
        &service_path,
        "<?php\nnamespace Vendor\\Pkg;\nfinal class Service extends BaseService {}\n",
    )
    .unwrap();
    fs::write(
        tmp_root.join("vendor/acme/pkg/src/BaseService.php"),
        "<?php\nnamespace Vendor\\Pkg;\nabstract class BaseService { public function inherited(): void {} }\n",
    )
    .unwrap();
    let installed_json = tmp_root.join("vendor/composer/installed.json");
    fs::create_dir_all(installed_json.parent().unwrap()).unwrap();
    fs::write(
        &installed_json,
        r#"{"packages":[{"name":"acme/pkg","install-path":"acme/pkg","autoload":{"psr-4":{"Vendor\\Pkg\\":"src/"}}}]}"#,
    )
    .unwrap();

    let app_path = src_dir.join("UsesVendor.php");
    let app_uri = path_to_uri(&app_path).unwrap();
    let app_code = r#"<?php
namespace App;

use Vendor\Pkg\Service;

final class UsesVendor
{
    public function handle(Service $service): void
    {
        $service->inherited();
    }
}
"#;
    fs::write(&app_path, app_code).unwrap();

    for file_index in 0..240 {
        let mut code = format!("<?php\nnamespace App\\Generated{};\n", file_index);
        for class_index in 0..12 {
            code.push_str(&format!(
                "final class Generated{}_{class_index} {{ public function method{class_index}(): void {{}} }}\n",
                file_index
            ));
        }
        fs::write(src_dir.join(format!("Generated{file_index}.php")), code).unwrap();
    }

    let root_uri = path_to_uri(&tmp_root).unwrap();
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(1, Some(&root_uri), None))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();
    wait_for_indexing_phase(&mut notifications, "indexing", Duration::from_secs(3)).await;

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&app_uri, app_code))
        .await
        .unwrap();
    let during_indexing =
        next_publish_diagnostics(&mut notifications, &app_uri, Duration::from_secs(3)).await;
    let during_indexing_messages = published_diagnostic_messages(&during_indexing);
    assert!(
        !during_indexing_messages
            .iter()
            .any(|message| message.contains("Vendor\\Pkg\\Service")),
        "diagnostics during indexing should be syntax-only, got: {:?}",
        during_indexing_messages
    );

    wait_for_indexing_phase(&mut notifications, "ready", Duration::from_secs(10)).await;
    let after_indexing =
        next_publish_diagnostics(&mut notifications, &app_uri, Duration::from_secs(5)).await;
    let after_indexing_messages = published_diagnostic_messages(&after_indexing);
    assert!(
        !after_indexing_messages
            .iter()
            .any(|message| message.contains("Vendor\\Pkg\\Service")),
        "post-index diagnostics should lazy-resolve vendor imports for open files, got: {:?}",
        after_indexing_messages
    );
    assert!(
        !after_indexing_messages
            .iter()
            .any(|message| message.contains("inherited")),
        "post-index diagnostics should lazy-resolve inherited vendor members for open files, got: {:?}",
        after_indexing_messages
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
    let _ = fs::remove_dir_all(&tmp_root);
}

#[tokio::test(flavor = "current_thread")]
async fn test_composer_vendor_metadata_watch_refreshes_unresolved_use_diagnostics() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-composer-watch-{}-{}",
        std::process::id(),
        nanos
    ));
    let _ = fs::remove_dir_all(&tmp_root);
    fs::create_dir_all(tmp_root.join("src")).unwrap();
    fs::write(
        tmp_root.join("composer.json"),
        r#"{"autoload":{"psr-4":{"App\\":"src/"}}}"#,
    )
    .unwrap();

    let root_uri = format!("file://{}", tmp_root.to_string_lossy());
    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(1, Some(&root_uri), None))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();
    wait_for_indexing_phase(&mut notifications, "ready", Duration::from_secs(2)).await;

    let app_path = tmp_root.join("src/App.php");
    let app_uri = format!("file://{}", app_path.to_string_lossy());
    let app_code = r#"<?php
namespace App;

use Vendor\Pkg\Service;

final class Handler
{
    public function handle(Service $service): void {}
}
"#;
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&app_uri, app_code))
        .await
        .unwrap();

    let unresolved =
        next_publish_diagnostics(&mut notifications, &app_uri, Duration::from_secs(1)).await;
    let unresolved_messages = published_diagnostic_messages(&unresolved);
    assert!(
        unresolved_messages
            .iter()
            .any(|message| message.contains("Unresolved use statement: Vendor\\Pkg\\Service")),
        "expected unresolved vendor use before composer install metadata exists, got: {:?}",
        unresolved_messages
    );

    let composer_dir = tmp_root.join("vendor/composer");
    let package_composer_json = composer_dir.join("75f4db74/acme-pkg/composer.json");
    fs::create_dir_all(package_composer_json.parent().unwrap()).unwrap();
    fs::write(
        &package_composer_json,
        r#"{"name":"acme/pkg","autoload":{"psr-4":{"Vendor\\Pkg\\":"src/"}}}"#,
    )
    .unwrap();
    let package_composer_uri = format!("file://{}", package_composer_json.to_string_lossy());
    service
        .ready()
        .await
        .unwrap()
        .call(did_change_watched_files_notification(vec![(
            &package_composer_uri,
            1,
        )]))
        .await
        .unwrap();
    expect_no_publish_diagnostics(&mut notifications, &app_uri, Duration::from_secs(1)).await;

    let package_src = tmp_root.join("vendor/acme/pkg/src");
    fs::create_dir_all(&composer_dir).unwrap();
    fs::create_dir_all(&package_src).unwrap();
    fs::write(
        package_src.join("Service.php"),
        "<?php\nnamespace Vendor\\Pkg;\nfinal class Service {}\n",
    )
    .unwrap();
    let installed_json = composer_dir.join("installed.json");
    fs::write(
        &installed_json,
        r#"{"packages":[{"name":"acme/pkg","install-path":"acme/pkg","autoload":{"psr-4":{"Vendor\\Pkg\\":"src/"}}}]}"#,
    )
    .unwrap();
    let installed_uri = format!("file://{}", installed_json.to_string_lossy());

    service
        .ready()
        .await
        .unwrap()
        .call(did_change_watched_files_notification(vec![(
            &installed_uri,
            1,
        )]))
        .await
        .unwrap();

    let refreshed =
        next_publish_diagnostics(&mut notifications, &app_uri, Duration::from_secs(2)).await;
    let refreshed_messages = published_diagnostic_messages(&refreshed);
    assert!(
        !refreshed_messages
            .iter()
            .any(|message| message.contains("Vendor\\Pkg\\Service")),
        "composer vendor metadata change should clear unresolved vendor diagnostics, got: {:?}",
        refreshed_messages
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
    let _ = fs::remove_dir_all(&tmp_root);
}

#[tokio::test(flavor = "current_thread")]
async fn test_did_change_debounces_diagnostics_and_ignores_stale_versions() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request(1))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();

    let uri = "file:///test/DidChangeDebounce.php";
    let original_code = "<?php\nfunction ready(): void {}\n";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, original_code))
        .await
        .unwrap();

    let opened = next_publish_diagnostics(&mut notifications, uri, Duration::from_secs(1)).await;
    assert_eq!(
        opened.get("version").and_then(|value| value.as_i64()),
        Some(1)
    );

    let broken_code = "<?php\nfunction broken( {\n";
    let fixed_code = "<?php\nfunction fixed(): void {}\n";
    service
        .ready()
        .await
        .unwrap()
        .call(did_change_full_notification(uri, 2, broken_code))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_change_full_notification(uri, 3, fixed_code))
        .await
        .unwrap();

    let latest = next_publish_diagnostics(&mut notifications, uri, Duration::from_secs(1)).await;
    assert_eq!(
        latest.get("version").and_then(|value| value.as_i64()),
        Some(3)
    );
    assert_eq!(
        latest
            .get("diagnostics")
            .and_then(|value| value.as_array())
            .map(Vec::len),
        Some(0),
        "latest diagnostics should be computed from fixed version 3, got: {}",
        latest
    );

    service
        .ready()
        .await
        .unwrap()
        .call(did_change_full_notification(uri, 2, broken_code))
        .await
        .unwrap();
    expect_no_publish_diagnostics(&mut notifications, uri, Duration::from_millis(300)).await;

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_reversed_did_change_preserves_snapshot_and_full_change_recovers() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request(1))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();

    let old_uri = "file:///test/ReversedDidChange.php";
    let new_uri = "file:///test/RenamedReversedDidChange.php";
    let original_code = "<?php\nclass Before {}\n";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(old_uri, original_code))
        .await
        .unwrap();

    service
        .ready()
        .await
        .unwrap()
        .call(
            Request::build("textDocument/didChange")
                .params(json!({
                    "textDocument": {
                        "uri": old_uri,
                        "version": 2
                    },
                    "contentChanges": [
                        {
                            "range": {
                                "start": { "line": 1, "character": 6 },
                                "end": { "line": 1, "character": 12 }
                            },
                            "text": "Mutated"
                        },
                        {
                            "range": {
                                "start": { "line": 1, "character": 13 },
                                "end": { "line": 1, "character": 6 }
                            },
                            "text": "Broken"
                        }
                    ]
                }))
                .finish(),
        )
        .await
        .unwrap();

    let document_symbols = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(document_symbol_request(2, old_uri))
            .await
            .unwrap(),
    );
    let document_symbol_names = document_symbols
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|symbol| symbol.get("name").and_then(|name| name.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        document_symbol_names,
        vec!["Before"],
        "reversed didChange must preserve the open parser snapshot"
    );

    let indexed_before = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(workspace_symbol_request(3, "Before"))
            .await
            .unwrap(),
    );
    assert!(
        workspace_symbol_names(&indexed_before)
            .iter()
            .any(|name| name == "Before"),
        "reversed didChange must preserve the previous workspace index snapshot"
    );

    service
        .ready()
        .await
        .unwrap()
        .call(did_rename_files_notification(vec![(old_uri, new_uri)]))
        .await
        .unwrap();

    service
        .ready()
        .await
        .unwrap()
        .call(
            Request::build("textDocument/didChange")
                .params(json!({
                    "textDocument": {
                        "uri": new_uri,
                        "version": 3
                    },
                    "contentChanges": [{
                        "range": {
                            "start": { "line": 1, "character": 6 },
                            "end": { "line": 1, "character": 12 }
                        },
                        "text": "Wrong"
                    }]
                }))
                .finish(),
        )
        .await
        .unwrap();

    let symbols_after_incremental = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(document_symbol_request(4, new_uri))
            .await
            .unwrap(),
    );
    let names_after_incremental = symbols_after_incremental
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|symbol| symbol.get("name").and_then(|name| name.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        names_after_incremental,
        vec!["Before"],
        "incremental changes must remain blocked until a full-text synchronization"
    );

    let index_after_incremental = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(workspace_symbol_request(5, "Before"))
            .await
            .unwrap(),
    );
    assert!(
        workspace_symbol_uris(&index_after_incremental)
            .iter()
            .any(|uri| uri == new_uri),
        "blocked incremental changes must preserve the renamed workspace index snapshot"
    );

    let recovered_code = "<?php\nclass After {}\n";
    service
        .ready()
        .await
        .unwrap()
        .call(did_change_full_notification(new_uri, 4, recovered_code))
        .await
        .unwrap();

    let recovered_symbols = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(document_symbol_request(6, new_uri))
            .await
            .unwrap(),
    );
    let recovered_symbol_names = recovered_symbols
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|symbol| symbol.get("name").and_then(|name| name.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        recovered_symbol_names,
        vec!["After"],
        "a later full-text didChange should recover parser synchronization"
    );

    let indexed_after = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(workspace_symbol_request(7, "After"))
            .await
            .unwrap(),
    );
    assert!(
        workspace_symbol_names(&indexed_after)
            .iter()
            .any(|name| name == "After"),
        "a later full-text didChange should replace the workspace index snapshot"
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_concurrent_did_open_and_dependent_incremental_change_publish_latest_text() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request(1))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();

    let uri = "file:///test/ConcurrentOpenIncremental.php";
    let uri_value = uri.parse::<Uri>().unwrap();
    let original_code = "<?php\nfunction ready(): void {}\n";
    let open = open_document_params(&uri_value, 1, original_code);
    let change = incremental_change_params(
        &uri_value,
        2,
        Range::new(Position::new(1, 15), Position::new(1, 16)),
        "",
    );
    let backend = service.inner();

    let (_, _) = tokio::join!(backend.did_open(open), backend.did_change(change));

    let latest =
        next_publish_diagnostics_for_version(&mut notifications, uri, 2, Duration::from_secs(2))
            .await;
    assert!(
        latest
            .get("diagnostics")
            .and_then(|value| value.as_array())
            .is_some_and(|items| !items.is_empty()),
        "version 2 diagnostics should reflect the broken incremental edit applied to didOpen text, got: {}",
        latest
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_concurrent_dependent_incremental_changes_preserve_edit_chain() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request(1))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();

    let uri = "file:///test/ConcurrentIncrementalChain.php";
    let uri_value = uri.parse::<Uri>().unwrap();
    let original_code = "<?php\nfunction ready(): void {}\n";
    let backend = service.inner();
    backend
        .did_open(open_document_params(&uri_value, 1, original_code))
        .await;
    let opened =
        next_publish_diagnostics_for_version(&mut notifications, uri, 1, Duration::from_secs(1))
            .await;
    assert!(
        opened
            .get("diagnostics")
            .and_then(|value| value.as_array())
            .is_some_and(|items| items.is_empty()),
        "initial document should be valid, got: {}",
        opened
    );

    let version_2 = incremental_change_params(
        &uri_value,
        2,
        Range::new(Position::new(1, 9), Position::new(1, 9)),
        "renamed_",
    );
    let version_3 = incremental_change_params(
        &uri_value,
        3,
        Range::new(Position::new(1, 17), Position::new(1, 22)),
        "",
    );

    let (_, _) = tokio::join!(backend.did_change(version_2), backend.did_change(version_3));

    let latest =
        next_publish_diagnostics_for_version(&mut notifications, uri, 3, Duration::from_secs(2))
            .await;
    assert!(
        latest
            .get("diagnostics")
            .and_then(|value| value.as_array())
            .is_some_and(|items| items.is_empty()),
        "version 3 must apply its range to version 2 (`function renamed_(): void {{}}`), got: {}",
        latest
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_close_reopen_never_publishes_stale_previous_generation_diagnostics() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request(1))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();

    let uri = "file:///test/CloseReopenDiagnostics.php";
    let uri_value = uri.parse::<Uri>().unwrap();
    let original_code = "<?php\nfunction ready(): void {}\n";
    let backend = service.inner();
    backend
        .did_open(open_document_params(&uri_value, 1, original_code))
        .await;
    let _ =
        next_publish_diagnostics_for_version(&mut notifications, uri, 1, Duration::from_secs(1))
            .await;

    backend
        .did_change(incremental_change_params(
            &uri_value,
            2,
            Range::new(Position::new(1, 15), Position::new(1, 16)),
            "",
        ))
        .await;

    let close = DidCloseTextDocumentParams {
        text_document: TextDocumentIdentifier::new(uri_value.clone()),
    };
    let reopen = open_document_params(&uri_value, 1, "<?php\nfunction reopened(): void {}\n");
    let (_, _) = tokio::join!(backend.did_close(close), backend.did_open(reopen));

    let reopened =
        next_publish_diagnostics_for_version(&mut notifications, uri, 1, Duration::from_secs(2))
            .await;
    assert!(
        reopened
            .get("diagnostics")
            .and_then(|value| value.as_array())
            .is_some_and(|items| items.is_empty()),
        "reopened generation should publish diagnostics for its valid text, got: {}",
        reopened
    );
    expect_no_publish_diagnostics(&mut notifications, uri, Duration::from_millis(400)).await;

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_stress_100_did_change_non_ascii_publishes_latest_version() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request(1))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();

    let uri = "file:///test/StressNonAscii.php";
    let initial_code =
        "<?php\nnamespace App;\nclass Stress { public function run(): void { echo \"привет\"; } }\n";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, initial_code))
        .await
        .unwrap();
    let _ = next_publish_diagnostics(&mut notifications, uri, Duration::from_secs(1)).await;

    let burst = async {
        for i in 0..100 {
            let version = i + 2;
            let code = if i == 99 {
                format!(
                    "<?php\nnamespace App;\nclass Stress {{ public function run(): void {{ echo \"финал {}\"; }} }}\n",
                    i
                )
            } else if i % 2 == 0 {
                format!(
                    "<?php\nnamespace App;\nclass Stress {{ public function run(): void {{ echo \"черновик {}\"; }}\n",
                    i
                )
            } else {
                format!(
                    "<?php\nnamespace App;\nclass Stress {{ public function run(): void {{ echo \"правка {}\"; }} }}\n",
                    i
                )
            };
            service
                .ready()
                .await
                .unwrap()
                .call(did_change_full_notification(uri, version, &code))
                .await
                .unwrap();
        }
    };
    tokio::time::timeout(Duration::from_secs(1), burst)
        .await
        .expect("100 didChange notifications should be accepted within one second");

    let latest = next_publish_diagnostics(&mut notifications, uri, Duration::from_secs(2)).await;
    assert_eq!(
        latest.get("version").and_then(|value| value.as_i64()),
        Some(101),
        "diagnostics should be published for the latest burst version, got: {}",
        latest
    );
    assert!(
        latest
            .get("diagnostics")
            .and_then(|value| value.as_array())
            .is_some_and(|items| items.is_empty()),
        "final valid version should have no diagnostics, got: {}",
        latest
    );
    expect_no_publish_diagnostics(&mut notifications, uri, Duration::from_millis(300)).await;

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_php_version_filters_version_gated_stubs() {
    let stubs_path_raw = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/stubs");
    if !stubs_path_raw.join("PhpStormStubsMap.php").exists() {
        eprintln!("Skipping test: server/data/stubs not found");
        return;
    }
    let stubs_path = stubs_path_raw.canonicalize().unwrap();

    let code = r#"<?php
sodium_crypto_stream_xchacha20_xor_ic('a', 'b', 0, 'c');
"#;
    let uri = "file:///test/PhpVersionStubs.php";

    let (mut service81, socket81) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket81.collect::<Vec<_>>().await;
    });
    let tmp_root81 =
        std::env::temp_dir().join(format!("php-lsp-version-stubs-81-{}", std::process::id()));
    fs::create_dir_all(&tmp_root81).unwrap();
    let root_uri81 = format!("file://{}", tmp_root81.to_string_lossy());
    service81
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(
            1,
            Some(&root_uri81),
            Some(json!({
                "stubsPath": stubs_path.to_string_lossy().to_string(),
                "phpVersion": "8.1",
                "stubExtensions": ["sodium"]
            })),
        ))
        .await
        .unwrap();
    service81
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();
    service81
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();
    let php81_definition = service81
        .ready()
        .await
        .unwrap()
        .call(definition_request(2, uri, 1, 5))
        .await
        .unwrap();
    assert!(
        extract_result(php81_definition).is_null(),
        "PHP 8.1 should not resolve an 8.2-only sodium function"
    );
    service81
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
    let _ = fs::remove_dir_all(&tmp_root81);

    let (mut service82, socket82) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket82.collect::<Vec<_>>().await;
    });
    let tmp_root82 =
        std::env::temp_dir().join(format!("php-lsp-version-stubs-82-{}", std::process::id()));
    fs::create_dir_all(&tmp_root82).unwrap();
    let root_uri82 = format!("file://{}", tmp_root82.to_string_lossy());
    service82
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(
            3,
            Some(&root_uri82),
            Some(json!({
                "stubsPath": stubs_path.to_string_lossy().to_string(),
                "phpVersion": "8.2",
                "stubExtensions": ["sodium"]
            })),
        ))
        .await
        .unwrap();
    service82
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();
    service82
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();
    let php82_definition = service82
        .ready()
        .await
        .unwrap()
        .call(definition_request(4, uri, 1, 5))
        .await
        .unwrap();
    let php82_result = extract_result(php82_definition);
    assert!(
        php82_result
            .get("uri")
            .and_then(|value| value.as_str())
            .is_some_and(|uri| uri.starts_with("phpstub://sodium/")),
        "PHP 8.2 should resolve the sodium function from stubs, got: {}",
        php82_result
    );
    service82
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(100))
        .await
        .unwrap();
    let _ = fs::remove_dir_all(&tmp_root82);
}
