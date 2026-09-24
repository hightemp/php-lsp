mod support;

use php_lsp_types::uri::path_to_uri;
use support::*;

#[tokio::test(flavor = "current_thread")]
async fn argument_count_diagnostics_distinguish_legacy_named_and_unpacked_calls() {
    let uri = path_to_uri(&std::env::temp_dir().join("php-lsp-argument-count/Calls.php")).unwrap();
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(message) = socket.next().await {
            let _ = tx.send(message);
        }
    });
    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(
            1,
            None,
            Some(json!({
                "stubExtensions": [], "diagnosticsMode": "basic-semantic"
            })),
        ))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();

    let code = "<?php\nfunction legacy($first = 1, $second): void {}\nfunction pair($left, $right): void {}\nlegacy(1);\npair(...[1, 2]);\npair(left: 1, right: 2);\npair(left: 1, left: 2);\npair(1, 2, 3);\n";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&uri, code))
        .await
        .unwrap();
    let published =
        next_publish_diagnostics(&mut notifications, &uri, Duration::from_secs(3)).await;
    let argument_diagnostics: Vec<_> = published["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|diagnostic| diagnostic["code"] == "php-lsp.argumentCountMismatch")
        .map(|diagnostic| {
            (
                diagnostic["range"]["start"]["line"].as_u64().unwrap(),
                diagnostic["message"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert!(
        argument_diagnostics
            .iter()
            .any(|(line, message)| *line == 3 && message.contains("at least 2")),
        "missing legacy required parameter was not reported: {argument_diagnostics:?}"
    );
    assert!(
        argument_diagnostics
            .iter()
            .any(|(line, message)| *line == 6 && message.contains("left")),
        "duplicate named argument was not reported: {argument_diagnostics:?}"
    );
    assert!(
        argument_diagnostics
            .iter()
            .all(|(line, _)| *line != 4 && *line != 5 && *line != 7),
        "valid unpacked/named/extra-positional calls were flagged: {argument_diagnostics:?}"
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}
