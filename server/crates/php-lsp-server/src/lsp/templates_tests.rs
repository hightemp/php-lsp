use super::*;

fn parse_test_file_symbols(source: &str, uri: &str) -> php_lsp_types::FileSymbols {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let tree = parser.tree().expect("test PHP should parse");
    extract_file_symbols(tree, source, uri)
}

#[test]
fn stale_twig_context_refresh_cannot_replace_reopened_document() {
    let uri = "file:///workspace/templates/page.html.twig";
    let stale_source = "{{ stale }}";
    let current_source = "{{ current }}";
    let stale_template = preprocess_twig_template(stale_source, &[]);
    let current_template = preprocess_twig_template(current_source, &[]);
    let mut stale_parser = FileParser::new();
    stale_parser.parse_full(stale_template.virtual_source());
    let mut current_parser = FileParser::new();
    current_parser.parse_full(current_template.virtual_source());

    let open_files = DashMap::new();
    open_files.insert(uri.to_string(), current_parser);
    let template_documents = DashMap::new();
    template_documents.insert(uri.to_string(), current_template);
    let document_versions = DashMap::new();
    document_versions.insert(
        uri.to_string(),
        OpenDocumentState {
            version: 1,
            generation: 1,
        },
    );

    assert!(!replace_open_template_if_current(
        OpenTemplateRefreshSnapshot {
            uri,
            state: OpenDocumentState {
                version: 1,
                generation: 1,
            },
            document: &stale_template,
        },
        stale_parser,
        stale_template.clone(),
        &open_files,
        &template_documents,
        &document_versions,
    ));
    assert_eq!(
        template_documents
            .get(uri)
            .expect("current template should remain open")
            .original_source(),
        current_source
    );
}

#[test]
fn stale_twig_refresh_cannot_cross_reopen_with_reused_lsp_version() {
    let uri = "file:///workspace/templates/reopened.html.twig";
    let source = "{{ value }}";
    let stale_template = preprocess_twig_template(source, &[]);
    let current_template = preprocess_twig_template(source, &[]);
    let mut stale_parser = FileParser::new();
    stale_parser.parse_full(stale_template.virtual_source());
    let mut current_parser = FileParser::new();
    current_parser.parse_full(current_template.virtual_source());
    let open_files = DashMap::new();
    open_files.insert(uri.to_string(), current_parser);
    let template_documents = DashMap::new();
    template_documents.insert(uri.to_string(), current_template);
    let document_versions = DashMap::new();
    document_versions.insert(
        uri.to_string(),
        OpenDocumentState {
            version: 1,
            generation: 2,
        },
    );

    assert!(!replace_open_template_if_current(
        OpenTemplateRefreshSnapshot {
            uri,
            state: OpenDocumentState {
                version: 1,
                generation: 1,
            },
            document: &stale_template,
        },
        stale_parser,
        stale_template.clone(),
        &open_files,
        &template_documents,
        &document_versions,
    ));
}

#[test]
fn stale_twig_context_refresh_cannot_replace_newer_document_version() {
    let uri = "file:///workspace/templates/versioned.html.twig";
    let source = "{{ value }}";
    let refreshed_template = preprocess_twig_template(source, &[]);
    let current_template = preprocess_twig_template(source, &[]);
    let mut refreshed_parser = FileParser::new();
    refreshed_parser.parse_full(refreshed_template.virtual_source());
    let mut current_parser = FileParser::new();
    current_parser.parse_full(current_template.virtual_source());

    let open_files = DashMap::new();
    open_files.insert(uri.to_string(), current_parser);
    let template_documents = DashMap::new();
    template_documents.insert(uri.to_string(), current_template);
    let document_versions = DashMap::new();
    document_versions.insert(
        uri.to_string(),
        OpenDocumentState {
            version: 2,
            generation: 1,
        },
    );

    assert!(!replace_open_template_if_current(
        OpenTemplateRefreshSnapshot {
            uri,
            state: OpenDocumentState {
                version: 1,
                generation: 1,
            },
            document: &refreshed_template,
        },
        refreshed_parser,
        refreshed_template.clone(),
        &open_files,
        &template_documents,
        &document_versions,
    ));
    assert_eq!(
        document_versions.get(uri).map(|state| state.version),
        Some(2)
    );
}

#[test]
fn older_twig_context_refresh_cannot_replace_newer_context_generation() {
    let uri = "file:///workspace/templates/context.html.twig";
    let source = "{{ value.name }}";
    let older_template = preprocess_twig_template(
        source,
        &[TemplateVariableType {
            name: "value".to_string(),
            type_text: "array{name: string}".to_string(),
            shape_definitions: vec![TemplateShapeKeyDefinition {
                target: TemplateShapeDefinitionTarget::Direct,
                path: vec!["name".to_string()],
                uri: "file:///workspace/old.php".to_string(),
                range: (1, 2, 1, 6),
            }],
        }],
    );
    let newer_template = preprocess_twig_template(
        source,
        &[TemplateVariableType {
            name: "value".to_string(),
            type_text: "array{name: string}".to_string(),
            shape_definitions: vec![TemplateShapeKeyDefinition {
                target: TemplateShapeDefinitionTarget::Direct,
                path: vec!["name".to_string()],
                uri: "file:///workspace/new.php".to_string(),
                range: (8, 4, 8, 8),
            }],
        }],
    );
    let older_virtual_source = older_template.virtual_source().to_string();
    let newer_virtual_source = newer_template.virtual_source().to_string();
    assert_eq!(
        older_virtual_source, newer_virtual_source,
        "definition metadata should not affect generated PHP"
    );

    let mut older_parser = FileParser::new();
    older_parser.parse_full(older_template.virtual_source());
    let mut newer_parser = FileParser::new();
    newer_parser.parse_full(newer_template.virtual_source());
    let open_files = DashMap::new();
    open_files.insert(uri.to_string(), newer_parser);
    let template_documents = DashMap::new();
    template_documents.insert(uri.to_string(), newer_template);
    let document_versions = DashMap::new();
    document_versions.insert(
        uri.to_string(),
        OpenDocumentState {
            version: 1,
            generation: 1,
        },
    );

    assert!(!replace_open_template_if_current(
        OpenTemplateRefreshSnapshot {
            uri,
            state: OpenDocumentState {
                version: 1,
                generation: 1,
            },
            document: &older_template,
        },
        older_parser,
        older_template.clone(),
        &open_files,
        &template_documents,
        &document_versions,
    ));
    assert_eq!(
        template_documents
            .get(uri)
            .expect("newer context should remain open")
            .virtual_source(),
        newer_virtual_source
    );
    let definition = template_documents
        .get(uri)
        .expect("newer context should remain open")
        .twig_shape_key_definition(
            "value",
            TemplateShapeDefinitionTarget::Direct,
            &["name".to_string()],
        )
        .expect("newer shape definition");
    assert_eq!(definition.uri.as_str(), "file:///workspace/new.php");
    assert_eq!(definition.range.start, Position::new(8, 4));
}

#[test]
fn superseded_indexing_run_cannot_publish_twig_refresh() {
    let root = PathBuf::from("/workspace");
    let uri = "file:///workspace/templates/page.html.twig";
    let source = "{{ value }}";
    let current_template = preprocess_twig_template(source, &[]);
    let stale_refresh = preprocess_twig_template(
        source,
        &[TemplateVariableType {
            name: "value".to_string(),
            type_text: "App\\Stale".to_string(),
            shape_definitions: Vec::new(),
        }],
    );
    let mut current_parser = FileParser::new();
    current_parser.parse_full(current_template.virtual_source());
    let mut stale_parser = FileParser::new();
    stale_parser.parse_full(stale_refresh.virtual_source());
    let open_files = DashMap::new();
    open_files.insert(uri.to_string(), current_parser);
    let template_documents = DashMap::new();
    template_documents.insert(uri.to_string(), current_template.clone());
    let document_versions = DashMap::new();
    let state = OpenDocumentState {
        version: 1,
        generation: 1,
    };
    document_versions.insert(uri.to_string(), state);
    let coordinator = Arc::new(IndexingRunCoordinator::default());
    let old = coordinator.start(root.clone());
    let old_run = old.lease();
    let _new = coordinator.start(root);

    assert!(old_run
        .commit_if_current(|| replace_open_template_if_current(
            OpenTemplateRefreshSnapshot {
                uri,
                state,
                document: &current_template,
            },
            stale_parser,
            stale_refresh,
            &open_files,
            &template_documents,
            &document_versions,
        ))
        .is_none());
    assert_eq!(
        template_documents
            .get(uri)
            .expect("current template remains")
            .virtual_source(),
        current_template.virtual_source()
    );
}

#[test]
fn infers_nested_literal_array_shape_type_for_twig_context() {
    let source = concat!(
        "[\n",
        "    'encryption' => ['temp_dir_path' => '/tmp', 'enabled' => true],\n",
        "    'sftp' => ['host' => 'localhost', 'port' => 22],\n",
        "]",
    );
    let file_symbols = php_lsp_types::FileSymbols::default();

    let type_text =
        infer_twig_context_value_type(source, (0, source.len()), &file_symbols, None, None)
            .expect("literal array shape type");

    assert_eq!(
            type_text,
            "array{encryption: array{temp_dir_path: string, enabled: bool}, sftp: array{host: string, port: int}}"
        );
}

#[test]
fn resolves_imported_complex_callback_return_in_literal_array_context() {
    let source = concat!(
        "<?php\n",
        "namespace App\\Controller;\n",
        "use App\\Dto\\FirstResult as Foo;\n",
        "use App\\Dto\\SecondResult as Bar;\n",
        "final class ReportController\n",
        "{\n",
        "    public function show(): void\n",
        "    {\n",
        "        $this->render('report.html.twig', [\n",
        "            'items' => array_map(\n",
        "                fn (mixed $value): Foo|Bar|self => $value,\n",
        "                [],\n",
        "            ),\n",
        "        ]);\n",
        "    }\n",
        "}\n",
    );
    let uri = "file:///workspace/src/Controller/ReportController.php";
    let file_symbols = parse_test_file_symbols(source, uri);
    let context_key = source.find("'items'").expect("render context key");
    let context_start = source[..context_key]
        .rfind('[')
        .expect("render context start");
    let context_end = context_start
        + source[context_start..]
            .find("]);")
            .expect("render context end")
        + 1;

    let type_text = infer_twig_context_value_type(
        source,
        (context_start, context_end),
        &file_symbols,
        None,
        None,
    )
    .expect("literal array callback type");

    assert_eq!(
            type_text,
            "array{items: array<int, App\\Dto\\FirstResult|App\\Dto\\SecondResult|App\\Controller\\ReportController>}"
        );
}

#[test]
fn infers_array_append_shape_type_for_twig_context_assignment() {
    let source = concat!(
        "<?php\n",
        "$items = [];\n",
        "$items[] = ['nr' => 'NR-1', 'code' => 'ERR', 'description' => 'Failure'];\n",
        "$this->render('index.html.twig', ['items' => $items]);\n",
    );
    let value_start = source.rfind("$items").expect("render variable usage");
    let file_symbols = php_lsp_types::FileSymbols::default();
    let mut visited_variables = HashSet::new();

    let type_text = infer_twig_context_assignment_value_type(
        source,
        value_start,
        "items",
        &file_symbols,
        None,
        None,
        &mut visited_variables,
    )
    .expect("append array shape type");

    assert_eq!(
        type_text,
        "array<int, array{nr: string, code: string, description: string}>"
    );
}

#[test]
fn resolves_imported_complex_callback_return_in_appended_array_context() {
    let source = concat!(
        "<?php\n",
        "namespace App\\Controller;\n",
        "use App\\Dto\\FirstResult as Foo;\n",
        "use App\\Dto\\SecondResult as Bar;\n",
        "final class ReportController\n",
        "{\n",
        "    public function show(): void\n",
        "    {\n",
        "        $items = [];\n",
        "        $items[] = array_map(\n",
        "            static fn (mixed $value): Foo|Bar|static => $value,\n",
        "            [],\n",
        "        );\n",
        "        $this->render('report.html.twig', ['items' => $items]);\n",
        "    }\n",
        "}\n",
    );
    let uri = "file:///workspace/src/Controller/ReportController.php";
    let file_symbols = parse_test_file_symbols(source, uri);
    let value_start = source.rfind("$items").expect("render variable usage");
    let mut visited_variables = HashSet::new();

    let type_text = infer_twig_context_assignment_value_type(
        source,
        value_start,
        "items",
        &file_symbols,
        None,
        None,
        &mut visited_variables,
    )
    .expect("appended array callback type");

    assert_eq!(
            type_text,
            "array<int, array<int, App\\Dto\\FirstResult|App\\Dto\\SecondResult|App\\Controller\\ReportController>>"
        );
}

#[test]
fn resolves_external_symbol_type_in_its_namespace_scope() {
    let source = concat!(
        "<?php\n",
        "namespace First\\Space;\n",
        "use Vendor\\FirstResult as SharedResult;\n",
        "class FirstService\n",
        "{\n",
        "    public function result(): SharedResult|self {}\n",
        "}\n",
        "namespace Second\\Space;\n",
        "use Vendor\\SecondResult as SharedResult;\n",
        "class SecondService\n",
        "{\n",
        "    public function result(): SharedResult|self {}\n",
        "}\n",
    );
    let uri = "file:///workspace/src/MultiNamespaceServices.php";
    let file_symbols = parse_test_file_symbols(source, uri);
    let symbol = file_symbols
        .symbols
        .iter()
        .find(|symbol| symbol.fqn == "Second\\Space\\SecondService::result")
        .cloned()
        .expect("second namespace method");
    let return_type = symbol_effective_return_type(&symbol).expect("second namespace return type");
    let index = WorkspaceIndex::new();
    index.update_file(uri, file_symbols);

    let type_text = twig_context_type_info_text_for_symbol(
        &index,
        &php_lsp_types::FileSymbols::default(),
        &symbol,
        "",
        &return_type,
    )
    .expect("scoped external symbol type");

    assert_eq!(
        type_text,
        "Vendor\\SecondResult|Second\\Space\\SecondService"
    );
}

#[test]
fn infers_reused_variable_in_sibling_shape_values() {
    let source = concat!(
        "<?php\n",
        "$messageLog = new MessageLog();\n",
        "['first' => $messageLog, 'second' => $messageLog]\n",
    );
    let start = source.find("['first'").expect("shape start");
    let file_symbols = php_lsp_types::FileSymbols::default();

    let type_text =
        infer_twig_context_value_type(source, (start, source.len()), &file_symbols, None, None)
            .expect("reused variable shape type");

    assert_eq!(type_text, "array{first: MessageLog, second: MessageLog}");
}

#[test]
fn infers_reused_variable_in_multiple_append_rows() {
    let source = concat!(
        "<?php\n",
        "$row = ['nr' => 'NR-1'];\n",
        "$items = [];\n",
        "$items[] = $row;\n",
        "$items[] = $row;\n",
        "$this->render('index.html.twig', ['items' => $items]);\n",
    );
    let value_start = source.rfind("$items").expect("render variable usage");
    let file_symbols = php_lsp_types::FileSymbols::default();
    let mut visited_variables = HashSet::new();

    let type_text = infer_twig_context_assignment_value_type(
        source,
        value_start,
        "items",
        &file_symbols,
        None,
        None,
        &mut visited_variables,
    )
    .expect("append array shape type");

    assert_eq!(type_text, "array<int, array{nr: string}>");
}

#[test]
fn keeps_latest_non_empty_assignment_type_when_array_is_appended_later() {
    let source = concat!(
        "<?php\n",
        "$items = [new MessageLog()];\n",
        "$items[] = ['nr' => 'NR-1'];\n",
        "$this->render('index.html.twig', ['items' => $items]);\n",
    );
    let value_start = source.rfind("$items").expect("render variable usage");
    let file_symbols = php_lsp_types::FileSymbols::default();
    let mut visited_variables = HashSet::new();

    let type_text = infer_twig_context_assignment_value_type(
        source,
        value_start,
        "items",
        &file_symbols,
        None,
        None,
        &mut visited_variables,
    )
    .expect("latest non-empty assignment type");

    assert_eq!(type_text, "array<int, MessageLog>");
}

#[test]
fn finds_phpdoc_list_shape_definition_after_non_ascii_text() {
    let comment = "/**\n * @return list<array{🇺🇸 中国 བོད note: string, npId: string}>\n */";
    let symbol = php_lsp_types::SymbolInfo {
        name: "fetchRows".to_string(),
        fqn: "App\\Repository\\MessageLogRepository::fetchRows".to_string(),
        kind: php_lsp_types::PhpSymbolKind::Method,
        uri: "file:///workspace/src/Repository/MessageLogRepository.php".to_string(),
        range: (12, 4, 15, 5),
        selection_range: (12, 20, 12, 29),
        visibility: php_lsp_types::Visibility::Public,
        modifiers: php_lsp_types::SymbolModifiers::default(),
        attributes: Vec::new(),
        doc_comment: Some(comment.to_string()),
        signature: None,
        parent_fqn: Some("App\\Repository\\MessageLogRepository".to_string()),
        extends: Vec::new(),
        implements: Vec::new(),
        traits: Vec::new(),
        templates: Vec::new(),
        template_bindings: Vec::new(),
    };
    let type_info = php_lsp_types::TypeInfo::Generic {
        base: "list".to_string(),
        args: vec![php_lsp_types::TypeInfo::ArrayShape(vec![
            php_lsp_types::ArrayShapeItem {
                key: Some("note".to_string()),
                optional: false,
                value: php_lsp_types::TypeInfo::Simple("string".to_string()),
            },
            php_lsp_types::ArrayShapeItem {
                key: Some("npId".to_string()),
                optional: false,
                value: php_lsp_types::TypeInfo::Simple("string".to_string()),
            },
        ])],
    };
    let definitions = collect_symbol_type_shape_definitions(
        &WorkspaceIndex::new(),
        &php_lsp_types::FileSymbols::default(),
        &symbol,
        "App\\Repository\\MessageLogRepository",
        &type_info,
        TemplateShapeDefinitionTarget::Direct,
    );
    let definition = definitions
        .iter()
        .find(|definition| definition.path == vec!["npId".to_string()])
        .expect("npId shape definition");
    let np_id_offset = comment.find("npId").expect("npId in PHPDoc");
    let line_start = comment[..np_id_offset].rfind('\n').map_or(0, |idx| idx + 1);
    let expected_character = comment[line_start..np_id_offset].encode_utf16().count() as u32;

    assert_eq!(
        definition.target,
        TemplateShapeDefinitionTarget::IterableValue
    );
    assert_eq!(definition.range.0, 10);
    assert_eq!(definition.range.1, expected_character);
}

#[test]
fn resolves_twig_template_paths_under_app_templates() {
    let root =
        std::env::temp_dir().join(format!("php-lsp-app-templates-path-{}", std::process::id()));
    let path = root.join("app/templates/components/autocomplete_input.html.twig");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(path.parent().expect("template parent")).unwrap();
    std::fs::write(&path, "").unwrap();

    assert_eq!(
        twig_template_path_for_key(&root, "components/autocomplete_input.html.twig").as_deref(),
        Some(path.as_path())
    );

    let uri = path_to_uri(&path).expect("template uri");
    assert_eq!(
        twig_template_name_for_uri(&uri, &root).as_deref(),
        Some("components/autocomplete_input.html.twig")
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn twig_context_file_collection_uses_safe_external_symlink_walker() {
    use std::os::unix::fs::symlink;

    let root = std::env::temp_dir().join(format!(
        "php-lsp-twig-context-symlink-{}",
        std::process::id()
    ));
    let external = std::env::temp_dir().join(format!(
        "php-lsp-twig-context-external-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&external);
    std::fs::create_dir_all(&root).expect("create workspace");
    std::fs::create_dir_all(&external).expect("create external source");
    std::fs::write(external.join("Controller.php"), "<?php class Controller {}")
        .expect("write external PHP file");
    symlink(&external, root.join("src")).expect("link external source");
    symlink(&root, external.join("back")).expect("create source cycle");

    let files = collect_twig_context_php_files_with_limits(
        &root,
        16,
        TraversalLimits {
            max_files: Some(16),
            max_entries: Some(128),
        },
        &[],
    );
    assert_eq!(files, vec![root.join("src/Controller.php")]);
    let excluded = collect_twig_context_php_files_with_limits(
        &root,
        16,
        TraversalLimits {
            max_files: Some(16),
            max_entries: Some(128),
        },
        &[PathBuf::from("src")],
    );
    assert!(excluded.is_empty());

    std::fs::remove_file(external.join("back")).expect("remove cycle link");
    std::fs::remove_dir_all(root).expect("remove workspace");
    std::fs::remove_dir_all(external).expect("remove external source");
}
