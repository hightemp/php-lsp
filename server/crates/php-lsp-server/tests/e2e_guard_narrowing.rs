mod support;
use support::*;
use tower_lsp::LanguageServer;

async fn send(service: &mut LspService<PhpLspBackend>, request: Request) -> serde_json::Value {
    let response = service.ready().await.unwrap().call(request).await.unwrap();
    response
        .map(|response| {
            assert!(response.error().is_none(), "{response:?}");
            extract_result(Some(response))
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
fn source(guard: &str) -> String {
    format!("<?php\r\ninterface Input {{function /*BASE*/common();}}\r\nclass Foo implements Input {{function /*FOO*/common(){{}} function onlyFoo(){{}}}}\r\nfunction run(Input $value,bool $flag) {{{guard}\r\n /* 😀 */ echo /*USE*/$value; $result=$value; $value->/*CALL*/common();}}")
}
fn expected_position(source: &str, marker: &str) -> serde_json::Value {
    let (line, col) = utf16_position_after(source, marker);
    json!({"line":line,"character":col})
}
fn assert_definition(result: &serde_json::Value, source: &str, marker: &str) {
    let location = result
        .as_array()
        .and_then(|values| values.first())
        .unwrap_or(result);
    assert_eq!(
        location["range"]["start"],
        expected_position(source, marker),
        "{result}"
    );
}
async fn check(service: &mut LspService<PhpLspBackend>, uri: &str, source: &str, narrowed: bool) {
    let (line, col) = utf16_position_after(source, "/*USE*/");
    let result = send(service, hover_request(20, uri, line, col + 1)).await;
    let text = hover_markdown_value(&result);
    let expected = if narrowed {
        "Foo $value"
    } else {
        "Input $value"
    };
    assert!(text.contains(expected), "expected {expected}: {text}");
    let (line, col) = utf16_position_after(source, "$value->");
    let result = send(service, completion_request(21, uri, line, col)).await;
    let items = result
        .as_array()
        .or_else(|| result["items"].as_array())
        .unwrap();
    assert!(
        items.iter().any(|item| item["label"] == "common"),
        "{result}"
    );
    assert_eq!(
        items.iter().any(|item| item["label"] == "onlyFoo"),
        narrowed,
        "{result}"
    );
    let (line, col) = utf16_position_after(source, "/*CALL*/");
    let result = send(service, definition_request(22, uri, line, col + 1)).await;
    assert_definition(
        &result,
        source,
        if narrowed { "/*FOO*/" } else { "/*BASE*/" },
    );
    let (line, col) = utf16_position_for_offset(source, source.len());
    let result = send(service, inlay_hint_request(23, uri, 0, 0, line, col)).await;
    let expected = if narrowed { ": Foo" } else { ": Input" };
    assert!(
        result
            .as_array()
            .unwrap()
            .iter()
            .any(|hint| inlay_hint_label_text(hint).as_deref() == Some(expected)),
        "{result}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn unsafe_boolean_and_fake_exits_keep_native_members_across_lsp_features() {
    let mut service = service().await;
    let uri = "file:///test/unsafe-guard.php";
    for guard in [
        "if(!($value instanceof Foo) && $flag)return;",
        "if(!($value instanceof Foo)){/*return;*/work();}",
        "if(!($value instanceof Foo)){$fn=function(){throw new Error;};}",
        "if(!($value instanceof Foo)){if($flag)return;}",
        "if(!($value instanceof Foo) && $flag){$value=new Foo;return;}",
    ] {
        let source = source(guard);
        send(&mut service, did_open_notification(uri, &source)).await;
        check(&mut service, uri, &source, false).await;
        send(&mut service, did_close_notification(uri)).await;
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn proven_boolean_exit_guards_resolve_exact_members_in_utf16_crlf_documents() {
    let mut service = service().await;
    let uri = "file:///test/proven-guard.php";
    for guard in [
        "if($flag || !($value instanceof Foo))return;",
        "if(/* 😀 */ !(($value instanceof Foo) && $flag)){work();return;}",
        "if(!($value instanceof Foo)){$value=null;return;}",
    ] {
        let source = source(guard);
        send(&mut service, did_open_notification(uri, &source)).await;
        check(&mut service, uri, &source, true).await;
        send(&mut service, did_close_notification(uri)).await;
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn guard_inference_follows_unsaved_versions_and_close_reopen() {
    let mut service = service().await;
    let uri = "file:///test/versioned-guard.php";
    let safe = source("if(!($value instanceof Foo))return;");
    let unsafe_source = source("if(!($value instanceof Foo) && $flag)return;");
    send(&mut service, did_open_notification(uri, &safe)).await;
    check(&mut service, uri, &safe, true).await;
    send(
        &mut service,
        did_change_full_notification(uri, 2, &unsafe_source),
    )
    .await;
    check(&mut service, uri, &unsafe_source, false).await;
    send(&mut service, did_change_full_notification(uri, 3, &safe)).await;
    send(
        &mut service,
        did_change_full_notification(uri, 2, &unsafe_source),
    )
    .await;
    check(&mut service, uri, &safe, true).await;
    send(&mut service, did_close_notification(uri)).await;
    send(&mut service, did_open_notification(uri, &unsafe_source)).await;
    check(&mut service, uri, &unsafe_source, false).await;
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_guard_edits_and_definitions_return_a_complete_snapshot() {
    let mut service = service().await;
    let uri = "file:///test/concurrent-guard.php";
    let safe = source("if(!($value instanceof Foo))return;");
    let unsafe_source = source("if(!($value instanceof Foo) && $flag)return;");
    send(&mut service, did_open_notification(uri, &safe)).await;
    let service = std::sync::Arc::new(service);
    tokio::time::timeout(Duration::from_secs(15), async {
        for version in 2..22 {
            let text = if version % 2 == 0 {
                &unsafe_source
            } else {
                &safe
            };
            let change = did_change_full_notification(uri, version, text);
            let params = serde_json::from_value(change.params().unwrap().clone()).unwrap();
            let (line, col) = utf16_position_after(text, "/*CALL*/");
            let query = definition_request(50, uri, line, col + 1);
            let query = serde_json::from_value(query.params().unwrap().clone()).unwrap();
            let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
            let writer = service.clone();
            let write_barrier = barrier.clone();
            let change = tokio::spawn(async move {
                write_barrier.wait().await;
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
            let location = value
                .as_array()
                .and_then(|values| values.first())
                .unwrap_or(&value);
            let position = &location["range"]["start"];
            assert!(
                *position == expected_position(text, "/*FOO*/")
                    || *position == expected_position(text, "/*BASE*/"),
                "{value}"
            );
            let query = definition_request(51, uri, line, col + 1);
            let query = serde_json::from_value(query.params().unwrap().clone()).unwrap();
            let value = serde_json::to_value(service.inner().goto_definition(query).await.unwrap())
                .unwrap();
            assert_definition(
                &value,
                text,
                if version % 2 == 0 {
                    "/*BASE*/"
                } else {
                    "/*FOO*/"
                },
            );
        }
    })
    .await
    .expect("guard edit/definition work stalled");
    service.inner().shutdown().await.unwrap();
}
