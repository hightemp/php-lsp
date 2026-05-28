mod support;

use support::*;

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
async fn test_document_links_for_static_include_require_paths() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-document-links-{}-{}",
        std::process::id(),
        nanos
    ));
    let lib_dir = tmp_root.join("lib");
    fs::create_dir_all(&lib_dir).unwrap();

    let main_path = tmp_root.join("main.php");
    let bootstrap_path = tmp_root.join("bootstrap.php");
    let helpers_path = lib_dir.join("helpers.php");
    fs::write(&bootstrap_path, "<?php\nfunction boot_app(): void {}\n").unwrap();
    fs::write(&helpers_path, "<?php\nfunction helper(): void {}\n").unwrap();

    let code = r#"<?php
require __DIR__ . '/bootstrap.php';
include_once dirname(__FILE__) . '/lib/helpers.php';
include 'missing.php';
function boot(): void {
    require_once 'bootstrap.php';
}
"#;
    fs::write(&main_path, code).unwrap();

    let root_uri = format!("file://{}", tmp_root.to_string_lossy());
    let main_uri = format!("file://{}", main_path.to_string_lossy());
    let bootstrap_uri = format!("file://{}", bootstrap_path.to_string_lossy());
    let helpers_uri = format!("file://{}", helpers_path.to_string_lossy());

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
        .call(did_open_notification(&main_uri, code))
        .await
        .unwrap();

    let resp = service
        .ready()
        .await
        .unwrap()
        .call(document_link_request(2, &main_uri))
        .await
        .unwrap();
    let result = extract_result(resp);
    let links = result.as_array().expect("document links array");
    assert_eq!(links.len(), 3, "expected 3 document links, got: {}", result);

    let target_uris: Vec<_> = links
        .iter()
        .filter_map(|link| link.get("target").and_then(|target| target.as_str()))
        .collect();
    assert!(
        target_uris
            .iter()
            .filter(|uri| **uri == bootstrap_uri)
            .count()
            == 2,
        "expected two links to bootstrap.php, got: {}",
        result
    );
    assert!(
        target_uris.iter().any(|uri| *uri == helpers_uri),
        "expected link to helpers.php, got: {}",
        result
    );

    let link_lines: Vec<_> = links
        .iter()
        .filter_map(|link| {
            link.get("range")
                .and_then(|range| range.get("start"))
                .and_then(|start| start.get("line"))
                .and_then(|line| line.as_u64())
        })
        .collect();
    assert!(link_lines.contains(&1), "missing __DIR__ link: {}", result);
    assert!(
        link_lines.contains(&2),
        "missing dirname(__FILE__) link: {}",
        result
    );
    assert!(
        link_lines.contains(&5),
        "missing nested require_once link: {}",
        result
    );
    assert!(
        !link_lines.contains(&3),
        "missing include should not produce a document link: {}",
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
