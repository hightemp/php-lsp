mod support;
use support::*;
use tower_lsp::LanguageServer;

async fn send(service: &mut LspService<PhpLspBackend>, request: Request) -> serde_json::Value {
    let response = service.ready().await.unwrap().call(request).await.unwrap();
    response
        .map(|r| {
            assert!(r.error().is_none(), "{r:?}");
            extract_result(Some(r))
        })
        .unwrap_or(serde_json::Value::Null)
}
async fn service() -> LspService<PhpLspBackend> {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move { while socket.next().await.is_some() {} });
    send(
        &mut service,
        initialize_request_with_options(
            1,
            None,
            Some(json!({"stubExtensions":[],"indexVendor":false,"diagnosticsMode":"off"})),
        ),
    )
    .await;
    service
}
fn source(key: &str, literal: bool) -> String {
    let binding = if literal {
        "$shape=['ready'=>new Plain,'ready?'=>new Question];"
    } else {
        "/** @var array{ready: Plain, 'ready?'?: Question} $shape */ $shape=[];"
    };
    format!("<?php\r\nclass Plain {{function /*PLAIN*/common(){{}} function onlyPlain(){{}}}}\r\nclass Question {{function /*QUESTION*/common(){{}} function onlyQuestion(){{}}}}\r\nfunction run(){{ /* 😀 */ {binding}\r\n $shape['/*KEY*/'];\r\n $selected=$shape['{key}'];\r\n echo /*USE*/$selected;\r\n $selected->/*CALL*/common();}}")
}
fn position(source: &str, marker: &str) -> serde_json::Value {
    let (line, col) = utf16_position_after(source, marker);
    json!({"line":line,"character":col})
}
async fn check(
    service: &mut LspService<PhpLspBackend>,
    uri: &str,
    source: &str,
    expected: Option<&str>,
) {
    let (line, col) = utf16_position_after(source, "/*USE*/");
    let result = send(service, hover_request(20, uri, line, col + 1)).await;
    let text = hover_markdown_value(&result);
    if let Some(name) = expected {
        assert!(text.contains(&format!("{name} $selected")), "{text}");
    } else {
        assert!(
            !text.contains("Plain") && !text.contains("Question"),
            "missing key borrowed a type: {text}"
        );
    }
    let (line, col) = utf16_position_after(source, "$selected->");
    let result = send(service, completion_request(21, uri, line, col)).await;
    let items = result
        .as_array()
        .or_else(|| result["items"].as_array())
        .cloned()
        .unwrap_or_default();
    for name in ["Plain", "Question"] {
        assert_eq!(
            items
                .iter()
                .any(|item| item["label"] == format!("only{name}")),
            expected == Some(name),
            "{result}"
        );
    }
    let (line, col) = utf16_position_after(source, "/*CALL*/");
    let result = send(service, definition_request(22, uri, line, col + 1)).await;
    if let Some(name) = expected {
        let location = result.as_array().and_then(|v| v.first()).unwrap_or(&result);
        assert_eq!(location["uri"], uri);
        assert_eq!(
            location["range"]["start"],
            position(
                source,
                if name == "Plain" {
                    "/*PLAIN*/"
                } else {
                    "/*QUESTION*/"
                }
            ),
            "{result}"
        );
    } else {
        assert!(
            result.is_null() || result.as_array().is_some_and(Vec::is_empty),
            "{result}"
        );
    }
    let (line, col) = utf16_position_for_offset(source, source.len());
    let hints = send(service, inlay_hint_request(23, uri, 0, 0, line, col)).await;
    let selected_position = position(source, "$selected");
    let selected = hints
        .as_array()
        .unwrap()
        .iter()
        .find(|hint| hint["position"] == selected_position);
    if let Some(name) = expected {
        assert_eq!(
            selected.and_then(inlay_hint_label_text).as_deref(),
            Some(format!(": {name}").as_str()),
            "{hints}"
        );
    } else {
        assert!(selected.is_none(), "missing key borrowed a hint: {hints}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn escaped_literal_keys_do_not_alias_different_php_string_values() {
    let mut service = service().await;
    let uri = "file:///test/escaped-shape.php";
    let text = r#"<?php class Plain {function common(){}} class Question {function /*QUESTION*/common(){}} function run(){/** @var array{'\n':Plain,"\n":Question} $shape */ $shape=[]; $selected=$shape["\n"]; echo /*USE*/$selected; $selected->/*CALL*/common();}"#;
    send(&mut service, did_open_notification(uri, text)).await;
    let (line, col) = utf16_position_after(text, "/*USE*/");
    let result = send(&mut service, hover_request(25, uri, line, col + 1)).await;
    assert!(
        hover_markdown_value(&result).contains("Question $selected"),
        "{result}"
    );
    let (line, col) = utf16_position_after(text, "/*CALL*/");
    let result = send(&mut service, definition_request(26, uri, line, col + 1)).await;
    let location = result.as_array().and_then(|v| v.first()).unwrap_or(&result);
    assert_eq!(
        location["range"]["start"],
        position(text, "/*QUESTION*/"),
        "{result}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn missing_shape_keys_do_not_offer_unrelated_members_or_definitions() {
    let mut service = service().await;
    let uri = "file:///test/missing-shape.php";
    for literal in [false, true] {
        let source = source("missing", literal);
        send(&mut service, did_open_notification(uri, &source)).await;
        check(&mut service, uri, &source, None).await;
        send(&mut service, did_close_notification(uri)).await;
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn literal_question_mark_keys_select_their_own_type_and_member() {
    let mut service = service().await;
    let uri = "file:///test/question-shape.php";
    for literal in [false, true] {
        let source = source("ready?", literal);
        send(&mut service, did_open_notification(uri, &source)).await;
        check(&mut service, uri, &source, Some("Question")).await;
        send(&mut service, did_close_notification(uri)).await;
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn question_mark_key_completion_and_definition_preserve_exact_spelling() {
    let mut service = service().await;
    let uri = "file:///test/shape-key-identity.php";
    for literal in [false, true] {
        let source = source("ready?", literal);
        send(&mut service, did_open_notification(uri, &source)).await;
        let (line, col) = utf16_position_after(&source, "$shape['");
        let result = send(&mut service, completion_request(30, uri, line, col)).await;
        let items = result
            .as_array()
            .or_else(|| result["items"].as_array())
            .unwrap();
        for key in ["ready", "ready?"] {
            assert_eq!(
                items.iter().filter(|item| item["label"] == key).count(),
                1,
                "{result}"
            );
        }
        let offset = source.find("$selected=$shape['ready?").unwrap() + "$selected=$shape['".len();
        let (line, col) = utf16_position_for_offset(&source, offset);
        let result = send(&mut service, definition_request(31, uri, line, col + 1)).await;
        let location = result.as_array().and_then(|v| v.first()).unwrap_or(&result);
        let declaration = source.find("'ready?'").unwrap() + 1;
        let (line, col) = utf16_position_for_offset(&source, declaration);
        assert_eq!(
            location["range"]["start"],
            json!({"line":line,"character":col}),
            "{result}"
        );
        assert_eq!(
            location["range"]["end"],
            json!({"line":line,"character":col+6}),
            "{result}"
        );
        send(&mut service, did_close_notification(uri)).await;
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn heterogeneous_shape_iteration_keeps_common_members_of_every_value() {
    let mut service = service().await;
    let uri = "file:///test/shape-iteration.php";
    let text="<?php class Plain {function common(){} function onlyPlain(){}} class Question {function common(){} function onlyQuestion(){}} function run(){/** @var array{first:Plain,second:Question} $shape */ $shape=[]; foreach($shape as $selected){echo /*USE*/$selected; $selected->/*CALL*/common();}}";
    send(&mut service, did_open_notification(uri, text)).await;
    let (line, col) = utf16_position_after(text, "/*USE*/");
    let result = send(&mut service, hover_request(60, uri, line, col + 1)).await;
    let text_hover = hover_markdown_value(&result);
    assert!(
        text_hover.contains("Plain") && text_hover.contains("Question"),
        "{text_hover}"
    );
    let (line, col) = utf16_position_after(text, "$selected->");
    let result = send(&mut service, completion_request(61, uri, line, col)).await;
    let items = result
        .as_array()
        .or_else(|| result["items"].as_array())
        .unwrap();
    assert!(
        items.iter().any(|item| item["label"] == "common"),
        "{result}"
    );
    assert!(
        !items
            .iter()
            .any(|item| item["label"] == "onlyPlain" || item["label"] == "onlyQuestion"),
        "{result}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_return_shapes_keep_open_tail_and_literal_keys_in_the_declaring_namespace() {
    let mut service = service().await;
    let provider_uri = "file:///test/shape-provider.php";
    let uri = "file:///test/shape-caller.php";
    let provider="<?php namespace Meta; class Plain {function common(){}} class Question {function /*DECL*/common(){} function onlyQuestion(){}} class Provider {/** @return array{ready:Plain,'ready?':Question,...<Question>} */ static function make():array {return [];}}";
    send(&mut service, did_open_notification(provider_uri, provider)).await;
    for key in ["ready?", "missing"] {
        let caller=format!("<?php namespace Caller; use Meta\\Provider; $shape=Provider::make(); $selected=$shape['{key}']; echo /*USE*/$selected; $selected->/*CALL*/common();");
        send(&mut service, did_open_notification(uri, &caller)).await;
        let (line, col) = utf16_position_after(&caller, "/*USE*/");
        let result = send(&mut service, hover_request(70, uri, line, col + 1)).await;
        assert!(
            hover_markdown_value(&result).contains("Question $selected"),
            "{result}"
        );
        let (line, col) = utf16_position_after(&caller, "/*CALL*/");
        let result = send(&mut service, definition_request(71, uri, line, col + 1)).await;
        let location = result.as_array().and_then(|v| v.first()).unwrap_or(&result);
        assert_eq!(location["uri"], provider_uri, "{result}");
        assert_eq!(
            location["range"]["start"],
            position(provider, "/*DECL*/"),
            "{result}"
        );
        send(&mut service, did_close_notification(uri)).await;
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn shape_lookup_follows_unsaved_versions_missing_keys_and_reopen() {
    let mut service = service().await;
    let uri = "file:///test/versioned-shape.php";
    let plain = source("ready", false);
    let question = source("ready?", false);
    let missing = source("missing", false);
    send(&mut service, did_open_notification(uri, &plain)).await;
    check(&mut service, uri, &plain, Some("Plain")).await;
    send(
        &mut service,
        did_change_full_notification(uri, 2, &question),
    )
    .await;
    check(&mut service, uri, &question, Some("Question")).await;
    send(&mut service, did_change_full_notification(uri, 3, &missing)).await;
    send(
        &mut service,
        did_change_full_notification(uri, 2, &question),
    )
    .await;
    check(&mut service, uri, &missing, None).await;
    send(&mut service, did_close_notification(uri)).await;
    send(&mut service, did_open_notification(uri, &question)).await;
    check(&mut service, uri, &question, Some("Question")).await;
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_key_edits_and_definitions_keep_complete_current_types() {
    let mut service = service().await;
    let uri = "file:///test/concurrent-shape.php";
    let plain = source("ready", false);
    let question = source("ready?", false);
    send(&mut service, did_open_notification(uri, &plain)).await;
    let service = std::sync::Arc::new(service);
    tokio::time::timeout(Duration::from_secs(15), async {
        for version in 2..22 {
            let text = if version % 2 == 0 { &question } else { &plain };
            let change = did_change_full_notification(uri, version, text);
            let params = serde_json::from_value(change.params().unwrap().clone()).unwrap();
            let (line, col) = utf16_position_after(text, "/*CALL*/");
            let query = definition_request(50, uri, line, col + 1);
            let query = serde_json::from_value(query.params().unwrap().clone()).unwrap();
            let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
            let writer = service.clone();
            let gate = barrier.clone();
            let change = tokio::spawn(async move {
                gate.wait().await;
                writer.inner().did_change(params).await;
            });
            let reader = service.clone();
            let definition = tokio::spawn(async move {
                barrier.wait().await;
                reader.inner().goto_definition(query).await
            });
            let (changed, definition) = tokio::join!(change, definition);
            changed.unwrap();
            let value = serde_json::to_value(definition.unwrap().unwrap()).unwrap();
            let location = value.as_array().and_then(|v| v.first()).unwrap_or(&value);
            assert!(
                location["range"]["start"] == position(text, "/*PLAIN*/")
                    || location["range"]["start"] == position(text, "/*QUESTION*/"),
                "{value}"
            );
            let query = definition_request(51, uri, line, col + 1);
            let query = serde_json::from_value(query.params().unwrap().clone()).unwrap();
            let value = serde_json::to_value(service.inner().goto_definition(query).await.unwrap())
                .unwrap();
            let location = value.as_array().and_then(|v| v.first()).unwrap_or(&value);
            assert_eq!(
                location["range"]["start"],
                position(
                    text,
                    if version % 2 == 0 {
                        "/*QUESTION*/"
                    } else {
                        "/*PLAIN*/"
                    }
                ),
                "{value}"
            );
        }
    })
    .await
    .expect("shape edit/definition work stalled");
    service.inner().shutdown().await.unwrap();
}
