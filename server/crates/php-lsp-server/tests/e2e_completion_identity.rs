mod support;
use support::*;
const URI: &str = "file:///test/p237-consumer.php";
const VENDOR:&str="<?php\nnamespace Vendor {\nclass Service {}\nfunction helper(): void {}\nconst VALUE = 1; }\nnamespace Other {\nclass Service {}\nfunction helper(): void {}\nconst VALUE = 2; }\nnamespace { class GlobalName {} function global_helper() {} const GLOBAL_VALUE=3; }";
async fn send(service: &mut LspService<PhpLspBackend>, request: Request) -> serde_json::Value {
    match service.ready().await.unwrap().call(request).await.unwrap() {
        Some(response) => {
            assert!(response.error().is_none(), "{response:?}");
            extract_result(Some(response))
        }
        None => serde_json::Value::Null,
    }
}
async fn service(source: &str) -> LspService<PhpLspBackend> {
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
    send(
        &mut service,
        did_open_notification("file:///test/p237-vendor.php", VENDOR),
    )
    .await;
    send(&mut service, did_open_notification(URI, source)).await;
    service
}
async fn completion(
    service: &mut LspService<PhpLspBackend>,
    source: &str,
    needle: &str,
) -> serde_json::Value {
    let (line, column) = utf16_position_after(source, needle);
    send(service, completion_request(2, URI, line, column)).await
}
fn candidate(response: &serde_json::Value, fqn: &str) -> Option<serde_json::Value> {
    completion_items_from_result(response)
        .into_iter()
        .find(|item| item["data"]["fqn"] == fqn)
}
#[tokio::test(flavor = "current_thread")]
async fn p237_framework_markdown_survives_completion_and_resolve() {
    let mut service = service("<?php App\\Post::pub").await;
    send(&mut service,did_open_notification("file:///test/p237-model.php","<?php namespace Illuminate\\Database\\Eloquent { class Model {} } namespace App { class Post extends \\Illuminate\\Database\\Eloquent\\Model { public function scopePublished($query) {} } }")).await;
    let result = completion(&mut service, "<?php App\\Post::pub", "::pub").await;
    let item = completion_items_from_result(&result)
        .into_iter()
        .find(|item| item["label"] == "published")
        .expect("framework scope candidate")
        .clone();
    assert_eq!(item["data"]["kind"], "framework-virtual-member", "{item}");
    assert_eq!(item["documentation"]["kind"], "markdown", "{item}");
    let text = item["documentation"]["value"].as_str().unwrap();
    assert!(
        text.contains("@method") && text.contains("published"),
        "{text}"
    );
    let resolved = send(&mut service, completion_resolve_request(3, item.clone())).await;
    assert_eq!(resolved["documentation"], item["documentation"]);
    send(&mut service, shutdown_request(99)).await;
}
#[tokio::test(flavor = "current_thread")]
async fn p237_conflicting_class_function_and_constant_short_candidates_are_hidden() {
    let source="<?php\r\nnamespace App;\r\nuse Other\\Service as service;\r\nuse function Other\\helper as HELPER;\r\nuse const Other\\VALUE;\r\nfunction page() { /* 😀 */ new Ser; hel(); VAL; }";
    let mut service = service(source).await;
    for (needle, fqn) in [
        ("new Ser", "Vendor\\Service"),
        (" hel", "Vendor\\helper"),
        (" VAL", "Vendor\\VALUE"),
    ] {
        let result = completion(&mut service, source, needle).await;
        assert!(candidate(&result, fqn).is_none(), "unsafe {fqn}: {result}");
    }
    send(&mut service, shutdown_request(99)).await;
}
#[tokio::test(flavor = "current_thread")]
async fn p237_existing_renamed_import_is_the_inserted_name() {
    let source="<?php\nnamespace App;\nuse Vendor\\Service as Selected;\nuse function Vendor\\helper as selected_helper;\nuse const Vendor\\VALUE as SELECTED_VALUE;\nfunction page() { new Ser; hel(); VAL; }";
    let mut service = service(source).await;
    for (needle, fqn, name) in [
        ("new Ser", "Vendor\\Service", "Selected"),
        (" hel", "Vendor\\helper", "selected_helper"),
        (" VAL", "Vendor\\VALUE", "SELECTED_VALUE"),
    ] {
        let result = completion(&mut service, source, needle).await;
        let item = candidate(&result, fqn).expect("existing imported symbol");
        assert_eq!(item["insertText"], name, "{item}");
        assert!(
            item["additionalTextEdits"]
                .as_array()
                .is_none_or(Vec::is_empty),
            "duplicate import: {item}"
        );
    }
    send(&mut service, shutdown_request(99)).await;
}
#[tokio::test(flavor = "current_thread")]
async fn p237_global_symbol_candidates_do_not_bypass_import_collisions() {
    let source="<?php\nnamespace App;\nuse Other\\Service as GlobalName;\nuse function Other\\helper as global_helper;\nuse const Other\\VALUE as GLOBAL_VALUE;\nfunction page() { new Glo; global_h(); GLOBAL_V; }";
    let mut service = service(source).await;
    for (needle, fqn) in [
        ("new Glo", "GlobalName"),
        (" global_h", "global_helper"),
        (" GLOBAL_V", "GLOBAL_VALUE"),
    ] {
        let result = completion(&mut service, source, needle).await;
        assert!(
            candidate(&result, fqn).is_none(),
            "unsafe global {fqn}: {result}"
        );
    }
    send(&mut service, shutdown_request(99)).await;
}
#[tokio::test(flavor = "current_thread")]
async fn p237_collision_does_not_leak_between_repeated_namespace_sections() {
    let source="<?php\nnamespace Same {\nuse Other\\Service;\nfunction first(){ new Ser; }\n}\nnamespace OtherScope {\nfunction second(){ new Ser; }\n}\nnamespace Same {\nfunction third(){ new Ser; }\n}";
    let mut service = service(source).await;
    assert!(candidate(
        &completion(&mut service, source, "first(){ new Ser").await,
        "Vendor\\Service"
    )
    .is_none());
    for needle in ["second(){ new Ser", "third(){ new Ser"] {
        let result = completion(&mut service, source, needle).await;
        let item = candidate(&result, "Vendor\\Service").expect("independent namespace import");
        assert_eq!(
            item["additionalTextEdits"][0]["newText"],
            "use Vendor\\Service;\n\n"
        );
    }
    send(&mut service, shutdown_request(99)).await;
}

fn offset(source: &str, position: &serde_json::Value) -> usize {
    let line = position["line"].as_u64().unwrap() as usize;
    let column = position["character"].as_u64().unwrap() as usize;
    let start = source
        .split_inclusive('\n')
        .take(line)
        .map(str::len)
        .sum::<usize>();
    let mut units = 0;
    for (rel, c) in source[start..].char_indices() {
        if units == column {
            return start + rel;
        }
        assert!(c != '\n', "edit exceeds line");
        units += c.len_utf16();
        assert!(units <= column, "half surrogate edit");
    }
    assert_eq!(units, column);
    source.len()
}
fn apply_item(source: &str, item: &serde_json::Value) -> String {
    let mut edits = item["additionalTextEdits"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    edits.push(item["textEdit"].clone());
    let mut edits = edits
        .iter()
        .map(|edit| {
            (
                offset(source, &edit["range"]["start"]),
                offset(source, &edit["range"]["end"]),
                edit["newText"].as_str().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    edits.sort_by_key(|e| e.0);
    let mut previous_end = None;
    for &(start, end, _) in &edits {
        assert!(start <= end);
        if let Some(previous) = previous_end {
            assert!(start > previous, "overlapping completion edits");
        }
        previous_end = Some(end);
    }
    let mut result = source.to_string();
    for (start, end, text) in edits.into_iter().rev() {
        result.replace_range(start..end, text);
    }
    result
}
async fn assert_accepted(source: &str, needle: &str, fqn: &str) {
    let mut service = service(source).await;
    let response = completion(&mut service, source, needle).await;
    let item = candidate(&response, fqn).unwrap_or_else(|| panic!("missing {fqn}: {response}"));
    let changed = apply_item(source, &item);
    let mut parser = php_lsp_parser::parser::FileParser::new();
    parser.parse_full(&changed);
    assert!(
        !parser.tree().unwrap().root_node().has_error(),
        "edits damaged PHP: {changed}"
    );
    send(&mut service, did_change_full_notification(URI, 2, &changed)).await;
    let name = item["textEdit"]["newText"].as_str().unwrap();
    let at = changed.rfind(name).unwrap() + name.len() - 1;
    let (line, column) = utf16_position_for_offset(&changed, at);
    let definition = send(&mut service, definition_request(4, URI, line, column)).await;
    let location = definition
        .as_array()
        .and_then(|v| v.first())
        .unwrap_or(&definition);
    assert_eq!(
        location["uri"], "file:///test/p237-vendor.php",
        "{fqn}: wrong accepted target: {definition}\n{changed}"
    );
    let mut library = php_lsp_parser::parser::FileParser::new();
    library.parse_full(VENDOR);
    let symbols = php_lsp_parser::symbols::extract_file_symbols(
        library.tree().unwrap(),
        VENDOR,
        "file:///test/p237-vendor.php",
    );
    let symbol = symbols.symbols.iter().find(|s| s.fqn == fqn).unwrap();
    let range = php_lsp_parser::utf16::range_byte_to_utf16(VENDOR, symbol.selection_range);
    assert_eq!(
        location["range"],
        json!({"start":{"line":range.0,"character":range.1},"end":{"line":range.2,"character":range.3}})
    );
    let resolved = send(&mut service, completion_resolve_request(5, item.clone())).await;
    assert_eq!(resolved["textEdit"], item["textEdit"]);
    assert_eq!(resolved["additionalTextEdits"], item["additionalTextEdits"]);
    send(&mut service, shutdown_request(99)).await;
}
#[tokio::test(flavor = "current_thread")]
async fn p237_acceptance_preserves_alias_and_auto_import_identity_with_utf16_crlf() {
    for (imports, expression, needle, fqn) in [
        ("", "new Ser;", "new Ser", "Vendor\\Service"),
        (
            "use Vendor\\Service as Selected;",
            "new SerZZ;",
            "new Ser",
            "Vendor\\Service",
        ),
        (
            "use function Vendor\\helper as selected_helper;",
            "hel();",
            "hel",
            "Vendor\\helper",
        ),
        (
            "use const Vendor\\VALUE as SELECTED_VALUE;",
            "VAL;",
            "VAL",
            "Vendor\\VALUE",
        ),
        (
            "use Other\\Service as helper;",
            "hel();",
            "hel",
            "Vendor\\helper",
        ),
        (
            "use const Other\\VALUE as value;",
            "VAL;",
            "VAL",
            "Vendor\\VALUE",
        ),
    ] {
        let source = format!(
            "<?php\r\nnamespace App;\r\n{imports}\r\nfunction page() {{ /* 😀 */ {expression} }}"
        );
        assert_accepted(&source, &format!("/* 😀 */ {needle}"), fqn).await;
    }
}
#[tokio::test(flavor = "current_thread")]
async fn p237_qualified_completion_keeps_its_qualifier_without_auto_imports() {
    for expression in ["new \\Vendor\\Ser;", "new Vendor\\Ser;"] {
        let source=format!("<?php\nnamespace App;\nuse Vendor;\nuse Other\\Service;\nfunction page() {{ {expression} }}");
        assert_accepted(&source, expression.trim_end_matches(';'), "Vendor\\Service").await;
    }
    let source = "<?php\nnamespace App;\nfunction page(){ new Vendor\\Ser; }";
    let mut service = service(source).await;
    let result = completion(&mut service, source, "new Vendor\\Ser").await;
    assert!(
        candidate(&result, "Vendor\\Service").is_none(),
        "relative name was treated as absolute: {result}"
    );
    send(&mut service, shutdown_request(99)).await;
}
#[tokio::test(flavor = "current_thread")]
async fn p237_inline_or_recovery_layout_does_not_put_import_in_an_anonymous_class_or_comment() {
    for source in [
        "<?php namespace App; new Ser;",
        "<?php\nnamespace App; $o = new class {\n};\nnew Ser;",
        "<?php\nnamespace App; /* header comment\ncontinues */\nnew Ser;",
        "<?php\nnamespace App;\nuse Other\\Service as OtherService; /* comment\ncontinues */\nnew Ser;",
    ] {assert_accepted(source,"new Ser","Vendor\\Service").await;}
}

#[tokio::test(flavor = "current_thread")]
async fn p237_strict_types_declare_remains_the_first_statement_after_completion() {
    for declaration in ["declare(strict_types=1);", "declare(\nstrict_types=1\n);"] {
        let source = format!("<?php\n{declaration}\nnew Ser;");
        let mut service = service(&source).await;
        let response = completion(&mut service, &source, "new Ser").await;
        let item = candidate(&response, "Vendor\\Service").unwrap();
        assert!(
            item["additionalTextEdits"]
                .as_array()
                .is_none_or(Vec::is_empty),
            "import moved strict_types from first position: {item}"
        );
        assert_eq!(item["textEdit"]["newText"], "\\Vendor\\Service");
        send(&mut service, shutdown_request(99)).await;
        assert_accepted(&source, "new Ser", "Vendor\\Service").await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn p237_multiline_group_and_comma_imports_remain_valid_after_acceptance() {
    for imports in [
        "use Other\\{\n Service as OtherService,\n Missing\n};",
        "use Other\\Service as OtherService,\n Other\\Missing;",
    ] {
        let source = format!("<?php\nnamespace App;\n{imports}\nnew Ser;");
        assert_accepted(&source, "new Ser", "Vendor\\Service").await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn p237_local_declarations_hide_foreign_short_names_but_keep_the_local_symbol() {
    let source="<?php\nnamespace App;\nclass Service {}\nfunction helper() {}\nconst VALUE=1;\nfunction page(){ new Ser; hel(); VAL; }";
    let mut service = service(source).await;
    for (needle, short) in [
        ("new Ser", "Service"),
        (" hel", "helper"),
        (" VAL", "VALUE"),
    ] {
        let result = completion(&mut service, source, needle).await;
        assert!(candidate(&result, &format!("Vendor\\{short}")).is_none());
        let item = candidate(&result, &format!("App\\{short}")).unwrap();
        assert_eq!(item["insertText"], short);
        assert!(item["additionalTextEdits"]
            .as_array()
            .is_none_or(Vec::is_empty));
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p237_completion_edit_reopen_orders_preserve_candidate_identity() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let source =
            "<?php\r\nnamespace App;\r\n/* unoccupied */\r\nfunction page(){ /* 😀 */ new Ser; }";
        let occupied = source.replace("/* unoccupied */", "use Other\\Service;");
        let mut service = service(source).await;
        let baseline = candidate(
            &completion(&mut service, source, "new Ser").await,
            "Vendor\\Service",
        )
        .unwrap();
        let (line, column) = utf16_position_after(source, "new Ser");
        for round in 0..20 {
            let query = service
                .ready()
                .await
                .unwrap()
                .call(completion_request(20, URI, line, column));
            let edit = service
                .ready()
                .await
                .unwrap()
                .call(did_change_full_notification(URI, 2 + round, &occupied));
            let response = if round % 2 == 0 {
                let (q, e) = tokio::join!(biased;query,edit);
                e.unwrap();
                q.unwrap()
            } else {
                let (e, q) = tokio::join!(biased;edit,query);
                e.unwrap();
                q.unwrap()
            };
            let result = extract_result(response);
            let item = candidate(&result, "Vendor\\Service");
            assert!(
                item.is_none() || item.as_ref() == Some(&baseline),
                "mixed completion edit plan: {item:?}"
            );
            assert!(candidate(
                &completion(&mut service, &occupied, "new Ser").await,
                "Vendor\\Service"
            )
            .is_none());
            send(&mut service, did_close_notification(URI)).await;
            send(&mut service, did_open_notification(URI, source)).await;
            assert_eq!(
                candidate(
                    &completion(&mut service, source, "new Ser").await,
                    "Vendor\\Service"
                ),
                Some(baseline.clone())
            );
        }
        send(&mut service, shutdown_request(99)).await;
    })
    .await
    .expect("bounded completion lifecycle rounds");
}
