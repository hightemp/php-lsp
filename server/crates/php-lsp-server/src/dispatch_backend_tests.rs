use super::*;

async fn open(wire: &mut Wire, uri: &str, name: &str) {
    wire.send(json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":uri,"languageId":"php","version":1,"text":format!("<?php\nfunction {name}() {{}}\n")}}})).await;
}
async fn change(wire: &mut Wire, uri: &str, version: i32, name: &str) {
    wire.send(json!({"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":uri,"version":version},"contentChanges":[{"range":{"start":{"line":1,"character":9},"end":{"line":1,"character":12}},"text":name}]}})).await;
}
async fn symbols(wire: &mut Wire, uri: &str, id: i64) -> Value {
    wire.send(json!({"jsonrpc":"2.0","id":id,"method":"textDocument/documentSymbol","params":{"textDocument":{"uri":uri}}})).await;
    wire.reply(id).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_real_document_commits_do_not_wait_for_notification_postprocessing() {
    for method in ["open", "change", "save", "close"] {
        let (service, socket) = LspService::new(crate::PhpLspBackend::new);
        let mut wire = Wire::with_service(service, socket).await;
        let uri = format!("file:///dispatch-{method}.php");
        if method != "open" {
            open(&mut wire, &uri, "foo").await;
            assert_eq!(
                symbols(&mut wire, &uri, 1).await["result"][0]["name"],
                "foo"
            );
        }
        let entered = Arc::new(Notify::new());
        let resume = Arc::new(Notify::new());
        let _resume = Resume(resume.clone());
        DOCUMENT_PAUSES
            .lock()
            .unwrap()
            .insert((uri.clone(), method), (entered.clone(), resume));
        match method {
            "open"=>open(&mut wire,&uri,"foo").await,
            "change"=>change(&mut wire,&uri,2,"bar").await,
            "save"=>wire.send(json!({"jsonrpc":"2.0","method":"textDocument/didSave","params":{"textDocument":{"uri":uri}}})).await,
            "close"=>wire.send(json!({"jsonrpc":"2.0","method":"textDocument/didClose","params":{"textDocument":{"uri":uri}}})).await,
            _=>unreachable!(),
        }
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        if method == "close" {
            open(&mut wire, &uri, "baz").await;
        } else {
            change(&mut wire, &uri, 7, "baz").await;
        }
        let response = symbols(&mut wire, &uri, 10).await;
        assert_eq!(response["result"][0]["name"], "baz", "{method}: {response}");
        drop(_resume);
        wire.finish().await;
    }
}

fn pause(uri: &str, phase: &'static str) -> (Arc<Notify>, Resume) {
    let entered = Arc::new(Notify::new());
    let resume = Arc::new(Notify::new());
    DOCUMENT_PAUSES
        .lock()
        .unwrap()
        .insert((uri.to_string(), phase), (entered.clone(), resume.clone()));
    (entered, Resume(resume))
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_real_query_snapshot_cannot_publish_after_edit_or_reopen() {
    for reopen in [false, true] {
        let (service, socket) = LspService::new(crate::PhpLspBackend::new);
        let mut wire = Wire::with_service(service, socket).await;
        let uri = "file:///dispatch-stale.php";
        open(&mut wire, uri, "foo").await;
        assert_eq!(symbols(&mut wire, uri, 1).await["result"][0]["name"], "foo");
        let (entered, resume) = pause(uri, "symbols_ready");
        wire.send(json!({"jsonrpc":"2.0","id":10,"method":"textDocument/documentSymbol","params":{"textDocument":{"uri":uri}}})).await;
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .unwrap();
        if reopen {
            wire.send(json!({"jsonrpc":"2.0","method":"textDocument/didClose","params":{"textDocument":{"uri":uri}}})).await;
            open(&mut wire, uri, "bar").await;
        } else {
            change(&mut wire, uri, 7, "bar").await;
        }
        assert_eq!(
            symbols(&mut wire, uri, 20).await["result"][0]["name"],
            "bar"
        );
        drop(resume);
        assert_eq!(wire.reply(10).await["error"]["code"], -32801);
        wire.finish().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_real_cross_file_definition_waits_for_earlier_dependency_change() {
    let (service, socket) = LspService::new(crate::PhpLspBackend::new);
    let mut wire = Wire::with_service(service, socket).await;
    let caller = "file:///dispatch-caller.php";
    let dependency = "file:///dispatch-dependency.php";
    for (uri, text) in [
        (caller, "<?php\nnew NewTarget();\n"),
        (dependency, "<?php\nclass OldTarget {}\n"),
    ] {
        wire.send(json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":uri,"languageId":"php","version":1,"text":text}}})).await;
    }
    symbols(&mut wire, dependency, 1).await;
    let (entered, resume) = pause(dependency, "before_change");
    wire.send(json!({"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":dependency,"version":7},"contentChanges":[{"text":"<?php\nclass NewTarget {}\n"}]}})).await;
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .unwrap();
    wire.send(json!({"jsonrpc":"2.0","id":10,"method":"textDocument/definition","params":{"textDocument":{"uri":caller},"position":{"line":1,"character":5}}})).await;
    wire.send(json!({"jsonrpc":"2.0","id":11,"method":"test/unknown"}))
        .await;
    assert_eq!(wire.reply(11).await["error"]["code"], -32601);
    drop(resume);
    let response = wire.reply(10).await;
    let result = &response["result"];
    let location = result
        .as_array()
        .and_then(|items| items.first())
        .unwrap_or(result);
    assert_eq!(location["uri"], dependency, "{response}");
    assert_eq!(location["range"]["start"], json!({"line":1,"character":6}));
    wire.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_real_lazy_action_cannot_resolve_against_the_prechange_document() {
    let (service, socket) = LspService::new(crate::PhpLspBackend::new);
    let mut wire = Wire::with_service(service, socket).await;
    let uri = "file:///dispatch-action.php";
    wire.send(json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":uri,"languageId":"php","version":1,"text":"<?php\n/** @return int */\nfunction value() {return 1;}\n"}}})).await;
    wire.send(json!({"jsonrpc":"2.0","id":1,"method":"textDocument/codeAction","params":{"textDocument":{"uri":uri},"range":{"start":{"line":2,"character":9},"end":{"line":2,"character":14}},"context":{"diagnostics":[],"only":["refactor.rewrite"]}}})).await;
    let response = wire.reply(1).await;
    let action = response["result"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["title"] == "Add return type `int`")
        .unwrap_or_else(|| panic!("{response}"))
        .clone();
    let (entered, resume) = pause(uri, "before_change");
    wire.send(json!({"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":uri,"version":7},"contentChanges":[{"text":"<?php\n/** @return string */\nfunction value() {return 'new';}\n"}]}})).await;
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .unwrap();
    wire.send(json!({"jsonrpc":"2.0","id":10,"method":"codeAction/resolve","params":action}))
        .await;
    wire.send(json!({"jsonrpc":"2.0","id":11,"method":"test/unknown"}))
        .await;
    assert_eq!(wire.reply(11).await["error"]["code"], -32601);
    drop(resume);
    let response = wire.reply(10).await;
    assert!(response.get("error").is_none(), "{response}");
    let edit = &response["result"]["edit"];
    let edits = edit["changes"].as_object().map_or(0, |files| {
        files
            .values()
            .map(|edits| edits.as_array().map_or(0, Vec::len))
            .sum::<usize>()
    });
    assert_eq!(edits, 0, "stale action: {response}");
    assert!(
        edit["documentChanges"].as_array().is_none_or(Vec::is_empty),
        "stale action: {response}"
    );
    wire.finish().await;
}
