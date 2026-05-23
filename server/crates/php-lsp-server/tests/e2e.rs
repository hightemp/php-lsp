//! End-to-end tests for the PHP LSP server.
//!
//! These tests exercise the full LSP protocol stack using tower-lsp's
//! in-process service, sending JSON-RPC requests and verifying responses.

use futures::StreamExt;
use serde_json::json;
use std::fs;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;
use tower::{Service, ServiceExt};
use tower_lsp::jsonrpc::{ErrorCode, Request};
use tower_lsp::LspService;

use php_lsp_server::PhpLspBackend;

fn initialize_request(id: i64) -> Request {
    Request::build("initialize")
        .params(json!({
            "capabilities": {},
            "rootUri": null
        }))
        .id(id)
        .finish()
}

fn initialize_request_with_options(
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

fn initialize_request_with_workspace_folders(id: i64, folders: Vec<(&str, &str)>) -> Request {
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

fn initialized_notification() -> Request {
    Request::build("initialized").params(json!({})).finish()
}

fn cancel_request(id: i64) -> Request {
    Request::build("$/cancelRequest")
        .params(json!({ "id": id }))
        .finish()
}

async fn next_publish_diagnostics(
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

async fn expect_no_publish_diagnostics(
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

async fn wait_for_indexing_phase(
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

fn shutdown_request(id: i64) -> Request {
    Request::build("shutdown").id(id).finish()
}

fn did_open_notification(uri: &str, text: &str) -> Request {
    Request::build("textDocument/didOpen")
        .params(json!({
            "textDocument": {
                "uri": uri,
                "languageId": "php",
                "version": 1,
                "text": text
            }
        }))
        .finish()
}

fn did_change_full_notification(uri: &str, version: i32, text: &str) -> Request {
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

fn did_change_watched_files_notification(changes: Vec<(&str, i32)>) -> Request {
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

fn did_change_configuration_notification(settings: serde_json::Value) -> Request {
    Request::build("workspace/didChangeConfiguration")
        .params(json!({ "settings": settings }))
        .finish()
}

fn did_create_files_notification(files: Vec<&str>) -> Request {
    let files: Vec<_> = files.into_iter().map(|uri| json!({ "uri": uri })).collect();
    Request::build("workspace/didCreateFiles")
        .params(json!({ "files": files }))
        .finish()
}

fn did_rename_files_notification(files: Vec<(&str, &str)>) -> Request {
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

fn did_delete_files_notification(files: Vec<&str>) -> Request {
    let files: Vec<_> = files.into_iter().map(|uri| json!({ "uri": uri })).collect();
    Request::build("workspace/didDeleteFiles")
        .params(json!({ "files": files }))
        .finish()
}

fn hover_request(id: i64, uri: &str, line: u32, character: u32) -> Request {
    Request::build("textDocument/hover")
        .params(json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }))
        .id(id)
        .finish()
}

fn definition_request(id: i64, uri: &str, line: u32, character: u32) -> Request {
    Request::build("textDocument/definition")
        .params(json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }))
        .id(id)
        .finish()
}

fn declaration_request(id: i64, uri: &str, line: u32, character: u32) -> Request {
    Request::build("textDocument/declaration")
        .params(json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }))
        .id(id)
        .finish()
}

fn type_definition_request(id: i64, uri: &str, line: u32, character: u32) -> Request {
    Request::build("textDocument/typeDefinition")
        .params(json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }))
        .id(id)
        .finish()
}

fn implementation_request(id: i64, uri: &str, line: u32, character: u32) -> Request {
    Request::build("textDocument/implementation")
        .params(json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }))
        .id(id)
        .finish()
}

fn document_highlight_request(id: i64, uri: &str, line: u32, character: u32) -> Request {
    Request::build("textDocument/documentHighlight")
        .params(json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }))
        .id(id)
        .finish()
}

fn selection_range_request(id: i64, uri: &str, positions: Vec<(u32, u32)>) -> Request {
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

fn linked_editing_range_request(id: i64, uri: &str, line: u32, character: u32) -> Request {
    Request::build("textDocument/linkedEditingRange")
        .params(json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }))
        .id(id)
        .finish()
}

fn completion_request(id: i64, uri: &str, line: u32, character: u32) -> Request {
    Request::build("textDocument/completion")
        .params(json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }))
        .id(id)
        .finish()
}

fn completion_resolve_request(id: i64, item: serde_json::Value) -> Request {
    Request::build("completionItem/resolve")
        .params(item)
        .id(id)
        .finish()
}

fn signature_help_request(id: i64, uri: &str, line: u32, character: u32) -> Request {
    Request::build("textDocument/signatureHelp")
        .params(json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }))
        .id(id)
        .finish()
}

fn inlay_hint_request(
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

fn folding_range_request(id: i64, uri: &str) -> Request {
    Request::build("textDocument/foldingRange")
        .params(json!({
            "textDocument": { "uri": uri }
        }))
        .id(id)
        .finish()
}

fn prepare_call_hierarchy_request(id: i64, uri: &str, line: u32, character: u32) -> Request {
    Request::build("textDocument/prepareCallHierarchy")
        .params(json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }))
        .id(id)
        .finish()
}

fn incoming_calls_request(id: i64, item: serde_json::Value) -> Request {
    Request::build("callHierarchy/incomingCalls")
        .params(json!({ "item": item }))
        .id(id)
        .finish()
}

fn outgoing_calls_request(id: i64, item: serde_json::Value) -> Request {
    Request::build("callHierarchy/outgoingCalls")
        .params(json!({ "item": item }))
        .id(id)
        .finish()
}

fn prepare_type_hierarchy_request(id: i64, uri: &str, line: u32, character: u32) -> Request {
    Request::build("textDocument/prepareTypeHierarchy")
        .params(json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }))
        .id(id)
        .finish()
}

fn type_hierarchy_supertypes_request(id: i64, item: serde_json::Value) -> Request {
    Request::build("typeHierarchy/supertypes")
        .params(json!({ "item": item }))
        .id(id)
        .finish()
}

fn type_hierarchy_subtypes_request(id: i64, item: serde_json::Value) -> Request {
    Request::build("typeHierarchy/subtypes")
        .params(json!({ "item": item }))
        .id(id)
        .finish()
}

fn formatting_request(id: i64, uri: &str) -> Request {
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

fn range_formatting_request(
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

fn on_type_formatting_request(id: i64, uri: &str, line: u32, character: u32, ch: &str) -> Request {
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

fn code_action_request(
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

fn code_action_request_with_only(
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

fn organize_imports_request(id: i64, uri: &str) -> Request {
    code_action_request_with_only(
        id,
        uri,
        ((0, 0), (0, 0)),
        json!([]),
        vec!["source.organizeImports"],
    )
}

fn add_return_type_request(id: i64, uri: &str, range: ((u32, u32), (u32, u32))) -> Request {
    code_action_request_with_only(id, uri, range, json!([]), vec!["refactor.rewrite"])
}

fn document_symbol_request(id: i64, uri: &str) -> Request {
    Request::build("textDocument/documentSymbol")
        .params(json!({
            "textDocument": { "uri": uri }
        }))
        .id(id)
        .finish()
}

fn code_lens_request(id: i64, uri: &str) -> Request {
    Request::build("textDocument/codeLens")
        .params(json!({
            "textDocument": { "uri": uri }
        }))
        .id(id)
        .finish()
}

fn workspace_symbol_request(id: i64, query: &str) -> Request {
    Request::build("workspace/symbol")
        .params(json!({ "query": query }))
        .id(id)
        .finish()
}

fn semantic_tokens_full_request(id: i64, uri: &str) -> Request {
    Request::build("textDocument/semanticTokens/full")
        .params(json!({
            "textDocument": { "uri": uri }
        }))
        .id(id)
        .finish()
}

fn semantic_tokens_full_delta_request(id: i64, uri: &str, previous_result_id: &str) -> Request {
    Request::build("textDocument/semanticTokens/full/delta")
        .params(json!({
            "textDocument": { "uri": uri },
            "previousResultId": previous_result_id
        }))
        .id(id)
        .finish()
}

fn semantic_tokens_range_request(
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

fn rename_request(id: i64, uri: &str, line: u32, character: u32, new_name: &str) -> Request {
    Request::build("textDocument/rename")
        .params(json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
            "newName": new_name
        }))
        .id(id)
        .finish()
}

fn prepare_rename_request(id: i64, uri: &str, line: u32, character: u32) -> Request {
    Request::build("textDocument/prepareRename")
        .params(json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }))
        .id(id)
        .finish()
}

fn references_request(
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

/// Helper to extract the "result" field from a JSON-RPC response.
fn extract_result(response: Option<tower_lsp::jsonrpc::Response>) -> serde_json::Value {
    let resp = response.expect("expected a response");
    // Response has .result() and .error() methods
    // We'll serialize and parse to get the result
    let serialized = serde_json::to_value(&resp).unwrap();
    serialized.get("result").cloned().unwrap_or(json!(null))
}

/// Helper to extract the "error.message" field from a JSON-RPC response.
fn extract_error_message(response: Option<tower_lsp::jsonrpc::Response>) -> Option<String> {
    let resp = response?;
    let serialized = serde_json::to_value(&resp).ok()?;
    serialized
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
}

fn extract_error_code(response: Option<tower_lsp::jsonrpc::Response>) -> Option<i64> {
    response?.error().map(|error| error.code.code())
}

fn hover_markdown_value(result: &serde_json::Value) -> String {
    result
        .get("contents")
        .and_then(|contents| contents.get("value"))
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string()
}

fn documentation_markdown_value(result: &serde_json::Value) -> String {
    result
        .get("documentation")
        .and_then(|documentation| documentation.get("value"))
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string()
}

fn selection_range_chain(value: &serde_json::Value) -> Vec<(u64, u64, u64, u64)> {
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

fn completion_items_from_result(result: &serde_json::Value) -> Vec<serde_json::Value> {
    if let Some(items) = result.as_array() {
        return items.clone();
    }

    result
        .get("items")
        .and_then(|items| items.as_array())
        .cloned()
        .unwrap_or_default()
}

fn semantic_token_data(result: &serde_json::Value) -> Vec<u64> {
    result
        .get("data")
        .and_then(|value| value.as_array())
        .expect("semantic token data array")
        .iter()
        .map(|value| value.as_u64().expect("semantic token integer"))
        .collect()
}

fn workspace_symbol_names(result: &serde_json::Value) -> Vec<String> {
    result
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("name").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect()
}

fn workspace_symbol_uris(result: &serde_json::Value) -> Vec<String> {
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

fn decode_semantic_tokens(result: &serde_json::Value) -> Vec<(u64, u64, u64, u64, u64)> {
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

fn apply_semantic_token_delta(mut data: Vec<u64>, delta_result: &serde_json::Value) -> Vec<u64> {
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
async fn test_open_file_and_hover() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    // Initialize
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

    // Open a PHP file with a class
    let code = r#"<?php
namespace App;

class Greeter {
    /** Say hello to someone. */
    public function greet(string $name): string {
        return "Hello, $name!";
    }
}

$g = new Greeter();
$g->greet("World");
"#;
    let uri = "file:///test/Greeter.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    // Hover over "Greeter" in "new Greeter()"
    let resp = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(2, uri, 10, 12))
        .await
        .unwrap();

    let result = extract_result(resp);
    // Result should contain hover content with "class" and "Greeter"
    if !result.is_null() {
        let contents = result
            .get("contents")
            .and_then(|c| c.get("value"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            contents.contains("class") || contents.contains("Greeter"),
            "hover should mention class or Greeter, got: {}",
            contents
        );
    }

    // Shutdown
    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_hover_local_variable_with_inline_phpdoc_var() {
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

    let code = r#"<?php
namespace App;

class Baz {
    public function test(): void {}
}

function makeBaz() {}

function run(): void {
    /**
     * Local baz variable.
     * @var Baz $baz2
     */
    $baz2 = makeBaz();
    $baz2->test();
}
"#;
    let uri = "file:///test/hover-var-phpdoc.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    // Hover on "$baz2" in "$baz2->test();"
    let resp = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(2, uri, 15, 6))
        .await
        .unwrap();

    let result = extract_result(resp);
    assert!(
        !result.is_null(),
        "hover should return content for local variable"
    );

    let contents = result
        .get("contents")
        .and_then(|c| c.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        contents.contains("$baz2") && contents.contains("Baz"),
        "hover should include variable name and inferred type, got: {}",
        contents
    );
    assert!(
        contents.contains("Local baz variable.") || contents.contains("@var"),
        "hover should include PHPDoc context, got: {}",
        contents
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
async fn test_goto_definition() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    // Initialize
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

    let code = r#"<?php
namespace App;

class Foo {
    public function bar(): void {}
}

$f = new Foo();
$f->bar();
"#;
    let uri = "file:///test/Foo.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    // Go to definition on "Foo" in "new Foo()"
    let resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(2, uri, 7, 12))
        .await
        .unwrap();

    let result = extract_result(resp);
    // Should return a location pointing to the class definition
    if !result.is_null() {
        let target_uri = result.get("uri").and_then(|u| u.as_str()).unwrap_or("");
        assert_eq!(target_uri, uri, "definition should point to the same file");
    }

    // Shutdown
    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_goto_definition_parent_scope_and_method() {
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

    let base_code = r#"<?php
namespace App;

class Base {
    public function run(): void {}
}
"#;
    let child_code = r#"<?php
namespace App;

class Child extends Base {
    public function test(): void {
        parent::run();
    }
}
"#;
    let base_uri = "file:///test/Base.php";
    let child_uri = "file:///test/ParentDefinition.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(base_uri, base_code))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(child_uri, child_code))
        .await
        .unwrap();

    let parent_resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(2, child_uri, 5, 8))
        .await
        .unwrap();
    let parent_result = extract_result(parent_resp);

    assert_eq!(
        parent_result.get("uri").and_then(|u| u.as_str()),
        Some(base_uri),
        "definition on parent scope should point to Base class, got: {}",
        parent_result
    );
    assert_eq!(
        parent_result["range"]["start"]["line"].as_u64(),
        Some(3),
        "definition on parent scope should point to Base class line, got: {}",
        parent_result
    );
    assert_eq!(
        parent_result["range"]["start"]["character"].as_u64(),
        Some(6),
        "definition on parent scope should point to Base class name, got: {}",
        parent_result
    );

    let method_resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(3, child_uri, 5, 16))
        .await
        .unwrap();
    let method_result = extract_result(method_resp);

    assert_eq!(
        method_result.get("uri").and_then(|u| u.as_str()),
        Some(base_uri),
        "definition on parent method should point to Base::run, got: {}",
        method_result
    );
    assert_eq!(
        method_result["range"]["start"]["line"].as_u64(),
        Some(4),
        "definition on parent method should point to run() line, got: {}",
        method_result
    );
    assert_eq!(
        method_result["range"]["start"]["character"].as_u64(),
        Some(20),
        "definition on parent method should point to run() name, got: {}",
        method_result
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
async fn test_goto_declaration_points_to_import_or_definition_fallback() {
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

    let vendor_code = r#"<?php
namespace Vendor;

class Service {}
"#;
    let vendor_uri = "file:///test/VendorService.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(vendor_uri, vendor_code))
        .await
        .unwrap();

    let app_code = r#"<?php
namespace App;

use Vendor\Service;

class Demo {
    public function run(): void {
        new Service();
    }
}
"#;
    let app_uri = "file:///test/DeclarationImport.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(app_uri, app_code))
        .await
        .unwrap();

    let import_resp = service
        .ready()
        .await
        .unwrap()
        .call(declaration_request(2, app_uri, 7, 12))
        .await
        .unwrap();
    let import_result = extract_result(import_resp);
    assert_eq!(
        import_result.get("uri").and_then(|value| value.as_str()),
        Some(app_uri),
        "declaration for imported class should point to current file use statement, got: {}",
        import_result
    );
    assert_eq!(
        import_result["range"]["start"]["line"].as_u64(),
        Some(3),
        "declaration should start on use statement, got: {}",
        import_result
    );
    assert_eq!(
        import_result["range"]["start"]["character"].as_u64(),
        Some(4),
        "declaration should point to imported FQN inside use statement, got: {}",
        import_result
    );

    let local_code = r#"<?php
namespace App;

class Local {}

new Local();
"#;
    let local_uri = "file:///test/DeclarationLocal.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(local_uri, local_code))
        .await
        .unwrap();

    let fallback_resp = service
        .ready()
        .await
        .unwrap()
        .call(declaration_request(3, local_uri, 5, 5))
        .await
        .unwrap();
    let fallback_result = extract_result(fallback_resp);
    assert_eq!(
        fallback_result.get("uri").and_then(|value| value.as_str()),
        Some(local_uri),
        "declaration without import should fall back to definition, got: {}",
        fallback_result
    );
    assert_eq!(
        fallback_result["range"]["start"]["line"].as_u64(),
        Some(3),
        "fallback declaration should point to class name definition, got: {}",
        fallback_result
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
async fn test_goto_type_definition_for_variables_returns_and_properties() {
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

    let code = r#"<?php
namespace App;

class Service {}

function makeService(): Service { return new Service(); }

class Demo {
    public Service $service;

    public function run(Service $param): void {
        /** @var Service $local */
        $local = makeService();
        $param;
        $local;
        makeService();
        $this->service;
    }
}
"#;
    let uri = "file:///test/TypeDefinition.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    for (id, line, character, label) in [
        (2, 13, 9, "typed parameter"),
        (3, 14, 9, "inline @var local"),
        (4, 15, 10, "function return"),
        (5, 16, 16, "property type"),
    ] {
        let resp = service
            .ready()
            .await
            .unwrap()
            .call(type_definition_request(id, uri, line, character))
            .await
            .unwrap();
        let result = extract_result(resp);
        assert_eq!(
            result.get("uri").and_then(|value| value.as_str()),
            Some(uri),
            "type definition for {} should point to current file, got: {}",
            label,
            result
        );
        assert_eq!(
            result["range"]["start"]["line"].as_u64(),
            Some(3),
            "type definition for {} should point to Service class, got: {}",
            label,
            result
        );
        assert_eq!(
            result["range"]["start"]["character"].as_u64(),
            Some(6),
            "type definition for {} should point to Service class name, got: {}",
            label,
            result
        );
    }

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_completion() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    // Initialize
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

    // Open file with a class and try completion after "$"
    let code = r#"<?php
$name = "test";
$count = 42;
echo $
"#;
    let uri = "file:///test/completion.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    // Request completion after "$" on line 3
    let resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(2, uri, 3, 6))
        .await
        .unwrap();

    let result = extract_result(resp);
    // Should return completion items (could be an array or CompletionList)
    assert!(
        !result.is_null(),
        "completion should return results for variable context"
    );

    // Shutdown
    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_completion_static_class_labels_inside_chained_call() {
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

    let code_with_marker = r#"<?php
namespace Symfony\Component\Validator\Constraints;

abstract class Constraint
{
    public const DEFAULT_GROUP = 'Default';
    public const CLASS_CONSTRAINT = 'class';
    public const PROPERTY_CONSTRAINT = 'property';

    public static function getErrorName(string $errorCode): string
    {
        return $errorCode;
    }

    public function validatedBy(): string
    {
        return static::class.'Validator';
    }
}

class Blank extends Constraint
{
    public const NOT_BLANK_ERROR = '183ad2de-533d-4796-a439-6d3c3852b549';
    public string $message = 'This value should be blank.';
}

class ViolationBuilder
{
    public function setCode(string $code): self
    {
        return $this;
    }
}

class Context
{
    public function buildViolation(string $message): ViolationBuilder
    {
        return new ViolationBuilder();
    }
}

class BlankValidator
{
    private Context $context;

    public function validate(Constraint $constraint): void
    {
        $this->context
            ->buildViolation($constraint->message)
            ->setCode(Blank::/*caret*/);
    }
}
"#;
    let marker = "/*caret*/";
    let offset = code_with_marker
        .find(marker)
        .expect("test code should contain caret marker");
    let code = code_with_marker.replace(marker, "");
    let prefix = &code[..offset];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let line_start = prefix.rfind('\n').map(|idx| idx + 1).unwrap_or(0);
    let character = (prefix.len() - line_start) as u32;
    let uri = "file:///test/blank-validator-completion.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, &code))
        .await
        .unwrap();

    let resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(2, uri, line, character))
        .await
        .unwrap();
    let result = extract_result(resp);
    let items = completion_items_from_result(&result);
    let labels: Vec<&str> = items
        .iter()
        .filter_map(|item| item.get("label").and_then(|label| label.as_str()))
        .collect();

    for expected in [
        "class",
        "NOT_BLANK_ERROR",
        "DEFAULT_GROUP",
        "CLASS_CONSTRAINT",
        "PROPERTY_CONSTRAINT",
        "getErrorName",
    ] {
        assert!(
            labels.contains(&expected),
            "expected static completion to include `{expected}`, got: {labels:?}"
        );
    }
    assert!(
        !labels.contains(&"validatedBy"),
        "instance method should stay hidden for ClassName:: completion"
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
async fn test_completion_member_access_from_inline_phpdoc_var() {
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

    let code = r#"<?php
namespace App;

class Baz {
    public function test(): void {}
}

function makeBaz() {}

function run(): void {
    /** @var Baz $baz2 */
    $baz2 = makeBaz();
    $baz2->
}
"#;
    let uri = "file:///test/phpdoc-completion.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    // Completion at the end of "$baz2->"
    let resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(2, uri, 12, 11))
        .await
        .unwrap();

    let result = extract_result(resp);
    assert!(
        !result.is_null(),
        "completion should return member items from inline @var type"
    );

    let labels: Vec<String> = if let Some(arr) = result.as_array() {
        arr.iter()
            .filter_map(|item| item.get("label").and_then(|v| v.as_str()))
            .map(|s| s.to_string())
            .collect()
    } else {
        result
            .get("items")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("label").and_then(|v| v.as_str()))
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default()
    };

    assert!(
        labels.iter().any(|label| label == "test"),
        "expected member completion to include `test`, got: {:?}",
        labels
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
async fn test_completion_member_access_from_this_property_chain() {
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

    let code = r#"<?php
namespace App;

class Browser {
    public string $requestHeaders;
    public function request(): void {}
}

class Controller {
    private Browser $client;
    public function test(): void {
        $this->client->reques
    }
}
"#;
    let uri = "file:///test/property-chain-completion.php";

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
        .call(completion_request(2, uri, 11, 29))
        .await
        .unwrap();

    let result = extract_result(resp);
    let items = completion_items_from_result(&result);
    let labels: Vec<_> = items
        .iter()
        .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
        .collect();

    assert_eq!(
        labels.first().copied(),
        Some("request"),
        "expected method completion to sort first, got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"requestHeaders"),
        "expected property completion from chained type, got: {:?}",
        labels
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
async fn test_completion_member_access_from_nested_fully_qualified_new_stub_type() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-reflection-completion-{}",
        std::process::id()
    ));
    fs::create_dir_all(&tmp_root).unwrap();
    let root_uri = format!("file://{}", tmp_root.to_string_lossy());
    let stubs_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../data/stubs")
        .canonicalize()
        .unwrap();

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(
            1,
            Some(&root_uri),
            Some(json!({
                "stubsPath": stubs_path.to_string_lossy().to_string()
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
    wait_for_indexing_phase(&mut notifications, "stubsLoaded", Duration::from_secs(5)).await;

    let code = r#"<?php
namespace App;

function validate(object $object, mixed $method): void
{
    if ($method instanceof \Closure) {
        $method($object);
    } elseif (\is_array($method)) {
        $method($object);
    } elseif (null !== $object) {
        if (!method_exists($object, $method)) {
            throw new \RuntimeException();
        }

        $reflMethod = new \ReflectionMethod($object, $method);

        if ($reflMethod->isStatic()) {
        }
    }
}
"#;
    let uri = "file:///test/ReflectionCompletion.php";
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
        .call(completion_request(2, uri, 16, 29))
        .await
        .unwrap();
    let result = extract_result(resp);
    let labels: Vec<String> = completion_items_from_result(&result)
        .iter()
        .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect();

    assert!(
        labels.iter().any(|label| label == "isStatic"),
        "expected ReflectionMethod completion to include isStatic, got: {:?}",
        labels
    );
    assert!(
        labels.iter().any(|label| label == "invoke"),
        "expected ReflectionMethod completion to include invoke, got: {:?}",
        labels
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
async fn test_completion_polish_snippets_and_auto_imports() {
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

    let vendor_uri = "file:///test/VendorService.php";
    let vendor_code = r#"<?php
namespace Vendor;

class Service {}
"#;
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(vendor_uri, vendor_code))
        .await
        .unwrap();

    let app_uri = "file:///test/CompletionPolish.php";
    let app_code = r#"<?php
namespace App;

class Demo {
    public function run(): void {
        Ser
    }
}
"#;
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(app_uri, app_code))
        .await
        .unwrap();

    let auto_import_resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(2, app_uri, 5, 11))
        .await
        .unwrap();
    let auto_import_result = extract_result(auto_import_resp);
    let auto_import_items = completion_items_from_result(&auto_import_result);
    let service_item = auto_import_items
        .iter()
        .find(|item| item.get("label").and_then(|value| value.as_str()) == Some("Service"))
        .unwrap_or_else(|| panic!("expected Service completion, got: {auto_import_items:?}"));
    assert!(
        service_item.get("sortText").is_some(),
        "completion item should include stable sortText"
    );
    assert!(
        service_item.get("filterText").is_some(),
        "completion item should include filterText"
    );
    let edits = service_item
        .get("additionalTextEdits")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        edits.len(),
        1,
        "Service completion should add one import edit"
    );
    assert_eq!(
        edits[0].get("newText").and_then(|value| value.as_str()),
        Some("use Vendor\\Service;\n"),
        "auto-import edit should insert the selected class import"
    );
    assert_eq!(
        edits[0]["range"]["start"]["line"].as_u64(),
        Some(2),
        "auto-import should be inserted after namespace declaration"
    );

    let snippet_uri = "file:///test/CompletionSnippet.php";
    let snippet_code = "<?php\ncla";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(snippet_uri, snippet_code))
        .await
        .unwrap();
    let snippet_resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(3, snippet_uri, 1, 3))
        .await
        .unwrap();
    let snippet_result = extract_result(snippet_resp);
    let snippet_items = completion_items_from_result(&snippet_result);
    let class_item = snippet_items
        .iter()
        .find(|item| item.get("label").and_then(|value| value.as_str()) == Some("class"))
        .unwrap_or_else(|| panic!("expected class snippet completion, got: {snippet_items:?}"));
    assert_eq!(
        class_item.get("kind").and_then(|value| value.as_u64()),
        Some(15),
        "class completion should be a snippet item"
    );
    assert_eq!(
        class_item
            .get("insertTextFormat")
            .and_then(|value| value.as_u64()),
        Some(2),
        "class completion should use snippet insert text format"
    );
    assert!(
        class_item
            .get("insertText")
            .and_then(|value| value.as_str())
            .is_some_and(|text| text.contains("${1:Name}")),
        "class snippet should include a name placeholder"
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
async fn test_document_highlight_variables_and_properties() {
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

    let code = r#"<?php
class Demo {
    public string $name;

    public function run(string $name): void {
        $name = $name . "!";
        echo $name;
        $this->name = $name;
        echo $this->name;
    }
}
"#;
    let uri = "file:///test/DocumentHighlight.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let variable_highlights = service
        .ready()
        .await
        .unwrap()
        .call(document_highlight_request(2, uri, 6, 15))
        .await
        .unwrap();
    let variable_result = extract_result(variable_highlights);
    let variable_items = variable_result.as_array().cloned().unwrap_or_default();
    assert_eq!(
        variable_items.len(),
        5,
        "local variable highlights should include declaration and scoped usages"
    );
    assert_eq!(
        variable_items
            .iter()
            .filter(|item| item.get("kind").and_then(|kind| kind.as_u64()) == Some(3))
            .count(),
        2,
        "parameter declaration and assignment target should be write highlights"
    );
    assert_eq!(
        variable_items
            .iter()
            .filter(|item| item.get("kind").and_then(|kind| kind.as_u64()) == Some(2))
            .count(),
        3,
        "variable usages should be read highlights"
    );

    let property_highlights = service
        .ready()
        .await
        .unwrap()
        .call(document_highlight_request(3, uri, 8, 21))
        .await
        .unwrap();
    let property_result = extract_result(property_highlights);
    let property_items = property_result.as_array().cloned().unwrap_or_default();
    assert_eq!(
        property_items.len(),
        3,
        "property highlights should include declaration and member accesses"
    );
    assert_eq!(
        property_items
            .iter()
            .filter(|item| item.get("kind").and_then(|kind| kind.as_u64()) == Some(3))
            .count(),
        2,
        "property declaration and assignment target should be write highlights"
    );
    assert_eq!(
        property_items
            .iter()
            .filter(|item| item.get("kind").and_then(|kind| kind.as_u64()) == Some(2))
            .count(),
        1,
        "property read access should be a read highlight"
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
async fn test_selection_range_expands_ast_chain() {
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

    let code = r#"<?php
class Demo {
    public function run(): void {
        $value = trim(" hi ");
        echo $value;
    }
}
"#;
    let uri = "file:///test/SelectionRange.php";
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
        .call(selection_range_request(2, uri, vec![(3, 18), (4, 15)]))
        .await
        .unwrap();
    let result = extract_result(response);
    let items = result.as_array().cloned().unwrap_or_default();
    assert_eq!(
        items.len(),
        2,
        "selectionRange should return one chain per requested position"
    );

    let call_chain = selection_range_chain(&items[0]);
    assert!(
        call_chain.len() >= 5,
        "function call selection should expand through expression, statement, method and class: {call_chain:?}"
    );
    assert_eq!(
        call_chain[0],
        (3, 17, 3, 21),
        "first selection range should be the function identifier"
    );
    assert!(
        call_chain
            .iter()
            .any(|range| range.0 == 3 && range.1 <= 17 && range.2 == 3 && range.3 > 21),
        "selection range should include a wider same-line expression: {call_chain:?}"
    );
    assert!(
        call_chain
            .iter()
            .any(|range| range.0 == 2 && range.1 == 4 && range.2 >= 5),
        "selection range should include the enclosing method: {call_chain:?}"
    );
    assert!(
        call_chain
            .iter()
            .any(|range| range.0 == 1 && range.1 == 0 && range.2 >= 6),
        "selection range should include the enclosing class: {call_chain:?}"
    );

    let variable_chain = selection_range_chain(&items[1]);
    assert!(
        variable_chain.len() >= 4,
        "variable selection should expand through expression and enclosing scopes: {variable_chain:?}"
    );
    assert!(
        variable_chain
            .iter()
            .any(|range| range.0 == 4 && range.1 <= 14 && range.2 == 4 && range.3 >= 19),
        "variable selection should include the variable node: {variable_chain:?}"
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
async fn test_linked_editing_range_for_use_alias_identifier() {
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

    let code = r#"<?php
namespace App;

use Vendor\Service as Service;
use Vendor\Other;

class Demo {}
"#;
    let uri = "file:///test/LinkedEditingRange.php";
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
        .call(linked_editing_range_request(2, uri, 3, 13))
        .await
        .unwrap();
    let result = extract_result(response);
    let ranges = result
        .get("ranges")
        .and_then(|ranges| ranges.as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        ranges.len(),
        2,
        "linked editing should return FQN tail and alias ranges"
    );
    let actual_ranges: Vec<_> = ranges
        .iter()
        .map(|range| {
            (
                range["start"]["line"].as_u64().unwrap_or(u64::MAX),
                range["start"]["character"].as_u64().unwrap_or(u64::MAX),
                range["end"]["line"].as_u64().unwrap_or(u64::MAX),
                range["end"]["character"].as_u64().unwrap_or(u64::MAX),
            )
        })
        .collect();
    assert_eq!(
        actual_ranges,
        vec![(3, 11, 3, 18), (3, 22, 3, 29)],
        "linked editing ranges should point to both Service identifiers"
    );
    assert_eq!(
        result.get("wordPattern").and_then(|value| value.as_str()),
        Some("[A-Za-z_][A-Za-z0-9_]*"),
        "linked editing should constrain edits to PHP identifier segments"
    );

    let single_name_response = service
        .ready()
        .await
        .unwrap()
        .call(linked_editing_range_request(3, uri, 4, 12))
        .await
        .unwrap();
    assert!(
        extract_result(single_name_response).is_null(),
        "linked editing should not return a single unpaired identifier"
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
async fn test_signature_help_for_function_method_and_constructor() {
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

    let code = r#"<?php
namespace App;

/**
 * Build a greeting.
 * @param string $name Person name.
 * @param int $count Repeat count.
 * @return string
 */
function greet(string $name, int $count = 1): string { return $name; }

class Greeter {
    /**
     * @param string $prefix Prefix text.
     */
    public function __construct(string $prefix) {}

    /**
     * @param string $name Person name.
     * @param int $count Repeat count.
     */
    public function say(string $name, int $count): string { return $name; }
}

function run(): void {
    greet("Ada", 2);
    $greeter = new Greeter("Hi");
    $greeter->say("Ada", 2);
}
"#;
    let uri = "file:///test/signature-help.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let function_resp = service
        .ready()
        .await
        .unwrap()
        .call(signature_help_request(2, uri, 25, 18))
        .await
        .unwrap();
    let function_result = extract_result(function_resp);
    assert!(
        function_result["signatures"][0]["label"]
            .as_str()
            .unwrap_or("")
            .contains("App\\greet(string $name, int $count = 1): string"),
        "expected function signature, got: {}",
        function_result
    );
    assert_eq!(
        function_result["activeParameter"].as_u64(),
        Some(1),
        "second function argument should be active"
    );
    assert!(
        function_result["signatures"][0]["parameters"][0]["documentation"]["value"]
            .as_str()
            .unwrap_or("")
            .contains("Person name."),
        "expected @param documentation, got: {}",
        function_result
    );

    let ctor_resp = service
        .ready()
        .await
        .unwrap()
        .call(signature_help_request(3, uri, 26, 30))
        .await
        .unwrap();
    let ctor_result = extract_result(ctor_resp);
    assert!(
        ctor_result["signatures"][0]["label"]
            .as_str()
            .unwrap_or("")
            .contains("App\\Greeter::__construct(string $prefix)"),
        "expected constructor signature, got: {}",
        ctor_result
    );

    let method_resp = service
        .ready()
        .await
        .unwrap()
        .call(signature_help_request(4, uri, 27, 26))
        .await
        .unwrap();
    let method_result = extract_result(method_resp);
    assert!(
        method_result["signatures"][0]["label"]
            .as_str()
            .unwrap_or("")
            .contains("App\\Greeter::say(string $name, int $count): string"),
        "expected method signature, got: {}",
        method_result
    );
    assert_eq!(
        method_result["activeParameter"].as_u64(),
        Some(1),
        "second method argument should be active"
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
async fn test_inlay_hints_for_parameters_and_phpdoc_types() {
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

    let code = r#"<?php
namespace App;

/**
 * @param string $name
 * @param int $count
 * @return string
 */
function label($name, $count) { return $name; }

class Formatter {
    /**
     * @param string $prefix
     * @return string
     */
    public function format($prefix) { return $prefix; }
}

function run(Formatter $formatter): void {
    label("Ada", 2);
    $formatter->format("Hi");
}
"#;
    let uri = "file:///test/inlay-hints.php";

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
        .call(inlay_hint_request(2, uri, 0, 0, 22, 0))
        .await
        .unwrap();
    let result = extract_result(response);
    let hints = result.as_array().expect("expected inlay hint array");
    let labels: Vec<&str> = hints
        .iter()
        .filter_map(|hint| hint.get("label").and_then(|label| label.as_str()))
        .collect();

    for expected in ["name:", "count:", "prefix:", ": string", ": int"] {
        assert!(
            labels.contains(&expected),
            "expected `{}` in inlay hint labels, got: {:?}",
            expected,
            labels
        );
    }
    assert!(
        hints
            .iter()
            .any(|hint| hint.get("kind").and_then(|kind| kind.as_u64()) == Some(2)),
        "expected parameter hint kind, got: {}",
        result
    );
    assert!(
        hints
            .iter()
            .any(|hint| hint.get("kind").and_then(|kind| kind.as_u64()) == Some(1)),
        "expected type hint kind, got: {}",
        result
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
async fn test_folding_ranges_for_php_structures() {
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

    let code = r#"<?php
namespace App {

/**
 * Demo service.
 * @return void
 */
class Demo {
    public function run(): void {
        if (true) {
            $items = [
                'one',
                'two',
            ];
        }
    }
}
}
"#;
    let uri = "file:///test/Folding.php";
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
        .call(folding_range_request(2, uri))
        .await
        .unwrap();
    let result = extract_result(resp);
    let ranges = result.as_array().expect("folding range array");

    let has_range = |start_line: u64, end_line: u64, kind: Option<&str>| {
        ranges.iter().any(|range| {
            range["startLine"].as_u64() == Some(start_line)
                && range["endLine"].as_u64() == Some(end_line)
                && kind.is_none_or(|kind| range["kind"].as_str() == Some(kind))
        })
    };

    assert!(
        has_range(1, 17, None),
        "expected namespace folding range, got: {}",
        result
    );
    assert!(
        has_range(3, 6, Some("comment")),
        "expected PHPDoc comment folding range, got: {}",
        result
    );
    assert!(
        has_range(7, 16, None),
        "expected class folding range, got: {}",
        result
    );
    assert!(
        has_range(8, 15, None),
        "expected method folding range, got: {}",
        result
    );
    assert!(
        has_range(9, 14, Some("region")),
        "expected block folding range, got: {}",
        result
    );
    assert!(
        has_range(10, 13, Some("region")),
        "expected array folding range, got: {}",
        result
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
async fn test_call_hierarchy_prepare_incoming_and_outgoing() {
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

    let code = r#"<?php
namespace App;

function helper(): void {}

function caller(): void {
    helper();
}

class Service {
    public function target(): void {}

    public function entry(): void {
        $this->target();
        helper();
    }
}

function run(Service $service): void {
    caller();
    $service->entry();
}
"#;
    let uri = "file:///test/call-hierarchy.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let entry_prepare = service
        .ready()
        .await
        .unwrap()
        .call(prepare_call_hierarchy_request(2, uri, 12, 22))
        .await
        .unwrap();
    let entry_result = extract_result(entry_prepare);
    let entry_items = entry_result
        .as_array()
        .expect("expected prepareCallHierarchy item array");
    let entry_item = entry_items[0].clone();
    assert_eq!(entry_item["name"].as_str(), Some("entry"));
    assert_eq!(
        entry_item["data"]["fqn"].as_str(),
        Some("App\\Service::entry"),
        "expected call hierarchy item data, got: {}",
        entry_item
    );

    let outgoing_resp = service
        .ready()
        .await
        .unwrap()
        .call(outgoing_calls_request(3, entry_item))
        .await
        .unwrap();
    let outgoing_result = extract_result(outgoing_resp);
    let outgoing = outgoing_result
        .as_array()
        .expect("expected outgoing call array");
    let outgoing_names: Vec<&str> = outgoing
        .iter()
        .filter_map(|call| call["to"]["name"].as_str())
        .collect();
    assert!(
        outgoing_names.contains(&"target") && outgoing_names.contains(&"helper"),
        "expected outgoing target/helper calls, got: {}",
        outgoing_result
    );
    assert!(
        outgoing.iter().any(|call| call["fromRanges"]
            .as_array()
            .is_some_and(|ranges| !ranges.is_empty())),
        "expected outgoing call ranges, got: {}",
        outgoing_result
    );

    let helper_prepare = service
        .ready()
        .await
        .unwrap()
        .call(prepare_call_hierarchy_request(4, uri, 3, 10))
        .await
        .unwrap();
    let helper_result = extract_result(helper_prepare);
    let helper_item = helper_result
        .as_array()
        .expect("expected helper prepare item array")[0]
        .clone();
    assert_eq!(helper_item["name"].as_str(), Some("helper"));

    let incoming_resp = service
        .ready()
        .await
        .unwrap()
        .call(incoming_calls_request(5, helper_item))
        .await
        .unwrap();
    let incoming_result = extract_result(incoming_resp);
    let incoming = incoming_result
        .as_array()
        .expect("expected incoming call array");
    let incoming_names: Vec<&str> = incoming
        .iter()
        .filter_map(|call| call["from"]["name"].as_str())
        .collect();
    assert!(
        incoming_names.contains(&"caller") && incoming_names.contains(&"entry"),
        "expected incoming caller/entry calls, got: {}",
        incoming_result
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
async fn test_type_hierarchy_prepare_supertypes_and_subtypes() {
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

    let code = r#"<?php
namespace App;

interface Contract {}

class Base {}

class Child extends Base implements Contract {}

class GrandChild extends Child {}

class Other implements Contract {}
"#;
    let uri = "file:///test/type-hierarchy.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let child_prepare = service
        .ready()
        .await
        .unwrap()
        .call(prepare_type_hierarchy_request(2, uri, 7, 8))
        .await
        .unwrap();
    let child_result = extract_result(child_prepare);
    let child_item = child_result
        .as_array()
        .expect("expected type hierarchy prepare array")[0]
        .clone();
    assert_eq!(child_item["name"].as_str(), Some("Child"));
    assert_eq!(child_item["data"]["fqn"].as_str(), Some("App\\Child"));

    let supertypes_resp = service
        .ready()
        .await
        .unwrap()
        .call(type_hierarchy_supertypes_request(3, child_item.clone()))
        .await
        .unwrap();
    let supertypes_result = extract_result(supertypes_resp);
    let supertypes = supertypes_result
        .as_array()
        .expect("expected supertypes array");
    let supertype_names: Vec<&str> = supertypes
        .iter()
        .filter_map(|item| item["name"].as_str())
        .collect();
    assert!(
        supertype_names.contains(&"Base") && supertype_names.contains(&"Contract"),
        "expected Base and Contract supertypes, got: {}",
        supertypes_result
    );

    let child_subtypes_resp = service
        .ready()
        .await
        .unwrap()
        .call(type_hierarchy_subtypes_request(4, child_item))
        .await
        .unwrap();
    let child_subtypes_result = extract_result(child_subtypes_resp);
    let child_subtype_names: Vec<&str> = child_subtypes_result
        .as_array()
        .expect("expected child subtypes array")
        .iter()
        .filter_map(|item| item["name"].as_str())
        .collect();
    assert_eq!(child_subtype_names, vec!["GrandChild"]);

    let contract_prepare = service
        .ready()
        .await
        .unwrap()
        .call(prepare_type_hierarchy_request(5, uri, 3, 12))
        .await
        .unwrap();
    let contract_result = extract_result(contract_prepare);
    let contract_item = contract_result
        .as_array()
        .expect("expected contract prepare array")[0]
        .clone();
    assert_eq!(contract_item["name"].as_str(), Some("Contract"));

    let contract_subtypes_resp = service
        .ready()
        .await
        .unwrap()
        .call(type_hierarchy_subtypes_request(6, contract_item))
        .await
        .unwrap();
    let contract_subtypes_result = extract_result(contract_subtypes_resp);
    let contract_subtype_names: Vec<&str> = contract_subtypes_result
        .as_array()
        .expect("expected contract subtypes array")
        .iter()
        .filter_map(|item| item["name"].as_str())
        .collect();
    assert!(
        contract_subtype_names.contains(&"Child") && contract_subtype_names.contains(&"Other"),
        "expected Child and Other contract subtypes, got: {}",
        contract_subtypes_result
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
async fn test_goto_implementation_for_types_and_methods() {
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

    let code = r#"<?php
namespace App;

interface Contract {
    public function work(): void;
}

class Base {
    public function run(): void {}
}

class Impl extends Base implements Contract {
    public function work(): void {}
    public function run(): void {}
}

class Other implements Contract {
    public function work(): void {}
}

function useIt(Contract $contract, Base $base): void {
    $contract->work();
    $base->run();
}
"#;
    let uri = "file:///test/implementation.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let type_response = service
        .ready()
        .await
        .unwrap()
        .call(implementation_request(2, uri, 3, 12))
        .await
        .unwrap();
    let type_result = extract_result(type_response);
    let type_impls = type_result
        .as_array()
        .expect("expected implementation locations for Contract");
    let type_lines: Vec<u64> = type_impls
        .iter()
        .filter_map(|location| location["range"]["start"]["line"].as_u64())
        .collect();
    assert!(
        type_lines.contains(&11) && type_lines.contains(&16),
        "expected Impl and Other implementation locations, got: {}",
        type_result
    );

    let method_response = service
        .ready()
        .await
        .unwrap()
        .call(implementation_request(3, uri, 4, 22))
        .await
        .unwrap();
    let method_result = extract_result(method_response);
    let method_impls = method_result
        .as_array()
        .expect("expected implementation locations for Contract::work");
    let method_lines: Vec<u64> = method_impls
        .iter()
        .filter_map(|location| location["range"]["start"]["line"].as_u64())
        .collect();
    assert!(
        method_lines.contains(&12) && method_lines.contains(&17),
        "expected Impl::work and Other::work implementation locations, got: {}",
        method_result
    );

    let call_response = service
        .ready()
        .await
        .unwrap()
        .call(implementation_request(4, uri, 21, 17))
        .await
        .unwrap();
    let call_result = extract_result(call_response);
    let call_impls = call_result
        .as_array()
        .expect("expected implementation locations for call-site Contract::work");
    assert_eq!(
        call_impls.len(),
        2,
        "expected two call-site implementations, got: {}",
        call_result
    );

    let override_response = service
        .ready()
        .await
        .unwrap()
        .call(implementation_request(5, uri, 8, 21))
        .await
        .unwrap();
    let override_result = extract_result(override_response);
    let override_impls = override_result
        .as_array()
        .expect("expected implementation locations for Base::run");
    assert!(
        override_impls
            .iter()
            .any(|location| location["range"]["start"]["line"].as_u64() == Some(13)),
        "expected Impl::run override location, got: {}",
        override_result
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
async fn test_document_range_formatting_uses_custom_external_command() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let formatted_range = "    echo \"one\";\n";
    let formatter_command = format!("printf '%s' '{}' > {{file}}", formatted_range);

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

#[tokio::test(flavor = "current_thread")]
async fn test_code_action_add_use_for_unknown_class_and_function() {
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

    let vendor_code = r#"<?php
namespace Vendor;

class Bar {}

function helper(): void {}
"#;
    let vendor_uri = "file:///test/Vendor.php";
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
        helper();
    }
}
"#;
    let app_uri = "file:///test/AddUse.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(app_uri, app_code))
        .await
        .unwrap();

    let class_diag = json!([{
        "range": {
            "start": { "line": 5, "character": 12 },
            "end": { "line": 5, "character": 15 }
        },
        "severity": 2,
        "source": "php-lsp",
        "message": "Unknown class: App\\Bar"
    }]);
    let class_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request(2, app_uri, 5, 12, 5, 15, class_diag))
        .await
        .unwrap();
    let class_result = extract_result(class_resp);
    let class_actions = class_result.as_array().expect("code actions array");
    assert!(
        class_actions.iter().any(
            |action| action.get("title").and_then(|v| v.as_str()) == Some("Import Vendor\\Bar")
        ),
        "expected import action for Vendor\\Bar, got: {}",
        class_result
    );
    let class_edit_text = class_actions[0]["edit"]["changes"][app_uri][0]["newText"]
        .as_str()
        .unwrap_or("");
    assert!(
        class_edit_text.contains("use Vendor\\Bar;"),
        "expected use insertion edit, got: {}",
        class_result
    );

    let function_diag = json!([{
        "range": {
            "start": { "line": 6, "character": 8 },
            "end": { "line": 6, "character": 14 }
        },
        "severity": 2,
        "source": "php-lsp",
        "message": "Unknown function: App\\helper"
    }]);
    let function_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request(3, app_uri, 6, 8, 6, 14, function_diag))
        .await
        .unwrap();
    let function_result = extract_result(function_resp);
    let function_actions = function_result.as_array().expect("code actions array");
    assert!(
        function_actions.iter().any(|action| {
            action.get("title").and_then(|v| v.as_str()) == Some("Import Vendor\\helper")
        }),
        "expected import action for Vendor\\helper, got: {}",
        function_result
    );
    let function_edit_text = function_actions[0]["edit"]["changes"][app_uri][0]["newText"]
        .as_str()
        .unwrap_or("");
    assert!(
        function_edit_text.contains("use function Vendor\\helper;"),
        "expected use function insertion edit, got: {}",
        function_result
    );

    let conflict_code = r#"<?php
namespace App;

use Other\Bar;

class ConflictDemo {
    public function run(): void {
        new Bar();
    }
}
"#;
    let conflict_uri = "file:///test/AddUseConflict.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(conflict_uri, conflict_code))
        .await
        .unwrap();

    let conflict_diag = json!([{
        "range": {
            "start": { "line": 7, "character": 12 },
            "end": { "line": 7, "character": 15 }
        },
        "severity": 2,
        "source": "php-lsp",
        "message": "Unknown class: Other\\Bar"
    }]);
    let conflict_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request(
            4,
            conflict_uri,
            7,
            12,
            7,
            15,
            conflict_diag,
        ))
        .await
        .unwrap();
    let conflict_result = extract_result(conflict_resp);
    let conflict_actions = conflict_result.as_array().expect("code actions array");
    assert!(
        conflict_actions.iter().any(|action| {
            action.get("title").and_then(|v| v.as_str()) == Some("Import Vendor\\Bar as BarImport")
        }),
        "expected aliased import action for Vendor\\Bar, got: {}",
        conflict_result
    );
    let conflict_edits = conflict_actions[0]["edit"]["changes"][conflict_uri]
        .as_array()
        .expect("edits");
    assert!(
        conflict_edits
            .iter()
            .any(|edit| edit["newText"].as_str() == Some("use Vendor\\Bar as BarImport;\n")),
        "expected aliased use insertion, got: {}",
        conflict_result
    );
    assert!(
        conflict_edits
            .iter()
            .any(|edit| edit["newText"].as_str() == Some("BarImport")),
        "expected usage replacement with alias, got: {}",
        conflict_result
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
async fn test_code_action_organize_imports_sorts_groups_and_removes_unused() {
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

    let code = r#"<?php
namespace App;

use Zed\Unused;
use function Vendor\zeta;
use Vendor\Foo;
use const Vendor\VALUE;
use Alpha\Bar;

class Demo {
    public function run(Foo $foo, Bar $bar): void {
        zeta();
        echo VALUE;
    }
}
"#;
    let uri = "file:///test/OrganizeImports.php";
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
        .call(organize_imports_request(2, uri))
        .await
        .unwrap();
    let result = extract_result(resp);
    let actions = result.as_array().expect("code actions array");
    let organize_action = actions
        .iter()
        .find(|action| action.get("title").and_then(|v| v.as_str()) == Some("Organize imports"))
        .unwrap_or_else(|| panic!("expected Organize imports action, got: {}", result));

    assert_eq!(
        organize_action.get("kind").and_then(|v| v.as_str()),
        Some("source.organizeImports")
    );

    let new_text = organize_action["edit"]["changes"][uri][0]["newText"]
        .as_str()
        .unwrap_or("");
    assert_eq!(
        new_text,
        "use Alpha\\Bar;\nuse Vendor\\Foo;\n\nuse function Vendor\\zeta;\n\nuse const Vendor\\VALUE;\n"
    );
    assert!(
        !new_text.contains("Zed\\Unused"),
        "unused import should be removed, got: {}",
        new_text
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
async fn test_code_action_add_return_type_from_phpdoc() {
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
            Some(json!({ "phpVersion": "8.2" })),
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

    let code = r#"<?php
namespace App;

/**
 * @return string|null
 */
function label($value) {
    return $value;
}

class Demo {
    /**
     * @return static
     */
    public function fluent() {
        return $this;
    }

    /** @return int */
    public function already(): int {
        return 1;
    }

    /** @return string */
    public function __construct() {}
}
"#;
    let uri = "file:///test/AddReturnType.php";
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
        .call(add_return_type_request(2, uri, ((0, 0), (26, 0))))
        .await
        .unwrap();
    let result = extract_result(resp);
    let actions = result.as_array().expect("code actions array");

    let function_action = actions
        .iter()
        .find(|action| {
            action.get("title").and_then(|v| v.as_str()) == Some("Add return type `string|null`")
        })
        .unwrap_or_else(|| panic!("expected string|null return type action, got: {}", result));
    assert_eq!(
        function_action.get("kind").and_then(|v| v.as_str()),
        Some("refactor.rewrite")
    );
    assert_eq!(
        function_action["edit"]["changes"][uri][0]["newText"].as_str(),
        Some(": string|null")
    );

    let method_action = actions
        .iter()
        .find(|action| {
            action.get("title").and_then(|v| v.as_str()) == Some("Add return type `static`")
        })
        .unwrap_or_else(|| panic!("expected static return type action, got: {}", result));
    assert_eq!(
        method_action["edit"]["changes"][uri][0]["newText"].as_str(),
        Some(": static")
    );
    assert!(
        !actions.iter().any(|action| {
            action
                .get("title")
                .and_then(|v| v.as_str())
                .is_some_and(|title| title.contains("int"))
        }),
        "should not offer action for declarations that already have native return type: {}",
        result
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
async fn test_code_action_add_return_type_respects_php_version() {
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
            Some(json!({ "phpVersion": "7.4" })),
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

    let code = r#"<?php
/**
 * @return string|null
 */
function label($value) {
    return $value;
}
"#;
    let uri = "file:///test/AddReturnTypePhp74.php";
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
        .call(add_return_type_request(2, uri, ((0, 0), (8, 0))))
        .await
        .unwrap();
    let result = extract_result(resp);
    let actions = result.as_array().expect("code actions array");
    assert!(
        actions.is_empty(),
        "PHP 7.4 should not offer PHP 8 union return type action, got: {}",
        result
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

#[tokio::test(flavor = "current_thread")]
async fn test_document_symbols() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    // Initialize
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

    let code = r#"<?php
namespace App;

class UserService {
    private string $name;

    public function getName(): string {
        return $this->name;
    }
}
"#;
    let uri = "file:///test/UserService.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    // Request document symbols
    let resp = service
        .ready()
        .await
        .unwrap()
        .call(document_symbol_request(2, uri))
        .await
        .unwrap();

    let result = extract_result(resp);
    assert!(!result.is_null(), "should return document symbols");

    // Should be an array of symbols
    if let Some(arr) = result.as_array() {
        assert!(!arr.is_empty(), "should have at least one symbol");
    }

    // Shutdown
    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_code_lens_reference_counts_for_types_and_methods() {
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

    let code = r#"<?php
namespace App;

class Demo {
    public static function run(): void {}
}

function boot(): void {
    Demo::run();
}
"#;
    let uri = "file:///test/CodeLens.php";

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
        .call(code_lens_request(2, uri))
        .await
        .unwrap();
    let result = extract_result(resp);
    let lenses = result.as_array().expect("code lens array");

    let demo_lens = lenses
        .iter()
        .find(|lens| lens["data"]["fqn"].as_str() == Some("App\\Demo"))
        .unwrap_or_else(|| panic!("expected class code lens, got: {}", result));
    assert_eq!(
        demo_lens["command"]["title"].as_str(),
        Some("1 reference"),
        "class code lens should count static class reference"
    );
    assert_eq!(
        demo_lens["command"]["command"].as_str(),
        Some("editor.action.showReferences")
    );

    let run_lens = lenses
        .iter()
        .find(|lens| lens["data"]["fqn"].as_str() == Some("App\\Demo::run"))
        .unwrap_or_else(|| panic!("expected method code lens, got: {}", result));
    assert_eq!(
        run_lens["command"]["title"].as_str(),
        Some("1 reference"),
        "method code lens should count static method call"
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
async fn test_workspace_references_use_indexed_closed_files() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-indexed-refs-{}-{}",
        std::process::id(),
        nanos
    ));
    fs::create_dir_all(&tmp_root).unwrap();

    let target_path = tmp_root.join("Target.php");
    let use_path = tmp_root.join("Use.php");
    let target_code = "<?php\nnamespace App;\n\nclass Target {}\n";
    fs::write(&target_path, target_code).unwrap();
    fs::write(
        &use_path,
        "<?php\nnamespace App;\n\nfunction consume(): void {\n    new Target();\n}\n",
    )
    .unwrap();

    let root_uri = format!("file://{}", tmp_root.to_string_lossy());
    let target_uri = format!("file://{}", target_path.to_string_lossy());
    let use_uri = format!("file://{}", use_path.to_string_lossy());

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

    let mut target_indexed = false;
    let mut use_indexed = false;
    for attempt in 0..50 {
        let target_resp = service
            .ready()
            .await
            .unwrap()
            .call(workspace_symbol_request(10 + attempt * 2, "Target"))
            .await
            .unwrap();
        let target_result = extract_result(target_resp);
        target_indexed = workspace_symbol_uris(&target_result)
            .iter()
            .any(|uri| uri == &target_uri);

        let use_resp = service
            .ready()
            .await
            .unwrap()
            .call(workspace_symbol_request(11 + attempt * 2, "consume"))
            .await
            .unwrap();
        let use_result = extract_result(use_resp);
        use_indexed = workspace_symbol_uris(&use_result)
            .iter()
            .any(|uri| uri == &use_uri);

        if target_indexed && use_indexed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        target_indexed && use_indexed,
        "workspace index should include both files before references request"
    );

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&target_uri, target_code))
        .await
        .unwrap();

    let resp = service
        .ready()
        .await
        .unwrap()
        .call(references_request(200, &target_uri, 3, 8, false))
        .await
        .unwrap();
    let result = extract_result(resp);
    let locations = result.as_array().expect("references result array");
    assert!(
        locations.iter().any(|location| {
            location.get("uri").and_then(|value| value.as_str()) == Some(use_uri.as_str())
        }),
        "references should include closed indexed usage file, got: {}",
        result
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();

    let _ = fs::remove_dir_all(&tmp_root);
}

#[tokio::test(flavor = "current_thread")]
async fn test_watched_files_incrementally_reindex_created_changed_deleted_php_files() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let tmp_root =
        std::env::temp_dir().join(format!("php-lsp-watch-{}-{}", std::process::id(), nanos));
    fs::create_dir_all(&tmp_root).unwrap();
    let root_uri = format!("file://{}", tmp_root.to_string_lossy());

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(1, Some(&root_uri), None))
        .await
        .unwrap();

    let watched_path = tmp_root.join("Watched.php");
    let watched_uri = format!("file://{}", watched_path.to_string_lossy());
    fs::write(
        &watched_path,
        "<?php\nnamespace Watched;\nclass Created {}\n",
    )
    .unwrap();

    service
        .ready()
        .await
        .unwrap()
        .call(did_change_watched_files_notification(vec![(
            &watched_uri,
            1,
        )]))
        .await
        .unwrap();

    let created_resp = service
        .ready()
        .await
        .unwrap()
        .call(workspace_symbol_request(2, "Created"))
        .await
        .unwrap();
    let created_result = extract_result(created_resp);
    let created_names = workspace_symbol_names(&created_result);
    assert!(
        created_names.iter().any(|name| name == "Created"),
        "created PHP file should be indexed, got: {}",
        created_result
    );

    fs::write(
        &watched_path,
        "<?php\nnamespace Watched;\nclass Updated {}\n",
    )
    .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_change_watched_files_notification(vec![(
            &watched_uri,
            2,
        )]))
        .await
        .unwrap();

    let updated_resp = service
        .ready()
        .await
        .unwrap()
        .call(workspace_symbol_request(3, "Updated"))
        .await
        .unwrap();
    let updated_result = extract_result(updated_resp);
    let updated_names = workspace_symbol_names(&updated_result);
    assert!(
        updated_names.iter().any(|name| name == "Updated"),
        "changed PHP file should update the index, got: {}",
        updated_result
    );

    let stale_resp = service
        .ready()
        .await
        .unwrap()
        .call(workspace_symbol_request(4, "Created"))
        .await
        .unwrap();
    let stale_result = extract_result(stale_resp);
    let stale_names = workspace_symbol_names(&stale_result);
    assert!(
        !stale_names.iter().any(|name| name == "Created"),
        "changed PHP file should remove stale symbols, got: {}",
        stale_result
    );

    fs::remove_file(&watched_path).unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_change_watched_files_notification(vec![(
            &watched_uri,
            3,
        )]))
        .await
        .unwrap();

    let deleted_resp = service
        .ready()
        .await
        .unwrap()
        .call(workspace_symbol_request(5, "Updated"))
        .await
        .unwrap();
    let deleted_result = extract_result(deleted_resp);
    let deleted_names = workspace_symbol_names(&deleted_result);
    assert!(
        !deleted_names.iter().any(|name| name == "Updated"),
        "deleted PHP file should be removed from the index, got: {}",
        deleted_result
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();

    let _ = fs::remove_dir_all(&tmp_root);
}

#[tokio::test(flavor = "current_thread")]
async fn test_workspace_file_operations_update_index_uris() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let tmp_root =
        std::env::temp_dir().join(format!("php-lsp-fileops-{}-{}", std::process::id(), nanos));
    fs::create_dir_all(&tmp_root).unwrap();
    let root_uri = format!("file://{}", tmp_root.to_string_lossy());

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(1, Some(&root_uri), None))
        .await
        .unwrap();

    let created_path = tmp_root.join("Created.php");
    let created_uri = format!("file://{}", created_path.to_string_lossy());
    fs::write(
        &created_path,
        "<?php\nnamespace FileOps;\nclass FileOperationTarget {}\n",
    )
    .unwrap();

    service
        .ready()
        .await
        .unwrap()
        .call(did_create_files_notification(vec![&created_uri]))
        .await
        .unwrap();

    let created_resp = service
        .ready()
        .await
        .unwrap()
        .call(workspace_symbol_request(2, "FileOperationTarget"))
        .await
        .unwrap();
    let created_result = extract_result(created_resp);
    assert!(
        workspace_symbol_uris(&created_result)
            .iter()
            .any(|uri| uri == &created_uri),
        "didCreateFiles should index the new PHP file, got: {}",
        created_result
    );

    let renamed_path = tmp_root.join("Renamed.php");
    let renamed_uri = format!("file://{}", renamed_path.to_string_lossy());
    fs::rename(&created_path, &renamed_path).unwrap();

    service
        .ready()
        .await
        .unwrap()
        .call(did_rename_files_notification(vec![(
            &created_uri,
            &renamed_uri,
        )]))
        .await
        .unwrap();

    let renamed_resp = service
        .ready()
        .await
        .unwrap()
        .call(workspace_symbol_request(3, "FileOperationTarget"))
        .await
        .unwrap();
    let renamed_result = extract_result(renamed_resp);
    let renamed_uris = workspace_symbol_uris(&renamed_result);
    assert!(
        renamed_uris.iter().any(|uri| uri == &renamed_uri)
            && !renamed_uris.iter().any(|uri| uri == &created_uri),
        "didRenameFiles should move symbol locations to the new URI, got: {}",
        renamed_result
    );

    fs::remove_file(&renamed_path).unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_delete_files_notification(vec![&renamed_uri]))
        .await
        .unwrap();

    let deleted_resp = service
        .ready()
        .await
        .unwrap()
        .call(workspace_symbol_request(4, "FileOperationTarget"))
        .await
        .unwrap();
    let deleted_result = extract_result(deleted_resp);
    assert!(
        workspace_symbol_names(&deleted_result).is_empty(),
        "didDeleteFiles should remove deleted PHP symbols, got: {}",
        deleted_result
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();

    let _ = fs::remove_dir_all(&tmp_root);
}

#[tokio::test(flavor = "current_thread")]
async fn test_workspace_folders_index_multiple_roots() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-multiroot-{}-{}",
        std::process::id(),
        nanos
    ));
    let root_a = tmp_root.join("root-a");
    let root_b = tmp_root.join("root-b");
    fs::create_dir_all(root_a.join("src")).unwrap();
    fs::create_dir_all(root_b.join("src")).unwrap();
    fs::write(
        root_a.join("composer.json"),
        r#"{"autoload":{"psr-4":{"RootA\\":"src/"}}}"#,
    )
    .unwrap();
    fs::write(
        root_b.join("composer.json"),
        r#"{"autoload":{"psr-4":{"RootB\\":"src/"}}}"#,
    )
    .unwrap();
    let root_a_service = root_a.join("src/RootAService.php");
    let root_b_service = root_b.join("src/RootBService.php");
    fs::write(
        &root_a_service,
        "<?php\nnamespace RootA;\nclass RootAService {}\n",
    )
    .unwrap();
    fs::write(
        &root_b_service,
        "<?php\nnamespace RootB;\nclass RootBService {}\n",
    )
    .unwrap();

    let root_a_uri = format!("file://{}", root_a.to_string_lossy());
    let root_b_uri = format!("file://{}", root_b.to_string_lossy());
    let init_resp = service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_workspace_folders(
            1,
            vec![("root-a", &root_a_uri), ("root-b", &root_b_uri)],
        ))
        .await
        .unwrap();
    let init_result = extract_result(init_resp);
    assert_eq!(
        init_result["capabilities"]["workspace"]["workspaceFolders"]["supported"].as_bool(),
        Some(true),
        "server should advertise workspaceFolders support, got: {}",
        init_result
    );

    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();

    let expected_a_uri = format!("file://{}", root_a_service.to_string_lossy());
    let expected_b_uri = format!("file://{}", root_b_service.to_string_lossy());
    let mut result = json!(null);
    for attempt in 0..50 {
        let resp = service
            .ready()
            .await
            .unwrap()
            .call(workspace_symbol_request(10 + attempt, "Root"))
            .await
            .unwrap();
        result = extract_result(resp);
        let uris = workspace_symbol_uris(&result);
        if uris.iter().any(|uri| uri == &expected_a_uri)
            && uris.iter().any(|uri| uri == &expected_b_uri)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let uris = workspace_symbol_uris(&result);
    assert!(
        uris.iter().any(|uri| uri == &expected_a_uri)
            && uris.iter().any(|uri| uri == &expected_b_uri),
        "workspace/symbol should include PHP symbols from both workspace folders, got: {}",
        result
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();

    let _ = fs::remove_dir_all(&tmp_root);
}

#[tokio::test(flavor = "current_thread")]
async fn test_semantic_tokens_full_returns_php_token_types() {
    const TOKEN_CLASS: u64 = 2;
    const TOKEN_PARAMETER: u64 = 5;
    const TOKEN_VARIABLE: u64 = 6;
    const TOKEN_PROPERTY: u64 = 7;
    const TOKEN_METHOD: u64 = 10;
    const MOD_DECLARATION: u64 = 1;

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

    let code = "<?php\nnamespace App\\Demo;\n\nclass UserService {\n    private readonly string $name = \"Ada\";\n\n    public function greet(int $count): string {\n        $message = \"Hi\";\n        return $message;\n    }\n}\n";
    let uri = "file:///test/SemanticTokens.php";
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
        .call(semantic_tokens_full_request(2, uri))
        .await
        .unwrap();
    let result = extract_result(resp);
    let tokens = decode_semantic_tokens(&result);
    assert!(
        !tokens.is_empty(),
        "expected semantic tokens, got: {}",
        result
    );

    assert!(
        tokens
            .iter()
            .any(|(line, start, length, token_type, modifiers)| *line == 3
                && *start == 6
                && *length == 11
                && *token_type == TOKEN_CLASS
                && (*modifiers & MOD_DECLARATION) != 0),
        "expected class declaration token for UserService, got: {:?}",
        tokens
    );
    assert!(
        tokens
            .iter()
            .any(|(line, start, length, token_type, _)| *line == 4
                && *start == 28
                && *length == 5
                && *token_type == TOKEN_PROPERTY),
        "expected property declaration token for $name, got: {:?}",
        tokens
    );
    assert!(
        tokens
            .iter()
            .any(|(line, start, length, token_type, _)| *line == 6
                && *start == 20
                && *length == 5
                && *token_type == TOKEN_METHOD),
        "expected method declaration token for greet, got: {:?}",
        tokens
    );
    assert!(
        tokens
            .iter()
            .any(|(line, start, length, token_type, _)| *line == 6
                && *start == 30
                && *length == 6
                && *token_type == TOKEN_PARAMETER),
        "expected parameter token for $count, got: {:?}",
        tokens
    );
    assert!(
        tokens
            .iter()
            .any(|(line, start, length, token_type, _)| *line == 7
                && *start == 8
                && *length == 8
                && *token_type == TOKEN_VARIABLE),
        "expected local variable token for $message, got: {:?}",
        tokens
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
async fn test_semantic_tokens_range_returns_only_requested_lines() {
    const TOKEN_METHOD: u64 = 10;
    const TOKEN_VARIABLE: u64 = 6;

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

    let uri = "file:///test/SemanticTokensRange.php";
    let code = "<?php\nclass Demo {\n    public function skip(): void {}\n    public function run(): void {\n        $value = \"one\";\n    }\n}\n";
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
        .call(semantic_tokens_range_request(2, uri, 3, 0, 5, 0))
        .await
        .unwrap();
    let result = extract_result(resp);
    let tokens = decode_semantic_tokens(&result);
    assert!(
        !tokens.is_empty(),
        "expected range semantic tokens, got: {}",
        result
    );
    assert!(
        tokens
            .iter()
            .all(|(line, _, _, _, _)| *line >= 3 && *line < 5),
        "range response should only include requested lines, got: {:?}",
        tokens
    );
    assert!(
        tokens
            .iter()
            .any(|(line, _, _, token_type, _)| *line == 3 && *token_type == TOKEN_METHOD),
        "expected method token inside range, got: {:?}",
        tokens
    );
    assert!(
        tokens
            .iter()
            .any(|(line, _, _, token_type, _)| *line == 4 && *token_type == TOKEN_VARIABLE),
        "expected variable token inside range, got: {:?}",
        tokens
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
async fn test_semantic_tokens_full_delta_updates_previous_result() {
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

    let uri = "file:///test/SemanticTokensDelta.php";
    let original_code = "<?php\nclass Demo {\n    public function run(): void {\n        $value = \"one\";\n    }\n}\n";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, original_code))
        .await
        .unwrap();

    let full_resp = service
        .ready()
        .await
        .unwrap()
        .call(semantic_tokens_full_request(2, uri))
        .await
        .unwrap();
    let full_result = extract_result(full_resp);
    let previous_result_id = full_result
        .get("resultId")
        .and_then(|value| value.as_str())
        .expect("semantic full resultId")
        .to_string();
    let previous_data = semantic_token_data(&full_result);

    let changed_code = "<?php\nclass Demo {\n    public function run(): void {\n        $value = \"one\";\n        $other = \"two\";\n    }\n}\n";
    service
        .ready()
        .await
        .unwrap()
        .call(did_change_full_notification(uri, 2, changed_code))
        .await
        .unwrap();

    let delta_resp = service
        .ready()
        .await
        .unwrap()
        .call(semantic_tokens_full_delta_request(
            3,
            uri,
            &previous_result_id,
        ))
        .await
        .unwrap();
    let delta_result = extract_result(delta_resp);
    let next_result_id = delta_result
        .get("resultId")
        .and_then(|value| value.as_str())
        .expect("semantic delta resultId");
    assert_ne!(
        next_result_id, previous_result_id,
        "delta should publish a fresh result id"
    );
    assert!(
        delta_result
            .get("edits")
            .and_then(|value| value.as_array())
            .is_some_and(|edits| !edits.is_empty()),
        "delta response should contain edits, got: {}",
        delta_result
    );

    let patched_data = apply_semantic_token_delta(previous_data, &delta_result);
    let fresh_full_resp = service
        .ready()
        .await
        .unwrap()
        .call(semantic_tokens_full_request(4, uri))
        .await
        .unwrap();
    let fresh_full_result = extract_result(fresh_full_resp);
    assert_eq!(
        patched_data,
        semantic_token_data(&fresh_full_result),
        "delta edits should transform old token data into fresh full token data"
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
async fn test_did_change_debounces_diagnostics_and_ignores_stale_versions() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
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

    let uri = "file:///test/DidChangeDebounce.php";
    let original_code = "<?php\nfunction ready(): void {}\n";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, original_code))
        .await
        .unwrap();

    let opened = next_publish_diagnostics(&mut notifications, uri, Duration::from_secs(1)).await;
    assert_eq!(
        opened.get("version").and_then(|value| value.as_i64()),
        Some(1)
    );

    let broken_code = "<?php\nfunction broken( {\n";
    let fixed_code = "<?php\nfunction fixed(): void {}\n";
    service
        .ready()
        .await
        .unwrap()
        .call(did_change_full_notification(uri, 2, broken_code))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_change_full_notification(uri, 3, fixed_code))
        .await
        .unwrap();

    let latest = next_publish_diagnostics(&mut notifications, uri, Duration::from_secs(1)).await;
    assert_eq!(
        latest.get("version").and_then(|value| value.as_i64()),
        Some(3)
    );
    assert_eq!(
        latest
            .get("diagnostics")
            .and_then(|value| value.as_array())
            .map(Vec::len),
        Some(0),
        "latest diagnostics should be computed from fixed version 3, got: {}",
        latest
    );

    service
        .ready()
        .await
        .unwrap()
        .call(did_change_full_notification(uri, 2, broken_code))
        .await
        .unwrap();
    expect_no_publish_diagnostics(&mut notifications, uri, Duration::from_millis(300)).await;

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_rename() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    // Initialize
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

    let code = r#"<?php
namespace App;

class OldName {
    public function hello(): void {}
}

$x = new OldName();
"#;
    let uri = "file:///test/OldName.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    // Rename "OldName" to "NewName" at class definition (line 3, col 6)
    let resp = service
        .ready()
        .await
        .unwrap()
        .call(rename_request(2, uri, 3, 8, "NewName"))
        .await
        .unwrap();

    let result = extract_result(resp);
    // Should return a WorkspaceEdit with changes
    if !result.is_null() {
        let changes = result.get("changes");
        assert!(
            changes.is_some(),
            "rename should return workspace edit with changes"
        );
    }

    // Shutdown
    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_builtin_function_fallback_blocks_rename_in_namespace() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let tmp_root = std::env::temp_dir().join(format!("php-lsp-e2e-{}", std::process::id()));
    fs::create_dir_all(&tmp_root).unwrap();
    let root_uri = format!("file://{}", tmp_root.to_string_lossy());

    let stubs_path_raw =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../client/stubs");
    // Skip test if client/stubs hasn't been built (CI without bundle-stubs.sh)
    if !stubs_path_raw.join("PhpStormStubsMap.php").exists() {
        eprintln!("Skipping test: client/stubs not found, run bundle-stubs.sh first");
        return;
    }
    let stubs_path = stubs_path_raw.canonicalize().unwrap();

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(
            1,
            Some(&root_uri),
            Some(json!({
                "stubsPath": stubs_path.to_string_lossy().to_string()
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

    let code = r#"<?php
namespace App\Lsp;

strlen("x");
"#;
    let uri = "file:///test/BuiltinRename.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    // Ensure function call in namespace resolves via global built-in fallback.
    let def_resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(2, uri, 3, 2))
        .await
        .unwrap();
    let def_result = extract_result(def_resp);
    assert!(
        !def_result.is_null(),
        "definition for strlen() in namespace should resolve via built-in fallback"
    );

    // Built-ins must not be renameable.
    let prepare_resp = service
        .ready()
        .await
        .unwrap()
        .call(prepare_rename_request(3, uri, 3, 2))
        .await
        .unwrap();
    let prepare_result = extract_result(prepare_resp);
    assert!(
        prepare_result.is_null(),
        "prepareRename should return null for built-in symbol"
    );

    let rename_resp = service
        .ready()
        .await
        .unwrap()
        .call(rename_request(4, uri, 3, 2, "str_len"))
        .await
        .unwrap();
    let err = extract_error_message(rename_resp).unwrap_or_default();
    assert!(
        err.contains("Cannot rename built-in symbols"),
        "rename should return built-in rename error, got: {}",
        err
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();

    let _ = fs::remove_dir_all(&tmp_root);
}

#[tokio::test(flavor = "current_thread")]
async fn test_php_version_filters_version_gated_stubs() {
    let stubs_path_raw = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/stubs");
    if !stubs_path_raw.join("PhpStormStubsMap.php").exists() {
        eprintln!("Skipping test: server/data/stubs not found");
        return;
    }
    let stubs_path = stubs_path_raw.canonicalize().unwrap();

    let code = r#"<?php
sodium_crypto_stream_xchacha20_xor_ic('a', 'b', 0, 'c');
"#;
    let uri = "file:///test/PhpVersionStubs.php";

    let (mut service81, socket81) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket81.collect::<Vec<_>>().await;
    });
    let tmp_root81 =
        std::env::temp_dir().join(format!("php-lsp-version-stubs-81-{}", std::process::id()));
    fs::create_dir_all(&tmp_root81).unwrap();
    let root_uri81 = format!("file://{}", tmp_root81.to_string_lossy());
    service81
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(
            1,
            Some(&root_uri81),
            Some(json!({
                "stubsPath": stubs_path.to_string_lossy().to_string(),
                "phpVersion": "8.1",
                "stubExtensions": ["sodium"]
            })),
        ))
        .await
        .unwrap();
    service81
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();
    service81
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();
    let php81_definition = service81
        .ready()
        .await
        .unwrap()
        .call(definition_request(2, uri, 1, 5))
        .await
        .unwrap();
    assert!(
        extract_result(php81_definition).is_null(),
        "PHP 8.1 should not resolve an 8.2-only sodium function"
    );
    service81
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
    let _ = fs::remove_dir_all(&tmp_root81);

    let (mut service82, socket82) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket82.collect::<Vec<_>>().await;
    });
    let tmp_root82 =
        std::env::temp_dir().join(format!("php-lsp-version-stubs-82-{}", std::process::id()));
    fs::create_dir_all(&tmp_root82).unwrap();
    let root_uri82 = format!("file://{}", tmp_root82.to_string_lossy());
    service82
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(
            3,
            Some(&root_uri82),
            Some(json!({
                "stubsPath": stubs_path.to_string_lossy().to_string(),
                "phpVersion": "8.2",
                "stubExtensions": ["sodium"]
            })),
        ))
        .await
        .unwrap();
    service82
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();
    service82
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();
    let php82_definition = service82
        .ready()
        .await
        .unwrap()
        .call(definition_request(4, uri, 1, 5))
        .await
        .unwrap();
    let php82_result = extract_result(php82_definition);
    assert!(
        php82_result
            .get("uri")
            .and_then(|value| value.as_str())
            .is_some_and(|uri| uri.starts_with("phpstub://sodium/")),
        "PHP 8.2 should resolve the sodium function from stubs, got: {}",
        php82_result
    );
    service82
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(100))
        .await
        .unwrap();
    let _ = fs::remove_dir_all(&tmp_root82);
}

#[tokio::test(flavor = "current_thread")]
async fn test_goto_definition_variables_and_constants() {
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

    let var_code = r#"<?php
function demo(): void {
    $value = 1;
    echo $value;
}
"#;
    let var_uri = "file:///test/GotoVariable.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(var_uri, var_code))
        .await
        .unwrap();

    // 1) Variable usage -> assignment definition
    let var_resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(2, var_uri, 3, 10))
        .await
        .unwrap();
    let var_result = extract_result(var_resp);
    let var_def_line = var_result
        .get("range")
        .and_then(|r| r.get("start"))
        .and_then(|s| s.get("line"))
        .and_then(|n| n.as_u64())
        .unwrap_or(u64::MAX);
    assert_eq!(
        var_def_line, 2,
        "variable usage should go to assignment line"
    );

    // 2) Global const usage -> const declaration
    let const_code = r#"<?php
namespace App;

const BUILD = 'dev';

echo BUILD;
"#;
    let const_uri = "file:///test/GotoConstant.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(const_uri, const_code))
        .await
        .unwrap();

    let build_resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(3, const_uri, 5, 5))
        .await
        .unwrap();
    let build_result = extract_result(build_resp);
    let build_def_line = build_result
        .get("range")
        .and_then(|r| r.get("start"))
        .and_then(|s| s.get("line"))
        .and_then(|n| n.as_u64())
        .unwrap_or(u64::MAX);
    assert_eq!(
        build_def_line, 3,
        "constant usage should go to const declaration line"
    );

    // 3) self::CLASS_CONST usage -> class const declaration
    let class_const_code = r#"<?php
namespace App;

class Foo {
    public const VERSION = '1.0';
    public function run(): string {
        return self::VERSION;
    }
}
"#;
    let class_const_uri = "file:///test/GotoClassConstant.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(class_const_uri, class_const_code))
        .await
        .unwrap();

    let cc_resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(4, class_const_uri, 6, 21))
        .await
        .unwrap();
    let cc_result = extract_result(cc_resp);
    let cc_def_line = cc_result
        .get("range")
        .and_then(|r| r.get("start"))
        .and_then(|s| s.get("line"))
        .and_then(|n| n.as_u64())
        .unwrap_or(u64::MAX);
    assert_eq!(
        cc_def_line, 4,
        "class constant usage should go to class const declaration line"
    );

    // 4) Static property usages: self::$created, static::$var, User::$var
    let static_prop_code = r#"<?php
namespace App;

class User {
    public static string $var = 'u';
}

class Demo {
    public static string $created = 'c';
    public static string $var = 'd';

    public function run(): void {
        echo self::$created;
        echo static::$var;
        echo User::$var;
    }
}
"#;
    let static_prop_uri = "file:///test/GotoStaticProperty.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(static_prop_uri, static_prop_code))
        .await
        .unwrap();

    let self_prop_resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(5, static_prop_uri, 12, 21))
        .await
        .unwrap();
    let self_prop_result = extract_result(self_prop_resp);
    let self_prop_line = self_prop_result
        .get("range")
        .and_then(|r| r.get("start"))
        .and_then(|s| s.get("line"))
        .and_then(|n| n.as_u64())
        .unwrap_or(u64::MAX);
    assert_eq!(
        self_prop_line, 8,
        "self::$created should go to static property declaration"
    );

    let static_prop_resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(6, static_prop_uri, 13, 23))
        .await
        .unwrap();
    let static_prop_result = extract_result(static_prop_resp);
    let static_prop_line = static_prop_result
        .get("range")
        .and_then(|r| r.get("start"))
        .and_then(|s| s.get("line"))
        .and_then(|n| n.as_u64())
        .unwrap_or(u64::MAX);
    assert_eq!(
        static_prop_line, 9,
        "static::$var should go to class static property declaration"
    );

    let user_prop_resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(7, static_prop_uri, 14, 20))
        .await
        .unwrap();
    let user_prop_result = extract_result(user_prop_resp);
    let user_prop_line = user_prop_result
        .get("range")
        .and_then(|r| r.get("start"))
        .and_then(|s| s.get("line"))
        .and_then(|n| n.as_u64())
        .unwrap_or(u64::MAX);
    assert_eq!(
        user_prop_line, 4,
        "User::$var should go to referenced class static property declaration"
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
async fn test_variable_references_and_rename() {
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

    let code = r#"<?php
function run(string $name): void {
    $name = $name . "!";
    echo $name;
}
"#;
    let uri = "file:///test/VariableRefsRename.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    // Cursor on second "$name" in assignment expression.
    let refs_with_decl = service
        .ready()
        .await
        .unwrap()
        .call(references_request(2, uri, 2, 13, true))
        .await
        .unwrap();
    let refs_with_decl_result = extract_result(refs_with_decl);
    let refs_with_decl_len = refs_with_decl_result
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);
    assert_eq!(
        refs_with_decl_len, 4,
        "variable references should include declaration + usages"
    );

    let refs_without_decl = service
        .ready()
        .await
        .unwrap()
        .call(references_request(3, uri, 2, 13, false))
        .await
        .unwrap();
    let refs_without_decl_result = extract_result(refs_without_decl);
    let refs_without_decl_len = refs_without_decl_result
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);
    assert_eq!(
        refs_without_decl_len, 2,
        "variable references without declaration should include only usages"
    );

    let prep = service
        .ready()
        .await
        .unwrap()
        .call(prepare_rename_request(4, uri, 3, 10))
        .await
        .unwrap();
    assert!(
        !extract_result(prep).is_null(),
        "prepareRename should work for local variables"
    );

    let rename = service
        .ready()
        .await
        .unwrap()
        .call(rename_request(5, uri, 3, 10, "title"))
        .await
        .unwrap();
    let rename_result = extract_result(rename);
    let edits = rename_result
        .get("changes")
        .and_then(|c| c.get(uri))
        .and_then(|arr| arr.as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        edits.len(),
        4,
        "rename should update all variable references"
    );
    assert!(
        edits
            .iter()
            .all(|e| e.get("newText").and_then(|t| t.as_str()) == Some("$title")),
        "variable rename should preserve '$' prefix"
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
async fn test_cancel_request_cancels_references_request() {
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

    let target_uri = "file:///test/CancelReferencesTarget.php";
    let target_code =
        "<?php\nnamespace App;\nclass Target {}\nfunction run(): void { new Target(); }\n";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(target_uri, target_code))
        .await
        .unwrap();

    for i in 0..96 {
        let uri = format!("file:///test/CancelReferencesUse{}.php", i);
        let code = format!(
            "<?php\nnamespace App;\nclass Use{} {{ public function run(): void {{ new Target(); }} }}\n",
            i
        );
        service
            .ready()
            .await
            .unwrap()
            .call(did_open_notification(&uri, &code))
            .await
            .unwrap();
    }

    let references = service
        .ready()
        .await
        .unwrap()
        .call(references_request(2, target_uri, 3, 29, true));
    let cancel = service.ready().await.unwrap().call(cancel_request(2));
    let (references_response, cancel_response) = futures::join!(references, cancel);

    assert!(
        cancel_response.unwrap().is_none(),
        "$/cancelRequest should not return a response"
    );
    assert_eq!(
        extract_error_code(references_response.unwrap()),
        Some(ErrorCode::RequestCancelled.code())
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
async fn test_property_rename_preserves_dollar_only_where_needed() {
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

    let code = r#"<?php
class Repo {
    private array $users = [];

    public function add(string $u): void {
        $this->users[] = $u;
        echo $this->users[0] ?? '';
    }
}
"#;
    let uri = "file:///test/PropertyRenameDollar.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    // Cursor on "users" in "$this->users[]"
    let rename = service
        .ready()
        .await
        .unwrap()
        .call(rename_request(2, uri, 5, 16, "users2"))
        .await
        .unwrap();
    let result = extract_result(rename);

    let edits = result
        .get("changes")
        .and_then(|c| c.get(uri))
        .and_then(|arr| arr.as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(edits.len(), 3, "declaration + 2 usages should be renamed");

    let mut has_decl = false;
    let mut has_usage_1 = false;
    let mut has_usage_2 = false;
    for e in edits {
        let line = e
            .get("range")
            .and_then(|r| r.get("start"))
            .and_then(|s| s.get("line"))
            .and_then(|n| n.as_u64())
            .unwrap_or(u64::MAX);
        let new_text = e.get("newText").and_then(|t| t.as_str()).unwrap_or("");

        if line == 2 && new_text == "$users2" {
            has_decl = true;
        }
        if line == 5 && new_text == "users2" {
            has_usage_1 = true;
        }
        if line == 6 && new_text == "users2" {
            has_usage_2 = true;
        }
    }

    assert!(has_decl, "declaration should keep '$' prefix");
    assert!(has_usage_1, "member usage should not add '$'");
    assert!(has_usage_2, "member usage should not add '$'");

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_phpdoc_fixture_hover_completion_definition_and_diagnostics() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    let fixture_root = lsp_cases_fixture_root();
    let root_uri = format!("file://{}", fixture_root.display());

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

    let supported_path = fixture_root.join("src/PhpDoc/SupportedTags.php");
    let supported_uri = format!("file://{}", supported_path.display());
    let supported_content = fs::read_to_string(&supported_path).unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&supported_uri, &supported_content))
        .await
        .unwrap();

    let usage_path = fixture_root.join("src/PhpDoc/VirtualMembers.php");
    let usage_uri = format!("file://{}", usage_path.display());
    let usage_content = fs::read_to_string(&usage_path).unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&usage_uri, &usage_content))
        .await
        .unwrap();

    let class_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(2, &supported_uri, 18, 8))
        .await
        .unwrap();
    let class_hover_result = extract_result(class_hover);
    let class_hover_text = hover_markdown_value(&class_hover_result);
    assert!(
        class_hover_text.contains("Class-level PHPDoc")
            && class_hover_text.contains("@property-read int $version")
            && class_hover_text.contains("@method User findById()"),
        "class hover should include PHPDoc summary and virtual members, got: {}",
        class_hover_text
    );

    let method_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(3, &supported_uri, 35, 22))
        .await
        .unwrap();
    let method_hover_result = extract_result(method_hover);
    let method_hover_text = hover_markdown_value(&method_hover_result);
    assert!(
        method_hover_text.contains("**Throws:**")
            && method_hover_text.contains("\\InvalidArgumentException")
            && method_hover_text.contains("Deprecated")
            && method_hover_text.contains("Use buildFromPayload() instead"),
        "method hover should include @throws and @deprecated, got: {}",
        method_hover_text
    );

    let completion = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(4, &usage_uri, 11, 23))
        .await
        .unwrap();
    let completion_result = extract_result(completion);
    let items = completion_items_from_result(&completion_result);
    let label_item = items
        .iter()
        .find(|item| item.get("label").and_then(|value| value.as_str()) == Some("label"))
        .cloned()
        .expect("completion should include @property $label");
    let find_by_id_item = items
        .iter()
        .find(|item| item.get("label").and_then(|value| value.as_str()) == Some("findById"))
        .cloned()
        .expect("completion should include @method findById");
    assert!(
        items.iter().any(|item| {
            item.get("label").and_then(|value| value.as_str()) == Some("version")
                && item.get("detail").and_then(|value| value.as_str()) == Some("@property-read int")
        }),
        "completion should include @property-read detail, got: {}",
        completion_result
    );
    assert!(
        items.iter().any(|item| {
            item.get("label").and_then(|value| value.as_str()) == Some("dirty")
                && item.get("detail").and_then(|value| value.as_str())
                    == Some("@property-write bool")
        }),
        "completion should include @property-write detail, got: {}",
        completion_result
    );

    let resolved_label = service
        .ready()
        .await
        .unwrap()
        .call(completion_resolve_request(5, label_item))
        .await
        .unwrap();
    let resolved_label_result = extract_result(resolved_label);
    let resolved_label_doc = documentation_markdown_value(&resolved_label_result);
    assert!(
        resolved_label_doc.contains("@property string $label")
            && resolved_label_doc.contains("Human-readable label"),
        "completionItem/resolve should document virtual property, got: {}",
        resolved_label_result
    );

    let resolved_method = service
        .ready()
        .await
        .unwrap()
        .call(completion_resolve_request(6, find_by_id_item))
        .await
        .unwrap();
    let resolved_method_result = extract_result(resolved_method);
    let resolved_method_doc = documentation_markdown_value(&resolved_method_result);
    assert!(
        resolved_method_doc.contains("@method User findById()"),
        "completionItem/resolve should document virtual method, got: {}",
        resolved_method_result
    );

    let virtual_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(7, &usage_uri, 11, 25))
        .await
        .unwrap();
    let virtual_hover_result = extract_result(virtual_hover);
    let virtual_hover_text = hover_markdown_value(&virtual_hover_result);
    assert!(
        virtual_hover_text.contains("@property string $label")
            && virtual_hover_text.contains("Human-readable label"),
        "hover on virtual property should use class PHPDoc tag, got: {}",
        virtual_hover_text
    );

    let property_definition = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(8, &usage_uri, 11, 25))
        .await
        .unwrap();
    let property_definition_result = extract_result(property_definition);
    assert_eq!(
        property_definition_result
            .get("uri")
            .and_then(|value| value.as_str()),
        Some(supported_uri.as_str()),
        "virtual property definition should point to SupportedTags.php, got: {}",
        property_definition_result
    );
    assert_eq!(
        property_definition_result["range"]["start"]["line"].as_u64(),
        Some(12),
        "virtual property definition should point at @property tag name, got: {}",
        property_definition_result
    );

    let method_definition = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(9, &usage_uri, 12, 24))
        .await
        .unwrap();
    let method_definition_result = extract_result(method_definition);
    assert_eq!(
        method_definition_result
            .get("uri")
            .and_then(|value| value.as_str()),
        Some(supported_uri.as_str()),
        "virtual method definition should point to SupportedTags.php, got: {}",
        method_definition_result
    );
    assert_eq!(
        method_definition_result["range"]["start"]["line"].as_u64(),
        Some(15),
        "virtual method definition should point at @method tag name, got: {}",
        method_definition_result
    );

    let prepare_rename = service
        .ready()
        .await
        .unwrap()
        .call(prepare_rename_request(10, &usage_uri, 11, 25))
        .await
        .unwrap();
    assert!(
        extract_result(prepare_rename).is_null(),
        "prepareRename should reject PHPDoc virtual members"
    );

    let rename = service
        .ready()
        .await
        .unwrap()
        .call(rename_request(11, &usage_uri, 11, 25, "caption"))
        .await
        .unwrap();
    let rename_error = extract_error_message(rename).unwrap_or_default();
    assert!(
        rename_error.contains("Cannot rename PHPDoc virtual members"),
        "rename should explicitly reject PHPDoc virtual members, got: {}",
        rename_error
    );

    let edge_path = fixture_root.join("src/PhpDoc/EdgeCases.php");
    let edge_uri = format!("file://{}", edge_path.display());
    let edge_content = fs::read_to_string(&edge_path).unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&edge_uri, &edge_content))
        .await
        .unwrap();
    let edge_diagnostics =
        next_publish_diagnostics(&mut notifications, &edge_uri, Duration::from_secs(2)).await;
    assert!(
        edge_diagnostics
            .get("diagnostics")
            .and_then(|value| value.as_array())
            .is_some(),
        "PHPDoc edge-case fixture should publish diagnostics without crashing, got: {}",
        edge_diagnostics
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// Vendor resolution / cross-file type resolution tests (H-015)
// ---------------------------------------------------------------------------

/// Helper: resolve the path to `test-fixtures/lsp-cases` directory.
fn lsp_cases_fixture_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../test-fixtures/lsp-cases")
        .canonicalize()
        .expect("test-fixtures/lsp-cases must exist")
}

/// Helper: resolve the path to `test-fixtures/vendor-resolve` directory.
fn vendor_resolve_fixture_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../test-fixtures/vendor-resolve")
        .canonicalize()
        .expect("test-fixtures/vendor-resolve must exist")
}

/// Bug 1 + Bug 2: go-to-definition on a method inherited from a vendor grandparent.
///
/// `$this->createStub(...)` in SampleTest where:
///   SampleTest extends TestCase (vendor) extends BaseAssert (vendor, has createStub).
///
/// Requires: stripping `::member` from FQN before PSR-4 vendor lookup,
/// and recursively lazy-loading parent classes from vendor.
#[tokio::test(flavor = "current_thread")]
async fn test_goto_definition_vendor_inherited_method() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let fixture_root = vendor_resolve_fixture_root();
    let root_uri = format!("file://{}", fixture_root.display());

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
    tokio::task::yield_now().await;

    // Open SampleTest.php
    let test_path = fixture_root.join("tests/SampleTest.php");
    let test_file_uri = format!("file://{}", test_path.display());
    let content = fs::read_to_string(&test_path).unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&test_file_uri, &content))
        .await
        .unwrap();

    // Cursor on "createStub" in:  $stub = $this->createStub(TimerService::class);
    // Line 40, col 23 (0-indexed)
    let resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(10, &test_file_uri, 40, 23))
        .await
        .unwrap();

    let result = extract_result(resp);
    assert!(
        !result.is_null(),
        "go-to-definition on createStub() should resolve to vendor BaseAssert::createStub"
    );

    let target_uri = result.get("uri").and_then(|u| u.as_str()).unwrap_or("");
    assert!(
        target_uri.contains("BaseAssert.php"),
        "definition should point to BaseAssert.php, got: {}",
        target_uri
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

/// Bug 1: go-to-definition on a vendor method via typed property in same file.
///
/// `$this->timerMock->method('start')` in SampleTest where:
///   timerMock is `private MockBuilder $timerMock` (same file),
///   MockBuilder is a vendor class with method().
///
/// Requires: stripping `::member` from FQN for vendor PSR-4 lookup.
#[tokio::test(flavor = "current_thread")]
async fn test_goto_definition_vendor_method_via_typed_property() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let fixture_root = vendor_resolve_fixture_root();
    let root_uri = format!("file://{}", fixture_root.display());

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
    tokio::task::yield_now().await;

    // Open SampleTest.php
    let test_path = fixture_root.join("tests/SampleTest.php");
    let test_file_uri = format!("file://{}", test_path.display());
    let content = fs::read_to_string(&test_path).unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&test_file_uri, &content))
        .await
        .unwrap();

    // Cursor on "method" in:  $this->timerMock->method('start');
    // Line 46, col 26 (0-indexed)
    let resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(10, &test_file_uri, 46, 26))
        .await
        .unwrap();

    let result = extract_result(resp);
    assert!(
        !result.is_null(),
        "go-to-definition on method() should resolve to vendor MockBuilder::method"
    );

    let target_uri = result.get("uri").and_then(|u| u.as_str()).unwrap_or("");
    assert!(
        target_uri.contains("MockBuilder.php"),
        "definition should point to MockBuilder.php, got: {}",
        target_uri
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

/// Bug 3 (cross-file): go-to-definition on method of a property declared in parent class.
///
/// `$this->timer->start('handle')` in ConcreteHandler where:
///   ConcreteHandler extends BaseHandler,
///   BaseHandler declares `protected TimerService $timer`.
///   The property is NOT in ConcreteHandler's file_symbols.
///
/// Requires: cross-file property type resolution (callback into WorkspaceIndex).
#[tokio::test(flavor = "current_thread")]
async fn test_goto_definition_cross_file_inherited_property_method() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let fixture_root = vendor_resolve_fixture_root();
    let root_uri = format!("file://{}", fixture_root.display());

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
    tokio::task::yield_now().await;

    // Open all needed files
    for rel in &[
        "src/ConcreteHandler.php",
        "src/BaseHandler.php",
        "src/TimerService.php",
    ] {
        let p = fixture_root.join(rel);
        let u = format!("file://{}", p.display());
        let c = fs::read_to_string(&p).unwrap();
        service
            .ready()
            .await
            .unwrap()
            .call(did_open_notification(&u, &c))
            .await
            .unwrap();
    }

    let handler_uri = format!("file://{}/src/ConcreteHandler.php", fixture_root.display());

    // Cursor on "start" in:  $this->timer->start('handle');
    // Line 21, col 22 (0-indexed)
    let resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(10, &handler_uri, 21, 22))
        .await
        .unwrap();

    let result = extract_result(resp);
    assert!(
        !result.is_null(),
        "go-to-definition on start() via inherited property $timer should resolve to TimerService::start"
    );

    let target_uri = result.get("uri").and_then(|u| u.as_str()).unwrap_or("");
    assert!(
        target_uri.contains("TimerService.php"),
        "definition should point to TimerService.php, got: {}",
        target_uri
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

/// Same-project cross-file: go-to-definition on method via property in same file.
///
/// `$this->timerService->start('benchmark')` in SampleTest where:
///   timerService is `private TimerService $timerService` (same file),
///   TimerService is a local class opened via did_open.
///
/// This validates the basic chained access + cross-file method resolution.
#[tokio::test(flavor = "current_thread")]
async fn test_goto_definition_cross_file_method_via_same_file_property() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let fixture_root = vendor_resolve_fixture_root();
    let root_uri = format!("file://{}", fixture_root.display());

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
    tokio::task::yield_now().await;

    // Open both needed files
    for rel in &["tests/SampleTest.php", "src/TimerService.php"] {
        let p = fixture_root.join(rel);
        let u = format!("file://{}", p.display());
        let c = fs::read_to_string(&p).unwrap();
        service
            .ready()
            .await
            .unwrap()
            .call(did_open_notification(&u, &c))
            .await
            .unwrap();
    }

    let test_file_uri = format!("file://{}/tests/SampleTest.php", fixture_root.display());

    // Cursor on "start" in:  $this->timerService->start('benchmark');
    // Line 58, col 29 (0-indexed)
    let resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(10, &test_file_uri, 58, 29))
        .await
        .unwrap();

    let result = extract_result(resp);
    assert!(
        !result.is_null(),
        "go-to-definition on start() via $timerService property should resolve to TimerService::start"
    );

    let target_uri = result.get("uri").and_then(|u| u.as_str()).unwrap_or("");
    assert!(
        target_uri.contains("TimerService.php"),
        "definition should point to TimerService.php, got: {}",
        target_uri
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}
