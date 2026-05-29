mod support;

use support::*;

#[tokio::test(flavor = "current_thread")]
async fn test_initialize_and_shutdown() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);

    // Spawn a task to drain server→client messages so client.log_message() etc. don't block.
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    // Send initialize
    let resp = service
        .ready()
        .await
        .unwrap()
        .call(initialize_request(1))
        .await
        .unwrap();

    let result = extract_result(resp);
    assert!(
        result.get("capabilities").is_some(),
        "expected capabilities in init result"
    );
    assert!(
        result
            .get("capabilities")
            .and_then(|c| c.get("signatureHelpProvider"))
            .is_some(),
        "expected signatureHelpProvider capability"
    );
    assert!(
        result
            .get("capabilities")
            .and_then(|c| c.get("declarationProvider"))
            .and_then(|v| v.as_bool())
            == Some(true),
        "expected declarationProvider capability"
    );
    assert!(
        result
            .get("capabilities")
            .and_then(|c| c.get("typeDefinitionProvider"))
            .and_then(|v| v.as_bool())
            == Some(true),
        "expected typeDefinitionProvider capability"
    );
    assert!(
        result
            .get("capabilities")
            .and_then(|c| c.get("implementationProvider"))
            .and_then(|v| v.as_bool())
            == Some(true),
        "expected implementationProvider capability"
    );
    assert!(
        result
            .get("capabilities")
            .and_then(|c| c.get("documentHighlightProvider"))
            .and_then(|v| v.as_bool())
            == Some(true),
        "expected documentHighlightProvider capability"
    );
    assert!(
        result
            .get("capabilities")
            .and_then(|c| c.get("selectionRangeProvider"))
            .and_then(|v| v.as_bool())
            == Some(true),
        "expected selectionRangeProvider capability"
    );
    assert!(
        result
            .get("capabilities")
            .and_then(|c| c.get("linkedEditingRangeProvider"))
            .and_then(|v| v.as_bool())
            == Some(true),
        "expected linkedEditingRangeProvider capability"
    );
    assert!(
        result
            .get("capabilities")
            .and_then(|c| c.get("callHierarchyProvider"))
            .and_then(|v| v.as_bool())
            == Some(true),
        "expected callHierarchyProvider capability"
    );
    assert!(
        result
            .get("capabilities")
            .and_then(|c| c.get("experimental"))
            .and_then(|experimental| experimental.get("typeHierarchyProvider"))
            .and_then(|v| v.as_bool())
            == Some(true),
        "expected experimental typeHierarchyProvider capability"
    );
    assert!(
        result
            .get("capabilities")
            .and_then(|c| c.get("inlayHintProvider"))
            .and_then(|v| v.as_bool())
            == Some(true),
        "expected inlayHintProvider capability"
    );
    assert_eq!(
        result
            .get("capabilities")
            .and_then(|c| c.get("codeLensProvider"))
            .and_then(|provider| provider.get("resolveProvider"))
            .and_then(|v| v.as_bool()),
        Some(false),
        "expected codeLensProvider without resolve, got: {}",
        result
    );
    assert!(
        result
            .get("capabilities")
            .and_then(|c| c.get("foldingRangeProvider"))
            .and_then(|v| v.as_bool())
            == Some(true),
        "expected foldingRangeProvider capability"
    );
    assert_eq!(
        result
            .get("capabilities")
            .and_then(|c| c.get("documentLinkProvider"))
            .and_then(|provider| provider.get("resolveProvider"))
            .and_then(|v| v.as_bool()),
        Some(false),
        "expected documentLinkProvider without resolve, got: {}",
        result
    );
    let file_operations = result
        .get("capabilities")
        .and_then(|c| c.get("workspace"))
        .and_then(|workspace| workspace.get("fileOperations"))
        .expect("expected workspace fileOperations capability");
    assert!(
        file_operations.get("didCreate").is_some()
            && file_operations.get("didRename").is_some()
            && file_operations.get("didDelete").is_some()
            && file_operations.get("willCreate").is_some()
            && file_operations.get("willDelete").is_some(),
        "expected implemented will/did file operation capabilities, got: {}",
        file_operations
    );
    assert!(
        file_operations.get("willRename").is_none(),
        "willRename should not be advertised until it returns meaningful edits, got: {}",
        file_operations
    );
    assert!(
        result
            .get("capabilities")
            .and_then(|c| c.get("codeActionProvider"))
            .is_some(),
        "expected codeActionProvider capability"
    );
    assert_eq!(
        result
            .get("capabilities")
            .and_then(|c| c.get("codeActionProvider"))
            .and_then(|provider| provider.get("resolveProvider"))
            .and_then(|v| v.as_bool()),
        Some(true),
        "expected codeActionProvider with resolve support, got: {}",
        result
    );
    assert!(
        result
            .get("capabilities")
            .and_then(|c| c.get("documentFormattingProvider"))
            .and_then(|v| v.as_bool())
            == Some(true),
        "expected documentFormattingProvider capability"
    );
    assert!(
        result
            .get("capabilities")
            .and_then(|c| c.get("documentRangeFormattingProvider"))
            .and_then(|v| v.as_bool())
            == Some(true),
        "expected documentRangeFormattingProvider capability"
    );
    let completion_triggers = result
        .get("capabilities")
        .and_then(|c| c.get("completionProvider"))
        .and_then(|provider| provider.get("triggerCharacters"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        ["[", "'", "\""].iter().all(|expected| {
            completion_triggers
                .iter()
                .any(|trigger| trigger.as_str() == Some(*expected))
        }),
        "expected shape completion triggers '[', '\\'', and '\"', got: {}",
        result
    );
    let on_type_provider = result
        .get("capabilities")
        .and_then(|c| c.get("documentOnTypeFormattingProvider"))
        .expect("expected documentOnTypeFormattingProvider capability");
    assert_eq!(
        on_type_provider
            .get("firstTriggerCharacter")
            .and_then(|v| v.as_str()),
        Some("\n"),
        "expected newline on-type trigger, got: {}",
        result
    );
    let on_type_more_triggers = on_type_provider
        .get("moreTriggerCharacter")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        on_type_more_triggers
            .iter()
            .any(|trigger| trigger.as_str() == Some(";"))
            && on_type_more_triggers
                .iter()
                .any(|trigger| trigger.as_str() == Some("}")),
        "expected ';' and '}}' on-type triggers, got: {}",
        result
    );
    let semantic_provider = result
        .get("capabilities")
        .and_then(|c| c.get("semanticTokensProvider"))
        .expect("expected semanticTokensProvider capability");
    let semantic_full = semantic_provider
        .get("full")
        .expect("expected full semantic tokens support");
    assert_eq!(
        semantic_full.get("delta").and_then(|v| v.as_bool()),
        Some(true),
        "expected full/delta semantic tokens support, got: {}",
        result
    );
    assert_eq!(
        semantic_provider.get("range").and_then(|v| v.as_bool()),
        Some(true),
        "expected semanticTokens/range support, got: {}",
        result
    );
    let semantic_token_types = semantic_provider
        .get("legend")
        .and_then(|legend| legend.get("tokenTypes"))
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    for expected in ["namespace", "class", "method", "property", "variable"] {
        assert!(
            semantic_token_types
                .iter()
                .any(|token_type| token_type.as_str() == Some(expected)),
            "expected semantic token type `{}`, got: {}",
            expected,
            result
        );
    }
    let code_action_kinds = result
        .get("capabilities")
        .and_then(|c| c.get("codeActionProvider"))
        .and_then(|p| p.get("codeActionKinds"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        code_action_kinds
            .iter()
            .any(|kind| kind.as_str() == Some("source.organizeImports")),
        "expected source.organizeImports capability, got: {}",
        result
    );
    assert!(
        code_action_kinds
            .iter()
            .any(|kind| kind.as_str() == Some("refactor.rewrite")),
        "expected refactor.rewrite capability, got: {}",
        result
    );
    assert!(
        code_action_kinds
            .iter()
            .any(|kind| kind.as_str() == Some("refactor.extract")),
        "expected refactor.extract capability, got: {}",
        result
    );
    assert!(
        code_action_kinds
            .iter()
            .any(|kind| kind.as_str() == Some("refactor.inline")),
        "expected refactor.inline capability, got: {}",
        result
    );
    assert!(
        result
            .get("serverInfo")
            .and_then(|s| s.get("name"))
            .and_then(|n| n.as_str())
            == Some("php-lsp"),
        "expected server name 'php-lsp'"
    );

    // Send initialized notification
    let resp = service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();
    assert!(
        resp.is_none(),
        "initialized is a notification, no response expected"
    );

    // Shutdown
    let resp = service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(2))
        .await
        .unwrap();
    assert!(resp.is_some(), "shutdown should return a response");
}

#[tokio::test(flavor = "current_thread")]
async fn test_project_config_controls_diagnostics_and_reloads_on_watch() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    let tmp_root =
        std::env::temp_dir().join(format!("php-lsp-project-config-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp_root);
    fs::create_dir_all(&tmp_root).unwrap();
    let config_path = tmp_root.join(".php-lsp.toml");
    fs::write(&config_path, "[diagnostics]\nmode = \"off\"\n").unwrap();

    let root_uri = format!("file://{}", tmp_root.to_string_lossy());
    let file_path = tmp_root.join("broken.php");
    let file_uri = format!("file://{}", file_path.to_string_lossy());
    let config_uri = format!("file://{}", config_path.to_string_lossy());

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(1, Some(&root_uri), None))
        .await
        .unwrap();

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(
            &file_uri,
            "<?php\nfunction broken( {\n",
        ))
        .await
        .unwrap();

    let disabled =
        next_publish_diagnostics(&mut notifications, &file_uri, Duration::from_secs(1)).await;
    assert_eq!(
        disabled["diagnostics"].as_array().map(Vec::len),
        Some(0),
        "project config should disable diagnostics"
    );

    fs::write(&config_path, "[diagnostics]\nmode = \"syntax-only\"\n").unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_change_watched_files_notification(vec![(
            &config_uri,
            2,
        )]))
        .await
        .unwrap();

    let reloaded =
        next_publish_diagnostics(&mut notifications, &file_uri, Duration::from_secs(1)).await;
    assert!(
        reloaded["diagnostics"]
            .as_array()
            .is_some_and(|diagnostics| !diagnostics.is_empty()),
        "config reload should republish syntax diagnostics, got: {}",
        reloaded
    );

    let _ = fs::remove_dir_all(&tmp_root);
}

#[tokio::test(flavor = "current_thread")]
async fn test_untrusted_project_config_does_not_execute_phpstan_command() {
    if cfg!(windows) {
        return;
    }

    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-untrusted-project-command-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp_root);
    fs::create_dir_all(&tmp_root).unwrap();
    let marker_path = tmp_root.join("phpstan-ran");
    let script_path = tmp_root.join("phpstan-command.sh");
    fs::write(
        &script_path,
        format!(
            "#!/bin/sh\nprintf ran > {}\ncat <<'JSON'\n{{\"totals\":{{\"errors\":0,\"file_errors\":0}},\"files\":{{}}}}\nJSON\n",
            marker_path.to_string_lossy()
        ),
    )
    .unwrap();
    fs::write(
        tmp_root.join(".php-lsp.toml"),
        format!(
            "[phpstan]\nenabled = true\ncommand = \"sh {} {{file}}\"\n",
            script_path.to_string_lossy()
        ),
    )
    .unwrap();

    let file_path = tmp_root.join("Subject.php");
    fs::write(&file_path, "<?php\nclass Subject {}\n").unwrap();
    let root_uri = format!("file://{}", tmp_root.to_string_lossy());
    let file_uri = format!("file://{}", file_path.to_string_lossy());

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(1, Some(&root_uri), None))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(
            &file_uri,
            "<?php\nclass Subject {}\n",
        ))
        .await
        .unwrap();

    let _ = next_publish_diagnostics(&mut notifications, &file_uri, Duration::from_secs(1)).await;
    assert!(
        !marker_path.exists(),
        "untrusted project PHPStan command should not execute"
    );

    let _ = fs::remove_dir_all(&tmp_root);
}

#[tokio::test(flavor = "current_thread")]
async fn test_trusted_project_config_can_execute_phpstan_command() {
    if cfg!(windows) {
        return;
    }

    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-trusted-project-command-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp_root);
    fs::create_dir_all(&tmp_root).unwrap();
    let marker_path = tmp_root.join("phpstan-ran");
    let script_path = tmp_root.join("phpstan-command.sh");
    fs::write(
        &script_path,
        format!(
            "#!/bin/sh\nprintf ran > {}\ncat <<'JSON'\n{{\"totals\":{{\"errors\":0,\"file_errors\":0}},\"files\":{{}}}}\nJSON\n",
            marker_path.to_string_lossy()
        ),
    )
    .unwrap();
    fs::write(
        tmp_root.join(".php-lsp.toml"),
        format!(
            "[phpstan]\nenabled = true\ncommand = \"sh {} {{file}}\"\n",
            script_path.to_string_lossy()
        ),
    )
    .unwrap();

    let file_path = tmp_root.join("Subject.php");
    fs::write(&file_path, "<?php\nclass Subject {}\n").unwrap();
    let root_uri = format!("file://{}", tmp_root.to_string_lossy());
    let file_uri = format!("file://{}", file_path.to_string_lossy());

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(
            1,
            Some(&root_uri),
            Some(json!({ "allowProjectCommands": true })),
        ))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(
            &file_uri,
            "<?php\nclass Subject {}\n",
        ))
        .await
        .unwrap();

    let _ = next_publish_diagnostics(&mut notifications, &file_uri, Duration::from_secs(1)).await;
    assert!(
        marker_path.exists(),
        "trusted project PHPStan command should execute"
    );

    let _ = fs::remove_dir_all(&tmp_root);
}

#[tokio::test(flavor = "current_thread")]
async fn test_did_change_configuration_updates_runtime_settings() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(
            1,
            None,
            Some(json!({
                "phpVersion": "7.4",
                "diagnosticsMode": "off"
            })),
        ))
        .await
        .unwrap();

    let return_type_code = r#"<?php
/**
 * @return string|null
 */
function label($value) {
    return $value;
}
"#;
    let return_type_uri = "file:///test/ConfigReturnType.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(return_type_uri, return_type_code))
        .await
        .unwrap();

    let php74_resp = service
        .ready()
        .await
        .unwrap()
        .call(add_return_type_request(
            2,
            return_type_uri,
            ((0, 0), (8, 0)),
        ))
        .await
        .unwrap();
    let php74_result = extract_result(php74_resp);
    assert!(
        php74_result
            .as_array()
            .is_some_and(|actions| actions.is_empty()),
        "PHP 7.4 should not offer union return type before config change, got: {}",
        php74_result
    );

    let vendor_code = r#"<?php
namespace Vendor;

class Bar {}
"#;
    let vendor_uri = "file:///test/ConfigVendor.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(vendor_uri, vendor_code))
        .await
        .unwrap();

    let app_code = r#"<?php
namespace App;

class Demo {
    public function run(): void {
        new Bar();
    }
}
"#;
    let app_uri = "file:///test/ConfigDiagnostics.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(app_uri, app_code))
        .await
        .unwrap();

    let diagnostics_off_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request(3, app_uri, 5, 12, 5, 15, json!([])))
        .await
        .unwrap();
    let diagnostics_off_result = extract_result(diagnostics_off_resp);
    assert!(
        diagnostics_off_result
            .as_array()
            .is_some_and(|actions| actions.is_empty()),
        "diagnostics off should suppress computed quick-fixes, got: {}",
        diagnostics_off_result
    );

    service
        .ready()
        .await
        .unwrap()
        .call(did_change_configuration_notification(json!({
            "phpLsp": {
                "phpVersion": "8.2",
                "diagnostics": {
                    "mode": "basic-semantic"
                },
                "formatting": {
                    "provider": "none",
                    "command": ""
                },
                "composer": {
                    "enabled": true
                },
                "indexVendor": true,
                "stubs": {
                    "extensions": []
                },
                "logLevel": "debug"
            }
        })))
        .await
        .unwrap();

    let php82_resp = service
        .ready()
        .await
        .unwrap()
        .call(add_return_type_request(
            4,
            return_type_uri,
            ((0, 0), (8, 0)),
        ))
        .await
        .unwrap();
    let php82_result = extract_result(php82_resp);
    let php82_actions = php82_result.as_array().expect("code actions array");
    assert!(
        php82_actions.iter().any(|action| {
            action.get("title").and_then(|value| value.as_str())
                == Some("Add return type `string|null`")
        }),
        "PHP 8.2 config should enable union return type action, got: {}",
        php82_result
    );

    let diagnostics_on_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request(5, app_uri, 5, 12, 5, 15, json!([])))
        .await
        .unwrap();
    let diagnostics_on_result = extract_result(diagnostics_on_resp);
    let diagnostics_on_actions = diagnostics_on_result
        .as_array()
        .expect("code actions array");
    assert!(
        diagnostics_on_actions.iter().any(|action| {
            action.get("title").and_then(|value| value.as_str()) == Some("Import Vendor\\Bar")
        }),
        "basic-semantic diagnostics should enable computed add-use quick-fix, got: {}",
        diagnostics_on_result
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}
