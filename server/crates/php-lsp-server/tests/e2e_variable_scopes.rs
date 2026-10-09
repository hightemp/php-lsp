mod support;
use support::*;

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

async fn assert_definition(
    service: &mut LspService<PhpLspBackend>,
    uri: &str,
    source: &str,
    has_definition: bool,
) {
    let (line, col) = utf16_position_after(source, "/*USE*/");
    let result = send(service, definition_request(20, uri, line, col + 1)).await;
    if !has_definition {
        assert!(
            result.is_null() || result.as_array().is_some_and(|items| items.is_empty()),
            "foreign definition: {result}"
        );
        return;
    }
    let location = result
        .as_array()
        .and_then(|items| items.first())
        .unwrap_or(&result);
    let (line, col) = utf16_position_after(source, "/*DEF*/");
    assert_eq!(location["uri"], uri, "wrong location: {result}");
    assert_eq!(
        location["range"]["start"],
        json!({"line":line,"character":col}),
        "wrong lexical definition: {result}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn outer_definition_and_type_ignore_nested_callable_writes() {
    let mut service = service().await;
    let uri = "file:///test/outer-binding.php";
    let source="<?php class Right {} class Wrong {} function outer() {/*DEF*/$value=new Right; $fn=function() {$value=new Wrong;}; echo /*USE*/$value;}";
    send(&mut service, did_open_notification(uri, source)).await;
    assert_definition(&mut service, uri, source, true).await;
    let (line, col) = utf16_position_after(source, "/*USE*/");
    let hover = send(&mut service, hover_request(21, uri, line, col + 1)).await;
    assert!(
        hover_markdown_value(&hover).contains("Right $value"),
        "wrong outer type: {hover}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn captures_and_parameter_shadowing_agree_for_definition_hover_and_rename() {
    let mut service = service().await;
    let uri = "file:///test/capture-bindings.php";
    let source="<?php class Right {} class Wrong {} function outer() {/*DEF*/$value=new Right; $capture=function() use($value) {echo /*USE*/$value;}; $arrow=fn()=> $value; $shadow=fn(Wrong $value)=>$value; $local=function() {$value=new Wrong;};}";
    send(&mut service, did_open_notification(uri, source)).await;
    assert_definition(&mut service, uri, source, true).await;
    let (line, col) = utf16_position_after(source, "/*USE*/");
    let hover = send(&mut service, hover_request(21, uri, line, col + 1)).await;
    assert!(
        hover_markdown_value(&hover).contains("Right $value"),
        "capture lost its type: {hover}"
    );
    let rename = send(
        &mut service,
        rename_request(22, uri, line, col + 1, "$renamed"),
    )
    .await;
    let edits = rename["changes"][uri]
        .as_array()
        .expect("local rename edits");
    assert_eq!(
        edits.len(),
        4,
        "capture rename crossed independent scopes: {rename}"
    );
    for (offset, _) in source.match_indices("$value").take(4) {
        let (line, character) = utf16_position_for_offset(source, offset);
        assert!(edits.iter().any(|edit|edit["range"]==json!({"start":{"line":line,"character":character},"end":{"line":line,"character":character+6}})),"missing captured binding at {offset}: {rename}");
    }
    for edit in edits {
        assert_eq!(edit["newText"], "$renamed");
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn uncaptured_closure_definition_never_falls_through_to_parent() {
    let mut service = service().await;
    let uri = "file:///test/uncaptured-binding.php";
    let source="<?php class Right {} function outer() {$value=new Right; $fn=function() {echo /*USE*/$value;};}";
    send(&mut service, did_open_notification(uri, source)).await;
    assert_definition(&mut service, uri, source, false).await;
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn arrow_local_assignment_from_indexed_method_has_hover_and_definition() {
    let mut service = service().await;
    let uri = "file:///test/arrow-local-rhs.php";
    let source="<?php class Right {} class Service {function load():Right {return new Right;}} $fn=fn()=> (/*DEF*/$value=(new Service())->load()) && /*USE*/$value;";
    send(&mut service, did_open_notification(uri, source)).await;
    assert_definition(&mut service, uri, source, true).await;
    let (line, col) = utf16_position_after(source, "/*USE*/");
    let hover = send(&mut service, hover_request(21, uri, line, col + 1)).await;
    assert!(
        hover_markdown_value(&hover).contains("Right $value"),
        "arrow RHS invisible: {hover}"
    );
    let (end_line, end_col) = utf16_position_for_offset(source, source.len());
    let hints = send(
        &mut service,
        inlay_hint_request(22, uri, 0, 0, end_line, end_col),
    )
    .await;
    assert!(
        hints
            .as_array()
            .unwrap()
            .iter()
            .any(|hint| inlay_hint_label_text(hint).as_deref() == Some(": Right")),
        "missing arrow local inlay: {hints}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn destructuring_definitions_update_in_unsaved_unicode_crlf_buffers() {
    let mut service = service().await;
    let uri = "file:///test/destructure-binding.php";
    let source =
        "<?php function outer($items) { /* 😀 */ [/*DEF*/$value]=$items;\r\necho /*USE*/$value;}";
    send(&mut service, did_open_notification(uri, source)).await;
    assert_definition(&mut service, uri, source, true).await;
    let changed = source.replace("[/*DEF*/$value]=$items;", "/*DEF*/$value =& $items;");
    send(&mut service, did_change_full_notification(uri, 2, &changed)).await;
    assert_definition(&mut service, uri, &changed, true).await;
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn capture_edits_and_close_reopen_replace_cached_bindings() {
    let mut service = service().await;
    let uri = "file:///test/capture-lifecycle.php";
    let captured="<?php class Right {} class Wrong {} function outer() {/*DEF*/$value=new Right; $fn=function() use($value) {echo /*USE*/$value;};}";
    send(&mut service, did_open_notification(uri, captured)).await;
    assert_definition(&mut service, uri, captured, true).await;
    let uncaptured = captured.replace("use($value)", "");
    send(
        &mut service,
        did_change_full_notification(uri, 2, &uncaptured),
    )
    .await;
    assert_definition(&mut service, uri, &uncaptured, false).await;
    let (line, col) = utf16_position_after(&uncaptured, "/*USE*/");
    let hover = send(&mut service, hover_request(31, uri, line, col + 1)).await;
    assert!(
        !hover_markdown_value(&hover).contains("Right $value"),
        "stale capture type: {hover}"
    );
    let shadowed = captured
        .replace("/*DEF*/$value=new Right;", "$value=new Right;")
        .replace("function() use($value)", "function(Wrong /*DEF*/$value)");
    send(
        &mut service,
        did_change_full_notification(uri, 3, &shadowed),
    )
    .await;
    assert_definition(&mut service, uri, &shadowed, true).await;
    let (line, col) = utf16_position_after(&shadowed, "/*USE*/");
    let hover = send(&mut service, hover_request(32, uri, line, col + 1)).await;
    assert!(
        hover_markdown_value(&hover).contains("Wrong $value"),
        "parameter shadow lost: {hover}"
    );
    send(&mut service, did_close_notification(uri)).await;
    send(&mut service, did_open_notification(uri, captured)).await;
    assert_definition(&mut service, uri, captured, true).await;
    let (line, col) = utf16_position_after(captured, "/*USE*/");
    let hover = send(&mut service, hover_request(33, uri, line, col + 1)).await;
    assert!(
        hover_markdown_value(&hover).contains("Right $value"),
        "reopen kept old scope: {hover}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn destructured_locals_complete_without_nested_callable_variables() {
    let mut service = service().await;
    let uri = "file:///test/destructure-completion.php";
    let source =
        "<?php function outer($items) {[$visible]=$items; $fn=function() {$visibleOnlyInside=1;}; echo $vi;}";
    send(&mut service, did_open_notification(uri, source)).await;
    let (line, col) = utf16_position_after(source, "echo $vi");
    let completion = send(&mut service, completion_request(30, uri, line, col)).await;
    let items = completion
        .as_array()
        .or_else(|| completion["items"].as_array())
        .unwrap();
    assert!(
        items.iter().any(|item| item["label"] == "$visible"),
        "destructuring absent: {completion}"
    );
    assert!(
        !items
            .iter()
            .any(|item| item["label"] == "$visibleOnlyInside"),
        "nested local leaked: {completion}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn reference_capture_keeps_definition_and_rename_without_a_stale_type() {
    let mut service = service().await;
    let uri = "file:///test/reference-capture.php";
    let source="<?php class Wrong {} class Right {} function outer() {/*DEF*/$value=new Wrong; $fn=function() use(&$value) {echo /*USE*/$value;}; $value=new Right; $fn();}";
    send(&mut service, did_open_notification(uri, source)).await;
    assert_definition(&mut service, uri, source, true).await;
    let (line, col) = utf16_position_after(source, "/*USE*/");
    let hover = send(&mut service, hover_request(30, uri, line, col + 1)).await;
    assert!(
        !hover_markdown_value(&hover).contains("Wrong $value"),
        "reference capture frozen: {hover}"
    );
    let rename = send(
        &mut service,
        rename_request(31, uri, line, col + 1, "$renamed"),
    )
    .await;
    assert_eq!(
        rename["changes"][uri].as_array().unwrap().len(),
        4,
        "alias disconnected: {rename}"
    );
    send(&mut service, shutdown_request(99)).await;
}
