mod support;

use support::*;

#[tokio::test(flavor = "current_thread")]
async fn test_document_formatting_uses_custom_external_command() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let formatted = "<?php\nfunction ok(): void\n{\n    echo \"ok\";\n}\n";
    let formatter_command = format!("printf '%s' '{}' > {{file}}", formatted);

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(
            1,
            None,
            Some(json!({
                "formattingProvider": "custom",
                "formattingCommand": formatter_command
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

    let code = "<?php\nfunction ok(): void { echo \"ok\"; }\n";
    let uri = "file:///test/Format.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let resp = service
        .ready()
        .await
        .unwrap()
        .call(formatting_request(2, uri))
        .await
        .unwrap();
    let result = extract_result(resp);
    let edits = result.as_array().expect("formatting edits array");
    assert_eq!(edits.len(), 1, "expected one full-document edit");
    assert_eq!(
        edits[0]["newText"].as_str(),
        Some(formatted),
        "formatter edit should contain formatted source, got: {}",
        result
    );
    assert_eq!(edits[0]["range"]["start"]["line"].as_u64(), Some(0));
    assert_eq!(edits[0]["range"]["start"]["character"].as_u64(), Some(0));

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_document_formatting_auto_detects_php_cs_fixer_from_composer_metadata() {
    if cfg!(windows) {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "php-lsp-format-auto-detect-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let vendor_bin = tmp.join("vendor/bin");
    fs::create_dir_all(&vendor_bin).unwrap();
    fs::write(
        tmp.join("composer.json"),
        r#"{"require-dev":{"friendsofphp/php-cs-fixer":"^3.0"}}"#,
    )
    .unwrap();

    let formatted = "<?php\nfunction ok(): void\n{\n    echo \"auto\";\n}\n";
    let tool_path = vendor_bin.join("php-cs-fixer");
    fs::write(
        &tool_path,
        format!(
            "#!/bin/sh\nfor arg in \"$@\"; do file=\"$arg\"; done\ncat > \"$file\" <<'PHP'\n{}PHP\n",
            formatted
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&tool_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&tool_path, permissions).unwrap();
    }

    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let root_uri = format!("file://{}", tmp.display());
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
        .call(initialized_notification())
        .await
        .unwrap();

    let code = "<?php\nfunction ok(): void { echo \"auto\"; }\n";
    let uri = format!("file://{}", tmp.join("AutoFormat.php").display());
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&uri, code))
        .await
        .unwrap();

    let resp = service
        .ready()
        .await
        .unwrap()
        .call(formatting_request(2, &uri))
        .await
        .unwrap();
    let result = extract_result(resp);
    let edits = result.as_array().expect("formatting edits array");
    assert_eq!(edits.len(), 1, "expected one auto-detected edit");
    assert_eq!(
        edits[0]["newText"].as_str(),
        Some(formatted),
        "auto-detected formatter edit should contain formatted source, got: {}",
        result
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
    let _ = fs::remove_dir_all(tmp);
}

#[tokio::test(flavor = "current_thread")]
async fn test_document_range_formatting_uses_custom_external_command() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let formatted_range = "    echo \"one\";\n";
    let formatter_output = format!("<?php\n{}", formatted_range);
    let formatter_command = format!("printf '%s' '{}' > {{file}}", formatter_output);

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(
            1,
            None,
            Some(json!({
                "formattingProvider": "custom",
                "formattingCommand": formatter_command
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

    let code = "<?php\nfunction ok(): void {\necho \"one\";\necho \"two\";\n}\n";
    let uri = "file:///test/RangeFormat.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let resp = service
        .ready()
        .await
        .unwrap()
        .call(range_formatting_request(2, uri, 2, 0, 3, 0))
        .await
        .unwrap();
    let result = extract_result(resp);
    let edits = result.as_array().expect("range formatting edits array");
    assert_eq!(edits.len(), 1, "expected one range edit");
    assert_eq!(
        edits[0]["newText"].as_str(),
        Some(formatted_range),
        "range formatter edit should contain formatted selection, got: {}",
        result
    );
    assert_eq!(edits[0]["range"]["start"]["line"].as_u64(), Some(2));
    assert_eq!(edits[0]["range"]["end"]["line"].as_u64(), Some(3));

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_multi_root_formatting_uses_resource_scoped_commands() {
    if cfg!(windows) {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "php-lsp-multiroot-format-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let root_a = tmp.join("root-a");
    let root_b = tmp.join("root-b");
    fs::create_dir_all(&root_a).unwrap();
    fs::create_dir_all(&root_b).unwrap();
    let root_a_uri = php_lsp_types::uri::path_to_uri(&root_a).unwrap();
    let root_b_uri = php_lsp_types::uri::path_to_uri(&root_b).unwrap();
    let file_a_uri = php_lsp_types::uri::path_to_uri(&root_a.join("Format.php")).unwrap();
    let file_b_uri = php_lsp_types::uri::path_to_uri(&root_b.join("Format.php")).unwrap();
    let formatted_a = "<?php\necho \"root-a\";\n";
    let formatted_b = "<?php\necho \"root-b\";\n";
    let command_a = format!("printf '%s' '{}' > {{file}}", formatted_a);
    let command_b = format!("printf '%s' '{}' > {{file}}", formatted_b);

    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });
    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_workspace_folders_and_options(
            1,
            vec![("root-a", &root_a_uri), ("root-b", &root_b_uri)],
            Some(json!({
                "configurationVersion": 2,
                "global": {},
                "workspaceFolders": [
                    {
                        "uri": root_a_uri,
                        "settings": {
                            "formattingProvider": "custom",
                            "formattingCommand": command_a
                        }
                    },
                    {
                        "uri": root_b_uri,
                        "settings": {
                            "formattingProvider": "custom",
                            "formattingCommand": command_b
                        }
                    }
                ]
            })),
        ))
        .await
        .unwrap();

    let source = "<?php echo 'original';\n";
    for uri in [&file_a_uri, &file_b_uri] {
        service
            .ready()
            .await
            .unwrap()
            .call(did_open_notification(uri, source))
            .await
            .unwrap();
    }

    for (id, uri, expected) in [(2, &file_a_uri, formatted_a), (3, &file_b_uri, formatted_b)] {
        let result = extract_result(
            service
                .ready()
                .await
                .unwrap()
                .call(formatting_request(id, uri))
                .await
                .unwrap(),
        );
        assert_eq!(
            result
                .as_array()
                .and_then(|edits| edits.first())
                .and_then(|edit| edit["newText"].as_str()),
            Some(expected),
            "formatter for {uri} must use its workspace-folder command, got: {result}"
        );
    }

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
    fs::remove_dir_all(tmp).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_document_range_formatting_uses_utf16_range_after_complex_unicode_prefix() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let formatted_range = "    echo \"one\";\n";
    let formatter_output = format!("<?php\n{}", formatted_range);
    let formatter_command = format!("printf '%s' '{}' > {{file}}", formatter_output);

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(
            1,
            None,
            Some(json!({
                "formattingProvider": "custom",
                "formattingCommand": formatter_command
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

    let code =
        "<?php\nfunction ok(): void {\n/* 🇺🇸 👨‍👩‍👧‍👦 👍🏽 ❤️ é བོད */ echo \"one\";\necho \"two\";\n}\n";
    let uri = "file:///test/RangeFormatComplexUnicode.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let start = utf16_position_at(code, "echo \"one\";");
    let end = utf16_position_after(code, "echo \"one\";");
    let resp = service
        .ready()
        .await
        .unwrap()
        .call(range_formatting_request(
            2, uri, start.0, start.1, end.0, end.1,
        ))
        .await
        .unwrap();
    let result = extract_result(resp);
    let edits = result.as_array().expect("range formatting edits array");
    assert_eq!(edits.len(), 1, "expected one range edit");
    assert_eq!(
        edits[0]["newText"].as_str(),
        Some(formatted_range),
        "range formatter edit should contain formatted selection, got: {}",
        result
    );
    assert_eq!(
        edits[0]["range"]["start"]["line"].as_u64(),
        Some(start.0 as u64)
    );
    assert_eq!(
        edits[0]["range"]["start"]["character"].as_u64(),
        Some(start.1 as u64)
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_document_range_formatting_rejects_output_without_synthetic_wrapper() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let malformed_output = "    echo \"wrapper lost\";\n";
    let formatter_command = format!("printf '%s' '{}' > {{file}}", malformed_output);

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(
            1,
            None,
            Some(json!({
                "formattingProvider": "custom",
                "formattingCommand": formatter_command
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

    let code = "<?php\nfunction ok(): void {\necho \"one\";\n}\n";
    let uri = "file:///test/RangeFormatMissingWrapper.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let response = service
        .ready()
        .await
        .unwrap()
        .call(range_formatting_request(2, uri, 2, 0, 3, 0))
        .await
        .unwrap();
    let result = extract_result(response);
    let edits = result.as_array().expect("range formatting edits array");
    assert!(
        edits.is_empty(),
        "formatter output without the synthetic wrapper must be discarded: {result}"
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_on_type_formatting_returns_local_indentation_edits() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request(1))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();

    let code = "<?php\nfunction ok(): void {\necho \"one\";\n    }\n";
    let uri = "file:///test/OnTypeFormat.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let newline_resp = service
        .ready()
        .await
        .unwrap()
        .call(on_type_formatting_request(2, uri, 2, 0, "\n"))
        .await
        .unwrap();
    let newline_result = extract_result(newline_resp);
    let newline_edits = newline_result.as_array().expect("newline edits array");
    assert_eq!(newline_edits.len(), 1, "expected newline indent edit");
    assert_eq!(newline_edits[0]["range"]["start"]["line"].as_u64(), Some(2));
    assert_eq!(
        newline_edits[0]["range"]["end"]["character"].as_u64(),
        Some(0)
    );
    assert_eq!(newline_edits[0]["newText"].as_str(), Some("    "));

    let semicolon_resp = service
        .ready()
        .await
        .unwrap()
        .call(on_type_formatting_request(3, uri, 2, 11, ";"))
        .await
        .unwrap();
    let semicolon_result = extract_result(semicolon_resp);
    let semicolon_edits = semicolon_result.as_array().expect("semicolon edits array");
    assert_eq!(semicolon_edits.len(), 1, "expected semicolon indent edit");
    assert_eq!(
        semicolon_edits[0]["range"]["start"]["line"].as_u64(),
        Some(2)
    );
    assert_eq!(semicolon_edits[0]["newText"].as_str(), Some("    "));

    let brace_resp = service
        .ready()
        .await
        .unwrap()
        .call(on_type_formatting_request(4, uri, 3, 5, "}"))
        .await
        .unwrap();
    let brace_result = extract_result(brace_resp);
    let brace_edits = brace_result.as_array().expect("brace edits array");
    assert_eq!(brace_edits.len(), 1, "expected closing-brace dedent edit");
    assert_eq!(brace_edits[0]["range"]["start"]["line"].as_u64(), Some(3));
    assert_eq!(
        brace_edits[0]["range"]["end"]["character"].as_u64(),
        Some(4)
    );
    assert_eq!(brace_edits[0]["newText"].as_str(), Some(""));

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}
