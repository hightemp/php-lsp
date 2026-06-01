mod support;

use php_lsp_types::uri::path_to_uri;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use support::*;

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{}-{suffix}", std::process::id()))
}

fn utf16_position_at(source: &str, needle: &str) -> (u32, u32) {
    let offset = source
        .find(needle)
        .unwrap_or_else(|| panic!("missing needle `{needle}`"));
    utf16_position_for_offset(source, offset)
}

fn utf16_position_after(source: &str, needle: &str) -> (u32, u32) {
    let offset = source
        .find(needle)
        .unwrap_or_else(|| panic!("missing needle `{needle}`"))
        + needle.len();
    utf16_position_for_offset(source, offset)
}

fn utf16_position_for_offset(source: &str, offset: usize) -> (u32, u32) {
    let prefix = &source[..offset];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let line_start = prefix.rfind('\n').map_or(0, |idx| idx + 1);
    let character = prefix[line_start..].encode_utf16().count() as u32;
    (line, character)
}

fn first_location(result: &serde_json::Value) -> &serde_json::Value {
    result
        .as_array()
        .and_then(|locations| locations.first())
        .unwrap_or(result)
}

fn location_uri(location: &serde_json::Value) -> Option<&str> {
    location
        .get("uri")
        .or_else(|| location.get("targetUri"))
        .and_then(|value| value.as_str())
}

fn location_start(location: &serde_json::Value) -> Option<(u32, u32)> {
    let range = location
        .get("range")
        .or_else(|| location.get("targetSelectionRange"))
        .or_else(|| location.get("targetRange"))?;
    let start = range.get("start")?;
    Some((
        start.get("line")?.as_u64()? as u32,
        start.get("character")?.as_u64()? as u32,
    ))
}

fn workspace_edit_start_lines(result: &serde_json::Value, uri: &str) -> BTreeSet<u64> {
    result["changes"][uri]
        .as_array()
        .expect("workspace edit should contain text edits for uri")
        .iter()
        .map(|edit| edit["range"]["start"]["line"].as_u64().unwrap())
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn test_foundation_project_config_cannot_self_trust_executable_commands() {
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

    let tmp_root = unique_temp_dir("php-lsp-foundation-untrusted-config");
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
            "allowProjectCommands = true\n[phpstan]\nenabled = true\ncommand = \"sh {} {{file}}\"\n",
            script_path.to_string_lossy()
        ),
    )
    .unwrap();

    let code = "<?php\nclass Subject {}\n";
    let file_path = tmp_root.join("Subject.php");
    fs::write(&file_path, code).unwrap();
    let root_uri = path_to_uri(&tmp_root).unwrap();
    let file_uri = path_to_uri(&file_path).unwrap();

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
        .call(did_open_notification(&file_uri, code))
        .await
        .unwrap();

    let _ = next_publish_diagnostics(&mut notifications, &file_uri, Duration::from_secs(1)).await;
    assert!(
        !marker_path.exists(),
        "project config must not be able to trust its own executable command settings"
    );

    let _ = fs::remove_dir_all(&tmp_root);
}

#[tokio::test(flavor = "current_thread")]
async fn test_foundation_encoded_uri_and_utf16_definition_round_trip() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let tmp_root = unique_temp_dir("php-lsp-foundation-uri");
    let workspace = tmp_root.join("workspace #100%");
    let file_path = workspace.join("src").join("Привет File.php");
    fs::create_dir_all(file_path.parent().unwrap()).unwrap();

    let code = r#"<?php
namespace App;

/* Привет */ class EncodedTarget {}

function make(): EncodedTarget {
    return new EncodedTarget();
}
"#;
    fs::write(&file_path, code).unwrap();
    let root_uri = path_to_uri(&workspace).unwrap();
    let file_uri = path_to_uri(&file_path).unwrap();
    assert!(
        file_uri.contains("%23") && file_uri.contains("%25") && file_uri.contains("%D0"),
        "file URI should be percent encoded: {file_uri}"
    );

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
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&file_uri, code))
        .await
        .unwrap();

    let usage = utf16_position_after(code, "new ");
    let class_name = utf16_position_after(code, "class ");
    let definition = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(2, &file_uri, usage.0, usage.1))
        .await
        .unwrap();
    let result = extract_result(definition);
    let location = first_location(&result);
    assert_eq!(location_uri(location), Some(file_uri.as_str()));
    assert_eq!(
        location_start(location),
        Some(class_name),
        "definition range should stay in UTF-16 coordinates after non-ASCII text: {result}"
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
async fn test_foundation_current_class_completion_uses_cursor_scope() {
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

    let marked_code = r#"<?php
namespace App;

class First {
    private function firstSecret(): void {}
    public function run(): void {
        $this->
    }
}

class Second {
    private function secondSecret(): void {}
    public function run(): void {
        $this->/*complete*/
    }
    }
"#;
    let position = utf16_position_at(marked_code, "/*complete*/");
    let code = marked_code.replace("/*complete*/", "");
    let tmp_root = unique_temp_dir("php-lsp-foundation-completion");
    let file_path = tmp_root.join("FoundationCompletionCurrentClass.php");
    fs::create_dir_all(&tmp_root).unwrap();
    fs::write(&file_path, &code).unwrap();
    let uri = path_to_uri(&file_path).unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&uri, &code))
        .await
        .unwrap();

    let completion = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(2, &uri, position.0, position.1))
        .await
        .unwrap();
    let result = extract_result(completion);
    let labels: BTreeSet<_> = completion_items_from_result(&result)
        .iter()
        .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect();

    assert!(
        labels.contains("secondSecret"),
        "$this-> completion should include private members from the class at the cursor: {result}"
    );
    assert!(
        !labels.contains("firstSecret"),
        "$this-> completion must not borrow private members from the first class in the file: {result}"
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
async fn test_foundation_member_rename_stays_on_resolved_same_named_owner() {
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
    public string $name = 'alpha';
    public function touch(): void {}
    public function run(): void {
        echo $this->name;
        $this->touch();
    }
}

class Beta {
    public string $name = 'beta';
    public function touch(): void {}
    public function run(): void {
        echo $this->name;
        $this->touch();
    }
}

function run(Alpha $alpha, Beta $beta): void {
    echo $alpha->name;
    $alpha->touch();
    echo $beta->name;
    $beta->touch();
}
"#;
    let tmp_root = unique_temp_dir("php-lsp-foundation-rename");
    let file_path = tmp_root.join("FoundationResolvedMemberRename.php");
    fs::create_dir_all(&tmp_root).unwrap();
    fs::write(&file_path, code).unwrap();
    let uri = path_to_uri(&file_path).unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&uri, code))
        .await
        .unwrap();

    let method_position = utf16_position_at(code, "touch(): void {}");
    let method_rename = service
        .ready()
        .await
        .unwrap()
        .call(rename_request(
            2,
            &uri,
            method_position.0,
            method_position.1 + 1,
            "foundationTouch",
        ))
        .await
        .unwrap();
    let method_result = extract_result(method_rename);
    let method_edit_lines = workspace_edit_start_lines(&method_result, &uri);
    let expected_method_lines = BTreeSet::from([
        utf16_position_at(code, "touch(): void {}").0 as u64,
        utf16_position_at(code, "$this->touch();").0 as u64,
        utf16_position_at(code, "$alpha->touch();").0 as u64,
    ]);
    assert_eq!(
        method_edit_lines, expected_method_lines,
        "method rename should not edit same-named Beta::touch references: {method_result}"
    );

    let property_position = utf16_position_after(code, "$alpha->");
    let property_rename = service
        .ready()
        .await
        .unwrap()
        .call(rename_request(
            3,
            &uri,
            property_position.0,
            property_position.1,
            "foundationName",
        ))
        .await
        .unwrap();
    let property_result = extract_result(property_rename);
    let property_edit_lines = workspace_edit_start_lines(&property_result, &uri);
    let expected_property_lines = BTreeSet::from([
        utf16_position_at(code, "$name = 'alpha';").0 as u64,
        utf16_position_at(code, "$this->name;").0 as u64,
        utf16_position_at(code, "$alpha->name;").0 as u64,
    ]);
    assert_eq!(
        property_edit_lines, expected_property_lines,
        "property rename should not edit same-named Beta::$name references: {property_result}"
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

#[test]
fn test_foundation_stubs_guard_rejects_incomplete_bundle() {
    if cfg!(windows) {
        return;
    }

    let tmp_root = unique_temp_dir("php-lsp-foundation-stubs");
    let stubs = tmp_root.join("stubs");
    fs::create_dir_all(stubs.join("Core")).unwrap();
    fs::write(stubs.join("Core").join("Core.php"), "<?php\n").unwrap();

    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let script = repo_root.join("scripts").join("check-stubs.sh");
    let output = Command::new("bash")
        .arg(script)
        .arg("--kind")
        .arg("bundled")
        .arg("--min-php-files")
        .arg("2")
        .arg(&stubs)
        .output()
        .expect("failed to run stubs guard script");

    assert!(
        !output.status.success(),
        "incomplete bundled stubs should fail the guard"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("missing required file") || stderr.contains("too few PHP files"),
        "guard should explain missing/incomplete stubs, stderr: {stderr}"
    );

    let _ = fs::remove_dir_all(&tmp_root);
}
