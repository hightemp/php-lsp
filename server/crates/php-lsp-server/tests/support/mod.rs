//! Shared JSON-RPC harness for php-lsp end-to-end tests.

#![allow(dead_code, unused_imports)]

pub use futures::StreamExt;
pub use serde_json::json;
pub use std::fs;
pub use std::time::Duration;
pub use tokio::sync::mpsc::UnboundedReceiver;
pub use tower::{Service, ServiceExt};
pub use tower_lsp::jsonrpc::{ErrorCode, Request};
pub use tower_lsp::LspService;

pub use php_lsp_server::PhpLspBackend;

pub fn initialize_request(id: i64) -> Request {
    Request::build("initialize")
        .params(json!({
            "capabilities": {},
            "rootUri": null
        }))
        .id(id)
        .finish()
}

pub fn initialize_request_with_options(
    id: i64,
    root_uri: Option<&str>,
    initialization_options: Option<serde_json::Value>,
) -> Request {
    let mut params = json!({
        "capabilities": {},
        "rootUri": root_uri
    });
    if let Some(opts) = initialization_options {
        params["initializationOptions"] = opts;
    }
    Request::build("initialize").params(params).id(id).finish()
}

pub fn initialize_request_with_workspace_folders(id: i64, folders: Vec<(&str, &str)>) -> Request {
    let workspace_folders: Vec<_> = folders
        .into_iter()
        .map(|(name, uri)| {
            json!({
                "name": name,
                "uri": uri
            })
        })
        .collect();

    Request::build("initialize")
        .params(json!({
            "capabilities": {
                "workspace": {
                    "workspaceFolders": true
                }
            },
            "rootUri": null,
            "workspaceFolders": workspace_folders
        }))
        .id(id)
        .finish()
}

pub fn initialized_notification() -> Request {
    Request::build("initialized").params(json!({})).finish()
}

pub fn cancel_request(id: i64) -> Request {
    Request::build("$/cancelRequest")
        .params(json!({ "id": id }))
        .finish()
}

pub async fn next_publish_diagnostics(
    notifications: &mut UnboundedReceiver<Request>,
    uri: &str,
    timeout: Duration,
) -> serde_json::Value {
    let started = std::time::Instant::now();
    loop {
        let remaining = timeout
            .checked_sub(started.elapsed())
            .expect("timed out waiting for publishDiagnostics");
        let notification = tokio::time::timeout(remaining, notifications.recv())
            .await
            .expect("timed out waiting for publishDiagnostics")
            .expect("notification channel closed");
        if notification.method() != "textDocument/publishDiagnostics" {
            continue;
        }

        let params = notification
            .params()
            .cloned()
            .expect("publishDiagnostics params");
        if params.get("uri").and_then(|value| value.as_str()) == Some(uri) {
            return params;
        }
    }
}

pub async fn expect_no_publish_diagnostics(
    notifications: &mut UnboundedReceiver<Request>,
    uri: &str,
    timeout: Duration,
) {
    let started = std::time::Instant::now();
    while let Some(remaining) = timeout.checked_sub(started.elapsed()) {
        match tokio::time::timeout(remaining, notifications.recv()).await {
            Ok(Some(notification))
                if notification.method() == "textDocument/publishDiagnostics"
                    && notification
                        .params()
                        .and_then(|params| params.get("uri"))
                        .and_then(|value| value.as_str())
                        == Some(uri) =>
            {
                panic!(
                    "unexpected publishDiagnostics for {}: {:?}",
                    uri,
                    notification.params()
                );
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
}

pub fn published_diagnostic_messages(params: &serde_json::Value) -> Vec<String> {
    params
        .get("diagnostics")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|diagnostic| diagnostic.get("message").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect()
}

pub async fn wait_for_indexing_phase(
    notifications: &mut UnboundedReceiver<Request>,
    phase: &str,
    timeout: Duration,
) {
    let started = std::time::Instant::now();
    loop {
        let remaining = timeout
            .checked_sub(started.elapsed())
            .unwrap_or_else(|| panic!("timed out waiting for indexing phase `{phase}`"));
        let notification = tokio::time::timeout(remaining, notifications.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for indexing phase `{phase}`"))
            .expect("notification channel closed");
        if notification.method() != "phpLsp/indexingStatus" {
            continue;
        }

        let params = notification
            .params()
            .cloned()
            .expect("indexingStatus params");
        if params.get("phase").and_then(|value| value.as_str()) == Some(phase) {
            return;
        }
    }
}

pub fn shutdown_request(id: i64) -> Request {
    Request::build("shutdown").id(id).finish()
}

pub fn did_open_notification(uri: &str, text: &str) -> Request {
    did_open_notification_with_language(uri, "php", text)
}

pub fn did_open_notification_with_language(uri: &str, language_id: &str, text: &str) -> Request {
    Request::build("textDocument/didOpen")
        .params(json!({
            "textDocument": {
                "uri": uri,
                "languageId": language_id,
                "version": 1,
                "text": text
            }
        }))
        .finish()
}

pub fn did_close_notification(uri: &str) -> Request {
    Request::build("textDocument/didClose")
        .params(json!({
            "textDocument": {
                "uri": uri
            }
        }))
        .finish()
}

pub fn did_change_full_notification(uri: &str, version: i32, text: &str) -> Request {
    Request::build("textDocument/didChange")
        .params(json!({
            "textDocument": {
                "uri": uri,
                "version": version
            },
            "contentChanges": [
                { "text": text }
            ]
        }))
        .finish()
}

pub fn did_change_watched_files_notification(changes: Vec<(&str, i32)>) -> Request {
    let changes: Vec<_> = changes
        .into_iter()
        .map(|(uri, typ)| {
            json!({
                "uri": uri,
                "type": typ
            })
        })
        .collect();

    Request::build("workspace/didChangeWatchedFiles")
        .params(json!({ "changes": changes }))
        .finish()
}

pub fn did_change_configuration_notification(settings: serde_json::Value) -> Request {
    Request::build("workspace/didChangeConfiguration")
        .params(json!({ "settings": settings }))
        .finish()
}

pub fn did_create_files_notification(files: Vec<&str>) -> Request {
    let files: Vec<_> = files.into_iter().map(|uri| json!({ "uri": uri })).collect();
    Request::build("workspace/didCreateFiles")
        .params(json!({ "files": files }))
        .finish()
}

pub fn did_rename_files_notification(files: Vec<(&str, &str)>) -> Request {
    let files: Vec<_> = files
        .into_iter()
        .map(|(old_uri, new_uri)| {
            json!({
                "oldUri": old_uri,
                "newUri": new_uri
            })
        })
        .collect();
    Request::build("workspace/didRenameFiles")
        .params(json!({ "files": files }))
        .finish()
}

pub fn did_delete_files_notification(files: Vec<&str>) -> Request {
    let files: Vec<_> = files.into_iter().map(|uri| json!({ "uri": uri })).collect();
    Request::build("workspace/didDeleteFiles")
        .params(json!({ "files": files }))
        .finish()
}

pub fn hover_request(id: i64, uri: &str, line: u32, character: u32) -> Request {
    Request::build("textDocument/hover")
        .params(json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }))
        .id(id)
        .finish()
}

pub fn definition_request(id: i64, uri: &str, line: u32, character: u32) -> Request {
    Request::build("textDocument/definition")
        .params(json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }))
        .id(id)
        .finish()
}

pub fn declaration_request(id: i64, uri: &str, line: u32, character: u32) -> Request {
    Request::build("textDocument/declaration")
        .params(json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }))
        .id(id)
        .finish()
}

pub fn type_definition_request(id: i64, uri: &str, line: u32, character: u32) -> Request {
    Request::build("textDocument/typeDefinition")
        .params(json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }))
        .id(id)
        .finish()
}

pub fn implementation_request(id: i64, uri: &str, line: u32, character: u32) -> Request {
    Request::build("textDocument/implementation")
        .params(json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }))
        .id(id)
        .finish()
}

pub fn document_highlight_request(id: i64, uri: &str, line: u32, character: u32) -> Request {
    Request::build("textDocument/documentHighlight")
        .params(json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }))
        .id(id)
        .finish()
}

pub fn selection_range_request(id: i64, uri: &str, positions: Vec<(u32, u32)>) -> Request {
    let positions: Vec<_> = positions
        .into_iter()
        .map(|(line, character)| json!({ "line": line, "character": character }))
        .collect();
    Request::build("textDocument/selectionRange")
        .params(json!({
            "textDocument": { "uri": uri },
            "positions": positions
        }))
        .id(id)
        .finish()
}

pub fn linked_editing_range_request(id: i64, uri: &str, line: u32, character: u32) -> Request {
    Request::build("textDocument/linkedEditingRange")
        .params(json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }))
        .id(id)
        .finish()
}

pub fn completion_request(id: i64, uri: &str, line: u32, character: u32) -> Request {
    Request::build("textDocument/completion")
        .params(json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }))
        .id(id)
        .finish()
}

pub fn completion_resolve_request(id: i64, item: serde_json::Value) -> Request {
    Request::build("completionItem/resolve")
        .params(item)
        .id(id)
        .finish()
}

pub fn code_action_resolve_request(id: i64, action: serde_json::Value) -> Request {
    Request::build("codeAction/resolve")
        .params(action)
        .id(id)
        .finish()
}

pub fn signature_help_request(id: i64, uri: &str, line: u32, character: u32) -> Request {
    Request::build("textDocument/signatureHelp")
        .params(json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }))
        .id(id)
        .finish()
}

pub fn inlay_hint_request(
    id: i64,
    uri: &str,
    start_line: u32,
    start_character: u32,
    end_line: u32,
    end_character: u32,
) -> Request {
    Request::build("textDocument/inlayHint")
        .params(json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": start_line, "character": start_character },
                "end": { "line": end_line, "character": end_character }
            }
        }))
        .id(id)
        .finish()
}

pub fn folding_range_request(id: i64, uri: &str) -> Request {
    Request::build("textDocument/foldingRange")
        .params(json!({
            "textDocument": { "uri": uri }
        }))
        .id(id)
        .finish()
}

pub fn document_link_request(id: i64, uri: &str) -> Request {
    Request::build("textDocument/documentLink")
        .params(json!({
            "textDocument": { "uri": uri }
        }))
        .id(id)
        .finish()
}

pub fn prepare_call_hierarchy_request(id: i64, uri: &str, line: u32, character: u32) -> Request {
    Request::build("textDocument/prepareCallHierarchy")
        .params(json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }))
        .id(id)
        .finish()
}

pub fn incoming_calls_request(id: i64, item: serde_json::Value) -> Request {
    Request::build("callHierarchy/incomingCalls")
        .params(json!({ "item": item }))
        .id(id)
        .finish()
}

pub fn outgoing_calls_request(id: i64, item: serde_json::Value) -> Request {
    Request::build("callHierarchy/outgoingCalls")
        .params(json!({ "item": item }))
        .id(id)
        .finish()
}

pub fn prepare_type_hierarchy_request(id: i64, uri: &str, line: u32, character: u32) -> Request {
    Request::build("textDocument/prepareTypeHierarchy")
        .params(json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }))
        .id(id)
        .finish()
}

pub fn type_hierarchy_supertypes_request(id: i64, item: serde_json::Value) -> Request {
    Request::build("typeHierarchy/supertypes")
        .params(json!({ "item": item }))
        .id(id)
        .finish()
}

pub fn type_hierarchy_subtypes_request(id: i64, item: serde_json::Value) -> Request {
    Request::build("typeHierarchy/subtypes")
        .params(json!({ "item": item }))
        .id(id)
        .finish()
}

pub fn formatting_request(id: i64, uri: &str) -> Request {
    Request::build("textDocument/formatting")
        .params(json!({
            "textDocument": { "uri": uri },
            "options": {
                "tabSize": 4,
                "insertSpaces": true
            }
        }))
        .id(id)
        .finish()
}

pub fn range_formatting_request(
    id: i64,
    uri: &str,
    start_line: u32,
    start_character: u32,
    end_line: u32,
    end_character: u32,
) -> Request {
    Request::build("textDocument/rangeFormatting")
        .params(json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": start_line, "character": start_character },
                "end": { "line": end_line, "character": end_character }
            },
            "options": {
                "tabSize": 4,
                "insertSpaces": true
            }
        }))
        .id(id)
        .finish()
}

pub fn on_type_formatting_request(
    id: i64,
    uri: &str,
    line: u32,
    character: u32,
    ch: &str,
) -> Request {
    Request::build("textDocument/onTypeFormatting")
        .params(json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
            "ch": ch,
            "options": {
                "tabSize": 4,
                "insertSpaces": true
            }
        }))
        .id(id)
        .finish()
}

pub fn code_action_request(
    id: i64,
    uri: &str,
    start_line: u32,
    start_character: u32,
    end_line: u32,
    end_character: u32,
    diagnostics: serde_json::Value,
) -> Request {
    code_action_request_with_only(
        id,
        uri,
        ((start_line, start_character), (end_line, end_character)),
        diagnostics,
        vec!["quickfix"],
    )
}

pub fn code_action_request_with_only(
    id: i64,
    uri: &str,
    range: ((u32, u32), (u32, u32)),
    diagnostics: serde_json::Value,
    only: Vec<&str>,
) -> Request {
    Request::build("textDocument/codeAction")
        .params(json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": range.0.0, "character": range.0.1 },
                "end": { "line": range.1.0, "character": range.1.1 }
            },
            "context": {
                "diagnostics": diagnostics,
                "only": only
            }
        }))
        .id(id)
        .finish()
}

pub fn organize_imports_request(id: i64, uri: &str) -> Request {
    code_action_request_with_only(
        id,
        uri,
        ((0, 0), (0, 0)),
        json!([]),
        vec!["source.organizeImports"],
    )
}

pub fn add_return_type_request(id: i64, uri: &str, range: ((u32, u32), (u32, u32))) -> Request {
    code_action_request_with_only(id, uri, range, json!([]), vec!["refactor.rewrite"])
}

pub fn document_symbol_request(id: i64, uri: &str) -> Request {
    Request::build("textDocument/documentSymbol")
        .params(json!({
            "textDocument": { "uri": uri }
        }))
        .id(id)
        .finish()
}

pub fn code_lens_request(id: i64, uri: &str) -> Request {
    Request::build("textDocument/codeLens")
        .params(json!({
            "textDocument": { "uri": uri }
        }))
        .id(id)
        .finish()
}

pub fn workspace_symbol_request(id: i64, query: &str) -> Request {
    Request::build("workspace/symbol")
        .params(json!({ "query": query }))
        .id(id)
        .finish()
}

pub fn semantic_tokens_full_request(id: i64, uri: &str) -> Request {
    Request::build("textDocument/semanticTokens/full")
        .params(json!({
            "textDocument": { "uri": uri }
        }))
        .id(id)
        .finish()
}

pub fn semantic_tokens_full_delta_request(id: i64, uri: &str, previous_result_id: &str) -> Request {
    Request::build("textDocument/semanticTokens/full/delta")
        .params(json!({
            "textDocument": { "uri": uri },
            "previousResultId": previous_result_id
        }))
        .id(id)
        .finish()
}

pub fn semantic_tokens_range_request(
    id: i64,
    uri: &str,
    start_line: u32,
    start_character: u32,
    end_line: u32,
    end_character: u32,
) -> Request {
    Request::build("textDocument/semanticTokens/range")
        .params(json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": start_line, "character": start_character },
                "end": { "line": end_line, "character": end_character }
            }
        }))
        .id(id)
        .finish()
}

pub fn rename_request(id: i64, uri: &str, line: u32, character: u32, new_name: &str) -> Request {
    Request::build("textDocument/rename")
        .params(json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
            "newName": new_name
        }))
        .id(id)
        .finish()
}

pub fn prepare_rename_request(id: i64, uri: &str, line: u32, character: u32) -> Request {
    Request::build("textDocument/prepareRename")
        .params(json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }))
        .id(id)
        .finish()
}

pub fn references_request(
    id: i64,
    uri: &str,
    line: u32,
    character: u32,
    include_declaration: bool,
) -> Request {
    Request::build("textDocument/references")
        .params(json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
            "context": { "includeDeclaration": include_declaration }
        }))
        .id(id)
        .finish()
}

pub fn lsp_cases_fixture_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../test-fixtures/lsp-cases")
        .canonicalize()
        .expect("test-fixtures/lsp-cases must exist")
}

pub fn vendor_resolve_fixture_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../test-fixtures/vendor-resolve")
        .canonicalize()
        .expect("test-fixtures/vendor-resolve must exist")
}

/// Helper to extract the "result" field from a JSON-RPC response.
pub fn extract_result(response: Option<tower_lsp::jsonrpc::Response>) -> serde_json::Value {
    let resp = response.expect("expected a response");
    // Response has .result() and .error() methods
    // We'll serialize and parse to get the result
    let serialized = serde_json::to_value(&resp).unwrap();
    serialized.get("result").cloned().unwrap_or(json!(null))
}

pub fn inlay_hint_label_text(hint: &serde_json::Value) -> Option<String> {
    let label = hint.get("label")?;
    if let Some(label) = label.as_str() {
        return Some(label.to_string());
    }
    label.as_array().map(|parts| {
        parts
            .iter()
            .filter_map(|part| part.get("value").and_then(|value| value.as_str()))
            .collect::<String>()
    })
}

pub fn inlay_hint_has_label_part_location(hint: &serde_json::Value, value: &str) -> bool {
    hint.get("label")
        .and_then(|label| label.as_array())
        .is_some_and(|parts| {
            parts.iter().any(|part| {
                part.get("value").and_then(|value| value.as_str()) == Some(value)
                    && part.get("location").is_some()
            })
        })
}

/// Helper to extract the "error.message" field from a JSON-RPC response.
pub fn extract_error_message(response: Option<tower_lsp::jsonrpc::Response>) -> Option<String> {
    let resp = response?;
    let serialized = serde_json::to_value(&resp).ok()?;
    serialized
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
}

pub fn extract_error_code(response: Option<tower_lsp::jsonrpc::Response>) -> Option<i64> {
    response?.error().map(|error| error.code.code())
}

pub fn hover_markdown_value(result: &serde_json::Value) -> String {
    result
        .get("contents")
        .and_then(|contents| contents.get("value"))
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string()
}

pub fn documentation_markdown_value(result: &serde_json::Value) -> String {
    result
        .get("documentation")
        .and_then(|documentation| documentation.get("value"))
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string()
}

pub fn selection_range_chain(value: &serde_json::Value) -> Vec<(u64, u64, u64, u64)> {
    let mut ranges = Vec::new();
    let mut current = Some(value);

    while let Some(selection_range) = current {
        if let Some(range) = selection_range.get("range") {
            let start = &range["start"];
            let end = &range["end"];
            ranges.push((
                start["line"].as_u64().unwrap_or(u64::MAX),
                start["character"].as_u64().unwrap_or(u64::MAX),
                end["line"].as_u64().unwrap_or(u64::MAX),
                end["character"].as_u64().unwrap_or(u64::MAX),
            ));
        }
        current = selection_range.get("parent");
    }

    ranges
}

pub fn completion_items_from_result(result: &serde_json::Value) -> Vec<serde_json::Value> {
    if let Some(items) = result.as_array() {
        return items.clone();
    }

    result
        .get("items")
        .and_then(|items| items.as_array())
        .cloned()
        .unwrap_or_default()
}

pub fn semantic_token_data(result: &serde_json::Value) -> Vec<u64> {
    result
        .get("data")
        .and_then(|value| value.as_array())
        .expect("semantic token data array")
        .iter()
        .map(|value| value.as_u64().expect("semantic token integer"))
        .collect()
}

pub fn workspace_symbol_names(result: &serde_json::Value) -> Vec<String> {
    result
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("name").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect()
}

pub fn workspace_symbol_uris(result: &serde_json::Value) -> Vec<String> {
    result
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            item.get("location")
                .and_then(|location| location.get("uri"))
                .and_then(|value| value.as_str())
        })
        .map(str::to_string)
        .collect()
}

pub fn decode_semantic_tokens(result: &serde_json::Value) -> Vec<(u64, u64, u64, u64, u64)> {
    let data = semantic_token_data(result);
    assert_eq!(
        data.len() % 5,
        0,
        "semantic token data should be grouped by five integers"
    );

    let mut line = 0u64;
    let mut start = 0u64;
    let mut tokens = Vec::new();
    for chunk in data.chunks(5) {
        let delta_line = chunk[0];
        let delta_start = chunk[1];
        line += delta_line;
        if delta_line == 0 {
            start += delta_start;
        } else {
            start = delta_start;
        }

        tokens.push((line, start, chunk[2], chunk[3], chunk[4]));
    }

    tokens
}

pub fn apply_semantic_token_delta(
    mut data: Vec<u64>,
    delta_result: &serde_json::Value,
) -> Vec<u64> {
    let edits = delta_result
        .get("edits")
        .and_then(|value| value.as_array())
        .expect("semantic token delta edits array");

    for edit in edits {
        let start = edit
            .get("start")
            .and_then(|value| value.as_u64())
            .expect("edit start") as usize;
        let delete_count = edit
            .get("deleteCount")
            .and_then(|value| value.as_u64())
            .expect("edit deleteCount") as usize;
        let inserted: Vec<u64> = edit
            .get("data")
            .and_then(|value| value.as_array())
            .map(|values| {
                values
                    .iter()
                    .map(|value| value.as_u64().expect("semantic token edit integer"))
                    .collect()
            })
            .unwrap_or_default();

        data.splice(start..start + delete_count, inserted);
    }

    data
}
