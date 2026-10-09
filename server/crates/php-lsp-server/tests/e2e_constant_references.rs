mod support;
use support::*;

async fn send(service: &mut LspService<PhpLspBackend>, request: Request) -> serde_json::Value {
    let response = service.ready().await.unwrap().call(request).await.unwrap();
    if let Some(response) = &response {
        assert!(response.error().is_none(), "{response:?}");
    }
    response
        .map(|response| extract_result(Some(response)))
        .unwrap_or(serde_json::Value::Null)
}

async fn service() -> LspService<PhpLspBackend> {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move { while socket.next().await.is_some() {} });
    send(
        &mut service,
        initialize_request_with_options(
            1,
            None,
            Some(json!({"stubExtensions":[],"indexVendor":false,"diagnosticsMode":"off"})),
        ),
    )
    .await;
    service
}

fn range(source: &str, marker: &str) -> serde_json::Value {
    let (line, character) = utf16_position_after(source, marker);
    json!({"start":{"line":line,"character":character},"end":{"line":line,"character":character+4}})
}

async fn assert_references(
    service: &mut LspService<PhpLspBackend>,
    uri: &str,
    source: &str,
    marker: &str,
    expected: &[&str],
) {
    let (line, col) = utf16_position_after(source, marker);
    let result = send(service, references_request(20, uri, line, col + 1, true)).await;
    let locations = result.as_array().expect("references array");
    assert_eq!(locations.len(), expected.len(), "{result}");
    for marker in expected {
        assert!(
            locations
                .iter()
                .any(|loc| loc["uri"] == uri && loc["range"] == range(source, marker)),
            "missing {marker}: {result}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn constant_references_and_rename_exclude_declarations_members_labels_and_attributes() {
    let mut service = service().await;
    let uri = "file:///test/constant-roles.php";
    let source="<?php\r\nconst /*GLOBAL*/FLAG=1;\r\n#[FLAG] class C {const FLAG=2; public $FLAG; function FLAG(){}}\r\nfunction run(C $obj) {/* 😀 */ echo /*USE*/FLAG; $obj?->FLAG(); $obj?->FLAG; f(FLAG:1);}";
    send(&mut service, did_open_notification(uri, source)).await;
    assert_references(
        &mut service,
        uri,
        source,
        "/*GLOBAL*/",
        &["/*GLOBAL*/", "/*USE*/"],
    )
    .await;
    let (line, col) = utf16_position_after(source, "/*GLOBAL*/");
    let rename = send(
        &mut service,
        rename_request(21, uri, line, col + 1, "RENAMED_FLAG"),
    )
    .await;
    let edits = rename["changes"][uri].as_array().expect("constant edits");
    assert_eq!(edits.len(), 2, "unsafe constant rename: {rename}");
    for marker in ["/*GLOBAL*/", "/*USE*/"] {
        assert!(
            edits
                .iter()
                .any(|e| e["range"] == range(source, marker) && e["newText"] == "RENAMED_FLAG"),
            "{rename}"
        );
    }
    let lenses = send(&mut service, code_lens_request(22, uri)).await;
    let method = lenses
        .as_array()
        .unwrap()
        .iter()
        .find(|lens| lens["data"]["fqn"] == "C::FLAG")
        .unwrap();
    assert_eq!(
        method["data"]["references"], 1,
        "method lens identity changed: {lenses}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn class_constant_and_enum_declaration_queries_keep_member_identity() {
    let mut service = service().await;
    for (uri, source, marker, uses) in [
        (
            "file:///test/class-constant.php",
            "<?php const FLAG=1; class C {const /*DECL*/FLAG=2;} echo C::/*USE*/FLAG; echo FLAG;",
            "/*DECL*/",
            vec!["/*DECL*/", "/*USE*/"],
        ),
        (
            "file:///test/enum-constant.php",
            "<?php const FLAG=1; enum E {case /*DECL*/FLAG;} echo E::/*USE*/FLAG; echo FLAG;",
            "/*DECL*/",
            vec!["/*DECL*/", "/*USE*/"],
        ),
    ] {
        send(&mut service, did_open_notification(uri, source)).await;
        assert_references(&mut service, uri, source, marker, &uses).await;
        send(&mut service, did_close_notification(uri)).await;
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn named_argument_labels_have_no_global_constant_definition_or_rename() {
    let mut service = service().await;
    let uri = "file:///test/named-label.php";
    let source = "<?php const FLAG=1; function f($FLAG) {} f(/*LABEL*/FLAG: FLAG);";
    send(&mut service, did_open_notification(uri, source)).await;
    let (line, col) = utf16_position_after(source, "/*LABEL*/");
    let definition = send(&mut service, definition_request(20, uri, line, col + 1)).await;
    assert!(
        definition.is_null() || definition.as_array().is_some_and(|v| v.is_empty()),
        "label navigated to constant: {definition}"
    );
    let rename = send(&mut service, prepare_rename_request(21, uri, line, col + 1)).await;
    assert!(rename.is_null(), "label offered constant rename: {rename}");
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn organize_imports_drops_constant_alias_used_only_as_unrelated_names() {
    let mut service = service().await;
    let uri = "file:///test/constant-import.php";
    let source = "<?php\nuse const Lib\\FLAG;\nclass C {function FLAG(){}}\nf(FLAG:1);\n";
    send(&mut service, did_open_notification(uri, source)).await;
    let result = send(
        &mut service,
        code_action_request_with_only(
            20,
            uri,
            ((0, 0), (0, 0)),
            json!([]),
            vec!["source.organizeImports"],
        ),
    )
    .await;
    let action = result
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["kind"] == "source.organizeImports")
        .expect("organize imports action");
    let edits = action["edit"]["changes"][uri]
        .as_array()
        .expect("organize edit");
    assert!(
        edits.iter().all(|e| !e["newText"]
            .as_str()
            .unwrap()
            .contains("use const Lib\\FLAG")),
        "false constant import retained: {result}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn edits_close_and_reopen_replace_constant_occurrence_roles() {
    let mut service = service().await;
    let uri = "file:///test/constant-role-edits.php";
    let member = "<?php const /*GLOBAL*/FLAG=1; function f($obj) {$obj?->FLAG();}";
    send(&mut service, did_open_notification(uri, member)).await;
    assert_references(&mut service, uri, member, "/*GLOBAL*/", &["/*GLOBAL*/"]).await;
    let read = member.replace("$obj?->FLAG();", "echo /*USE*/FLAG;");
    send(&mut service, did_change_full_notification(uri, 2, &read)).await;
    assert_references(
        &mut service,
        uri,
        &read,
        "/*GLOBAL*/",
        &["/*GLOBAL*/", "/*USE*/"],
    )
    .await;
    send(&mut service, did_change_full_notification(uri, 3, member)).await;
    assert_references(&mut service, uri, member, "/*GLOBAL*/", &["/*GLOBAL*/"]).await;
    send(&mut service, did_close_notification(uri)).await;
    send(&mut service, did_open_notification(uri, &read)).await;
    assert_references(
        &mut service,
        uri,
        &read,
        "/*GLOBAL*/",
        &["/*GLOBAL*/", "/*USE*/"],
    )
    .await;
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn qualified_constant_prefix_cursor_prepares_and_renames_only_terminal_name() {
    let mut service = service().await;
    let uri = "file:///test/qualified-constant-rename.php";
    let source = "<?php namespace Lib {const /*DECL*/FLAG=1;} namespace App {echo \\Lib\\FLAG;}";
    send(&mut service, did_open_notification(uri, source)).await;
    let (line, col) = utf16_position_after(source, "echo \\");
    let prepared = send(&mut service, prepare_rename_request(20, uri, line, col + 1)).await;
    assert_eq!(
        prepared.get("range").unwrap_or(&prepared),
        &range(source, "\\Lib\\"),
        "{prepared}"
    );
    let rename = send(
        &mut service,
        rename_request(21, uri, line, col + 1, "OTHER_FLAG"),
    )
    .await;
    let edits = rename["changes"][uri].as_array().unwrap();
    assert_eq!(edits.len(), 2, "{rename}");
    for marker in ["/*DECL*/", "\\Lib\\"] {
        assert!(
            edits
                .iter()
                .any(|e| e["range"] == range(source, marker) && e["newText"] == "OTHER_FLAG"),
            "{rename}"
        );
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn organize_imports_keeps_attribute_class_imports_and_real_constant_arguments() {
    let mut service = service().await;
    let uri = "file:///test/attribute-import.php";
    let source = "<?php\nuse Lib\\Tag;\nuse const Lib\\FLAG;\n#[Tag(FLAG)] function f(){}\n";
    send(&mut service, did_open_notification(uri, source)).await;
    let result = send(
        &mut service,
        code_action_request_with_only(
            20,
            uri,
            ((0, 0), (0, 0)),
            json!([]),
            vec!["source.organizeImports"],
        ),
    )
    .await;
    let action = result
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["kind"] == "source.organizeImports")
        .unwrap();
    let edits = action["edit"]["changes"][uri].as_array().unwrap();
    assert!(
        edits
            .iter()
            .any(|e| e["newText"].as_str().unwrap().contains("use Lib\\Tag;")),
        "attribute class import lost: {result}"
    );
    assert!(
        edits.iter().any(|e| e["newText"]
            .as_str()
            .unwrap()
            .contains("use const Lib\\FLAG;")),
        "constant argument import lost: {result}"
    );
    send(&mut service, shutdown_request(99)).await;
}
