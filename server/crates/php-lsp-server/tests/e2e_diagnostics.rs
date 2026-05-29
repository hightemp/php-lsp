mod support;

use support::*;

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
