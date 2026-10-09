mod support;
use support::*;

#[tokio::test(flavor = "current_thread")]
async fn invokable_class_docs_do_not_trigger_unproven_callable_mismatches() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = tx.send(notification);
        }
    });
    send(
        &mut service,
        initialize_request_with_options(
            1,
            None,
            Some(json!({"stubExtensions":[],"indexVendor":false})),
        ),
    )
    .await;
    let uri = "file:///test/invokable-native-contract.php";
    let source = "<?php class Invokable {function __invoke(): void {}} /**\n * @param Invokable $callback\n * @return Invokable\n */ function subject(callable $callback): callable {return $callback;}\nsubject(new Invokable());";
    send(&mut service, did_open_notification(uri, source)).await;
    let params = next_publish_diagnostics(&mut notifications, uri, Duration::from_secs(5)).await;
    assert!(
        !params["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["code"] == "phpdoc-type-mismatch"),
        "inheritance does not prove a callable mismatch: {params}"
    );
    let (line, column) = utf16_position_after(source, "\nsubject(");
    let result = send(&mut service, signature_help_request(2, uri, line, column)).await;
    assert!(
        result["signatures"][0]["label"]
            .as_str()
            .unwrap()
            .contains("callable $callback"),
        "unproven refinement replaced native: {result}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn contradictory_return_doc_cannot_redirect_member_completion_or_type_definition() {
    let mut service = service().await;
    let uri = "file:///test/native-member-contract.php";
    let source = "<?php\nclass NativeModel {function nativeOnly(): void {}}\nclass WrongModel {function wrongOnly(): void {}}\nclass Service {/** @return WrongModel */ function load(): NativeModel {return new NativeModel();}}\n$result = (new Service())->load();\n$result->";
    send(&mut service, did_open_notification(uri, source)).await;
    let (line, column) = utf16_position_after(source, "$result->");
    let result = send(&mut service, completion_request(2, uri, line, column)).await;
    let items = result["items"]
        .as_array()
        .or_else(|| result.as_array())
        .unwrap();
    assert!(
        items.iter().any(|item| item["label"] == "nativeOnly"),
        "native member lost: {result}"
    );
    assert!(
        !items.iter().any(|item| item["label"] == "wrongOnly"),
        "foreign member leaked: {result}"
    );
    let definition = send(&mut service, type_definition_request(3, uri, line, 2)).await;
    let location = definition
        .as_array()
        .and_then(|items| items.first())
        .unwrap_or(&definition);
    assert_eq!(location["uri"], uri, "wrong definition: {definition}");
    assert_eq!(
        location["range"]["start"]["line"], 1,
        "foreign definition: {definition}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn unresolved_key_domains_preserve_native_scalars_without_false_warnings() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = tx.send(notification);
        }
    });
    send(
        &mut service,
        initialize_request_with_options(
            1,
            None,
            Some(json!({"stubExtensions":[],"indexVendor":false})),
        ),
    )
    .await;
    for (n, native) in ["string", "int"].iter().enumerate() {
        let uri = format!("file:///test/key-domain-{native}.php");
        let source = format!("<?php class Foo {{const KEYS=['name'=>1];}} /**\n * @param key-of<Foo::KEYS> $value\n * @return key-of<Foo::KEYS>\n */ function subject({native} $value): {native} {{return $value;}}\nsubject({});", if *native == "string" { "'name'" } else { "1" });
        send(&mut service, did_open_notification(&uri, &source)).await;
        let params =
            next_publish_diagnostics(&mut notifications, &uri, Duration::from_secs(5)).await;
        assert!(
            !params["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|diagnostic| diagnostic["code"] == "phpdoc-type-mismatch"),
            "unproven contradiction: {params}"
        );
        let (line, column) = utf16_position_after(&source, "\nsubject(");
        let result = send(
            &mut service,
            signature_help_request(10 + n as i64, &uri, line, column),
        )
        .await;
        let label = result["signatures"][0]["label"].as_str().unwrap();
        assert!(
            label.contains(&format!("{native} $value")),
            "unknown keys replaced native: {label}"
        );
        send(&mut service, did_close_notification(&uri)).await;
    }
    send(&mut service, shutdown_request(99)).await;
}

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
            Some(json!({"stubExtensions":[], "diagnosticsMode":"off", "indexVendor":false})),
        ),
    )
    .await;
    service
}

#[tokio::test(flavor = "current_thread")]
async fn incompatible_param_doc_keeps_native_signature_help() {
    let mut service = service().await;
    let uri = "file:///test/native-param-contract.php";
    let source = "<?php\nclass Wrong {}\n/** @param Wrong $id */\nfunction subject(int $id): int { return $id; }\nsubject(1);";
    send(&mut service, did_open_notification(uri, source)).await;
    let (line, column) = utf16_position_after(source, "subject(1");
    let result = send(&mut service, signature_help_request(2, uri, line, column)).await;
    let label = result["signatures"][0]["label"].as_str().unwrap();
    assert!(
        label.contains("int $id"),
        "PHPDoc replaced native parameter: {result}"
    );
    assert!(!label.contains("Wrong"));
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn incompatible_return_doc_keeps_native_assignment_hover_and_type_links() {
    let mut service = service().await;
    let uri = "file:///test/native-return-contract.php";
    let source = "<?php\nclass Wrong {}\nclass Service { /** @return Wrong */ function load(): int { return 1; } }\n$service = new Service();\n$result = $service->load();\n$result;";
    send(&mut service, did_open_notification(uri, source)).await;
    let (line, column) = utf16_position_at(source, "$result;");
    let hover = send(&mut service, hover_request(2, uri, line, column + 1)).await;
    let text = hover_markdown_value(&hover);
    assert!(text.contains("int $result"), "wrong assigned type: {text}");
    assert!(!text.contains("Wrong"), "foreign object link: {text}");
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn contradictory_phpdoc_is_diagnosed_without_rejecting_a_valid_native_call() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = tx.send(notification);
        }
    });
    send(
        &mut service,
        initialize_request_with_options(
            1,
            None,
            Some(json!({"stubExtensions":[], "indexVendor":false})),
        ),
    )
    .await;
    let uri = "file:///test/phpdoc-native-diagnostic.php";
    let source = "<?php\nclass Wrong {}\n/**\n * @param Wrong $id\n * @return Wrong\n */\nfunction subject(int $id): int { return $id; }\nsubject(1);";
    send(&mut service, did_open_notification(uri, source)).await;
    let params = next_publish_diagnostics(&mut notifications, uri, Duration::from_secs(5)).await;
    let diagnostics = params["diagnostics"].as_array().unwrap();
    let mismatch = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic["code"] == "phpdoc-type-mismatch")
        .collect::<Vec<_>>();
    assert_eq!(
        mismatch.len(),
        2,
        "PHPDoc contradictions were not diagnosed: {diagnostics:?}"
    );
    assert!(
        !published_diagnostic_messages(&params)
            .iter()
            .any(|message| message.contains("Argument") && message.contains("Wrong")),
        "valid native argument rejected: {params}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn broader_return_doc_does_not_erase_native_late_static_owner() {
    let mut service = service().await;
    let uri = "file:///test/native-static-contract.php";
    for doc in ["self", "Base"] {
        let source=format!("<?php\nclass Base {{ /** @return {doc} */ function make(): static {{return new static;}} }}\nclass Child extends Base {{function childOnly() {{}}}}\n$child = new Child();\n$made = $child->make();\n$made;");
        send(&mut service, did_open_notification(uri, &source)).await;
        let (line, column) = utf16_position_at(&source, "$made;");
        let hover = send(&mut service, hover_request(2, uri, line, column + 1)).await;
        let text = hover_markdown_value(&hover);
        assert!(
            text.contains("Child $made"),
            "{doc} weakened static: {text}"
        );
        send(&mut service, did_close_notification(uri)).await;
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn compatible_array_param_refinement_preserves_foreach_hover_in_the_body() {
    let mut service = service().await;
    let uri = "file:///test/compatible-param-refinement.php";
    let source="<?php\nclass Model {}\n/** @param array<int, Model> $items */\nfunction subject(array $items) { foreach ($items as $item) { $item; } }";
    send(&mut service, did_open_notification(uri, source)).await;
    let (line, column) = utf16_position_at(source, "$item;");
    let hover = send(&mut service, hover_request(2, uri, line, column + 1)).await;
    let text = hover_markdown_value(&hover);
    assert!(
        text.contains("Model $item"),
        "compatible parameter detail lost: {text}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn unsaved_doc_correction_updates_diagnostics_with_utf16_crlf_ranges() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = tx.send(notification);
        }
    });
    send(
        &mut service,
        initialize_request_with_options(
            1,
            None,
            Some(json!({"stubExtensions":[],"indexVendor":false})),
        ),
    )
    .await;
    let uri = "file:///test/native-doc-edit.php";
    let wrong="<?php class Wrong {} /* 😀 */ /** @param Wrong $id */ function subject(int $id): int {return $id;}\r\nsubject(1);";
    send(&mut service, did_open_notification(uri, wrong)).await;
    let first = next_publish_diagnostics(&mut notifications, uri, Duration::from_secs(5)).await;
    let diagnostic = first["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|diagnostic| diagnostic["code"] == "phpdoc-type-mismatch")
        .expect("contradiction warning");
    let (line, column) = utf16_position_at(wrong, "/** @param");
    assert_eq!(
        diagnostic["range"]["start"],
        json!({"line":line,"character":column})
    );
    let fixed = wrong.replace("@param Wrong", "@param positive-int");
    send(&mut service, did_change_full_notification(uri, 2, &fixed)).await;
    let second = next_publish_diagnostics(&mut notifications, uri, Duration::from_secs(5)).await;
    assert_eq!(second["version"], 2);
    assert!(
        !second["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["code"] == "phpdoc-type-mismatch"),
        "stale contradiction: {second}"
    );
    send(&mut service, did_change_full_notification(uri, 3, wrong)).await;
    let third = next_publish_diagnostics(&mut notifications, uri, Duration::from_secs(5)).await;
    assert_eq!(third["version"], 3);
    assert!(third["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .any(|diagnostic| diagnostic["code"] == "phpdoc-type-mismatch"));
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn scalar_generic_operator_conflict_keeps_native_object_and_warns() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = tx.send(notification);
        }
    });
    send(
        &mut service,
        initialize_request_with_options(
            1,
            None,
            Some(json!({"stubExtensions":[],"indexVendor":false})),
        ),
    )
    .await;
    let uri = "file:///test/key-of-native-object.php";
    let source="<?php class Foo {const KEYS=['name'=>1];} /**\n * @param key-of<Foo::KEYS> $value\n * @return key-of<Foo::KEYS>\n */ function subject(object $value): object {return $value;}";
    send(&mut service, did_open_notification(uri, source)).await;
    let params = next_publish_diagnostics(&mut notifications, uri, Duration::from_secs(5)).await;
    let mismatches = params["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|diagnostic| diagnostic["code"] == "phpdoc-type-mismatch")
        .count();
    assert_eq!(
        mismatches, 2,
        "key-of incorrectly treated as object: {params}"
    );
    let (line, column) = utf16_position_after(source, "function subject(");
    let hover = send(&mut service, hover_request(2, uri, line, column - 3)).await;
    let text = hover_markdown_value(&hover);
    assert!(
        !text.contains("key-of<Foo::KEYS> $value"),
        "unsafe signature: {text}"
    );
    send(&mut service, shutdown_request(99)).await;
}
