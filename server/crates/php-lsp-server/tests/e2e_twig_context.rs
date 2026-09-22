mod support;
use php_lsp_types::uri::path_to_uri;
use support::*;

async fn send(service: &mut LspService<PhpLspBackend>, request: Request) -> serde_json::Value {
    match service.ready().await.unwrap().call(request).await.unwrap() {
        Some(response) => {
            assert!(response.error().is_none(), "{response:?}");
            extract_result(Some(response))
        }
        None => serde_json::Value::Null,
    }
}

async fn assert_context(
    service: &mut LspService<PhpLspBackend>,
    uri: &str,
    kind: Option<(&str, &str, &str)>,
) {
    let completion = send(service, completion_request(20, uri, 0, 10)).await;
    let labels = completion_items_from_result(&completion)
        .into_iter()
        .filter_map(|item| item["label"].as_str().map(str::to_string))
        .collect::<Vec<_>>();
    let hover = send(service, hover_request(21, uri, 0, 5)).await;
    let definition = send(service, definition_request(22, uri, 0, 12)).await;
    let location = definition
        .as_array()
        .and_then(|items| items.first())
        .unwrap_or(&definition);
    let target = location
        .get("targetUri")
        .or_else(|| location.get("uri"))
        .and_then(|value| value.as_str());
    if let Some((name, member, target_uri)) = kind {
        assert!(
            labels.iter().any(|label| label == member),
            "missing {member}: {completion}"
        );
        assert!(
            hover_markdown_value(&hover).contains(name),
            "missing {name}: {hover}"
        );
        assert_eq!(target, Some(target_uri), "wrong definition: {definition}");
    } else {
        assert!(
            labels
                .iter()
                .all(|label| label != "userOnly" && label != "otherOnly"),
            "stale completion: {completion}"
        );
        assert!(target.is_none(), "stale definition: {definition}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn twig_context_controller_caller_partial_updates_remove_restore_and_rename() {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "php-lsp-twig-context-flow-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("templates")).unwrap();
    fs::write(
        root.join("composer.json"),
        r#"{"autoload":{"psr-4":{"":"src/"}}}"#,
    )
    .unwrap();
    fs::write(
        root.join("src/User.php"),
        "<?php class User { public string $name; public string $userOnly; }",
    )
    .unwrap();
    fs::write(
        root.join("src/Other.php"),
        "<?php class Other { public int $name; public int $otherOnly; }",
    )
    .unwrap();
    let controller =
        "<?php function page() { $this->render('caller.twig', ['item' => new User()]); }";
    let changed = controller.replace("new User()", "new Other()");
    let caller = "{% include 'partial.twig' with { person: item } %}";
    let partial = "{{ person.name }}";
    fs::write(root.join("src/Controller.php"), controller).unwrap();
    fs::write(root.join("templates/caller.twig"), caller).unwrap();
    fs::write(root.join("templates/partial.twig"), partial).unwrap();
    let root_uri = path_to_uri(&root).unwrap();
    let uri = path_to_uri(&root.join("templates/partial.twig")).unwrap();
    let caller_uri = path_to_uri(&root.join("templates/caller.twig")).unwrap();
    let controller_uri = path_to_uri(&root.join("src/Controller.php")).unwrap();
    let user_uri = path_to_uri(&root.join("src/User.php")).unwrap();
    let other_uri = path_to_uri(&root.join("src/Other.php")).unwrap();
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
            Some(json!({"stubExtensions":[], "indexVendor":false})),
        ),
    )
    .await;
    send(&mut service, initialized_notification()).await;
    wait_for_indexing_phase(&mut notifications, "ready", Duration::from_secs(15)).await;
    send(
        &mut service,
        did_open_notification_with_language(&caller_uri, "twig", caller),
    )
    .await;
    send(
        &mut service,
        did_open_notification_with_language(&uri, "twig", partial),
    )
    .await;
    assert_context(&mut service, &uri, Some(("User", "userOnly", &user_uri))).await;

    // Unsaved controller replaces disk data and updates the already open partial.
    send(
        &mut service,
        did_open_notification(&controller_uri, &changed),
    )
    .await;
    assert_context(&mut service, &uri, Some(("Other", "otherOnly", &other_uri))).await;
    send(
        &mut service,
        did_change_full_notification(&caller_uri, 2, "removed include"),
    )
    .await;
    assert_context(&mut service, &uri, None).await;
    send(&mut service, did_close_notification(&caller_uri)).await;
    assert_context(&mut service, &uri, Some(("Other", "otherOnly", &other_uri))).await;
    // Opening an unsaved caller without its old include must also refresh dependents.
    send(
        &mut service,
        did_open_notification_with_language(&caller_uri, "twig", "removed again"),
    )
    .await;
    assert_context(&mut service, &uri, None).await;
    send(&mut service, did_close_notification(&caller_uri)).await;
    send(&mut service, did_close_notification(&controller_uri)).await;
    assert_context(&mut service, &uri, Some(("User", "userOnly", &user_uri))).await;

    fs::remove_file(root.join("src/Controller.php")).unwrap();
    send(
        &mut service,
        did_change_watched_files_notification(vec![(&controller_uri, 3)]),
    )
    .await;
    assert_context(&mut service, &uri, None).await;
    let created_uri = path_to_uri(&root.join("src/Created.php")).unwrap();
    fs::write(root.join("src/Created.php"), &changed).unwrap();
    send(
        &mut service,
        did_create_files_notification(vec![&created_uri]),
    )
    .await;
    assert_context(&mut service, &uri, Some(("Other", "otherOnly", &other_uri))).await;
    fs::rename(
        root.join("templates/caller.twig"),
        root.join("templates/renamed.twig"),
    )
    .unwrap();
    let renamed_uri = path_to_uri(&root.join("templates/renamed.twig")).unwrap();
    fs::write(
        root.join("src/Created.php"),
        changed.replace("caller.twig", "renamed.twig"),
    )
    .unwrap();
    send(
        &mut service,
        did_rename_files_notification(vec![(&caller_uri, &renamed_uri)]),
    )
    .await;
    send(
        &mut service,
        did_change_watched_files_notification(vec![(&created_uri, 2)]),
    )
    .await;
    assert_context(&mut service, &uri, Some(("Other", "otherOnly", &other_uri))).await;
    send(&mut service, shutdown_request(99)).await;
    let cache = php_lsp_index::cache::cache_file_path(&root);
    if let Some(dir) = cache.parent().and_then(std::path::Path::parent) {
        let _ = fs::remove_dir_all(dir);
    }
    fs::remove_dir_all(root).unwrap();
}
