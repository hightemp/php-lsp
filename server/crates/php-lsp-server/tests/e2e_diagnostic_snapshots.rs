mod support;
use php_lsp_parser::{diagnostics::extract_syntax_errors, parser::FileParser};
#[cfg(unix)]
use php_lsp_types::uri::path_to_uri;
use support::*;

async fn send(service: &mut LspService<PhpLspBackend>, request: Request) {
    let response = service.ready().await.unwrap().call(request).await.unwrap();
    if let Some(response) = response {
        assert!(response.error().is_none(), "{response:?}");
    }
}
async fn service(
    options: serde_json::Value,
) -> (LspService<PhpLspBackend>, UnboundedReceiver<Request>) {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = tx.send(notification);
        }
    });
    send(
        &mut service,
        initialize_request_with_options(1, None, Some(options)),
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
            let result = next_publish_diagnostics(rx, uri, Duration::from_secs(5)).await;
            if result["version"] == version {
                return result;
            }
        }
    })
    .await
    .expect("versioned diagnostics stalled")
}
fn assert_syntax_spans(result: &serde_json::Value, source: &str) {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let expected = extract_syntax_errors(parser.tree().unwrap(), source);
    assert!(!expected.is_empty());
    let actual = result["diagnostics"].as_array().unwrap();
    assert_eq!(actual.len(), expected.len(), "{result}");
    for (actual, expected) in actual.iter().zip(expected) {
        assert_eq!(
            actual["range"],
            serde_json::to_value(expected.range).unwrap(),
            "{result}"
        );
        assert_eq!(actual["code"], "syntax-error");
    }
}
#[cfg(unix)]
fn external_count(result: &serde_json::Value) -> usize {
    result["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|diagnostic| matches!(diagnostic["source"].as_str(), Some("phpstan" | "psalm")))
        .count()
}

#[tokio::test(flavor = "current_thread")]
async fn syntax_ranges_follow_utf16_buffer_versions_and_reopen() {
    let options = json!({"stubExtensions":[],"indexVendor":false,"diagnosticsMode":"syntax-only"});
    let (mut service, mut rx) = service(options).await;
    let uri = "file:///test/UnicodeSyntax.php";
    let original = "<?php /* 😀 é 中文 */ function broken( {}";
    let changed = "<?php\r\n/* 😀😀 é */ function broken( {}\r\n";
    send(&mut service, did_open_notification(uri, original)).await;
    assert_syntax_spans(&versioned(&mut rx, uri, 1).await, original);
    send(&mut service, did_change_full_notification(uri, 2, changed)).await;
    assert_syntax_spans(&versioned(&mut rx, uri, 2).await, changed);
    send(&mut service, did_change_full_notification(uri, 1, original)).await;
    send(
        &mut service,
        Request::build("textDocument/didSave")
            .params(json!({"textDocument":{"uri":uri}}))
            .finish(),
    )
    .await;
    assert_syntax_spans(&versioned(&mut rx, uri, 2).await, changed);
    send(
        &mut service,
        did_change_full_notification(uri, 3, "<?php function valid() {}"),
    )
    .await;
    assert!(versioned(&mut rx, uri, 3).await["diagnostics"]
        .as_array()
        .unwrap()
        .is_empty());
    send(&mut service, did_close_notification(uri)).await;
    send(&mut service, did_open_notification(uri, changed)).await;
    assert_syntax_spans(&versioned(&mut rx, uri, 1).await, changed);
    send(&mut service, shutdown_request(99)).await;
}

#[cfg(unix)]
struct Fixture {
    root: std::path::PathBuf,
    file: std::path::PathBuf,
}
#[cfg(unix)]
impl Fixture {
    fn new(source: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("php-lsp-p230-e2e-{}-{unique}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let file = root.join("Subject é %.php");
        fs::write(&file, source).unwrap();
        Self { root, file }
    }
    fn options(&self) -> serde_json::Value {
        let mut commands = Vec::new();
        for analyzer in ["phpstan", "psalm"] {
            let output = if analyzer == "phpstan" {
                json!({"files":{(self.file.to_string_lossy().to_string()):{"messages":[{"message":"checked disk","line":1}]}}})
            } else {
                json!([{"file_path":self.file,"message":"checked disk","line_from":1}])
            };
            let script = self.root.join(format!("{analyzer}.sh"));
            fs::write(&script, format!("cat <<'JSON'\n{output}\nJSON\n")).unwrap();
            commands.push(format!(
                "sh '{}' {{file}}",
                script.to_string_lossy().replace('\'', "'\\''")
            ));
        }
        json!({"stubExtensions":[],"indexVendor":false,"diagnosticsMode":"basic-semantic","diagnosticsSeverity":"error","phpstanEnabled":true,"phpstanCommand":commands[0],"phpstanMemoryLimit":"","psalmEnabled":true,"psalmCommand":commands[1]})
    }
}
#[cfg(unix)]
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn external_analyzers_follow_saved_bytes_and_keep_unsaved_builtin_diagnostics() {
    let disk = "<?php namespace App; new DiskClass();";
    let buffer = "<?php namespace App; new BufferClass();";
    let fixture = Fixture::new(disk);
    let uri = path_to_uri(&fixture.file).unwrap();
    let (mut service, mut rx) = service(fixture.options()).await;
    send(&mut service, did_open_notification(&uri, buffer)).await;
    let result = versioned(&mut rx, &uri, 1).await;
    assert_eq!(external_count(&result), 0, "{result}");
    assert!(
        published_diagnostic_messages(&result)
            .iter()
            .any(|message| message.contains("BufferClass")),
        "{result}"
    );
    fs::write(&fixture.file, buffer).unwrap();
    let save = || {
        Request::build("textDocument/didSave")
            .params(json!({"textDocument":{"uri":uri}}))
            .finish()
    };
    send(&mut service, save()).await;
    assert_eq!(external_count(&versioned(&mut rx, &uri, 1).await), 2);
    send(&mut service, did_change_full_notification(&uri, 2, disk)).await;
    assert_eq!(external_count(&versioned(&mut rx, &uri, 2).await), 0);
    send(&mut service, save()).await;
    assert_eq!(external_count(&versioned(&mut rx, &uri, 2).await), 0);
    fs::remove_file(&fixture.file).unwrap();
    send(&mut service, save()).await;
    assert_eq!(external_count(&versioned(&mut rx, &uri, 2).await), 0);
    fs::write(&fixture.file, disk).unwrap();
    send(&mut service, save()).await;
    assert_eq!(external_count(&versioned(&mut rx, &uri, 2).await), 2);
    send(&mut service, did_close_notification(&uri)).await;
    send(&mut service, did_open_notification(&uri, buffer)).await;
    assert_eq!(external_count(&versioned(&mut rx, &uri, 1).await), 0);
    send(&mut service, shutdown_request(99)).await;
}
