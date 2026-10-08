mod support;
use support::*;

async fn send(service: &mut LspService<PhpLspBackend>, request: Request) -> serde_json::Value {
    let response = service.ready().await.unwrap().call(request).await.unwrap();
    response
        .map(|response| {
            assert!(response.error().is_none(), "{response:?}");
            extract_result(Some(response))
        })
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
            Some(json!({"stubExtensions": [], "diagnosticsMode": "off", "indexVendor": false})),
        ),
    )
    .await;
    service
}

async fn assert_foreach_owner(
    service: &mut LspService<PhpLspBackend>,
    uri: &str,
    code: &str,
    class: &str,
) {
    let (line, column) = utf16_position_at(code, "$item;");
    let hover = send(service, hover_request(10, uri, line, column + 1)).await;
    let markdown = hover_markdown_value(&hover);
    assert!(
        markdown.contains(&format!("{class} $item")),
        "wrong lexical owner hover: {markdown}"
    );
    assert!(
        !markdown.contains("Decoy"),
        "unrelated class leaked into hover: {markdown}"
    );
    let declaration = ["class", "trait", "enum"]
        .into_iter()
        .map(|kind| format!("{kind} {class}"))
        .find(|declaration| code.contains(declaration))
        .expect("type declaration");
    let (definition_line, _) = utf16_position_at(code, &declaration);
    assert!(
        markdown.contains(&format!("{uri}#L{}", definition_line + 1)),
        "missing actual class link: {markdown}"
    );
    let (end_line, end_col) = utf16_position_for_offset(code, code.len());
    let hints = send(
        service,
        inlay_hint_request(11, uri, 0, 0, end_line, end_col),
    )
    .await;
    let offset = code.find("$item)").unwrap() + "$item".len();
    let (item_line, item_column) = utf16_position_for_offset(code, offset);
    let hint = hints
        .as_array()
        .unwrap()
        .iter()
        .find(|hint| {
            hint["position"] == json!({"line": item_line,"character": item_column})
                && hint["kind"] == 1
        })
        .expect("foreach item type hint");
    assert_eq!(
        inlay_hint_label_text(hint).as_deref(),
        Some(format!(": {class}").as_str()),
        "wrong foreach type hint: {hint}"
    );
    let parts = hint["label"].as_array().expect("navigable type label");
    let location = parts
        .iter()
        .find_map(|part| part.get("location"))
        .expect("class type location");
    assert_eq!(location["uri"], uri);
    assert_eq!(location["range"]["start"]["line"], definition_line);
}

#[tokio::test(flavor = "current_thread")]
async fn foreach_self_hover_and_inlay_use_second_class_and_exact_type_link() {
    let mut service = service().await;
    let uri = "file:///test/multi-class-self-owner.php";
    let code = "<?php namespace App;\nclass Decoy {}\nclass Actual {\nfunction run() {\n/** @var array<int, self> $items */\n$items = [];\nforeach ($items as $item) { /* 😀 */ $item; }\n}\n}";
    send(&mut service, did_open_notification(uri, code)).await;
    assert_foreach_owner(&mut service, uri, code, "Actual").await;
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn foreach_static_hover_and_inlay_preserve_owner_after_crlf_unsaved_changes() {
    let mut service = service().await;
    let uri = "file:///test/multi-class-static-owner.php";
    let code = "<?php namespace App;\nclass Decoy {}\nclass Actual {\nfunction run() {\n/** @var array<int, static> $items */\n$items = [];\nforeach ($items as $item) { /* 😀 */ $item; }\n}\n}";
    send(&mut service, did_open_notification(uri, code)).await;
    assert_foreach_owner(&mut service, uri, code, "Actual").await;
    let changed = code
        .replace("class Actual", "class Updated")
        .replace('\n', "\r\n");
    send(&mut service, did_change_full_notification(uri, 2, &changed)).await;
    assert_foreach_owner(&mut service, uri, &changed, "Updated").await;
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn foreach_type_owner_supports_trait_and_enum_after_a_decoy_class() {
    let mut service = service().await;
    let uri = "file:///test/class-like-owner.php";
    for declaration in ["trait Actual", "enum Actual"] {
        let code = format!("<?php\nclass Decoy {{}}\n{declaration} {{\nfunction run() {{\n/** @var array<int, self> $items */\n$items = [];\nforeach ($items as $item) {{ $item; }}\n}}\n}}");
        send(&mut service, did_open_notification(uri, &code)).await;
        assert_foreach_owner(&mut service, uri, &code, "Actual").await;
        send(&mut service, did_close_notification(uri)).await;
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn foreach_type_imports_and_owner_stay_inside_repeated_namespace_sections() {
    let mut service = service().await;
    let uri = "file:///test/namespace-section-owners.php";
    let code = "<?php\nnamespace Vendor { class Wrong {} class Right {} }\nnamespace Shared { use Vendor\\Wrong as Local; class Decoy {} }\nnamespace Shared {\nuse Vendor\\Right as Local;\nclass Actual { function run() {\n/** @var array<int, self> $items */\n$items = [];\nforeach ($items as $item) { $item; }\n/** @var array<int, Local> $objects */\n$objects = [];\nforeach ($objects as $object) { $object; }\n} }\n}";
    send(&mut service, did_open_notification(uri, code)).await;
    assert_foreach_owner(&mut service, uri, code, "Actual").await;
    let (line, col) = utf16_position_at(code, "$object;");
    let hover = send(&mut service, hover_request(12, uri, line, col + 1)).await;
    let text = hover_markdown_value(&hover);
    assert!(text.contains("Local $object"), "{text}");
    assert!(!text.contains("Wrong"), "foreign import: {text}");
    let expected_offset = code.find("class Right").unwrap() + "class ".len();
    let (expected_line, _) = utf16_position_for_offset(code, expected_offset);
    assert!(
        text.contains(&format!("{uri}#L{}", expected_line + 1)),
        "wrong type link: {text}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn top_level_phpdoc_self_does_not_link_to_an_unrelated_class() {
    let mut service = service().await;
    let uri = "file:///test/no-class-owner.php";
    let code = "<?php\nclass Decoy {}\nfunction run() {\n/** @var array<int, self> $items */\n$items = [];\nforeach ($items as $item) { $item; }\n}";
    send(&mut service, did_open_notification(uri, code)).await;
    let (line, col) = utf16_position_at(code, "$item;");
    let hover = send(&mut service, hover_request(10, uri, line, col + 1)).await;
    assert!(!hover_markdown_value(&hover).contains("Decoy"), "{hover}");
    let (end_line, end_col) = utf16_position_for_offset(code, code.len());
    let hints = send(
        &mut service,
        inlay_hint_request(11, uri, 0, 0, end_line, end_col),
    )
    .await;
    assert!(!hints.to_string().contains("Decoy"), "{hints}");
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn foreach_method_return_type_keeps_supplier_owner_and_links() {
    let mut service = service().await;
    let uri = "file:///test/declaring-return-owner.php";
    let code = "<?php\nclass Decoy {}\nclass Supplier { /** @return array<int, self> */ function rows() { return []; } }\nclass Actual { function run(Supplier $supplier) { $items = $supplier->rows(); foreach ($items as $item) { $item; } } }";
    send(&mut service, did_open_notification(uri, code)).await;
    assert_foreach_owner(&mut service, uri, code, "Supplier").await;
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn anonymous_class_phpdoc_self_never_borrows_the_named_outer_type_link() {
    let mut service = service().await;
    let uri = "file:///test/anonymous-owner-boundary.php";
    let code = "<?php class Outer { function run() { $object = new class { function work() { /** @var array<int, self> $items */ $items = []; foreach ($items as $item) { $item; } } }; } }";
    send(&mut service, did_open_notification(uri, code)).await;
    let (line, col) = utf16_position_at(code, "$item;");
    let hover = send(&mut service, hover_request(10, uri, line, col + 1)).await;
    assert!(
        !hover_markdown_value(&hover).contains("Outer"),
        "wrong anonymous self owner: {hover}"
    );
    let (end_line, end_col) = utf16_position_for_offset(code, code.len());
    let hints = send(
        &mut service,
        inlay_hint_request(11, uri, 0, 0, end_line, end_col),
    )
    .await;
    assert!(
        !hints.to_string().contains("Outer"),
        "wrong anonymous self link: {hints}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn parent_navigation_respects_anonymous_body_and_constructor_boundaries() {
    let mut service = service().await;
    let uri = "file:///test/anonymous-parent-owner.php";
    for (expression, expected) in [
        ("new class(parent::class) extends Other {}", "Grand"),
        (
            "new /* trivia */ class extends Other { function work() { parent::class; } }",
            "Other",
        ),
        (
            "new #[Marker] class extends Other { function work() { parent::class; } }",
            "Other",
        ),
    ] {
        let code = format!("<?php\nclass Grand {{}}\nclass Other {{}}\nclass Outer extends Grand {{ function run() {{ $object = {expression}; }} }}");
        send(&mut service, did_open_notification(uri, &code)).await;
        let (line, column) = utf16_position_at(&code, "parent::class");
        let definition = send(&mut service, definition_request(12, uri, line, column)).await;
        let locations = definition
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_else(|| std::slice::from_ref(&definition));
        let (expected_line, _) = utf16_position_at(&code, &format!("class {expected}"));
        assert!(
            locations.iter().any(|location| {
                location["uri"] == uri && location["range"]["start"]["line"] == expected_line
            }),
            "wrong parent definition for {expression}: {definition}"
        );
        send(&mut service, did_close_notification(uri)).await;
    }
    send(&mut service, shutdown_request(99)).await;
}
