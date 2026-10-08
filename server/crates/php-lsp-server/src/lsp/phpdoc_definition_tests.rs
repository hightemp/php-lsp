use super::*;
use php_lsp_types::uri::path_to_uri;

fn owner(source: &str, uri: &str) -> php_lsp_types::SymbolInfo {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    extract_file_symbols(parser.tree().unwrap(), source, uri)
        .symbols
        .into_iter()
        .find(|symbol| symbol.name == "Owned")
        .unwrap()
}

fn property(owner: php_lsp_types::SymbolInfo, name: &str) -> PhpDocVirtualMember {
    PhpDocVirtualMember {
        owner: Arc::new(owner),
        name: name.to_string(),
        kind: PhpDocVirtualMemberKind::Property,
        type_info: None,
        access: Some(php_lsp_types::PhpDocPropertyAccess::WriteOnly),
        return_type: None,
        params: Vec::new(),
        description: None,
        is_static: false,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn phpdoc_definition_identical_comments_select_the_actual_owner() {
    let source = "<?php\n/** @property-write string $slug */\nclass Foreign {}\n/** @property-write string $slug */\n#[Marker]\n\nclass Owned {}";
    let uri = "file:///phpdoc-owner.php";
    let member = property(owner(source, uri), "slug");
    let (service, _socket) = tower_lsp::LspService::new(PhpLspBackend::new);
    let backend = service.inner();
    let mut parser = FileParser::new();
    parser.parse_full(source);
    backend.open_files.insert(uri.to_string(), parser);
    let request = backend.request_context_for_uri(uri).await;
    let location = backend
        .phpdoc_virtual_member_location(&request, &member)
        .await
        .unwrap();
    assert_eq!(location.range.start.line, 3);
    assert_eq!(text_at_lsp_range(source, location.range), Some("slug"));
}

#[tokio::test(flavor = "current_thread")]
async fn phpdoc_definition_stale_owner_does_not_fall_back_to_foreign_comment() {
    let source = "<?php\n/** @property-write string $slug */\nclass Foreign {}\n/** @property-write string $slug */\nclass Owned {}";
    let uri = "file:///phpdoc-stale-owner.php";
    let member = property(owner(source, uri), "slug");
    let start = source.rfind("/**").unwrap();
    let end = source[start..].find("*/").unwrap() + start + 2;
    let mut changed = source.to_string();
    changed.replace_range(start..end, &" ".repeat(end - start));
    let (service, _socket) = tower_lsp::LspService::new(PhpLspBackend::new);
    let backend = service.inner();
    let mut parser = FileParser::new();
    parser.parse_full(&changed);
    backend.open_files.insert(uri.to_string(), parser);
    let request = backend.request_context_for_uri(uri).await;
    assert!(backend
        .phpdoc_virtual_member_location(&request, &member)
        .await
        .is_none());
}

#[test]
fn phpdoc_definition_property_name_is_not_a_prefix_or_description_match() {
    let source = "<?php\n/**\n * @property-write string $slugLong mentions $slug\n * @property-write string $slug actual\n */\nclass Owned {}";
    let member = property(owner(source, "file:///phpdoc-tag.php"), "slug");
    let comment = member.owner.doc_comment.as_ref().unwrap();
    let range =
        phpdoc_virtual_member_range(source, comment, source.find("/**").unwrap(), &member).unwrap();
    assert_eq!(range.0, 3);
}

#[test]
fn phpdoc_definition_missing_tag_has_no_fabricated_comment_location() {
    let source = "<?php\n/** @property-write string $slug */\nclass Owned {}";
    let member = property(owner(source, "file:///phpdoc-missing.php"), "absent");
    assert!(phpdoc_virtual_member_range(
        source,
        member.owner.doc_comment.as_ref().unwrap(),
        source.find("/**").unwrap(),
        &member
    )
    .is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn phpdoc_definition_stale_owner_cannot_cross_namespace_change() {
    let source = "<?php namespace First; /** @property-write string $slug */ class Owned {}";
    let changed = source.replace("First", "Other");
    let uri = "file:///phpdoc-namespace-change.php";
    let member = property(owner(source, uri), "slug");
    let (service, _socket) = tower_lsp::LspService::new(PhpLspBackend::new);
    let backend = service.inner();
    let mut parser = FileParser::new();
    parser.parse_full(&changed);
    backend.open_files.insert(uri.to_string(), parser);
    let request = backend.request_context_for_uri(uri).await;
    assert!(backend
        .phpdoc_virtual_member_location(&request, &member)
        .await
        .is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn phpdoc_definition_closed_source_must_match_indexed_fingerprint() {
    let root = std::env::temp_dir().join(format!("php-lsp-doc-fingerprint-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("Owned.php");
    let uri = path_to_uri(&path).unwrap();
    let source = "<?php namespace First; /** @property-write string $slug */ class Owned {}";
    std::fs::write(&path, source.replace("First", "Other")).unwrap();
    let (service, _socket) = tower_lsp::LspService::new(PhpLspBackend::new);
    let backend = service.inner();
    let request = backend.request_context_for_uri(&uri).await;
    let symbol = owner(source, &uri);
    request
        .index(&backend.index)
        .update_file_with_references_from_source(
            &uri,
            php_lsp_types::FileSymbols {
                symbols: vec![symbol.clone()],
                ..Default::default()
            },
            Vec::new(),
            SourceFingerprint::from_bytes(source.as_bytes()),
        );
    let member = property(symbol, "slug");
    assert!(backend
        .phpdoc_virtual_member_location(&request, &member)
        .await
        .is_none());
    std::fs::write(&path, source).unwrap();
    assert!(backend
        .phpdoc_virtual_member_location(&request, &member)
        .await
        .is_some());
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn phpdoc_definition_indexed_virtual_symbols_reject_changed_closed_source() {
    let root = std::env::temp_dir().join(format!("php-lsp-doc-indexed-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("Owned.php");
    let uri = path_to_uri(&path).unwrap();
    let source = "<?php namespace First; /**\n * @property string $slug\n * @method string fetch()\n */ class Owned {}";
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let symbols = extract_file_symbols(parser.tree().unwrap(), source, &uri);
    let (service, _socket) = tower_lsp::LspService::new(PhpLspBackend::new);
    let backend = service.inner();
    let request = backend.request_context_for_uri(&uri).await;
    request
        .index(&backend.index)
        .update_file_with_references_from_source(
            &uri,
            symbols.clone(),
            Vec::new(),
            SourceFingerprint::from_bytes(source.as_bytes()),
        );
    std::fs::write(&path, source.replace("First", "Other")).unwrap();
    for symbol in symbols
        .symbols
        .iter()
        .filter(|symbol| symbol.parent_fqn.is_some())
    {
        assert!(
            backend
                .location_for_symbol_selection_in_request(&request, symbol, "test virtual symbol")
                .await
                .is_none(),
            "{}",
            symbol.fqn
        );
    }
    std::fs::write(&path, source).unwrap();
    for symbol in symbols
        .symbols
        .iter()
        .filter(|symbol| symbol.parent_fqn.is_some())
    {
        let location = backend
            .location_for_symbol_selection_in_request(&request, symbol, "test virtual symbol")
            .await
            .unwrap();
        assert_eq!(
            text_at_lsp_range(source, location.range),
            Some(symbol.name.as_str())
        );
        let mut missing_span = symbol.clone();
        missing_span.doc_comment_range = None;
        assert!(backend
            .location_for_symbol_selection_in_request(
                &request,
                &missing_span,
                "test missing doc span"
            )
            .await
            .is_none());
    }
    // A legacy/derived index update lacks disk provenance. Equal text and
    // positions alone cannot identify the owner after a namespace edit.
    request
        .index(&backend.index)
        .update_file_with_references(&uri, symbols.clone(), Vec::new());
    assert!(request
        .index(&backend.index)
        .read()
        .source_fingerprints()
        .get(&uri)
        .is_none());
    std::fs::write(&path, source.replace("First", "Other")).unwrap();
    for symbol in symbols
        .symbols
        .iter()
        .filter(|symbol| symbol.parent_fqn.is_some())
    {
        assert!(
            backend
                .location_for_symbol_selection_in_request(
                    &request,
                    symbol,
                    "test untracked virtual symbol"
                )
                .await
                .is_none(),
            "{}",
            symbol.fqn
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn phpdoc_definition_closed_index_replacement_invalidates_captured_owner_and_tags() {
    let root = std::env::temp_dir().join(format!("php-lsp-doc-replacement-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("Owned.php");
    let uri = path_to_uri(&path).unwrap();
    let source = "<?php namespace First; /**\n * @property string $slug\n * @method string fetch()\n */ class Owned {}";
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let captured = extract_file_symbols(parser.tree().unwrap(), source, &uri);
    let changed = source.replace("First", "Other");
    parser.parse_full(&changed);
    let current = extract_file_symbols(parser.tree().unwrap(), &changed, &uri);
    std::fs::write(&path, &changed).unwrap();
    let (service, _socket) = tower_lsp::LspService::new(PhpLspBackend::new);
    let backend = service.inner();
    let request = backend.request_context_for_uri(&uri).await;
    // Publish the new parse/fingerprint after capturing request symbols.
    request
        .index(&backend.index)
        .update_file_with_references_from_source(
            &uri,
            current,
            Vec::new(),
            SourceFingerprint::from_bytes(changed.as_bytes()),
        );
    let member = property(owner(source, &uri), "slug");
    assert!(backend
        .phpdoc_virtual_member_location(&request, &member)
        .await
        .is_none());
    for symbol in captured
        .symbols
        .iter()
        .filter(|symbol| symbol.parent_fqn.is_some())
    {
        assert!(backend
            .location_for_symbol_selection_in_request(&request, symbol, "test replaced index")
            .await
            .is_none());
    }
    std::fs::remove_dir_all(root).unwrap();
}
