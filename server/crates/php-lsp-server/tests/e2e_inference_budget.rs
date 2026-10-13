mod support;

use php_lsp_types::uri::path_to_uri;
use support::*;

struct Fixture(std::path::PathBuf);

impl Fixture {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!(
            "php-lsp-inference-budget-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        Self(directory)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

async fn send(service: &mut LspService<PhpLspBackend>, request: Request) -> serde_json::Value {
    let response = service.ready().await.unwrap().call(request).await.unwrap();
    response
        .map(|response| {
            assert!(response.error().is_none(), "{response:?}");
            extract_result(Some(response))
        })
        .unwrap_or(serde_json::Value::Null)
}

#[tokio::test(flavor = "current_thread")]
async fn indexing_unknown_member_chains_reaches_ready_and_preserves_foreach_targets() {
    let tmp = Fixture::new();
    let mut unknown = String::from("<?php function unresolved($node){");
    for index in 0..12 {
        let previous = if index == 0 {
            "$node".to_string()
        } else {
            format!("$node{}", index - 1)
        };
        unknown.push_str(&format!("$node{index}={previous}->child;"));
    }
    unknown.push_str("$node11->run();}");
    fs::write(tmp.path().join("Unknown.php"), unknown).unwrap();
    let source = "<?php\nclass Item {function /*TARGET*/run(){}}\nclass Container {/** @var Item[] */ public array $children; function run(){}}\nfunction walk(Container $node){foreach(/*ITER*/$node->children as /*BIND*/$node){$node->/*CALL*/run();}}";
    let file = tmp.path().join("Known.php");
    fs::write(&file, source).unwrap();
    let uri = path_to_uri(&file).unwrap();
    let root_uri = path_to_uri(tmp.path()).unwrap();
    // Repeat with the same root: both cold parsing and persisted references must
    // complete, keeping the exact Item declaration rather than Container::run.
    for attempt in 0..2 {
        let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
        let (tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(notification) = socket.next().await {
                let _ = tx.send(notification);
            }
        });
        send(
            &mut service,
            initialize_request_with_options(
                1,
                Some(&root_uri),
                Some(json!({
                    "stubExtensions":[], "indexVendor":false, "diagnosticsMode":"off"
                })),
            ),
        )
        .await;
        send(&mut service, initialized_notification()).await;
        let ready =
            next_indexing_status_for_phase(&mut notifications, "ready", Duration::from_secs(10))
                .await;
        assert_eq!(ready["indexedFiles"], 2, "{ready}");
        if attempt == 1 {
            assert_eq!(
                ready["cacheFilesLoaded"], 2,
                "second run must replay disk references: {ready}"
            );
        }
        // Query from another open document while Known.php remains closed, so
        // its receiver references must come from cold indexing / cache replay.
        let probe_uri = path_to_uri(&tmp.path().join("Probe.php")).unwrap();
        let probe = "<?php function probe(Item $item){$item->run();}";
        send(&mut service, did_open_notification(&probe_uri, probe)).await;
        let (line, column) = utf16_position_after(probe, "$item->");
        let refs = send(
            &mut service,
            references_request(3, &probe_uri, line, column + 1, false),
        )
        .await;
        let (line, column) = utf16_position_after(source, "/*CALL*/");
        assert!(
            refs.as_array()
                .is_some_and(|items| items.iter().any(|item| item["uri"] == uri
                    && item["range"]["start"] == json!({"line":line,"character":column}))),
            "closed-file receiver missing: {refs}"
        );
        send(&mut service, did_close_notification(&probe_uri)).await;
        send(&mut service, did_open_notification(&uri, source)).await;
        let (line, column) = utf16_position_after(source, "/*CALL*/");
        let result = send(&mut service, definition_request(2, &uri, line, column + 1)).await;
        let location = result
            .as_array()
            .and_then(|items| items.first())
            .unwrap_or(&result);
        let (line, column) = utf16_position_after(source, "/*TARGET*/");
        assert_eq!(location["uri"], uri, "{result}");
        assert_eq!(
            location["range"]["start"],
            json!({"line":line,"character":column}),
            "{result}"
        );
        send(&mut service, did_close_notification(&uri)).await;
        send(&mut service, shutdown_request(99)).await;
    }
}
