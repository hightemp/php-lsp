mod support;

use php_lsp_types::uri::path_to_uri;
use support::*;

async fn request(service: &mut LspService<PhpLspBackend>, request: Request) -> serde_json::Value {
    extract_result(service.ready().await.unwrap().call(request).await.unwrap())
}

#[tokio::test(flavor = "current_thread")]
async fn clone_this_exposes_members_for_completion_hover_and_definition() {
    let source = r#"<?php
namespace App;
class Example {
    public string $mapping;
    public function copy(): void {
        $copy = clone $this;
        $copy->mapping;
        (clone $this)->mapping;
    }
}
"#;
    let uri =
        php_lsp_types::uri::path_to_uri(&std::env::temp_dir().join("php-lsp-clone/Example.php"))
            .unwrap();
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move { socket.collect::<Vec<_>>().await });
    request(
        &mut service,
        initialize_request_with_options(1, None, Some(json!({ "stubExtensions": [] }))),
    )
    .await;
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&uri, source))
        .await
        .unwrap();

    let (line, column) = utf16_position_at(source, "$copy->mapping");
    let completion = request(
        &mut service,
        completion_request(2, &uri, line, column + "$copy->".len() as u32),
    )
    .await;
    assert!(
        completion_items_from_result(&completion)
            .iter()
            .any(|item| item["label"] == "mapping"),
        "clone $this completion: {completion}"
    );

    let hover = request(
        &mut service,
        hover_request(3, &uri, line, column + "$copy->".len() as u32 + 1),
    )
    .await;
    assert!(hover_markdown_value(&hover).contains("mapping"), "{hover}");

    let definition = request(
        &mut service,
        definition_request(4, &uri, line, column + "$copy->".len() as u32 + 1),
    )
    .await;
    assert_eq!(definition["uri"], uri, "{definition}");

    let (line, column) = utf16_position_at(source, "(clone $this)->mapping");
    let direct = request(
        &mut service,
        completion_request(5, &uri, line, column + "(clone $this)->".len() as u32),
    )
    .await;
    assert!(
        completion_items_from_result(&direct)
            .iter()
            .any(|item| item["label"] == "mapping"),
        "direct clone receiver completion: {direct}"
    );
    request(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn clone_parens_and_nested_clone_keep_typed_operand_after_change() {
    let uri =
        php_lsp_types::uri::path_to_uri(&std::env::temp_dir().join("php-lsp-clone/Operand.php"))
            .unwrap();
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move { socket.collect::<Vec<_>>().await });
    request(
        &mut service,
        initialize_request_with_options(1, None, Some(json!({ "stubExtensions": [] }))),
    )
    .await;
    for (version, expression) in [(1, "clone($item)"), (2, "clone (clone $item)")] {
        let source = format!(
            "<?php class Model {{ public string $value; }}\nfunction copy(Model $item) {{ $copy = {expression}; $copy->value; }}\n"
        );
        let notification = if version == 1 {
            did_open_notification(&uri, &source)
        } else {
            did_change_full_notification(&uri, version, &source)
        };
        service
            .ready()
            .await
            .unwrap()
            .call(notification)
            .await
            .unwrap();
        let (line, column) = utf16_position_at(&source, "$copy->value");
        let completion = request(
            &mut service,
            completion_request(
                i64::from(version + 1),
                &uri,
                line,
                column + "$copy->".len() as u32,
            ),
        )
        .await;
        assert!(
            completion_items_from_result(&completion)
                .iter()
                .any(|item| item["label"] == "value"),
            "{expression}: {completion}"
        );
    }
    request(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn clone_of_cold_vendor_method_result_loads_operand_dependencies() {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("php-lsp-clone-vendor-{nonce}"));
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("vendor/composer")).unwrap();
    fs::create_dir_all(root.join("vendor/acme/library/src")).unwrap();
    fs::write(
        root.join("composer.json"),
        r#"{"autoload":{"psr-4":{"Client\\":"src/"}}}"#,
    )
    .unwrap();
    fs::write(root.join("vendor/composer/installed.json"), r#"{"packages":[{"name":"acme/library","install-path":"../acme/library","autoload":{"psr-4":{"Vendor\\":"src/"}}}]}"#).unwrap();
    fs::write(
        root.join("vendor/acme/library/src/Factory.php"),
        "<?php namespace Vendor; class Factory { public function make(): Product { return new Product(); } }",
    )
    .unwrap();
    fs::write(
        root.join("vendor/acme/library/src/Product.php"),
        "<?php namespace Vendor; class Product { public function productOnly(): void {} }",
    )
    .unwrap();
    let uri = path_to_uri(&root.join("src/Use.php")).unwrap();
    let root_uri = path_to_uri(&root).unwrap();
    for expression in [
        "clone /* outer */ (/* inner */ $factory->make())",
        "clone $factory->make()",
    ] {
        let source = format!("<?php\nnamespace Client;\nfunction useClone(\\Vendor\\Factory $factory) {{\n    $copy = {expression};\n    $copy->productOnly();\n}}\n");
        let (mut service, socket) = LspService::new(PhpLspBackend::new);
        tokio::spawn(async move { socket.collect::<Vec<_>>().await });
        request(
            &mut service,
            initialize_request_with_options(
                1,
                Some(&root_uri),
                Some(json!({"stubExtensions": [], "indexVendor": true, "diagnosticsMode": "off"})),
            ),
        )
        .await;
        service
            .ready()
            .await
            .unwrap()
            .call(did_open_notification(&uri, &source))
            .await
            .unwrap();

        let (line, column) = utf16_position_at(&source, "$copy->productOnly");
        let completion = request(
            &mut service,
            completion_request(2, &uri, line, column + "$copy->".len() as u32),
        )
        .await;
        assert!(
            completion_items_from_result(&completion)
                .iter()
                .any(|item| item["label"] == "productOnly"),
            "cold clone operand completion for {expression}: {completion}"
        );
        request(&mut service, shutdown_request(99)).await;
    }
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_clone_reassignment_removes_stale_completion_members() {
    let source = "<?php class Known { public function knownOnly(): void {} }\nfunction copy($item) { $copy = new Known(); $copy = clone $item; $copy->knownOnly(); }\n";
    let uri = path_to_uri(&std::env::temp_dir().join("php-lsp-clone/Unknown.php")).unwrap();
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move { socket.collect::<Vec<_>>().await });
    request(
        &mut service,
        initialize_request_with_options(1, None, Some(json!({"stubExtensions": []}))),
    )
    .await;
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&uri, source))
        .await
        .unwrap();
    let (line, column) = utf16_position_at(source, "$copy->knownOnly");
    let completion = request(
        &mut service,
        completion_request(2, &uri, line, column + "$copy->".len() as u32),
    )
    .await;
    assert!(
        !completion_items_from_result(&completion)
            .iter()
            .any(|item| item["label"] == "knownOnly"),
        "stale members after unknown clone: {completion}"
    );
    request(&mut service, shutdown_request(99)).await;
}
