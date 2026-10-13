mod support;
use php_lsp_types::uri::path_to_uri;
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
async fn check(
    service: &mut LspService<PhpLspBackend>,
    uri: &str,
    source: &str,
    variable: &str,
    expected: Option<&str>,
    target: Option<&str>,
) {
    let (line, column) = utf16_position_after(source, "/*USE*/");
    let hover = send(service, hover_request(20, uri, line, column + 1)).await;
    let markdown = hover_markdown_value(&hover);
    if let Some(expected) = expected {
        assert!(
            markdown.contains(&format!("{expected} {variable}")),
            "wrong hover: {hover}"
        );
    } else {
        assert!(
            !["Item", "Other", "Known", "Holder"]
                .iter()
                .any(|class| markdown.contains(class)),
            "fabricated type: {hover}"
        );
    }
    if source.contains("/*CALL*/") {
        let (line, column) = utf16_position_after(source, "/*CALL*/");
        let result = send(service, definition_request(21, uri, line, column + 1)).await;
        if let Some(target) = target {
            let location = result
                .as_array()
                .and_then(|items| items.first())
                .unwrap_or(&result);
            let (line, column) = utf16_position_after(source, target);
            assert_eq!(location["uri"], uri, "{result}");
            assert_eq!(
                location["range"]["start"],
                json!({"line":line,"character":column}),
                "wrong declaration: {result}"
            );
        } else {
            assert!(
                result.is_null() || result.as_array().is_some_and(Vec::is_empty),
                "foreign definition: {result}"
            );
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn foreach_body_writes_update_hover_definition_and_references() {
    let mut service = service().await;
    let uri = "file:///test/foreach-write.php";
    let source="<?php class Item{function /*ITEM*/run(){}} class Other{function /*OTHER*/run(){}} /** @param Item[] $rows */ function walk(array $rows){foreach($rows as $value){$value=new Other;/*USE*/$value->/*CALL*/run();}}";
    send(&mut service, did_open_notification(uri, source)).await;
    check(
        &mut service,
        uri,
        source,
        "$value",
        Some("Other"),
        Some("/*OTHER*/"),
    )
    .await;
    let (line, column) = utf16_position_after(source, "/*OTHER*/");
    let refs = send(
        &mut service,
        references_request(22, uri, line, column + 1, false),
    )
    .await;
    let (call_line, call_column) = utf16_position_after(source, "/*CALL*/");
    assert!(
        refs.as_array().unwrap().iter().any(|r| r["uri"] == uri
            && r["range"]["start"] == json!({"line":call_line,"character":call_column})),
        "missing resolved call: {refs}"
    );
    let (line, column) = utf16_position_after(source, "/*ITEM*/");
    let wrong = send(
        &mut service,
        references_request(23, uri, line, column + 1, false),
    )
    .await;
    assert!(
        wrong.is_null() || wrong.as_array().is_some_and(Vec::is_empty),
        "foreign references: {wrong}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn foreach_types_respect_nested_parameters_and_uncaptured_closures() {
    let mut service = service().await;
    let uri = "file:///test/foreach-scope.php";
    for (body, expected, target) in [
        (
            "function separate(Other $value){/*USE*/$value->/*CALL*/run();}",
            Some("Other"),
            Some("/*OTHER*/"),
        ),
        (
            "$fn=function(Other $value){/*USE*/$value->/*CALL*/run();};",
            Some("Other"),
            Some("/*OTHER*/"),
        ),
        (
            "$fn=fn(Other $value)=>/*USE*/$value->/*CALL*/run();",
            Some("Other"),
            Some("/*OTHER*/"),
        ),
        ("$fn=function(){/*USE*/$value->/*CALL*/run();};", None, None),
        (
            "$fn=function()use($value){/*USE*/$value->/*CALL*/run();};",
            Some("Item"),
            Some("/*ITEM*/"),
        ),
        (
            "$fn=fn()=>/*USE*/$value->/*CALL*/run();",
            Some("Item"),
            Some("/*ITEM*/"),
        ),
    ] {
        let source=format!("<?php class Item{{function /*ITEM*/run(){{}}}} class Other{{function /*OTHER*/run(){{}}}} function walk(){{/** @var Item[] $rows */$rows=[];foreach($rows as $value){{{body}}}}}");
        send(&mut service, did_open_notification(uri, &source)).await;
        check(&mut service, uri, &source, "$value", expected, target).await;
        send(&mut service, did_close_notification(uri)).await;
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn foreach_property_and_offset_targets_preserve_owner_type() {
    let mut service = service().await;
    let uri = "file:///test/foreach-owner.php";
    for target in ["$holder->child", "$holder[0]"] {
        let source=format!("<?php class Item{{function /*ITEM*/run(){{}}}} class Holder extends ArrayObject{{public $child;function /*HOLDER*/run(){{}}}} /** @param Item[] $rows */ function walk(array $rows){{$holder=new Holder;foreach($rows as {target}){{/*USE*/$holder->/*CALL*/run();}}}}");
        send(&mut service, did_open_notification(uri, &source)).await;
        check(
            &mut service,
            uri,
            &source,
            "$holder",
            Some("Holder"),
            Some("/*HOLDER*/"),
        )
        .await;
        send(&mut service, did_close_notification(uri)).await;
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn later_scalar_and_unknown_writes_never_reuse_an_old_object() {
    let mut service = service().await;
    let uri = "file:///test/stale-type.php";
    for write in [
        "$value=42;",
        "$value=$unknown;",
        "$value=&$unknown;",
        "unset($value);",
        "/** @var Known $value */$value=$unknown;$value=$unknown;",
    ] {
        let source=format!("<?php class Known{{function run(){{}}}} function walk($unknown){{$value=new Known;{write}/*USE*/$value->/*CALL*/run();}}");
        send(&mut service, did_open_notification(uri, &source)).await;
        check(&mut service, uri, &source, "$value", None, None).await;
        send(&mut service, did_close_notification(uri)).await;
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn current_write_phpdoc_remains_available_to_server_inference() {
    let mut service = service().await;
    let uri = "file:///test/current-write-doc.php";
    let source="<?php class Known{function /*KNOWN*/run(){}} function walk($unknown){/** @var Known $value */$value=$unknown;/*USE*/$value->/*CALL*/run();}";
    send(&mut service, did_open_notification(uri, source)).await;
    check(
        &mut service,
        uri,
        source,
        "$value",
        Some("Known"),
        Some("/*KNOWN*/"),
    )
    .await;
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn foreach_destructuring_resolves_each_field_independently() {
    let mut service = service().await;
    let uri = "file:///test/foreach-pattern.php";
    for (variable, expected, target) in [
        ("$first", "Item", "/*ITEM*/"),
        ("$second", "Other", "/*OTHER*/"),
    ] {
        let source=format!("<?php class Item{{function /*ITEM*/run(){{}}}} class Other{{function /*OTHER*/run(){{}}}} /** @param array<int,array{{first:Item,second:Other}}> $rows */ function walk(array $rows){{foreach($rows as ['first'=>$first,'second'=>$second]){{/*USE*/{variable}->/*CALL*/run();}}}}");
        send(&mut service, did_open_notification(uri, &source)).await;
        check(
            &mut service,
            uri,
            &source,
            variable,
            Some(expected),
            Some(target),
        )
        .await;
        send(&mut service, did_close_notification(uri)).await;
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn foreach_pattern_header_has_a_hint_for_each_written_variable() {
    let mut service = service().await;
    let uri = "file:///test/pattern-hints.php";
    let source="<?php class Item{} class Other{} /** @param array<int,array{first:Item,second:Other}> $rows */ function walk(array $rows){foreach($rows as ['first'=>$first,'second'=>$second]){}}";
    send(&mut service, did_open_notification(uri, source)).await;
    let hints = send(
        &mut service,
        inlay_hint_request(30, uri, 0, 0, 0, source.len() as u32),
    )
    .await;
    for (variable, ty) in [("$first", "Item"), ("$second", "Other")] {
        let column = source.find(variable).unwrap() + variable.len();
        assert!(
            hints
                .as_array()
                .is_some_and(|items| items.iter().any(|hint| hint["position"]
                    == json!({"line":0,"character":column})
                    && inlay_hint_label_text(hint).as_deref() == Some(format!(": {ty}").as_str()))),
            "missing {variable}: {hints}"
        );
    }
    send(&mut service, shutdown_request(99)).await;
}

fn changing_source(assigned: &str) -> String {
    format!("<?php\r\n/* 😀 */ class Item{{function /*ITEM*/run(){{}} function onlyItem(){{}}}} class Other{{function /*OTHER*/run(){{}} function onlyOther(){{}}}} class Third{{function /*THIRD*/run(){{}} function onlyThird(){{}}}} /** @param Item[] $rows */ function walk(array $rows){{foreach($rows as $value){{$value=new {assigned};/*USE*/$value->/*CALL*/run();}}}}")
}

async fn assert_members(
    service: &mut LspService<PhpLspBackend>,
    uri: &str,
    source: &str,
    owner: &str,
) {
    let end = source.find("/*CALL*/").unwrap();
    let offset = source[..end].rfind("->").unwrap() + 2;
    let (line, column) = utf16_position_for_offset(source, offset);
    let result = send(service, completion_request(40, uri, line, column)).await;
    let items = result
        .as_array()
        .or_else(|| result["items"].as_array())
        .unwrap();
    for candidate in ["Item", "Other", "Third"] {
        assert_eq!(
            items
                .iter()
                .any(|item| item["label"] == format!("only{candidate}")),
            candidate == owner,
            "wrong completion: {result}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn edits_removing_a_foreach_write_and_reopen_update_all_features() {
    let mut service = service().await;
    let uri = "file:///test/foreach-edit.php";
    let other = changing_source("Other");
    let item = other.replace("$value=new Other;", "");
    send(&mut service, did_open_notification(uri, &other)).await;
    check(
        &mut service,
        uri,
        &other,
        "$value",
        Some("Other"),
        Some("/*OTHER*/"),
    )
    .await;
    assert_members(&mut service, uri, &other, "Other").await;
    send(&mut service, did_change_full_notification(uri, 2, &item)).await;
    check(
        &mut service,
        uri,
        &item,
        "$value",
        Some("Item"),
        Some("/*ITEM*/"),
    )
    .await;
    assert_members(&mut service, uri, &item, "Item").await;
    send(&mut service, did_close_notification(uri)).await;
    send(&mut service, did_open_notification(uri, &other)).await;
    check(
        &mut service,
        uri,
        &other,
        "$value",
        Some("Other"),
        Some("/*OTHER*/"),
    )
    .await;
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_foreach_writes_and_definitions_keep_coherent_targets() {
    let mut service = service().await;
    let uri = "file:///test/foreach-concurrent.php";
    let other = changing_source("Other");
    let third = changing_source("Third");
    send(&mut service, did_open_notification(uri, &other)).await;
    let service = std::sync::Arc::new(service);
    tokio::time::timeout(Duration::from_secs(20), async {
        for version in 2..22 {
            let source = if version % 2 == 0 { &third } else { &other };
            let change = did_change_full_notification(uri, version, source);
            let params = serde_json::from_value(change.params().unwrap().clone()).unwrap();
            let (line, column) = utf16_position_after(source, "/*CALL*/");
            let request = definition_request(50, uri, line, column + 1);
            let params_query = serde_json::from_value(request.params().unwrap().clone()).unwrap();
            let gate = std::sync::Arc::new(tokio::sync::Barrier::new(2));
            let writer = service.clone();
            let start = gate.clone();
            let write = tokio::spawn(async move {
                start.wait().await;
                writer.inner().did_change(params).await;
            });
            let reader = service.clone();
            let read = tokio::spawn(async move {
                gate.wait().await;
                reader.inner().goto_definition(params_query).await
            });
            let (written, read) = tokio::join!(write, read);
            written.unwrap();
            let result = serde_json::to_value(read.unwrap().unwrap()).unwrap();
            let location = result
                .as_array()
                .and_then(|items| items.first())
                .unwrap_or(&result);
            let valid = ["/*OTHER*/", "/*THIRD*/"].iter().any(|marker| {
                let (line, column) = utf16_position_after(source, marker);
                location["range"]["start"] == json!({"line":line,"character":column})
            });
            assert!(valid, "mixed/stale foreach target: {result}");
            let request = definition_request(51, uri, line, column + 1);
            let query = serde_json::from_value(request.params().unwrap().clone()).unwrap();
            let result =
                serde_json::to_value(service.inner().goto_definition(query).await.unwrap())
                    .unwrap();
            let location = result
                .as_array()
                .and_then(|items| items.first())
                .unwrap_or(&result);
            let (line, column) = utf16_position_after(
                source,
                if version % 2 == 0 {
                    "/*THIRD*/"
                } else {
                    "/*OTHER*/"
                },
            );
            assert_eq!(
                location["range"]["start"],
                json!({"line":line,"character":column}),
                "newest accepted version lost: {result}"
            );
        }
    })
    .await
    .expect("concurrent inference watchdog");
    service.inner().shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn close_restores_disk_foreach_receivers_before_reopen() {
    let root = std::env::temp_dir().join(format!(
        "php-lsp-foreach-close-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    let file = root.join("Subject.php");
    let uri = path_to_uri(&file).unwrap();
    let disk = changing_source("Other");
    let buffer = changing_source("Third");
    fs::write(&file, &disk).unwrap();
    let mut service = service().await;
    send(&mut service, did_open_notification(&uri, &buffer)).await;
    check(
        &mut service,
        &uri,
        &buffer,
        "$value",
        Some("Third"),
        Some("/*THIRD*/"),
    )
    .await;
    send(&mut service, did_close_notification(&uri)).await;
    let probe_uri = path_to_uri(&root.join("Probe.php")).unwrap();
    let probe = "<?php function probe(Other $value){$value->run();}";
    send(&mut service, did_open_notification(&probe_uri, probe)).await;
    let (line, column) = utf16_position_after(probe, "$value->");
    let refs = send(
        &mut service,
        references_request(61, &probe_uri, line, column + 1, false),
    )
    .await;
    let (line, column) = utf16_position_after(&disk, "/*CALL*/");
    assert!(
        refs.as_array()
            .is_some_and(|items| items.iter().any(|item| item["uri"] == uri
                && item["range"]["start"] == json!({"line":line,"character":column}))),
        "closed disk receiver missing: {refs}"
    );
    send(&mut service, did_open_notification(&uri, &buffer)).await;
    check(
        &mut service,
        &uri,
        &buffer,
        "$value",
        Some("Third"),
        Some("/*THIRD*/"),
    )
    .await;
    send(&mut service, shutdown_request(99)).await;
    fs::remove_dir_all(root).unwrap();
}
