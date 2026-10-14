mod support;
use php_lsp_types::uri::path_to_uri;
use support::*;

const MODEL: &str = "<?php\r\nclass First { /* 😀 */ public string $label; public string $firstOnly; }\r\nclass Second { /* 😀 */ public int $label; public int $secondOnly; }\r\nclass Kept { public string $keepOnly; }";
const CONTROLLER: &str = "<?php\r\nfunction page() { /* 😀 */ $this->render('caller.twig', ['item' => new First(), 'other' => new Second(), 'keep' => new Kept(), 'payload' => ['code' => 'first'], 'other_payload' => ['code' => 42]]); }";
const PARTIAL: &str = "😀 {{ item.label }}\r\n{{ keep.keepOnly }}\r\n😀 {{ payload.code }}";
const CALLER: &str = "{% include 'partial.twig' %}";

async fn send(service: &mut LspService<PhpLspBackend>, request: Request) -> serde_json::Value {
    match service.ready().await.unwrap().call(request).await.unwrap() {
        Some(response) => {
            assert!(response.error().is_none(), "{response:?}");
            extract_result(Some(response))
        }
        None => serde_json::Value::Null,
    }
}

struct Fixture {
    root: std::path::PathBuf,
    model: String,
    controller: String,
    caller: String,
    partial: String,
}
impl Fixture {
    async fn new() -> (Self, LspService<PhpLspBackend>) {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("php-lsp-p234-{}-{nonce}", std::process::id()));
        fs::create_dir_all(root.join("templates")).unwrap();
        let write = |path: &str, source: &str| {
            let path = root.join(path);
            fs::write(&path, source).unwrap();
            path_to_uri(&path).unwrap()
        };
        let model = write("Model.php", MODEL);
        let controller = write("Controller.php", CONTROLLER);
        let caller = write("templates/caller.twig", CALLER);
        let partial = write("templates/partial.twig", PARTIAL);
        let root_uri = path_to_uri(&root).unwrap();
        let fixture = Self {
            root,
            model,
            controller,
            caller,
            partial,
        };
        let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
        tokio::spawn(async move { while socket.next().await.is_some() {} });
        send(
            &mut service,
            initialize_request_with_options(
                1,
                Some(&root_uri),
                Some(json!({
                    "stubExtensions": [], "indexVendor": false, "diagnosticsMode": "off"
                })),
            ),
        )
        .await;
        send(&mut service, did_open_notification(&fixture.model, MODEL)).await;
        send(
            &mut service,
            did_open_notification(&fixture.controller, CONTROLLER),
        )
        .await;
        send(
            &mut service,
            did_open_notification_with_language(&fixture.partial, "twig", PARTIAL),
        )
        .await;
        (fixture, service)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
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
    needle: &str,
) -> serde_json::Value {
    let (line, character) = utf16_position_after(PARTIAL, needle);
    send(service, definition_request(20, uri, line, character)).await
}

fn assert_location(
    value: &serde_json::Value,
    uri: &str,
    source: &str,
    start: usize,
    length: usize,
) {
    let (line, character) = utf16_position_for_offset(source, start);
    let (end_line, end_character) = utf16_position_for_offset(source, start + length);
    assert_eq!(location(value)["uri"], uri, "{value}");
    assert_eq!(
        location(value)["range"],
        json!({
            "start": {"line": line, "character": character},
            "end": {"line": end_line, "character": end_character}
        }),
        "{value}"
    );
}

async fn assert_item(
    service: &mut LspService<PhpLspBackend>,
    fixture: &Fixture,
    expected: Option<&str>,
) {
    let (line, column) = utf16_position_after(PARTIAL, "{{ item.");
    let completion = send(
        service,
        completion_request(21, &fixture.partial, line, column),
    )
    .await;
    let items = completion_items_from_result(&completion);
    for (class, member) in [("First", "firstOnly"), ("Second", "secondOnly")] {
        assert_eq!(
            items.iter().any(|item| item["label"] == member),
            expected == Some(class),
            "{completion}"
        );
    }
    let (line, column) = utf16_position_after(PARTIAL, "{{ it");
    let hover = send(service, hover_request(22, &fixture.partial, line, column)).await;
    let markdown = hover_markdown_value(&hover);
    let target = definition(service, &fixture.partial, "item.la").await;
    if let Some(class) = expected {
        assert!(markdown.contains(class), "{hover}");
        let start = if class == "First" {
            MODEL.find("$label")
        } else {
            MODEL.rfind("$label")
        }
        .unwrap();
        assert_location(&target, &fixture.model, MODEL, start, "$label".len());
    } else {
        assert!(
            !markdown.contains("First") && !markdown.contains("Second"),
            "stale hover: {hover}"
        );
        assert!(
            target.is_null() || target.as_array().is_some_and(Vec::is_empty),
            "stale definition: {target}"
        );
    }
}

async fn assert_keep(service: &mut LspService<PhpLspBackend>, fixture: &Fixture, present: bool) {
    let (line, column) = utf16_position_after(PARTIAL, "{{ keep.");
    let completion = send(
        service,
        completion_request(23, &fixture.partial, line, column),
    )
    .await;
    assert_eq!(
        completion_items_from_result(&completion)
            .iter()
            .any(|item| item["label"] == "keepOnly"),
        present,
        "{completion}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn plain_with_only_and_unsaved_removal_refresh_already_open_partial() {
    let (fixture, mut service) = Fixture::new().await;
    assert_item(&mut service, &fixture, Some("First")).await;
    assert_keep(&mut service, &fixture, true).await;
    send(
        &mut service,
        did_open_notification_with_language(
            &fixture.caller,
            "twig",
            "{% include 'partial.twig' with {item: other} %}",
        ),
    )
    .await;
    assert_item(&mut service, &fixture, Some("Second")).await;
    assert_keep(&mut service, &fixture, true).await;
    send(
        &mut service,
        did_change_full_notification(
            &fixture.caller,
            2,
            "{% include 'partial.twig' with {item: other} only %}",
        ),
    )
    .await;
    assert_item(&mut service, &fixture, Some("Second")).await;
    assert_keep(&mut service, &fixture, false).await;
    for (version, source) in [
        (3, "{% include 'partial.twig' only %}"),
        (4, "include removed"),
    ] {
        send(
            &mut service,
            did_change_full_notification(&fixture.caller, version, source),
        )
        .await;
        assert_item(&mut service, &fixture, None).await;
        assert_keep(&mut service, &fixture, false).await;
    }
    send(&mut service, did_close_notification(&fixture.caller)).await;
    assert_item(&mut service, &fixture, Some("First")).await;
    send(
        &mut service,
        did_open_notification_with_language(
            &fixture.caller,
            "twig",
            "{# {% include 'partial.twig' %} #}",
        ),
    )
    .await;
    assert_item(&mut service, &fixture, None).await;
    send(&mut service, did_close_notification(&fixture.caller)).await;
    assert_item(&mut service, &fixture, Some("First")).await;
    fs::write(
        fixture.root.join("templates/caller.twig"),
        "{% include 'partial.twig' only %}",
    )
    .unwrap();
    send(
        &mut service,
        did_change_watched_files_notification(vec![(&fixture.caller, 2)]),
    )
    .await;
    assert_item(&mut service, &fixture, None).await;
    fs::write(fixture.root.join("templates/caller.twig"), CALLER).unwrap();
    send(
        &mut service,
        did_change_watched_files_notification(vec![(&fixture.caller, 2)]),
    )
    .await;
    assert_item(&mut service, &fixture, Some("First")).await;
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn include_shape_overrides_replace_exact_utf16_definition_provenance() {
    let (fixture, mut service) = Fixture::new().await;
    let target = definition(&mut service, &fixture.partial, "payload.co").await;
    assert_location(
        &target,
        &fixture.controller,
        CONTROLLER,
        CONTROLLER.find("'code'").unwrap() + 1,
        4,
    );
    send(
        &mut service,
        did_open_notification_with_language(
            &fixture.caller,
            "twig",
            "{% include 'partial.twig' with {payload: other_payload} %}",
        ),
    )
    .await;
    let target = definition(&mut service, &fixture.partial, "payload.co").await;
    assert_location(
        &target,
        &fixture.controller,
        CONTROLLER,
        CONTROLLER.rfind("'code'").unwrap() + 1,
        4,
    );
    for (version, value) in [(2, "null"), (3, "unresolved"), (4, "payload|keys")] {
        let source = format!("{{% include 'partial.twig' with {{payload: {value}}} %}}");
        send(
            &mut service,
            did_change_full_notification(&fixture.caller, version, &source),
        )
        .await;
        let target = definition(&mut service, &fixture.partial, "payload.co").await;
        assert!(
            target.is_null() || target.as_array().is_some_and(Vec::is_empty),
            "stale provenance for {value}: {target}"
        );
    }
    send(&mut service, did_close_notification(&fixture.caller)).await;
    let target = definition(&mut service, &fixture.partial, "payload.co").await;
    assert_location(
        &target,
        &fixture.controller,
        CONTROLLER,
        CONTROLLER.find("'code'").unwrap() + 1,
        4,
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn local_writes_and_unknown_maps_do_not_reuse_render_object_members() {
    let (fixture, mut service) = Fixture::new().await;
    for (index, source) in [
        "{% set item = other %}{% include 'partial.twig' %}",
        "{% for item in other %}{% include 'partial.twig' %}{% endfor %}",
        "{% include 'partial.twig' with {item: item|length} %}",
        "{% include 'partial.twig' with unknown_map %}",
    ]
    .into_iter()
    .enumerate()
    {
        if index == 0 {
            send(
                &mut service,
                did_open_notification_with_language(&fixture.caller, "twig", source),
            )
            .await;
        } else {
            send(
                &mut service,
                did_change_full_notification(&fixture.caller, index as i32 + 1, source),
            )
            .await;
        }
        assert_item(&mut service, &fixture, None).await;
    }
    send(&mut service, shutdown_request(99)).await;
}
