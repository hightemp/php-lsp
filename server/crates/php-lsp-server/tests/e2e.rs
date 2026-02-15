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

fn completion_request(id: i64, uri: &str, line: u32, character: u32) -> Request {
    Request::build("textDocument/completion")
        .params(json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }))
        .id(id)
        .finish()
}

fn document_symbol_request(id: i64, uri: &str) -> Request {
    Request::build("textDocument/documentSymbol")
        .params(json!({
            "textDocument": { "uri": uri }
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

    let stubs_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../client/stubs")
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
