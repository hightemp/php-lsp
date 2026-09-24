mod support;

use php_lsp_types::uri::path_to_uri;
use support::*;

async fn diagnostics_for_version(
    notifications: &mut UnboundedReceiver<Request>,
    uri: &str,
    version: i64,
) -> serde_json::Value {
    let started = std::time::Instant::now();
    loop {
        let remaining = Duration::from_secs(3)
            .checked_sub(started.elapsed())
            .expect("timed out waiting for arrow diagnostics");
        let diagnostics = next_publish_diagnostics(notifications, uri, remaining).await;
        if diagnostics["version"] == version {
            return diagnostics;
        }
    }
}

fn variable_messages(diagnostics: &serde_json::Value) -> Vec<(String, u64)> {
    diagnostics["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let message = item["message"].as_str()?;
            (message.starts_with("Unused variable:")
                || message.starts_with("Unused parameter:")
                || message.starts_with("Undefined variable:"))
            .then_some((
                message.to_string(),
                item["range"]["start"]["line"].as_u64()?,
            ))
        })
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn arrow_shadowing_and_capture_refresh_published_diagnostics() {
    let uri = path_to_uri(&std::env::temp_dir().join("php-lsp-arrow-diagnostics/Use.php")).unwrap();
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

    let shadowed = "<?php\nfunction run(): mixed {\n    $shadowed = 1;\n    return fn($shadowed) => $shadowed + 1;\n}\n";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&uri, shadowed))
        .await
        .unwrap();
    let first = diagnostics_for_version(&mut notifications, &uri, 1).await;
    assert!(
        variable_messages(&first).contains(&("Unused variable: $shadowed".into(), 2)),
        "outer shadowed variable was not reported: {first}"
    );

    let captured = "<?php\nfunction run(): mixed {\n    $captured = 1;\n    return fn() => $captured + 1;\n}\n";
    service
        .ready()
        .await
        .unwrap()
        .call(did_change_full_notification(&uri, 2, captured))
        .await
        .unwrap();
    let second = diagnostics_for_version(&mut notifications, &uri, 2).await;
    assert!(
        variable_messages(&second).is_empty(),
        "valid implicit capture was lost: {second}"
    );

    let nested = "<?php\nfunction run(): mixed {\n    $outer = 1;\n    return fn($x) => fn($outer) => $outer + $x;\n}\n";
    service
        .ready()
        .await
        .unwrap()
        .call(did_change_full_notification(&uri, 3, nested))
        .await
        .unwrap();
    let third = diagnostics_for_version(&mut notifications, &uri, 3).await;
    assert!(
        variable_messages(&third).contains(&("Unused variable: $outer".into(), 2)),
        "nested parameter shadowing credited the outer variable: {third}"
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}
