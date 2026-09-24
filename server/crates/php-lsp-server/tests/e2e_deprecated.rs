mod support;

use php_lsp_types::uri::path_to_uri;
use support::*;

fn has_deprecated_tag(item: &serde_json::Value) -> bool {
    item["tags"]
        .as_array()
        .is_some_and(|tags| tags.contains(&json!(1)))
}

async fn completion_item(
    service: &mut LspService<PhpLspBackend>,
    uri: &str,
    line: u32,
    character: u32,
    label: &str,
) -> serde_json::Value {
    let result = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(completion_request(20, uri, line, character))
            .await
            .unwrap(),
    );
    completion_items_from_result(&result)
        .into_iter()
        .find(|item| item["label"] == label)
        .unwrap_or_else(|| panic!("missing {label} completion: {result}"))
}

#[tokio::test(flavor = "current_thread")]
async fn deprecated_source_tags_reach_completion_and_symbol_responses() {
    let uri = path_to_uri(&std::env::temp_dir().join("php-lsp-deprecated/Symbols.php")).unwrap();
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });
    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(
            1,
            None,
            Some(json!({ "phpVersion": "8.4", "stubExtensions": [] })),
        ))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();

    let deprecated = "<?php\n/** @deprecated */\nclass OldThing {}\n#[\\Deprecated] function oldTask(): void {}\nfunction currentTask(): void {}\nOldTh\n";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&uri, deprecated))
        .await
        .unwrap();
    let item = completion_item(&mut service, &uri, 5, 5, "OldThing").await;
    assert!(has_deprecated_tag(&item), "completion: {item}");

    let document = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(document_symbol_request(21, &uri))
            .await
            .unwrap(),
    );
    for name in ["OldThing", "oldTask"] {
        let symbol = document
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["name"] == name)
            .unwrap();
        assert!(
            has_deprecated_tag(symbol),
            "document symbol {name}: {document}"
        );
    }
    let current = document
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["name"] == "currentTask")
        .unwrap();
    assert!(!has_deprecated_tag(current), "document symbol: {document}");

    let workspace = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(workspace_symbol_request(22, "OldThing"))
            .await
            .unwrap(),
    );
    let symbol = workspace
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["name"] == "OldThing")
        .unwrap();
    assert!(has_deprecated_tag(symbol), "workspace symbol: {workspace}");

    let current_source = "<?php\nclass OldThing {}\nfunction oldTask(): void {}\nfunction currentTask(): void {}\nOldTh\n";
    service
        .ready()
        .await
        .unwrap()
        .call(did_change_full_notification(&uri, 2, current_source))
        .await
        .unwrap();
    let item = completion_item(&mut service, &uri, 4, 5, "OldThing").await;
    assert!(!has_deprecated_tag(&item), "stale completion tag: {item}");
    let document = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(document_symbol_request(23, &uri))
            .await
            .unwrap(),
    );
    assert!(
        document
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["name"] == "OldThing" || item["name"] == "oldTask")
            .all(|item| !has_deprecated_tag(item)),
        "stale document tags: {document}"
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
async fn deprecated_attribute_tag_follows_runtime_php_version_changes() {
    let uri = path_to_uri(&std::env::temp_dir().join("php-lsp-deprecated/Version.php")).unwrap();
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });
    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(
            1,
            None,
            Some(json!({ "phpVersion": "8.3", "stubExtensions": [] })),
        ))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(
            &uri,
            "<?php\n#[\\Deprecated] function attrOnly(): void {}\n",
        ))
        .await
        .unwrap();

    let symbols = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(document_symbol_request(31, &uri))
            .await
            .unwrap(),
    );
    let symbol = symbols
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["name"] == "attrOnly")
        .unwrap();
    assert!(!has_deprecated_tag(symbol), "PHP 8.3 tag: {symbols}");

    service
        .ready()
        .await
        .unwrap()
        .call(did_change_configuration_notification(json!({
            "phpLsp": { "phpVersion": "8.4", "stubs": { "extensions": [] } }
        })))
        .await
        .unwrap();
    let started = std::time::Instant::now();
    loop {
        let symbols = extract_result(
            service
                .ready()
                .await
                .unwrap()
                .call(document_symbol_request(32, &uri))
                .await
                .unwrap(),
        );
        let symbol = symbols
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["name"] == "attrOnly")
            .unwrap();
        if has_deprecated_tag(symbol) {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "PHP 8.4 tag never appeared: {symbols}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn closed_workspace_symbol_deprecation_uses_root_php_version() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "php-lsp-deprecated-root-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("Closed.php"),
        "<?php\n#[\\Deprecated] function closedDeprecated(): void {}\n",
    )
    .unwrap();
    let root_uri = path_to_uri(&root).unwrap();

    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(message) = socket.next().await {
            let _ = tx.send(message);
        }
    });
    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(
            1,
            Some(&root_uri),
            Some(json!({ "phpVersion": "8.3", "stubExtensions": [] })),
        ))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();
    wait_for_indexing_phase(&mut notifications, "ready", Duration::from_secs(10)).await;

    let result = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(workspace_symbol_request(41, "closedDeprecated"))
            .await
            .unwrap(),
    );
    let symbol = result
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["name"] == "closedDeprecated")
        .unwrap();
    assert!(
        !has_deprecated_tag(symbol),
        "PHP 8.3 workspace symbol: {result}"
    );

    service
        .ready()
        .await
        .unwrap()
        .call(did_change_configuration_notification(json!({
            "phpLsp": { "phpVersion": "8.4", "stubs": { "extensions": [] } }
        })))
        .await
        .unwrap();
    wait_for_indexing_phase(&mut notifications, "ready", Duration::from_secs(10)).await;
    let result = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(workspace_symbol_request(42, "closedDeprecated"))
            .await
            .unwrap(),
    );
    let symbol = result
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["name"] == "closedDeprecated")
        .unwrap();
    assert!(
        has_deprecated_tag(symbol),
        "PHP 8.4 workspace symbol: {result}"
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
    fs::remove_dir_all(root).unwrap();
}
