mod support;
use php_lsp_types::uri::path_to_uri;
use support::*;

async fn send(service: &mut LspService<PhpLspBackend>, request: Request) -> serde_json::Value {
    let response = service.ready().await.unwrap().call(request).await.unwrap();
    if let Some(response) = response {
        assert!(response.error().is_none(), "{response:?}");
        extract_result(Some(response))
    } else {
        serde_json::Value::Null
    }
}

async fn service(root: Option<&str>) -> LspService<PhpLspBackend> {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move { while socket.next().await.is_some() {} });
    send(
        &mut service,
        initialize_request_with_options(
            1,
            root,
            Some(json!({
                "stubExtensions": [], "indexVendor": false, "diagnosticsMode": "off"
            })),
        ),
    )
    .await;
    service
}

fn location(value: &serde_json::Value) -> &serde_json::Value {
    value
        .as_array()
        .and_then(|items| items.first())
        .unwrap_or(value)
}

async fn definition(
    service: &mut LspService<PhpLspBackend>,
    uri: &str,
    source: &str,
    needle: &str,
) -> serde_json::Value {
    let (line, column) = utf16_position_after(source, needle);
    send(service, definition_request(10, uri, line, column)).await
}

#[tokio::test(flavor = "current_thread")]
async fn escaped_blade_echoes_have_no_php_features_and_edits_restore_them() {
    let mut service = service(None).await;
    let php_uri = "file:///test/p233-echo.php";
    let php =
        "<?php class Probe {public static function run(float $value): float {return $value;}}";
    let uri = "file:///test/p233-echo.blade.php";
    let source = "😀 @{{ Probe::run(1.5) }}\r\n{{ Probe::run(2.5) }}";
    send(&mut service, did_open_notification(php_uri, php)).await;
    send(
        &mut service,
        did_open_notification_with_language(uri, "blade", source),
    )
    .await;
    assert!(definition(&mut service, uri, source, "@{{ Probe::ru")
        .await
        .is_null());
    let (line, column) = utf16_position_after(source, "@{{ Probe::ru");
    assert!(send(&mut service, hover_request(11, uri, line, column))
        .await
        .is_null());
    let (line, column) = utf16_position_after(source, "@{{ Probe::run(");
    assert!(
        send(&mut service, signature_help_request(12, uri, line, column))
            .await
            .is_null()
    );
    let active = definition(&mut service, uri, source, "\n{{ Probe::ru").await;
    assert_eq!(location(&active)["uri"], php_uri);
    let (line, column) = utf16_position_after(php, "function ");
    assert_eq!(
        location(&active)["range"]["start"],
        json!({"line":line,"character":column})
    );
    let tokens = send(&mut service, semantic_tokens_full_request(13, uri)).await;
    assert!(decode_semantic_tokens(&tokens)
        .iter()
        .all(|(line, _, _, _, _)| *line == 1));
    let unescaped = source.replacen('@', "", 1);
    send(
        &mut service,
        did_change_full_notification(uri, 2, &unescaped),
    )
    .await;
    assert_eq!(
        location(&definition(&mut service, uri, &unescaped, "{{ Probe::ru").await)["uri"],
        php_uri
    );
    send(&mut service, did_change_full_notification(uri, 3, source)).await;
    assert!(definition(&mut service, uri, source, "@{{ Probe::ru")
        .await
        .is_null());
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn twig_numeric_set_values_have_real_types_and_local_definitions() {
    let mut service = service(None).await;
    let uri = "file:///test/p233-numbers.twig";
    for (version, literal, expected_type) in [
        (1, "1.5", "float"),
        (2, "2.5E+2", "float"),
        (3, "08", "int"),
    ] {
        let source = format!("😀 {{% set amount = {literal} %}}\r\n{{{{ amount }}}}");
        if version == 1 {
            send(
                &mut service,
                did_open_notification_with_language(uri, "twig", &source),
            )
            .await;
        } else {
            send(
                &mut service,
                did_change_full_notification(uri, version, &source),
            )
            .await;
        }
        let (line, column) = utf16_position_after(&source, "{{ a");
        let hover = send(&mut service, hover_request(20, uri, line, column)).await;
        let markdown = hover_markdown_value(&hover);
        assert!(
            markdown.contains(expected_type) && markdown.contains("amount"),
            "{literal}: {markdown}"
        );
        let target = definition(&mut service, uri, &source, "{{ a").await;
        let (line, column) = utf16_position_after(&source, "set ");
        assert_eq!(location(&target)["uri"], uri);
        assert_eq!(
            location(&target)["range"]["start"],
            json!({"line":line,"character":column})
        );
    }
    send(&mut service, shutdown_request(99)).await;
}

struct Workspace(std::path::PathBuf);
impl Workspace {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("php-lsp-p233-{}-{nonce}", std::process::id()));
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("templates")).unwrap();
        Self(root)
    }
    fn write(&self, relative: &str, source: &str) -> String {
        let path = self.0.join(relative);
        fs::write(&path, source).unwrap();
        path_to_uri(&path).unwrap()
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn twig_key_value_loop_resolves_key_and_value_through_controller_context() {
    let workspace = Workspace::new();
    let model = "<?php class Row {public function getName(): string {return 'row';}}";
    let controller = "<?php class Page {/** @param array<string, Row> $rows */ public function show(array $rows): void {$this->render('rows.html.twig', ['rows' => $rows]);}}";
    let source = "😀 {% for key, row in rows %}\r\n{{ key }} {{ row.getName() }}\r\n{{ row. }}\r\n{% endfor %}";
    let model_uri = workspace.write("Row.php", model);
    let controller_uri = workspace.write("Page.php", controller);
    let uri = workspace.write("templates/rows.html.twig", source);
    let root_uri = path_to_uri(&workspace.0).unwrap();
    let mut service = service(Some(&root_uri)).await;
    send(&mut service, did_open_notification(&model_uri, model)).await;
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
    let (line, column) = utf16_position_after(source, "{{ k");
    let key = send(&mut service, hover_request(30, &uri, line, column)).await;
    assert!(
        hover_markdown_value(&key).contains("string $key"),
        "key type: {key}"
    );
    let target = definition(&mut service, &uri, source, "{{ k").await;
    let (line, column) = utf16_position_after(source, "for ");
    assert_eq!(location(&target)["uri"], uri);
    assert_eq!(
        location(&target)["range"]["start"],
        json!({"line":line,"character":column})
    );
    let target = definition(&mut service, &uri, source, "row.getN").await;
    assert_eq!(location(&target)["uri"], model_uri);
    let (line, column) = utf16_position_after(model, "function ");
    assert_eq!(
        location(&target)["range"]["start"],
        json!({"line":line,"character":column})
    );
    let (line, column) = utf16_position_after(source, "{{ row.");
    let completion = send(&mut service, completion_request(31, &uri, line, column)).await;
    assert!(
        completion_items_from_result(&completion)
            .iter()
            .any(|item| item["label"] == "getName"),
        "{completion}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn rejected_blade_edit_batch_preserves_navigation_until_full_sync() {
    let mut service = service(None).await;
    let php_uri = "file:///test/p233-edit.php";
    let php = "<?php\nclass Before {public static function run(): void {}}\nclass After {public static function run(): void {}}";
    let uri = "file:///test/p233-edit.blade.php";
    let source = "{{ Before::run() }}";
    send(&mut service, did_open_notification(php_uri, php)).await;
    send(
        &mut service,
        did_open_notification_with_language(uri, "blade", source),
    )
    .await;
    send(&mut service, Request::build("textDocument/didChange").params(json!({
        "textDocument": {"uri":uri,"version":2}, "contentChanges":[
            {"range":{"start":{"line":0,"character":3},"end":{"line":0,"character":9}},"text":"After"},
            {"range":{"start":{"line":99,"character":0},"end":{"line":99,"character":1}},"text":"bad"}
        ]
    })).finish()).await;
    let target = definition(&mut service, uri, source, "::ru").await;
    assert_eq!(
        location(&target)["range"]["start"]["line"],
        1,
        "partial edit became visible: {target}"
    );
    let fixed = "{{ After::run() }}";
    send(&mut service, did_change_full_notification(uri, 3, fixed)).await;
    let target = definition(&mut service, uri, fixed, "::ru").await;
    assert_eq!(location(&target)["range"]["start"]["line"], 2);
    send(&mut service, did_close_notification(uri)).await;
    send(
        &mut service,
        did_open_notification_with_language(uri, "blade", source),
    )
    .await;
    let target = definition(&mut service, uri, source, "::ru").await;
    assert_eq!(location(&target)["range"]["start"]["line"], 1);
    send(&mut service, shutdown_request(99)).await;
}
