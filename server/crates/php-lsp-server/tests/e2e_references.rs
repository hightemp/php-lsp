mod support;

use std::collections::BTreeSet;
use support::*;

fn line_col(code: &str, needle: &str) -> (u32, u32) {
    for (line, text) in code.lines().enumerate() {
        if let Some(col) = text.find(needle) {
            return (line as u32, col as u32);
        }
    }
    panic!("needle not found: {needle}");
}

fn workspace_edit_start_lines(result: &serde_json::Value, uri: &str) -> BTreeSet<u64> {
    result
        .get("changes")
        .and_then(|changes| changes.get(uri))
        .and_then(|edits| edits.as_array())
        .unwrap_or_else(|| panic!("workspace edit should contain edits for {uri}: {result}"))
        .iter()
        .map(|edit| edit["range"]["start"]["line"].as_u64().unwrap_or(u64::MAX))
        .collect()
}

fn location_start_lines(result: &serde_json::Value) -> BTreeSet<u64> {
    result
        .as_array()
        .unwrap_or_else(|| panic!("references result should be an array: {result}"))
        .iter()
        .map(|location| {
            location["range"]["start"]["line"]
                .as_u64()
                .unwrap_or(u64::MAX)
        })
        .collect()
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
async fn test_references_do_not_return_duplicate_locations() {
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

class Target {}

function run(): void {
    new Target();
    new Target();
}
"#;
    let uri = "file:///test/DedupReferences.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let (line, col) = line_col(code, "class Target");
    let resp = service
        .ready()
        .await
        .unwrap()
        .call(references_request(
            2,
            uri,
            line,
            col + "class ".len() as u32 + 1,
            true,
        ))
        .await
        .unwrap();
    let result = extract_result(resp);
    let locations = result
        .as_array()
        .unwrap_or_else(|| panic!("references result should be an array: {result}"));
    let unique_locations: BTreeSet<_> = locations
        .iter()
        .map(|location| {
            (
                location["uri"].as_str().unwrap_or_default().to_string(),
                location["range"]["start"]["line"]
                    .as_u64()
                    .unwrap_or(u64::MAX),
                location["range"]["start"]["character"]
                    .as_u64()
                    .unwrap_or(u64::MAX),
                location["range"]["end"]["line"]
                    .as_u64()
                    .unwrap_or(u64::MAX),
                location["range"]["end"]["character"]
                    .as_u64()
                    .unwrap_or(u64::MAX),
            )
        })
        .collect();

    assert_eq!(
        locations.len(),
        unique_locations.len(),
        "references response should not contain duplicate locations: {result}"
    );
    assert_eq!(
        location_start_lines(&result),
        BTreeSet::from([3, 6, 7]),
        "references should include declaration and both usages exactly once: {result}"
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
async fn test_rename_rejects_invalid_new_names_by_symbol_kind() {
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

class RenameClass {
    public const LIMIT = 1;
    public string $prop = '';

    public function run(string $arg): void {
        $local = $arg;
        echo self::LIMIT;
        echo $this->prop;
    }
}

interface RenameInterface {}
trait RenameTrait {}
enum RenameEnum {
    case Ready;
}

function rename_function(): void {}
const GLOBAL_LIMIT = 1;

rename_function();
echo GLOBAL_LIMIT;
RenameEnum::Ready;
"#;
    let uri = "file:///test/RenameValidation.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let cases = [
        ("RenameClass", 1, "123", "Invalid class name"),
        ("RenameInterface", 1, "$Contract", "Invalid interface name"),
        ("RenameTrait", 1, "trait", "Invalid trait name"),
        (
            "enum RenameEnum",
            "enum ".len() as u32 + 1,
            "enum-name",
            "Invalid enum name",
        ),
        ("rename_function();", 1, "return", "Invalid function name"),
        ("run(string", 1, "$run", "Invalid method name"),
        (
            "$this->prop",
            "$this->".len() as u32 + 1,
            "prop-name",
            "Invalid property name",
        ),
        (
            "self::LIMIT",
            "self::".len() as u32 + 1,
            "MAX-VALUE",
            "Invalid constant name",
        ),
        (
            "echo GLOBAL_LIMIT",
            "echo ".len() as u32 + 1,
            "$GLOBAL",
            "Invalid constant name",
        ),
        (
            "::Ready",
            "::".len() as u32 + 1,
            "123",
            "Invalid enum case name",
        ),
        ("$local =", 1, "$1local", "Invalid variable name"),
    ];

    for (idx, (needle, offset, new_name, expected_error)) in cases.iter().enumerate() {
        let (line, col) = line_col(code, needle);
        let resp = service
            .ready()
            .await
            .unwrap()
            .call(rename_request(
                2 + idx as i64,
                uri,
                line,
                col + offset,
                new_name,
            ))
            .await
            .unwrap();
        let err = extract_error_message(resp).unwrap_or_default();
        assert!(
            err.contains(expected_error),
            "rename of {needle:?} to {new_name:?} should fail with {expected_error:?}, got: {err}"
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
async fn test_cancel_request_cancels_rename_request() {
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

    let target_uri = "file:///test/CancelRenameTarget.php";
    let target_code =
        "<?php\nnamespace App;\nclass Target {}\nfunction run(): void { new Target(); }\n";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(target_uri, target_code))
        .await
        .unwrap();

    for i in 0..160 {
        let uri = format!("file:///test/CancelRenameUse{}.php", i);
        let code = format!(
            "<?php\nnamespace App;\nclass RenameUse{} {{ public function run(): void {{ new Target(); }} }}\n",
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

    let rename =
        service
            .ready()
            .await
            .unwrap()
            .call(rename_request(2, target_uri, 2, 8, "RenamedTarget"));
    let cancel = service.ready().await.unwrap().call(cancel_request(2));
    let (rename_response, cancel_response) = futures::join!(rename, cancel);

    assert!(
        cancel_response.unwrap().is_none(),
        "$/cancelRequest should not return a response"
    );
    assert_eq!(
        extract_error_code(rename_response.unwrap()),
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
async fn test_member_rename_uses_resolved_receivers_only() {
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

class Alpha {
    public function touch(): void {}
    public function run(): void {
        $this->touch();
    }
}

class Beta {
    public function touch(): void {}
    public function run(): void {
        $this->touch();
    }
}

function run(Alpha $alpha, Beta $beta): void {
    $alpha->touch();
    $beta->touch();
}
"#;
    let uri = "file:///test/ResolvedMemberRename.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let (rename_line, rename_col) = line_col(code, "touch(): void {}");
    let rename = service
        .ready()
        .await
        .unwrap()
        .call(rename_request(
            2,
            uri,
            rename_line,
            rename_col + 1,
            "renamedTouch",
        ))
        .await
        .unwrap();
    let result = extract_result(rename);
    let edit_lines = workspace_edit_start_lines(&result, uri);

    let expected_lines = BTreeSet::from([
        line_col(code, "touch(): void {}").0 as u64,
        line_col(code, "$this->touch();").0 as u64,
        line_col(code, "$alpha->touch();").0 as u64,
    ]);
    assert_eq!(
        edit_lines, expected_lines,
        "method rename should not touch unrelated Beta::touch references: {result}"
    );
    for edit in result["changes"][uri].as_array().unwrap() {
        assert_eq!(edit["newText"].as_str(), Some("renamedTouch"));
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
async fn test_property_rename_uses_resolved_receivers_only() {
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

class Alpha {
    public string $name = '';
    public function run(): void {
        echo $this->name;
    }
}

class Beta {
    public string $name = '';
    public function run(): void {
        echo $this->name;
    }
}

function run(Alpha $alpha, Beta $beta): void {
    echo $alpha->name;
    echo $beta->name;
}
"#;
    let uri = "file:///test/ResolvedPropertyRename.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let (rename_line, rename_col) = line_col(code, "$alpha->name");
    let rename = service
        .ready()
        .await
        .unwrap()
        .call(rename_request(
            2,
            uri,
            rename_line,
            rename_col + "$alpha->".len() as u32,
            "label",
        ))
        .await
        .unwrap();
    let result = extract_result(rename);
    let edit_lines = workspace_edit_start_lines(&result, uri);

    let expected_lines = BTreeSet::from([
        line_col(code, "$name = '';").0 as u64,
        line_col(code, "$this->name;").0 as u64,
        line_col(code, "$alpha->name;").0 as u64,
    ]);
    assert_eq!(
        edit_lines, expected_lines,
        "property rename should not touch unrelated Beta::$name references: {result}"
    );

    let edits = result["changes"][uri].as_array().unwrap();
    assert!(edits
        .iter()
        .any(|edit| edit["newText"].as_str() == Some("$label")));
    assert_eq!(
        edits
            .iter()
            .filter(|edit| edit["newText"].as_str() == Some("label"))
            .count(),
        2
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
async fn test_references_include_resolved_inheritance_and_interface_receivers() {
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
    public function handle(): void;
}

class Base {
    public function handle(): void {}
}

class Child extends Base {}

class Impl implements Contract {
    public function handle(): void { echo 'impl'; }
}

function run(Child $child, Contract $contract, Impl $impl): void {
    $child->handle();
    $contract->handle();
    $impl->handle();
}
"#;
    let uri = "file:///test/ResolvedInheritanceReferences.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let (base_line, base_col) = line_col(code, "handle(): void {}");
    let base_refs = service
        .ready()
        .await
        .unwrap()
        .call(references_request(2, uri, base_line, base_col + 1, true))
        .await
        .unwrap();
    let base_result = extract_result(base_refs);
    let base_lines = location_start_lines(&base_result);
    let expected_base_lines = BTreeSet::from([
        base_line as u64,
        line_col(code, "$child->handle();").0 as u64,
    ]);
    assert_eq!(
        base_lines, expected_base_lines,
        "base method references should include resolved child receiver only: {base_result}"
    );

    let (contract_line, contract_col) = line_col(code, "handle(): void;");
    let contract_refs = service
        .ready()
        .await
        .unwrap()
        .call(references_request(
            3,
            uri,
            contract_line,
            contract_col + 1,
            true,
        ))
        .await
        .unwrap();
    let contract_result = extract_result(contract_refs);
    let contract_lines = location_start_lines(&contract_result);
    let expected_contract_lines = BTreeSet::from([
        contract_line as u64,
        line_col(code, "handle(): void { echo 'impl'; }").0 as u64,
        line_col(code, "$contract->handle();").0 as u64,
        line_col(code, "$impl->handle();").0 as u64,
    ]);
    assert_eq!(
        contract_lines, expected_contract_lines,
        "interface references should include resolved interface and implementer receivers: {contract_result}"
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
async fn test_member_rename_rejects_unresolved_receiver() {
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
function run($obj): void {
    $obj->touch();
}
"#;
    let uri = "file:///test/UnresolvedMemberRename.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let (line, col) = line_col(code, "touch();");
    let prepare = service
        .ready()
        .await
        .unwrap()
        .call(prepare_rename_request(2, uri, line, col + 1))
        .await
        .unwrap();
    assert!(
        extract_result(prepare).is_null(),
        "prepareRename should reject unresolved member receivers"
    );

    let rename = service
        .ready()
        .await
        .unwrap()
        .call(rename_request(3, uri, line, col + 1, "renamedTouch"))
        .await
        .unwrap();
    let err = extract_error_message(rename).unwrap_or_default();
    assert!(
        err.contains("Cannot safely rename member without a resolved receiver type"),
        "rename should return a safe unresolved receiver error, got: {err}"
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
