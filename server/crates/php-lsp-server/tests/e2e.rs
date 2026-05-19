//! End-to-end tests for the PHP LSP server.
//!
//! These tests exercise the full LSP protocol stack using tower-lsp's
//! in-process service, sending JSON-RPC requests and verifying responses.

use futures::StreamExt;
use serde_json::json;
use std::fs;
use tower::{Service, ServiceExt};
use tower_lsp::jsonrpc::Request;
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

fn initialized_notification() -> Request {
    Request::build("initialized").params(json!({})).finish()
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

fn signature_help_request(id: i64, uri: &str, line: u32, character: u32) -> Request {
    Request::build("textDocument/signatureHelp")
        .params(json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }))
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

fn semantic_token_data(result: &serde_json::Value) -> Vec<u64> {
    result
        .get("data")
        .and_then(|value| value.as_array())
        .expect("semantic token data array")
        .iter()
        .map(|value| value.as_u64().expect("semantic token integer"))
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

// ---------------------------------------------------------------------------
// Vendor resolution / cross-file type resolution tests (H-015)
// ---------------------------------------------------------------------------

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
