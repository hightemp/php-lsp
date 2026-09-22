#![cfg(unix)]

mod support;
use php_lsp_types::uri::path_to_uri;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use support::*;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "php-lsp-vendor-metadata-e2e-{}-{nonce}",
            std::process::id()
        )))
    }
    fn write(&self, path: &str, source: &str) {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let cache_path = php_lsp_index::cache::cache_file_path(&self.0.join("workspace"));
        if let Some(dir) = cache_path.parent().and_then(Path::parent) {
            let _ = fs::remove_dir_all(dir);
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

async fn send(service: &mut LspService<PhpLspBackend>, request: Request) -> serde_json::Value {
    match service.ready().await.unwrap().call(request).await.unwrap() {
        Some(response) => {
            assert!(response.error().is_none(), "{response:?}");
            extract_result(Some(response))
        }
        None => serde_json::Value::Null,
    }
}

fn definition_uri(result: &serde_json::Value) -> Option<&str> {
    let location = result
        .as_array()
        .and_then(|items| items.first())
        .unwrap_or(result);
    location
        .get("targetUri")
        .or_else(|| location.get("uri"))
        .and_then(|uri| uri.as_str())
}

#[tokio::test(flavor = "current_thread")]
async fn vendor_metadata_symlink_definition_physical_changes_create_and_warm_cache() {
    let fixture = Fixture::new();
    fixture.write(
        "workspace/composer.json",
        r#"{"autoload":{"psr-4":{"App\\":"src/"}}}"#,
    );
    fixture.write(
        "library/src/Subject.php",
        "<?php namespace Acme; class Subject {} class BeforeWatch {}",
    );
    fixture.write(
        "library/legacy/Legacy/Subject.php",
        "<?php class Legacy_Subject {}",
    );
    fixture.write(
        "library/src/Excluded.php",
        "<?php namespace Acme; class Excluded {}",
    );
    fixture.write(
        "library/dev-bootstrap.php",
        "<?php class DependencyDevOnly {}",
    );
    fixture.write(
        "hostile/Leaked.php",
        "<?php namespace Hostile; class Leaked {}",
    );
    fixture.write("hostile/bootstrap.php", "<?php class HostileEntrypoint {}");
    let escaped_include = fixture.0.join("hostile/bootstrap.php");
    fixture.write(
        "library/bootstrap.php",
        &format!(
            "<?php function linked_bootstrap() {{}} require {};",
            json!(escaped_include.to_string_lossy())
        ),
    );
    fixture.write(
        "workspace/vendor/composer/installed.json",
        &json!({"packages":[
            {"name":"acme/lib", "install-path":"../acme/lib", "autoload":{
                "psr-4":{"Acme\\":"src/"}, "psr-0":{"Legacy_":"legacy/"}, "files":["bootstrap.php"]
            }, "autoload-dev":{"files":["dev-bootstrap.php"]}},
            {"name":"hostile/pkg", "install-path": fixture.0.join("hostile"), "autoload":{
                "psr-4":{"Hostile\\":""}, "files":["bootstrap.php"], "classmap":["."]
            }}
        ]})
        .to_string(),
    );
    fs::create_dir_all(fixture.0.join("workspace/vendor/acme")).unwrap();
    symlink(
        fixture.0.join("library"),
        fixture.0.join("workspace/vendor/acme/lib"),
    )
    .unwrap();
    let code = "<?php\nnew \\Acme\\Subject();\nnew \\Legacy_Subject();\nnew \\Acme\\Created();\nnew \\Hostile\\Leaked();\nnew \\Acme\\Excluded();\nlinked_bootstrap();\n";
    fixture.write("workspace/src/Probe.php", code);
    let root = fixture.0.join("workspace");
    let root_uri = path_to_uri(&root).unwrap();
    let uri = path_to_uri(&root.join("src/Probe.php")).unwrap();
    let subject_uri = path_to_uri(&root.join("vendor/acme/lib/src/Subject.php")).unwrap();
    let legacy_uri = path_to_uri(&root.join("vendor/acme/lib/legacy/Legacy/Subject.php")).unwrap();

    // The second full lifecycle exercises the disk cache with the same metadata.
    for warm in [false, true] {
        if warm {
            let cache_path = php_lsp_index::cache::cache_file_path_for_namespace(
                &root,
                php_lsp_index::cache::CacheNamespace::Vendor,
            );
            let cache = php_lsp_index::cache::load_cache(&cache_path)
                .expect("persisted vendor cache before restart");
            assert!(cache.files.iter().any(|file| file.uri == subject_uri));
            assert!(cache.files.iter().any(|file| file.uri == legacy_uri));
            assert!(cache.files.iter().all(|file| file
                .uri
                .starts_with(&path_to_uri(&root.join("vendor")).unwrap())));
        }
        let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
        let (tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(message) = socket.next().await {
                let _ = tx.send(message);
            }
        });
        send(
            &mut service,
            initialize_request_with_options(
                1,
                Some(&root_uri),
                Some(json!({
                    "stubExtensions": [], "indexVendor": true,
                    "excludePaths": ["vendor/acme/lib/src/Excluded.php"]
                })),
            ),
        )
        .await;
        send(&mut service, initialized_notification()).await;
        wait_for_indexing_phase(&mut notifications, "ready", Duration::from_secs(15)).await;
        send(&mut service, did_open_notification(&uri, code)).await;
        for (line, expected) in [(1, &subject_uri), (2, &legacy_uri)] {
            let definition = send(
                &mut service,
                definition_request(10 + line as i64, &uri, line, 10),
            )
            .await;
            assert_eq!(
                definition_uri(&definition),
                Some(expected.as_str()),
                "logical library definition (warm={warm}): {definition}"
            );
        }
        for line in [4, 5] {
            let definition = send(
                &mut service,
                definition_request(20 + line as i64, &uri, line, 12),
            )
            .await;
            assert!(
                definition_uri(&definition).is_none(),
                "forbidden definition: {definition}"
            );
        }
        for name in [
            "Leaked",
            "HostileEntrypoint",
            "DependencyDevOnly",
            "Excluded",
        ] {
            let symbols = send(&mut service, workspace_symbol_request(30, name)).await;
            assert!(
                workspace_symbol_names(&symbols).is_empty(),
                "forbidden symbol {name}: {symbols}"
            );
        }
        let bootstrap = send(&mut service, definition_request(31, &uri, 6, 5)).await;
        let bootstrap_uri = path_to_uri(&root.join("vendor/acme/lib/bootstrap.php")).unwrap();
        assert_eq!(definition_uri(&bootstrap), Some(bootstrap_uri.as_str()));
        if !warm {
            // A miss must retain the package alias for subsequent create events.
            let missing = send(&mut service, definition_request(40, &uri, 3, 12)).await;
            assert!(definition_uri(&missing).is_none());
            fixture.write(
                "library/src/Subject.php",
                "<?php namespace Acme; class Subject {} class AfterWatch {}",
            );
            let physical = path_to_uri(&fixture.0.join("library/src/Subject.php")).unwrap();
            let mut updated = false;
            for attempt in 0..80 {
                send(
                    &mut service,
                    did_change_watched_files_notification(vec![(&physical, 2)]),
                )
                .await;
                let symbols = send(
                    &mut service,
                    workspace_symbol_request(50 + attempt, "AfterWatch"),
                )
                .await;
                if workspace_symbol_uris(&symbols).contains(&subject_uri) {
                    updated = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(
                updated,
                "physical event did not update the logical vendor URI"
            );
            let stale = send(&mut service, workspace_symbol_request(140, "BeforeWatch")).await;
            assert!(workspace_symbol_names(&stale).is_empty());
            fixture.write(
                "library/src/Created.php",
                "<?php namespace Acme; class Created {}",
            );
            let physical = path_to_uri(&fixture.0.join("library/src/Created.php")).unwrap();
            send(
                &mut service,
                did_change_watched_files_notification(vec![(&physical, 1)]),
            )
            .await;
            let created_uri = path_to_uri(&root.join("vendor/acme/lib/src/Created.php")).unwrap();
            // Unopened vendor files intentionally stay lazy; the next request
            // must discover a newly created file through the same logical alias.
            let definition = send(&mut service, definition_request(142, &uri, 3, 12)).await;
            assert_eq!(definition_uri(&definition), Some(created_uri.as_str()));
            let symbols = send(&mut service, workspace_symbol_request(143, "Created")).await;
            assert!(workspace_symbol_uris(&symbols).contains(&created_uri));
        }
        send(&mut service, shutdown_request(999)).await;
    }
}
