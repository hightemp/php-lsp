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
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });
    send(
        &mut service,
        initialize_request_with_options(
            1,
            None,
            Some(json!({
                "stubExtensions": [], "diagnosticsMode": "off", "indexVendor": false
            })),
        ),
    )
    .await;
    service
}

fn assert_location(
    result: &serde_json::Value,
    uri: &str,
    source: &str,
    declaration: &str,
    name: &str,
) {
    let (line, col) = utf16_position_at(source, declaration);
    assert_eq!(result["uri"], uri, "{result}");
    assert_eq!(
        result["range"],
        json!({
            "start": {"line": line, "character": col},
            "end": {"line": line, "character": col + name.encode_utf16().count() as u32}
        }),
        "{result}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn phpdoc_definition_write_only_duplicate_owners_follow_unsaved_edits_and_removal() {
    let uri = "file:///test/phpdoc-owner.php";
    let source = "<?php\nnamespace First {\n/** @property-write string $slug */\nclass Owned {}\n}\nnamespace Second {\n/* 😀 */ /** @property-write string $slug */\n#[Marker]\n\nclass Owned {}\nfunction useIt(Owned $object) { $object->slug = 'new'; }\n}\n";
    let mut service = service().await;
    send(&mut service, did_open_notification(uri, source)).await;
    for (version, source) in [
        (2, source.replace('\n', "\r\n")),
        (3, source.replace("#[Marker]", "#[Marker]\n\n#[Other]")),
    ] {
        send(
            &mut service,
            did_change_full_notification(uri, version, &source),
        )
        .await;
        let (line, col) = utf16_position_at(&source, "slug =");
        let result = send(&mut service, definition_request(10, uri, line, col + 1)).await;
        let offset = source.rfind("$slug */").unwrap() + 1;
        let prefix = &source[..offset];
        let expected_line = prefix.bytes().filter(|byte| *byte == b'\n').count();
        let line_start = prefix.rfind('\n').map_or(0, |idx| idx + 1);
        assert_eq!(result["uri"], uri, "{result}");
        assert_eq!(
            result["range"]["start"],
            json!({"line": expected_line, "character": source[line_start..offset].encode_utf16().count()})
        );
        assert_eq!(
            result["range"]["end"]["character"].as_u64(),
            result["range"]["start"]["character"]
                .as_u64()
                .map(|start| start + 4)
        );
    }
    let removed = source.replacen("/* 😀 */ /** @property-write string $slug */", "", 1);
    send(&mut service, did_change_full_notification(uri, 4, &removed)).await;
    let (line, col) = utf16_position_at(&removed, "slug =");
    assert!(
        send(&mut service, definition_request(11, uri, line, col + 1))
            .await
            .is_null()
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn phpdoc_definition_exact_method_and_property_tags_keep_inherited_and_native_precedence() {
    let uri = "file:///test/phpdoc-members.php";
    let source = "<?php\n/**\n * @property string $slugLong mentions $slug\n * @property string $slug actual\n * @method string longFetch() mentions fetch()\n * @method\n * static string\n * Fetch () actual\n * @method string real()\n */\n#[Marker]\nclass Owned { public function real() {} }\nclass Child extends Owned {}\nfunction useIt(Child $object) { $object->slug; $object?->fETCH(); Owned::Fetch(); $object->real(); }";
    let mut service = service().await;
    send(&mut service, did_open_notification(uri, source)).await;
    for (call, declaration, name) in [
        ("slug;", "slug actual", "slug"),
        ("fETCH();", "Fetch ()", "Fetch"),
        ("Fetch();", "Fetch ()", "Fetch"),
        ("real();", "real() {}", "real"),
    ] {
        let (line, col) = utf16_position_at(source, call);
        let result = send(&mut service, definition_request(10, uri, line, col + 1)).await;
        assert_location(&result, uri, source, declaration, name);
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn phpdoc_definition_twig_shape_uses_real_docblock_before_attributes() {
    let root = std::env::temp_dir().join(format!("php-lsp-phpdoc-shape-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("templates")).unwrap();
    let uri = |path: &std::path::Path| php_lsp_types::uri::path_to_uri(path).unwrap();
    let repository = "<?php\r\nclass Repository {\r\n    /**\r\n     * @return list<array{slug: string}>\r\n     */\r\n\r\n    #[Marker]\r\n    public function rows() { return []; }\r\n}";
    let controller = "<?php class Controller { function page(Repository $repository) { $rows = $repository->rows(); $this->render('page.twig', ['rows' => $rows]); } }";
    let template = "{% for row in rows %}{{ row.slug }}{% endfor %}";
    let repository_uri = uri(&root.join("src/Repository.php"));
    let controller_uri = uri(&root.join("src/Controller.php"));
    let template_uri = uri(&root.join("templates/page.twig"));
    fs::write(root.join("src/Repository.php"), repository).unwrap();
    fs::write(root.join("src/Controller.php"), controller).unwrap();
    fs::write(root.join("templates/page.twig"), template).unwrap();
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
            Some(&uri(&root)),
            Some(json!({
                "stubExtensions": [], "diagnosticsMode": "off", "indexVendor": false
            })),
        ),
    )
    .await;
    send(&mut service, initialized_notification()).await;
    wait_for_indexing_phase(&mut notifications, "ready", Duration::from_secs(30)).await;
    for (file_uri, source) in [
        (&repository_uri, repository),
        (&controller_uri, controller),
        (&template_uri, template),
    ] {
        send(&mut service, did_open_notification(file_uri, source)).await;
    }
    let (line, col) = utf16_position_at(template, "slug");
    let result = send(
        &mut service,
        definition_request(10, &template_uri, line, col + 1),
    )
    .await;
    assert_location(&result, &repository_uri, repository, "slug:", "slug");
    send(&mut service, shutdown_request(99)).await;
    let _ = fs::remove_file(php_lsp_index::cache::cache_file_path(&root));
    fs::remove_dir_all(root).unwrap();
}
