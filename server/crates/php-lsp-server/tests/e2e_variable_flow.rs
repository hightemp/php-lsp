mod support;
use support::*;
use tower_lsp::LanguageServer;

async fn send(service: &mut LspService<PhpLspBackend>, request: Request) {
    let response = service.ready().await.unwrap().call(request).await.unwrap();
    if let Some(response) = response {
        assert!(response.error().is_none(), "{response:?}");
    }
}
async fn service() -> (LspService<PhpLspBackend>, UnboundedReceiver<Request>) {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(request) = socket.next().await {
            let _ = tx.send(request);
        }
    });
    send(
        &mut service,
        initialize_request_with_options(
            1,
            None,
            Some(
                json!({"stubExtensions":[],"indexVendor":false,"diagnosticsMode":"basic-semantic"}),
            ),
        ),
    )
    .await;
    (service, rx)
}
async fn versioned(
    rx: &mut UnboundedReceiver<Request>,
    uri: &str,
    version: i64,
) -> serde_json::Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let value = next_publish_diagnostics(rx, uri, Duration::from_secs(5)).await;
            if value["version"] == version {
                return value;
            }
        }
    })
    .await
    .expect("variable diagnostics stalled")
}
fn source(complete: bool) -> String {
    let assignment = if complete {
        "$x ??= 1;"
    } else {
        "if($flag){$x=1;}"
    };
    format!(
        "<?php\r\nfunction run(bool $flag){{ /* 😀 中文 */ {assignment} echo /*read*/$x; }}\r\n"
    )
}
fn assert_read(result: &serde_json::Value, source: &str, missing: bool) {
    let found = result["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|diagnostic| {
            diagnostic["code"] == "php-lsp.undefinedVariable"
                && diagnostic["message"] == "Undefined variable: $x"
        })
        .collect::<Vec<_>>();
    assert_eq!(found.len(), usize::from(missing), "{result}");
    if missing {
        let offset = source.find("/*read*/").unwrap() + "/*read*/".len();
        let (line, column) = utf16_position_for_offset(source, offset);
        assert_eq!(
            found[0]["range"],
            json!({"start":{"line":line,"character":column},"end":{"line":line,"character":column+2}}),
            "{result}"
        );
    }
}
fn save(uri: &str) -> Request {
    Request::build("textDocument/didSave")
        .params(json!({"textDocument":{"uri":uri}}))
        .finish()
}

#[tokio::test(flavor = "current_thread")]
async fn branch_joins_and_coalescing_assignment_follow_unsaved_versions_and_reopen() {
    let (mut service, mut rx) = service().await;
    let uri = "file:///test/VariableFlow.php";
    let broken = source(false);
    let fixed = source(true);
    send(&mut service, did_open_notification(uri, &broken)).await;
    assert_read(&versioned(&mut rx, uri, 1).await, &broken, true);
    send(&mut service, did_change_full_notification(uri, 2, &fixed)).await;
    assert_read(&versioned(&mut rx, uri, 2).await, &fixed, false);
    send(&mut service, did_change_full_notification(uri, 1, &broken)).await;
    send(&mut service, save(uri)).await;
    assert_read(&versioned(&mut rx, uri, 2).await, &fixed, false);
    send(&mut service, did_close_notification(uri)).await;
    send(&mut service, did_open_notification(uri, &broken)).await;
    assert_read(&versioned(&mut rx, uri, 1).await, &broken, true);
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn loop_catch_probe_and_capture_reads_have_exact_lsp_ranges() {
    let (mut service, mut rx) = service().await;
    let uri = "file:///test/VariableFlowCases.php";
    for body in [
        "foreach($items as $x){} echo /*read*/$x;",
        "if($flag){$x=1;}else{$f=fn()=>/*read*/$x;}",
        "try{work();}catch(Throwable $x){} echo /*read*/$x;",
        "if(empty($x)){echo /*read*/$x;}",
        "[$x]=[/*read*/$x];",
    ] {
        let text = format!("<?php\r\nfunction run(bool $flag,array $items){{ /* 😀 */ {body} }}");
        send(&mut service, did_open_notification(uri, &text)).await;
        assert_read(&versioned(&mut rx, uri, 1).await, &text, true);
        send(&mut service, did_close_notification(uri)).await;
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_edits_and_diagnostic_requests_keep_the_accepted_flow_snapshot() {
    let (mut service, mut rx) = service().await;
    let uri = "file:///test/ConcurrentVariableFlow.php";
    let broken = source(false);
    let fixed = source(true);
    send(&mut service, did_open_notification(uri, &broken)).await;
    assert_read(&versioned(&mut rx, uri, 1).await, &broken, true);
    let service = std::sync::Arc::new(service);
    tokio::time::timeout(Duration::from_secs(15), async {
        for version in 2..22 {
            let text = if version % 2 == 0 { &fixed } else { &broken };
            let change = did_change_full_notification(uri, version, text);
            let change = serde_json::from_value(change.params().unwrap().clone()).unwrap();
            let params = serde_json::from_value(save(uri).params().unwrap().clone()).unwrap();
            let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
            let writer = service.clone();
            let gate = barrier.clone();
            let edit = tokio::spawn(async move {
                gate.wait().await;
                writer.inner().did_change(change).await;
            });
            let reader = service.clone();
            let reading = tokio::spawn(async move {
                barrier.wait().await;
                reader.inner().did_save(params).await;
            });
            let (a, b) = tokio::join!(edit, reading);
            a.unwrap();
            b.unwrap();
            service
                .inner()
                .did_save(serde_json::from_value(save(uri).params().unwrap().clone()).unwrap())
                .await;
            assert_read(
                &versioned(&mut rx, uri, i64::from(version)).await,
                text,
                version % 2 != 0,
            );
        }
    })
    .await
    .expect("concurrent flow diagnostics stalled");
    service.inner().shutdown().await.unwrap();
}
