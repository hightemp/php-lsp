mod support;
use support::*;

async fn send(service: &mut LspService<PhpLspBackend>, request: Request) -> serde_json::Value {
    match service.ready().await.unwrap().call(request).await.unwrap() {
        Some(response) => {
            assert!(response.error().is_none(), "{response:?}");
            extract_result(Some(response))
        }
        None => serde_json::Value::Null,
    }
}
async fn service() -> LspService<PhpLspBackend> {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move { while socket.next().await.is_some() {} });
    send(
        &mut service,
        initialize_request_with_options(
            1,
            None,
            Some(json!({"stubExtensions":[],"indexVendor":false,"diagnosticsMode":"off","analyzerCodeActions":{"enabled":true}})),
        ),
    )
    .await;
    service
}
fn starts(source: &str) -> Vec<(usize, usize)> {
    let bytes = source.as_bytes();
    let mut result = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' || bytes[i] == b'\n' {
            result.push((start, i));
            if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
                i += 1;
            }
            start = i + 1;
        }
        i += 1;
    }
    result.push((start, source.len()));
    result
}
fn position(source: &str, offset: usize) -> (u32, u32) {
    let lines = starts(source);
    let row = lines.partition_point(|(start, _)| *start <= offset) - 1;
    (
        row as u32,
        source[lines[row].0..offset].encode_utf16().count() as u32,
    )
}
fn at(source: &str, needle: &str) -> (u32, u32) {
    position(source, source.find(needle).unwrap())
}
fn range(source: &str, needle: &str) -> serde_json::Value {
    let start = source.find(needle).unwrap();
    let (a, b) = position(source, start);
    let (c, d) = position(source, start + needle.len());
    json!({"start":{"line":a,"character":b},"end":{"line":c,"character":d}})
}
fn offset(source: &str, position: &serde_json::Value) -> usize {
    let lines = starts(source);
    let (start, end) = lines[position["line"].as_u64().unwrap() as usize];
    let col = position["character"].as_u64().unwrap() as usize;
    let mut utf16 = 0;
    for (i, ch) in source[start..end].char_indices() {
        if utf16 == col {
            return start + i;
        }
        utf16 += ch.len_utf16();
    }
    assert_eq!(utf16, col);
    end
}
fn apply(source: &str, edits: &serde_json::Value) -> String {
    let mut edits = edits
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                offset(source, &e["range"]["start"]),
                offset(source, &e["range"]["end"]),
                e["newText"].as_str().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    edits.sort_by_key(|e| std::cmp::Reverse(e.0));
    let mut result = source.to_string();
    for (start, end, text) in edits {
        result.replace_range(start..end, text);
    }
    result
}

#[tokio::test(flavor = "current_thread")]
async fn line_model_incremental_unicode_separators_keep_symbols_and_reopen_state() {
    for separator in ['\u{2028}', '\u{2029}', '\u{85}', '\u{b}', '\u{c}'] {
        let mut service = service().await;
        let uri = "file:///line-model.php";
        let source = format!("<?php\n// a{separator}b\nfunction foo() {{}}\n");
        send(&mut service, did_open_notification(uri, &source)).await;
        send(&mut service,Request::build("textDocument/didChange").params(json!({"textDocument":{"uri":uri,"version":2},"contentChanges":[{"range":range(&source,"foo"),"text":"bar"}]})).finish()).await;
        let symbols = send(&mut service, document_symbol_request(2, uri)).await;
        assert_eq!(
            symbols[0]["name"], "bar",
            "separator {separator:?}: {symbols}"
        );
        assert_eq!(symbols[0]["selectionRange"], range(&source, "foo"));
        send(&mut service, did_close_notification(uri)).await;
        send(&mut service, did_open_notification(uri, &source)).await;
        let symbols = send(&mut service, document_symbol_request(3, uri)).await;
        assert_eq!(symbols[0]["name"], "foo");
        send(&mut service, shutdown_request(99)).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn line_model_bare_cr_navigation_references_and_rename_use_client_positions() {
    for eol in ["\n", "\r\n", "\r"] {
        let mut service = service().await;
        let uri = "file:///line-navigation.php";
        let source = format!("<?php{eol}/* 😀 */ function foo():int {{return 1;}}{eol}foo();{eol}class Box {{}}{eol}");
        send(&mut service, did_open_notification(uri, &source)).await;
        let symbols = send(&mut service, document_symbol_request(2, uri)).await;
        assert_eq!(
            symbols[0]["selectionRange"],
            range(&source, "foo"),
            "{eol:?}: {symbols}"
        );
        let (line, col) = position(&source, source.rfind("foo").unwrap() + 1);
        let hover = send(&mut service, hover_request(3, uri, line, col)).await;
        assert!(!hover.is_null(), "hover: {eol:?}");
        let definition = send(&mut service, definition_request(4, uri, line, col)).await;
        let definition = definition
            .as_array()
            .and_then(|a| a.first())
            .unwrap_or(&definition);
        assert_eq!(
            definition["range"],
            range(&source, "foo"),
            "definition {eol:?}: {definition}"
        );
        let refs = send(&mut service, references_request(5, uri, line, col, true)).await;
        assert_eq!(refs.as_array().unwrap().len(), 2, "{refs}");
        for reference in refs.as_array().unwrap() {
            assert_eq!(
                &source[offset(&source, &reference["range"]["start"])
                    ..offset(&source, &reference["range"]["end"])],
                "foo"
            );
        }
        let edit = send(&mut service, rename_request(6, uri, line, col, "bar")).await;
        assert_eq!(
            apply(&source, &edit["changes"][uri]),
            source.replace("foo", "bar")
        );
        let lenses = send(
            &mut service,
            Request::build("textDocument/codeLens")
                .id(7)
                .params(json!({"textDocument":{"uri":uri}}))
                .finish(),
        )
        .await;
        assert!(
            lenses
                .as_array()
                .unwrap()
                .iter()
                .any(|lens| lens["range"] == range(&source, "Box")),
            "{lenses}"
        );
        send(&mut service, shutdown_request(99)).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn line_model_bare_cr_completion_and_code_action_edits_target_the_right_line() {
    let mut service = service().await;
    let uri = "file:///line-actions.php";
    let source = "<?php\rclass Box { public function work():void {} }\r$obj=new Box;\r$obj->wo\r";
    send(&mut service, did_open_notification(uri, source)).await;
    let (line, col) = position(source, source.find("->wo").unwrap() + 4);
    let completion = send(&mut service, completion_request(2, uri, line, col)).await;
    let items = completion
        .as_array()
        .or_else(|| completion["items"].as_array())
        .unwrap();
    assert!(
        items.iter().any(|i| i["label"]
            .as_str()
            .is_some_and(|name| name.starts_with("work"))),
        "{completion}"
    );
    let source = "<?php\r/** @return int */\rfunction value() {return 1;}\r";
    send(&mut service, did_change_full_notification(uri, 2, source)).await;
    let (line, col) = at(source, "value");
    let actions = send(
        &mut service,
        code_action_request_with_only(
            3,
            uri,
            ((line, col), (line, col)),
            json!([]),
            vec!["refactor.rewrite"],
        ),
    )
    .await;
    let action = actions
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["title"] == "Add return type `int`")
        .unwrap_or_else(|| panic!("{actions}"));
    let resolved = send(&mut service, code_action_resolve_request(4, action.clone())).await;
    assert_eq!(
        apply(source, &resolved["edit"]["changes"][uri]),
        source.replace("value()", "value(): int")
    );
    send(&mut service, shutdown_request(99)).await;
}

fn validate_tokens(source: &str, result: &serde_json::Value) -> Vec<(u32, u32, u32)> {
    let mut row = 0;
    let mut col = 0;
    let mut tokens = Vec::new();
    let lines = starts(source);
    for token in result["data"].as_array().unwrap().chunks_exact(5) {
        let delta = token[0].as_u64().unwrap() as u32;
        if delta > 0 {
            row += delta;
            col = 0;
        }
        col += token[1].as_u64().unwrap() as u32;
        let len = token[2].as_u64().unwrap() as u32;
        assert!(
            (row as usize) < lines.len(),
            "token row outside source: {result}"
        );
        let (a, b) = lines[row as usize];
        assert!(
            col + len <= source[a..b].encode_utf16().count() as u32,
            "token crosses LSP line: {row}:{col}+{len}, {source:?}"
        );
        tokens.push((row, col, len));
    }
    tokens
}

#[tokio::test(flavor = "current_thread")]
async fn line_model_bare_cr_semantic_tokens_folds_and_selection_stay_within_client_lines() {
    let mut service = service().await;
    let uri = "file:///line-tokens.php";
    let source = "<?php\rfunction foo() {\r    echo \"😀 a\rb\";\r}\r";
    send(&mut service, did_open_notification(uri, source)).await;
    let result = send(&mut service, semantic_tokens_full_request(2, uri)).await;
    let tokens = validate_tokens(source, &result);
    let (line, col) = at(source, "foo");
    assert!(tokens.contains(&(line, col, 3)), "{tokens:?}");
    let folds = send(
        &mut service,
        Request::build("textDocument/foldingRange")
            .id(3)
            .params(json!({"textDocument":{"uri":uri}}))
            .finish(),
    )
    .await;
    assert!(
        folds
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["startLine"] == 1 && f["endLine"] == 4),
        "{folds}"
    );
    let selections = send(
        &mut service,
        selection_range_request(4, uri, vec![(line, col), (3, 0)]),
    )
    .await;
    assert_eq!(selections.as_array().unwrap().len(), 2);
    assert_eq!(selections[0]["range"], range(source, "foo"));
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn line_model_template_incremental_cr_edit_preserves_original_positions() {
    let mut service = service().await;
    let uri = "file:///line-template.blade.php";
    let source = "{{ $first }}\r{{ $other }}\r";
    send(&mut service, did_open_notification(uri, source)).await;
    send(&mut service,Request::build("textDocument/didChange").params(json!({"textDocument":{"uri":uri,"version":2},"contentChanges":[{"range":range(source,"$other"),"text":"$renamed"}]})).finish()).await;
    let expected = source.replace("$other", "$renamed");
    let result = send(&mut service, semantic_tokens_full_request(2, uri)).await;
    let tokens = validate_tokens(&expected, &result);
    assert!(tokens.contains(&(1, 3, 8)), "{tokens:?}");
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn line_model_bare_cr_phpdoc_virtual_definition_uses_the_attached_tag() {
    let mut service = service().await;
    let uri = "file:///line-phpdoc.php";
    let source = "<?php\r/**\r * @method int magic()\r */\rclass Box {}\r(new Box)->magic();\r";
    send(&mut service, did_open_notification(uri, source)).await;
    let (line, col) = position(source, source.rfind("magic").unwrap() + 1);
    let locations = send(&mut service, definition_request(2, uri, line, col)).await;
    let location = locations
        .as_array()
        .and_then(|a| a.first())
        .unwrap_or(&locations);
    assert_eq!(location["range"], range(source, "magic"), "{locations}");
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn line_model_bare_cr_import_fix_keeps_namespace_and_neighboring_statements() {
    let mut service = service().await;
    let uri = "file:///line-import.php";
    send(
        &mut service,
        did_open_notification(
            "file:///line-vendor.php",
            "<?php namespace Vendor; class Thing {}",
        ),
    )
    .await;
    let source = "<?php\rnamespace App;\r\rnew Thing();\r";
    send(&mut service, did_open_notification(uri, source)).await;
    let diagnostic = json!({"range":range(source,"Thing"),"source":"php-lsp","message":"Unknown class: App\\Thing"});
    let (line, col) = at(source, "Thing");
    let actions = send(
        &mut service,
        code_action_request(2, uri, line, col, line, col, json!([diagnostic])),
    )
    .await;
    let action = actions
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["title"] == "Import Vendor\\Thing")
        .unwrap_or_else(|| panic!("{actions}"));
    let edited = apply(source, &action["edit"]["changes"][uri]);
    assert!(edited.starts_with("<?php\rnamespace App;\r"), "{edited:?}");
    assert!(
        edited.find("use Vendor\\Thing;").unwrap() < edited.find("new Thing()").unwrap(),
        "{edited:?}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn line_model_bare_cr_on_type_indentation_targets_current_client_line() {
    let mut service = service().await;
    let uri = "file:///line-indent.php";
    let source = "<?php\rfunction f() {\rreturn 1;\r}\r";
    send(&mut service, did_open_notification(uri, source)).await;
    let edits=send(&mut service,Request::build("textDocument/onTypeFormatting").id(2).params(json!({"textDocument":{"uri":uri},"position":{"line":2,"character":0},"ch":"\n","options":{"tabSize":4,"insertSpaces":true}})).finish()).await;
    assert!(!edits.is_null(), "missing indentation");
    assert_eq!(
        apply(source, &edits),
        source.replace("return 1;", "    return 1;")
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn line_model_multi_edit_batch_rebases_positions_after_line_model_changes() {
    let mut service = service().await;
    let uri = "file:///line-batch.php";
    let source = "<?php\nfunction foo() {}\nfoo();\n";
    send(&mut service, did_open_notification(uri, source)).await;
    let intermediate = source.replacen("function", "/* a\u{2028}b */\rfunction", 1);
    let after_name = intermediate.replacen("foo", "bar", 1);
    let call_start = after_name.rfind("foo").unwrap();
    let (a, b) = position(&after_name, call_start);
    let (c, d) = position(&after_name, call_start + 3);
    send(&mut service,Request::build("textDocument/didChange").params(json!({"textDocument":{"uri":uri,"version":2},"contentChanges":[
        {"range":{"start":{"line":1,"character":0},"end":{"line":1,"character":0}},"text":"/* a\u{2028}b */\r"},
        {"range":range(&intermediate,"foo"),"text":"bar"},
        {"range":{"start":{"line":a,"character":b},"end":{"line":c,"character":d}},"text":"bar"}
    ]})).finish()).await;
    let expected = after_name.replacen("foo", "bar", 1);
    let symbols = send(&mut service, document_symbol_request(2, uri)).await;
    assert_eq!(symbols[0]["name"], "bar");
    assert_eq!(symbols[0]["selectionRange"], range(&expected, "bar"));
    let definition = send(&mut service, definition_request(3, uri, a, b)).await;
    let location = definition
        .as_array()
        .and_then(|a| a.first())
        .unwrap_or(&definition);
    assert_eq!(location["range"], range(&expected, "bar"));
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn line_model_signature_and_inlay_hints_use_both_components_of_cr_positions() {
    let mut service = service().await;
    let uri = "file:///line-hints.php";
    let source="<?php\rfunction sum(int $first, int $second):int {return $first+$second;}\r$result=sum(1, 2);\r";
    send(&mut service, did_open_notification(uri, source)).await;
    let (line, col) = position(source, source.find("2);").unwrap());
    let help = send(&mut service, signature_help_request(2, uri, line, col)).await;
    assert_eq!(help["activeParameter"], 1, "{help}");
    let hints = send(&mut service, inlay_hint_request(3, uri, 2, 0, 3, 0)).await;
    assert!(
        hints
            .as_array()
            .unwrap()
            .iter()
            .any(|hint| hint["label"] == "first:"),
        "{hints}"
    );
    for hint in hints.as_array().unwrap() {
        assert_eq!(hint["position"]["line"], 2, "{hint}");
        offset(source, &hint["position"]);
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn line_model_hierarchies_and_linked_editing_roundtrip_cr_ranges() {
    let mut service = service().await;
    let uri = "file:///line-hierarchy.php";
    let source="<?php\rfunction callee():void {}\rfunction caller():void {\r    callee();\r}\rclass Base {}\rclass Child extends Base {}\r";
    send(&mut service, did_open_notification(uri, source)).await;
    let (line, col) = at(source, "caller");
    let items = send(
        &mut service,
        prepare_call_hierarchy_request(2, uri, line, col),
    )
    .await;
    assert_eq!(
        items[0]["selectionRange"],
        range(source, "caller"),
        "{items}"
    );
    let calls = send(&mut service, outgoing_calls_request(3, items[0].clone())).await;
    assert_eq!(
        calls[0]["to"]["selectionRange"],
        range(source, "callee"),
        "{calls}"
    );
    assert_eq!(calls[0]["fromRanges"][0]["start"]["line"], 3);
    let (line, col) = at(source, "Child");
    let types = send(
        &mut service,
        prepare_type_hierarchy_request(4, uri, line, col),
    )
    .await;
    let parents = send(
        &mut service,
        type_hierarchy_supertypes_request(5, types[0].clone()),
    )
    .await;
    assert_eq!(
        parents[0]["selectionRange"],
        range(source, "Base"),
        "{parents}"
    );
    let source = "<?php\rnamespace App;\ruse Vendor\\Thing as Thing;\r";
    send(&mut service, did_change_full_notification(uri, 2, source)).await;
    let (line, col) = at(source, "Thing");
    let links = send(
        &mut service,
        linked_editing_range_request(6, uri, line, col),
    )
    .await;
    assert_eq!(links["ranges"].as_array().unwrap().len(), 2, "{links}");
    for range in links["ranges"].as_array().unwrap() {
        assert_eq!(range["start"]["line"], 2);
        assert_eq!(
            &source[offset(source, &range["start"])..offset(source, &range["end"])],
            "Thing"
        );
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn line_model_native_diagnostic_publication_preserves_cr_rows_without_double_conversion() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(request) = socket.next().await {
            let _ = tx.send(request);
        }
    });
    send(
        &mut service,
        initialize_request_with_options(
            1,
            None,
            Some(
                json!({"stubExtensions":[],"indexVendor":false,"diagnosticsMode":"basic-semantic"}),
            ),
        ),
    )
    .await;
    let uri = "file:///line-diagnostics.php";
    let source = "<?php\r/* 😀 */ function run(UnknownType $value):void {}\r";
    send(&mut service, did_open_notification(uri, source)).await;
    send(
        &mut service,
        Request::build("textDocument/didSave")
            .params(json!({"textDocument":{"uri":uri}}))
            .finish(),
    )
    .await;
    let published = next_publish_diagnostics(&mut rx, uri, Duration::from_secs(3)).await;
    let diagnostic = published["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| {
            d["message"]
                .as_str()
                .is_some_and(|m| m.starts_with("Unused parameter:"))
        })
        .unwrap_or_else(|| panic!("{published}"));
    assert_eq!(diagnostic["range"], range(source, "$value"));
    let source = "<?php\rfunction broken( {}\r";
    send(&mut service, did_change_full_notification(uri, 2, source)).await;
    send(
        &mut service,
        Request::build("textDocument/didSave")
            .params(json!({"textDocument":{"uri":uri}}))
            .finish(),
    )
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "missing version 2 diagnostics"
        );
        let published = next_publish_diagnostics(&mut rx, uri, Duration::from_secs(3)).await;
        if published["version"] != 2 {
            continue;
        }
        let diagnostics = published["diagnostics"].as_array().unwrap();
        assert!(!diagnostics.is_empty());
        for diagnostic in diagnostics {
            assert!(diagnostic["range"]["start"]["line"].as_u64().unwrap() >= 1);
            offset(source, &diagnostic["range"]["start"]);
            offset(source, &diagnostic["range"]["end"]);
        }
        break;
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn line_model_external_edits_cannot_bypass_the_bare_cr_guard_via_legacy_metadata() {
    let mut service = service().await;
    let uri = "file:///line-external.php";
    for (version, eol) in [(2, "\n"), (3, "\r\n"), (4, "\r")] {
        let source = format!("<?php{eol}fail();{eol}");
        if version == 2 {
            send(&mut service, did_open_notification(uri, &source)).await;
        } else {
            send(
                &mut service,
                did_change_full_notification(uri, version, &source),
            )
            .await;
        }
        for analyzer in ["phpstan", "psalm"] {
            let diagnostic = json!({"range":range(&source,"fail"),"source":analyzer,"code":"InvalidArgument","message":"fixture","data":{"phpLsp":{"replacement":{"newText":"replacement"}}}});
            let actions = send(
                &mut service,
                code_action_request(version as i64 + 10, uri, 1, 0, 1, 4, json!([diagnostic])),
            )
            .await;
            let actions = actions.as_array().unwrap();
            if eol != "\r" {
                assert!(
                    actions
                        .iter()
                        .any(|a| a["title"] == "Replace with `replacement`"),
                    "{actions:?}"
                );
            }
            assert_eq!(
                !actions.is_empty(),
                eol != "\r",
                "{eol:?}: {analyzer} {actions:?}"
            );
        }
    }
    send(&mut service, shutdown_request(99)).await;
}
