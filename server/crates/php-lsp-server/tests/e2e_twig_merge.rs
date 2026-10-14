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
struct Workspace(std::path::PathBuf);
impl Workspace {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("php-lsp-p235-rpc-{}-{nonce}", std::process::id()));
        fs::create_dir_all(path.join("src")).unwrap();
        fs::create_dir_all(path.join("templates")).unwrap();
        Self(path)
    }
    fn uri(&self, path: &str) -> String {
        path_to_uri(&self.0.join(path)).unwrap()
    }
    fn write(&self, path: &str, text: &str) -> String {
        let target = self.0.join(path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, text).unwrap();
        self.uri(path)
    }
    async fn service(&self) -> LspService<PhpLspBackend> {
        let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
        tokio::spawn(async move { while socket.next().await.is_some() {} });
        let root = path_to_uri(&self.0).unwrap();
        send(
            &mut service,
            initialize_request_with_options(
                1,
                Some(&root),
                Some(json!({"stubExtensions":[],"indexVendor":false,"diagnosticsMode":"off"})),
            ),
        )
        .await;
        service
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

async fn hover(
    service: &mut LspService<PhpLspBackend>,
    uri: &str,
    source: &str,
    needle: &str,
) -> String {
    let (line, column) = utf16_position_after(source, needle);
    hover_markdown_value(&send(service, hover_request(10, uri, line, column)).await).to_string()
}
async fn labels(
    service: &mut LspService<PhpLspBackend>,
    uri: &str,
    source: &str,
    needle: &str,
) -> Vec<String> {
    let (line, column) = utf16_position_after(source, needle);
    let value = send(service, completion_request(11, uri, line, column)).await;
    completion_items_from_result(&value)
        .into_iter()
        .filter_map(|item| item["label"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn twig_caller_union_reaches_partial_and_tracks_unsaved_removal_and_close() {
    let w = Workspace::new();
    let models="<?php\r\nclass First { /* 😀 */ public string $firstOnly; public string $shared; }\r\nclass Second { /* 😀 */ public int $secondOnly; public int $shared; }";
    let model_uri = w.write("src/Models.php", models);
    let controller="<?php function a() { $this->render('caller.twig', ['item'=>new First()]); } function b() { $this->render('caller.twig', ['item'=>new Second()]); }";
    let controller_uri = w.write("src/Controller.php", controller);
    w.write("templates/caller.twig", "{% include 'partial.twig' %}");
    let source = "😀 {{ item.shared }}\r\n{{ item.secondOnly }}";
    let uri = w.write("templates/partial.twig", source);
    let mut service = w.service().await;
    send(&mut service, did_open_notification(&model_uri, models)).await;
    send(
        &mut service,
        did_open_notification(&controller_uri, controller),
    )
    .await;
    send(
        &mut service,
        did_open_notification_with_language(&uri, "twig", source),
    )
    .await;
    let ty = hover(&mut service, &uri, source, "{{ it").await;
    assert!(ty.contains("First") && ty.contains("Second"), "{ty}");
    let members = labels(&mut service, &uri, source, "{{ item.").await;
    assert!(
        members.iter().any(|s| s == "shared")
            && !members
                .iter()
                .any(|s| s == "firstOnly" || s == "secondOnly"),
        "{members:?}"
    );
    let (line, column) = utf16_position_after(source, "item.sha");
    let definitions = send(&mut service, definition_request(15, &uri, line, column)).await;
    let definitions = definitions.as_array().expect("union member definitions");
    assert_eq!(definitions.len(), 2, "{definitions:?}");
    for offset in [
        models.find("$shared").unwrap(),
        models.rfind("$shared").unwrap(),
    ] {
        let (line, column) = utf16_position_for_offset(models, offset);
        assert!(
            definitions
                .iter()
                .any(|location| location["uri"] == model_uri
                    && location["range"]["start"] == json!({"line":line,"character":column})),
            "missing declaration at {offset}: {definitions:?}"
        );
    }
    let changed = "<?php function a() { $this->render('caller.twig', ['item'=>new Second()]); }";
    send(
        &mut service,
        did_change_full_notification(&controller_uri, 2, changed),
    )
    .await;
    let ty = hover(&mut service, &uri, source, "{{ it").await;
    assert!(ty.contains("Second") && !ty.contains("First"), "{ty}");
    let members = labels(&mut service, &uri, source, "{{ item.").await;
    assert!(
        !members.iter().any(|s| s == "firstOnly") && members.iter().any(|s| s == "secondOnly"),
        "{members:?}"
    );
    let (line, column) = utf16_position_after(source, "item.secondO");
    let target = send(&mut service, definition_request(12, &uri, line, column)).await;
    let target = target
        .as_array()
        .and_then(|values| values.first())
        .unwrap_or(&target);
    assert_eq!(target["uri"], model_uri);
    let (line, column) = utf16_position_at(models, "$secondOnly");
    assert_eq!(
        target["range"]["start"],
        json!({"line":line,"character":column})
    );
    send(&mut service, did_close_notification(&controller_uri)).await;
    let ty = hover(&mut service, &uri, source, "{{ it").await;
    assert!(ty.contains("First") && ty.contains("Second"), "{ty}");
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn standalone_and_laravel_twig_have_no_symfony_fallbacks() {
    for metadata in [
        "{}",
        r#"{"require":{"laravel/framework":"*","twig/twig":"*","symfony/http-foundation":"*"}}"#,
    ] {
        let w = Workspace::new();
        w.write("composer.json", metadata);
        let source="😀 {{ app.current_route }}\r\n{{ error.message }}\r\n{% for error in errors %}{{ error.message }}{% endfor %}";
        let uri = w.write("templates/page.twig", source);
        let mut service = w.service().await;
        send(
            &mut service,
            did_open_notification_with_language(&uri, "twig", source),
        )
        .await;
        let members = labels(&mut service, &uri, source, "{{ app.").await;
        assert!(
            !members
                .iter()
                .any(|s| ["current_route", "user", "request"].contains(&s.as_str())),
            "{members:?}"
        );
        for needle in ["{{ ap", "{{ er"] {
            let ty = hover(&mut service, &uri, source, needle).await;
            assert!(
                !ty.contains("Symfony") && !ty.contains("current_route"),
                "{ty}"
            );
        }
        send(&mut service, shutdown_request(99)).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn watched_symfony_metadata_toggles_globals_on_an_already_open_template() {
    let w = Workspace::new();
    let metadata_uri = w.write("vendor/composer/installed.json", "{\"packages\":[]}");
    let source = "😀 {{ app.current_route }}";
    let uri = w.write("templates/page.twig", source);
    let mut service = w.service().await;
    send(
        &mut service,
        did_open_notification_with_language(&uri, "twig", source),
    )
    .await;
    assert!(!hover(&mut service, &uri, source, "{{ ap")
        .await
        .contains("current_route"));
    for (contents, present) in [
        (
            r#"{"packages":[{"name":"symfony/twig-bundle","version":"v7.0"}]}"#,
            true,
        ),
        ("broken", false),
        (
            r#"{"packages":[{"name":"symfony/twig-bundle","version":"v7.0"}]}"#,
            true,
        ),
    ] {
        w.write("vendor/composer/installed.json", contents);
        send(
            &mut service,
            did_change_watched_files_notification(vec![(&metadata_uri, 2)]),
        )
        .await;
        let ty = hover(&mut service, &uri, source, "{{ ap").await;
        assert_eq!(ty.contains("current_route"), present, "{ty}");
    }
    fs::remove_file(w.0.join("vendor/composer/installed.json")).unwrap();
    send(
        &mut service,
        did_change_watched_files_notification(vec![(&metadata_uri, 3)]),
    )
    .await;
    assert!(!hover(&mut service, &uri, source, "{{ ap")
        .await
        .contains("current_route"));
    send(&mut service, shutdown_request(99)).await;
}
