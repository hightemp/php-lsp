mod support;

use support::*;

fn utf16_position_at(source: &str, needle: &str) -> (u32, u32) {
    let offset = source
        .find(needle)
        .unwrap_or_else(|| panic!("needle `{needle}` not found"));
    utf16_position_for_offset(source, offset)
}

fn utf16_position_after(source: &str, needle: &str) -> (u32, u32) {
    let offset = source
        .find(needle)
        .unwrap_or_else(|| panic!("needle `{needle}` not found"))
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

fn assert_lsp_range_start(value: &serde_json::Value, expected: (u32, u32), context: &str) {
    assert_lsp_field_range_start(value, "range", expected, context);
}

fn assert_lsp_selection_range_start(
    value: &serde_json::Value,
    expected: (u32, u32),
    context: &str,
) {
    assert_lsp_field_range_start(value, "selectionRange", expected, context);
}

fn assert_lsp_field_range_start(
    value: &serde_json::Value,
    field: &str,
    expected: (u32, u32),
    context: &str,
) {
    assert_eq!(
        value[field]["start"]["line"].as_u64(),
        Some(expected.0 as u64),
        "{context}: wrong start line in {value}"
    );
    assert_eq!(
        value[field]["start"]["character"].as_u64(),
        Some(expected.1 as u64),
        "{context}: wrong start character in {value}"
    );
}

fn find_document_symbol<'a>(
    symbols: &'a [serde_json::Value],
    name: &str,
) -> Option<&'a serde_json::Value> {
    for symbol in symbols {
        if symbol.get("name").and_then(|value| value.as_str()) == Some(name) {
            return Some(symbol);
        }
        if let Some(children) = symbol.get("children").and_then(|value| value.as_array()) {
            if let Some(found) = find_document_symbol(children, name) {
                return Some(found);
            }
        }
    }
    None
}

fn find_workspace_symbol<'a>(
    symbols: &'a [serde_json::Value],
    name: &str,
) -> Option<&'a serde_json::Value> {
    symbols
        .iter()
        .find(|symbol| symbol.get("name").and_then(|value| value.as_str()) == Some(name))
}

fn find_code_lens_for_fqn<'a>(
    lenses: &'a [serde_json::Value],
    fqn: &str,
) -> Option<&'a serde_json::Value> {
    lenses
        .iter()
        .find(|lens| lens["data"]["fqn"].as_str() == Some(fqn))
}

fn location_starts_with(location: &serde_json::Value, expected: (u32, u32)) -> bool {
    location["range"]["start"]["line"].as_u64() == Some(expected.0 as u64)
        && location["range"]["start"]["character"].as_u64() == Some(expected.1 as u64)
}

#[tokio::test(flavor = "current_thread")]
async fn test_lsp_ranges_are_utf16_after_non_ascii_prefixes() {
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

/* 🇺🇸 👨‍👩‍👧‍👦 👍🏽 ❤️ é བོད контракт */ interface Contract {
    /* 🇺🇸 👨‍👩‍👧‍👦 👍🏽 ❤️ é བོད контрактный метод */ public function makeTarget(): Target;
}

/* 🇺🇸 👨‍👩‍👧‍👦 👍🏽 ❤️ é བོད класс */ class Target {
    /* 🇺🇸 👨‍👩‍👧‍👦 👍🏽 ❤️ é བོད свойство */ public int $value = 0;

    /* 🇺🇸 👨‍👩‍👧‍👦 👍🏽 ❤️ é བོད метод */ public function callMe(): void {}
}

/* 🇺🇸 👨‍👩‍👧‍👦 👍🏽 ❤️ é བོད реализация */ class TargetImpl implements Contract {
    /* 🇺🇸 👨‍👩‍👧‍👦 👍🏽 ❤️ é བོད метод */ public function makeTarget(): Target { return new Target(); }
}

/* 🇺🇸 👨‍👩‍👧‍👦 👍🏽 ❤️ é བོད переменная */ $usage = new Target();
/* 🇺🇸 👨‍👩‍👧‍👦 👍🏽 ❤️ é བོད вызов */ $usage->callMe();
/* 🇺🇸 👨‍👩‍👧‍👦 👍🏽 ❤️ é བོད тип */ $made = (new TargetImpl())->makeTarget();
"#;
    let uri = "file:///test/utf16-ranges.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let class_keyword = utf16_position_at(code, "class Target");
    let class_name = utf16_position_after(code, "class ");
    let contract_method_name = utf16_position_at(code, "makeTarget(): Target;");
    let impl_method_name = utf16_position_at(code, "makeTarget(): Target {");
    let method_keyword = utf16_position_at(code, "public function callMe");
    let method_name = utf16_position_at(code, "callMe(): void");
    let property_keyword = utf16_position_at(code, "public int $value");
    let property_name = utf16_position_at(code, "$value");
    let usage_class_name = utf16_position_after(code, "new ");
    let usage_method_name = utf16_position_at(code, "callMe();");
    let type_definition_call = utf16_position_at(code, "makeTarget();");
    let usage_variable = utf16_position_at(code, "$usage =");
    let usage_variable_call = utf16_position_at(code, "$usage->");
    let made_variable_end = utf16_position_after(code, "$made");

    let definition_result = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(definition_request(
                2,
                uri,
                usage_class_name.0,
                usage_class_name.1,
            ))
            .await
            .unwrap(),
    );
    assert_lsp_range_start(&definition_result, class_name, "definition");

    let declaration_result = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(declaration_request(
                10,
                uri,
                usage_class_name.0,
                usage_class_name.1,
            ))
            .await
            .unwrap(),
    );
    assert_lsp_range_start(&declaration_result, class_name, "declaration");

    let type_definition_result = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(type_definition_request(
                11,
                uri,
                type_definition_call.0,
                type_definition_call.1,
            ))
            .await
            .unwrap(),
    );
    assert_lsp_range_start(&type_definition_result, class_name, "typeDefinition");

    let implementation_result = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(implementation_request(
                12,
                uri,
                contract_method_name.0,
                contract_method_name.1,
            ))
            .await
            .unwrap(),
    );
    let implementations = implementation_result
        .as_array()
        .expect("implementation should return an array");
    assert!(
        implementations
            .iter()
            .any(|location| location_starts_with(location, impl_method_name)),
        "implementation should point to TargetImpl::makeTarget UTF-16 range, got: {implementation_result}"
    );

    let references_result = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(references_request(
                13,
                uri,
                class_name.0,
                class_name.1,
                true,
            ))
            .await
            .unwrap(),
    );
    let references = references_result
        .as_array()
        .expect("references should return an array");
    assert!(
        references
            .iter()
            .any(|location| location_starts_with(location, class_name)),
        "references should include declaration UTF-16 range, got: {references_result}"
    );
    assert!(
        references
            .iter()
            .any(|location| location_starts_with(location, usage_class_name)),
        "references should include usage UTF-16 range, got: {references_result}"
    );

    let hover_result = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(hover_request(
                3,
                uri,
                usage_method_name.0,
                usage_method_name.1,
            ))
            .await
            .unwrap(),
    );
    assert_lsp_range_start(&hover_result, usage_method_name, "hover");

    let document_symbols_result = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(document_symbol_request(4, uri))
            .await
            .unwrap(),
    );
    let document_symbols = document_symbols_result
        .as_array()
        .expect("documentSymbol should return an array");
    let class_symbol =
        find_document_symbol(document_symbols, "Target").expect("Target document symbol");
    assert_lsp_range_start(class_symbol, class_keyword, "documentSymbol class range");
    assert_lsp_selection_range_start(
        class_symbol,
        class_name,
        "documentSymbol class selectionRange",
    );
    let method_symbol =
        find_document_symbol(document_symbols, "callMe").expect("callMe document symbol");
    assert_lsp_range_start(method_symbol, method_keyword, "documentSymbol method range");
    assert_lsp_selection_range_start(
        method_symbol,
        method_name,
        "documentSymbol method selectionRange",
    );
    let property_symbol =
        find_document_symbol(document_symbols, "value").expect("value document symbol");
    assert_lsp_range_start(
        property_symbol,
        property_keyword,
        "documentSymbol property range",
    );
    assert_lsp_selection_range_start(
        property_symbol,
        property_name,
        "documentSymbol property selectionRange",
    );

    let workspace_symbols_result = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(workspace_symbol_request(5, "Target"))
            .await
            .unwrap(),
    );
    let workspace_symbols = workspace_symbols_result
        .as_array()
        .expect("workspace/symbol should return an array");
    assert_lsp_range_start(
        &find_workspace_symbol(workspace_symbols, "Target").expect("Target workspace symbol")
            ["location"],
        class_keyword,
        "workspaceSymbol",
    );

    let code_lens_result = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(code_lens_request(6, uri))
            .await
            .unwrap(),
    );
    let code_lenses = code_lens_result
        .as_array()
        .expect("codeLens should return an array");
    assert_lsp_range_start(
        find_code_lens_for_fqn(code_lenses, "App\\Target::callMe").expect("callMe code lens"),
        method_name,
        "codeLens",
    );

    let call_hierarchy_result = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(prepare_call_hierarchy_request(
                7,
                uri,
                method_name.0,
                method_name.1,
            ))
            .await
            .unwrap(),
    );
    let call_item = &call_hierarchy_result
        .as_array()
        .expect("prepareCallHierarchy should return an array")[0];
    assert_lsp_range_start(call_item, method_keyword, "callHierarchy range");
    assert_lsp_selection_range_start(call_item, method_name, "callHierarchy selectionRange");

    let type_hierarchy_result = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(prepare_type_hierarchy_request(
                8,
                uri,
                class_name.0,
                class_name.1,
            ))
            .await
            .unwrap(),
    );
    let type_item = &type_hierarchy_result
        .as_array()
        .expect("prepareTypeHierarchy should return an array")[0];
    assert_lsp_range_start(type_item, class_keyword, "typeHierarchy range");
    assert_lsp_selection_range_start(type_item, class_name, "typeHierarchy selectionRange");

    let inlay_result = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(inlay_hint_request(14, uri, 0, 0, 99, 0))
            .await
            .unwrap(),
    );
    let inlay_hints = inlay_result
        .as_array()
        .expect("inlayHint should return array");
    assert!(
        inlay_hints.iter().any(|hint| {
            inlay_hint_label_text(hint).as_deref() == Some(": Target")
                && hint["position"]["line"].as_u64() == Some(made_variable_end.0 as u64)
                && hint["position"]["character"].as_u64() == Some(made_variable_end.1 as u64)
        }),
        "inlay hints should place inferred type after $made using UTF-16 range, got: {inlay_result}"
    );

    let rename_result = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(rename_request(
                9,
                uri,
                usage_variable.0,
                usage_variable.1,
                "$renamed",
            ))
            .await
            .unwrap(),
    );
    let edits = rename_result["changes"][uri]
        .as_array()
        .expect("rename should return text edits");
    let edit_starts: Vec<_> = edits
        .iter()
        .map(|edit| {
            (
                edit["range"]["start"]["line"].as_u64().unwrap() as u32,
                edit["range"]["start"]["character"].as_u64().unwrap() as u32,
            )
        })
        .collect();
    assert!(
        edit_starts.contains(&usage_variable),
        "rename should edit declaration UTF-16 range, got: {rename_result}"
    );
    assert!(
        edit_starts.contains(&usage_variable_call),
        "rename should edit usage UTF-16 range, got: {rename_result}"
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
async fn test_completion_text_edit_range_survives_non_ascii_context() {
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
    let app_uri = "file:///test/CompletionUtf16Edit.php";
    let app_code = r#"<?php
namespace App;

class Demo {
    public function run(): void {
        /* 🇺🇸 👨‍👩‍👧‍👦 👍🏽 ❤️ é བོད префикс */ Ser
    }
}
"#;

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(vendor_uri, vendor_code))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(app_uri, app_code))
        .await
        .unwrap();

    let completion_position = utf16_position_after(app_code, "Ser");
    let completion_result = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(completion_request(
                2,
                app_uri,
                completion_position.0,
                completion_position.1,
            ))
            .await
            .unwrap(),
    );
    let completion_items = completion_items_from_result(&completion_result);
    let service_item = completion_items
        .iter()
        .find(|item| item.get("label").and_then(|value| value.as_str()) == Some("Service"))
        .unwrap_or_else(|| panic!("expected Service completion, got: {completion_items:?}"));
    let edits = service_item["additionalTextEdits"]
        .as_array()
        .expect("Service completion should include auto-import edit");
    assert_eq!(
        edits[0]["newText"].as_str(),
        Some("use Vendor\\Service;\n"),
        "completion text edit should import Vendor\\Service, got: {service_item}"
    );
    assert_lsp_range_start(&edits[0], (2, 0), "completion additionalTextEdit");

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_template_source_map_ranges_are_utf16_after_non_ascii_prefix() {
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

    let php_uri = "file:///test/BladeUser.php";
    let blade_uri = "file:///test/show.blade.php";
    let php_code = "<?php\nclass User {}\n";
    let blade = "🇺🇸 👨‍👩‍👧‍👦 👍🏽 ❤️ é བོད {{ User::class }}\n";
    let user_position = utf16_position_at(blade, "User::class");

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(php_uri, php_code))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification_with_language(
            blade_uri, "blade", blade,
        ))
        .await
        .unwrap();

    let hover_result = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(hover_request(
                2,
                blade_uri,
                user_position.0,
                user_position.1,
            ))
            .await
            .unwrap(),
    );
    assert_lsp_range_start(&hover_result, user_position, "Blade hover source-map range");

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}
