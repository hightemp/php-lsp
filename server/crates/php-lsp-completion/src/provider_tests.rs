use super::*;
use php_lsp_types::*;

fn make_symbol(
    name: &str,
    fqn: &str,
    kind: PhpSymbolKind,
    parent_fqn: Option<&str>,
    visibility: Visibility,
    is_static: bool,
) -> SymbolInfo {
    SymbolInfo {
        name: name.to_string(),
        fqn: fqn.to_string(),
        kind,
        uri: "file:///test.php".to_string(),
        range: (0, 0, 0, 0),
        selection_range: (0, 0, 0, name.len() as u32),
        visibility,
        modifiers: SymbolModifiers {
            is_static,
            ..Default::default()
        },
        attributes: vec![],
        doc_comment: None,
        signature: if matches!(kind, PhpSymbolKind::Method | PhpSymbolKind::Function) {
            Some(Signature {
                params: vec![],
                return_type: None,
            })
        } else {
            None
        },
        parent_fqn: parent_fqn.map(str::to_string),
        extends: vec![],
        implements: vec![],
        traits: vec![],
        templates: vec![],
        template_bindings: vec![],
    }
}

fn with_range(mut symbol: SymbolInfo, range: (u32, u32, u32, u32)) -> SymbolInfo {
    symbol.range = range;
    symbol
}

fn with_params(mut symbol: SymbolInfo, params: Vec<ParamInfo>) -> SymbolInfo {
    symbol.signature = Some(Signature {
        params,
        return_type: None,
    });
    symbol
}

fn test_param(name: &str, type_info: Option<TypeInfo>, is_promoted: bool) -> ParamInfo {
    ParamInfo {
        name: name.to_string(),
        type_info,
        default_value: None,
        is_variadic: false,
        is_by_ref: false,
        is_promoted,
    }
}

#[test]
fn test_keyword_completion() {
    let index = WorkspaceIndex::new();
    let file_symbols = FileSymbols::default();
    let ctx = CompletionContext::Free {
        prefix: "cla".to_string(),
    };
    let items = provide_completions(&ctx, &index, &file_symbols);
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"class"), "Should contain 'class' keyword");
    let class_item = items
        .iter()
        .find(|item| item.label == "class")
        .expect("class keyword completion");
    assert_eq!(class_item.kind, Some(CompletionItemKind::SNIPPET));
    assert_eq!(
        class_item.insert_text_format,
        Some(InsertTextFormat::SNIPPET)
    );
    assert!(
        class_item
            .insert_text
            .as_deref()
            .is_some_and(|text| text.contains("${1:Name}")),
        "class completion should use snippet placeholders"
    );
}

#[test]
fn test_class_completion() {
    let index = WorkspaceIndex::new();
    let file_symbols = FileSymbols {
        namespace: Some("App".to_string()),
        use_statements: vec![],
        symbols: vec![SymbolInfo {
            name: "UserService".to_string(),
            fqn: "App\\UserService".to_string(),
            kind: PhpSymbolKind::Class,
            uri: "file:///test.php".to_string(),
            range: (0, 0, 10, 0),
            selection_range: (0, 6, 0, 17),
            visibility: Visibility::Public,
            modifiers: SymbolModifiers::default(),
            attributes: vec![],
            doc_comment: None,
            signature: None,
            parent_fqn: None,
            extends: vec![],
            implements: vec![],
            traits: vec![],
            templates: vec![],
            template_bindings: vec![],
        }],
        ..Default::default()
    };
    index.update_file("file:///test.php", file_symbols.clone());

    let ctx = CompletionContext::Free {
        prefix: "User".to_string(),
    };
    let items = provide_completions(&ctx, &index, &file_symbols);
    assert!(
        items.iter().any(|i| i.label == "UserService"),
        "Should find UserService"
    );
}

#[test]
fn test_use_statement_completion_inserts_full_fqn() {
    let mut class = make_symbol(
        "ClassName",
        "Vendor\\Package\\ClassName",
        PhpSymbolKind::Class,
        None,
        Visibility::Public,
        false,
    );
    class.uri = "file:///vendor/ClassName.php".to_string();
    let symbols = FileSymbols {
        symbols: vec![class],
        ..Default::default()
    };
    let index = WorkspaceIndex::new();
    index.update_file("file:///vendor/ClassName.php", symbols);

    let ctx = CompletionContext::UseStatement {
        prefix: "Ven".to_string(),
    };
    let items = provide_completions(&ctx, &index, &FileSymbols::default());
    let item = items
        .iter()
        .find(|item| item.label == "ClassName")
        .expect("use completion should keep short class label");

    assert_eq!(
        item.insert_text.as_deref(),
        Some("Vendor\\Package\\ClassName")
    );
    assert_eq!(item.detail.as_deref(), Some("Vendor\\Package\\ClassName"));
}

#[test]
fn test_namespace_completion_prioritizes_fqn_prefix_over_contains_matches() {
    let file_symbols = FileSymbols {
        symbols: vec![
            make_symbol(
                "AlphaNoise",
                "Vendor\\App\\AlphaNoise",
                PhpSymbolKind::Class,
                None,
                Visibility::Public,
                false,
            ),
            make_symbol(
                "ZedService",
                "App\\ZedService",
                PhpSymbolKind::Class,
                None,
                Visibility::Public,
                false,
            ),
        ],
        ..Default::default()
    };
    let index = WorkspaceIndex::new();
    index.update_file("file:///test.php", file_symbols.clone());

    let ctx = CompletionContext::Namespace {
        prefix: "App\\".to_string(),
    };
    let items = provide_completions(&ctx, &index, &file_symbols);
    let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();

    assert_eq!(labels.first(), Some(&"ZedService"));
    assert_eq!(labels, vec!["ZedService", "AlphaNoise"]);
}

#[test]
fn test_namespace_completion_keeps_prefix_matches_before_truncating_contains_noise() {
    let mut symbols = Vec::new();
    for idx in 0..120 {
        symbols.push(make_symbol(
            &format!("AlphaTyNoise{idx:03}"),
            &format!("App\\AlphaTyNoise{idx:03}"),
            PhpSymbolKind::Class,
            None,
            Visibility::Public,
            false,
        ));
    }
    symbols.push(make_symbol(
        "TypeGuess",
        "App\\TypeGuess",
        PhpSymbolKind::Class,
        None,
        Visibility::Public,
        false,
    ));
    let file_symbols = FileSymbols {
        namespace: Some("App".to_string()),
        use_statements: vec![],
        symbols,
        ..Default::default()
    };
    let index = WorkspaceIndex::new();
    index.update_file("file:///test.php", file_symbols.clone());

    let ctx = CompletionContext::Namespace {
        prefix: "Ty".to_string(),
    };
    let items = provide_completions(&ctx, &index, &file_symbols);

    assert_eq!(
        items.first().map(|item| item.label.as_str()),
        Some("TypeGuess")
    );
    assert!(
        items.iter().any(|item| item.label == "TypeGuess"),
        "prefix match should survive namespace completion truncation"
    );
}

#[test]
fn test_free_completion_ranks_prefix_matches_before_contains_matches() {
    let mut symbols = Vec::new();
    for idx in 0..120 {
        symbols.push(make_symbol(
            &format!("OtherTyNoise{idx:03}"),
            &format!("App\\OtherTyNoise{idx:03}"),
            PhpSymbolKind::Class,
            None,
            Visibility::Public,
            false,
        ));
    }
    symbols.push(make_symbol(
        "TypeGuess",
        "App\\TypeGuess",
        PhpSymbolKind::Class,
        None,
        Visibility::Public,
        false,
    ));
    let file_symbols = FileSymbols {
        namespace: Some("App".to_string()),
        use_statements: vec![],
        symbols,
        ..Default::default()
    };
    let index = WorkspaceIndex::new();
    index.update_file("file:///test.php", file_symbols.clone());

    let ctx = CompletionContext::Free {
        prefix: "Ty".to_string(),
    };
    let items = provide_completions(&ctx, &index, &file_symbols);

    assert_eq!(
        items.first().map(|item| item.label.as_str()),
        Some("TypeGuess")
    );
    assert!(
        items.iter().any(|item| item.label == "TypeGuess"),
        "prefix match should survive truncation"
    );
}

#[test]
fn test_variable_completion() {
    let file_symbols = FileSymbols {
        namespace: None,
        use_statements: vec![],
        symbols: vec![SymbolInfo {
            name: "test".to_string(),
            fqn: "test".to_string(),
            kind: PhpSymbolKind::Function,
            uri: "file:///test.php".to_string(),
            range: (0, 0, 5, 0),
            selection_range: (0, 9, 0, 13),
            visibility: Visibility::Public,
            modifiers: SymbolModifiers::default(),
            attributes: vec![],
            doc_comment: None,
            signature: Some(Signature {
                params: vec![ParamInfo {
                    name: "username".to_string(),
                    type_info: Some(TypeInfo::Simple("string".to_string())),
                    default_value: None,
                    is_variadic: false,
                    is_by_ref: false,
                    is_promoted: false,
                }],
                return_type: None,
            }),
            parent_fqn: None,
            extends: vec![],
            implements: vec![],
            traits: vec![],
            templates: vec![],
            template_bindings: vec![],
        }],
        ..Default::default()
    };
    let index = WorkspaceIndex::new();

    let ctx = CompletionContext::Variable {
        prefix: "user".to_string(),
    };
    let items = provide_completions_at_range(&ctx, &index, &file_symbols, (2, 4, 2, 4));
    assert!(
        items.iter().any(|i| i.label == "$username"),
        "Should find $username"
    );
    assert_eq!(
        items
            .iter()
            .find(|item| item.label == "$username")
            .and_then(|item| item.detail.as_deref()),
        Some("string")
    );

    let range_less_items = provide_completions(&ctx, &index, &file_symbols);
    assert!(
        !range_less_items
            .iter()
            .any(|item| item.label == "$username"),
        "Range-less completion must not guess a callable scope"
    );
}

#[test]
fn test_variable_completion_uses_only_the_innermost_callable_parameters() {
    let outer = with_params(
        with_range(
            make_symbol(
                "outer",
                "outer",
                PhpSymbolKind::Function,
                None,
                Visibility::Public,
                false,
            ),
            (0, 0, 20, 1),
        ),
        vec![test_param("outerParam", None, false)],
    );
    let nested = with_params(
        with_range(
            make_symbol(
                "nested",
                "nested",
                PhpSymbolKind::Function,
                None,
                Visibility::Public,
                false,
            ),
            (5, 4, 10, 5),
        ),
        vec![test_param("nestedParam", None, false)],
    );
    let sibling = with_params(
        with_range(
            make_symbol(
                "sibling",
                "sibling",
                PhpSymbolKind::Function,
                None,
                Visibility::Public,
                false,
            ),
            (22, 0, 30, 1),
        ),
        vec![test_param("siblingParam", None, false)],
    );
    let constructor = with_params(
        with_range(
            make_symbol(
                "__construct",
                "App\\Subject::__construct",
                PhpSymbolKind::Method,
                Some("App\\Subject"),
                Visibility::Public,
                false,
            ),
            (32, 4, 40, 5),
        ),
        vec![test_param(
            "promotedParam",
            Some(TypeInfo::Simple("string".to_string())),
            true,
        )],
    );
    let other_method = with_params(
        with_range(
            make_symbol(
                "other",
                "App\\Subject::other",
                PhpSymbolKind::Method,
                Some("App\\Subject"),
                Visibility::Public,
                false,
            ),
            (42, 4, 48, 5),
        ),
        vec![test_param("otherMethodParam", None, false)],
    );
    let file_symbols = FileSymbols {
        symbols: vec![outer, nested, sibling, constructor, other_method],
        ..Default::default()
    };
    let index = WorkspaceIndex::new();
    let context = CompletionContext::Variable {
        prefix: String::new(),
    };

    let labels_at = |line| {
        provide_completions_at_range(&context, &index, &file_symbols, (line, 8, line, 8))
            .into_iter()
            .map(|item| item.label)
            .collect::<Vec<_>>()
    };

    let nested_labels = labels_at(7);
    assert!(nested_labels.contains(&"$nestedParam".to_string()));
    assert!(!nested_labels.contains(&"$outerParam".to_string()));
    assert!(!nested_labels.contains(&"$siblingParam".to_string()));

    let outer_labels = labels_at(15);
    assert!(outer_labels.contains(&"$outerParam".to_string()));
    assert!(!outer_labels.contains(&"$nestedParam".to_string()));

    let sibling_labels = labels_at(25);
    assert!(sibling_labels.contains(&"$siblingParam".to_string()));
    assert!(!sibling_labels.contains(&"$outerParam".to_string()));

    let constructor_items =
        provide_completions_at_range(&context, &index, &file_symbols, (35, 8, 35, 8));
    let promoted = constructor_items
        .iter()
        .find(|item| item.label == "$promotedParam")
        .expect("promoted constructor parameter");
    assert_eq!(promoted.detail.as_deref(), Some("string"));
    assert!(!constructor_items
        .iter()
        .any(|item| item.label == "$otherMethodParam"));

    let other_method_labels = labels_at(45);
    assert!(other_method_labels.contains(&"$otherMethodParam".to_string()));
    assert!(!other_method_labels.contains(&"$promotedParam".to_string()));

    let global_labels = labels_at(50);
    for leaked in [
        "$outerParam",
        "$nestedParam",
        "$siblingParam",
        "$promotedParam",
        "$otherMethodParam",
    ] {
        assert!(!global_labels.contains(&leaked.to_string()));
    }
}

#[test]
fn test_member_completion_uses_inferred_class_fqn() {
    let file_symbols = FileSymbols {
        namespace: Some("App".to_string()),
        use_statements: vec![],
        symbols: vec![
            SymbolInfo {
                name: "Baz".to_string(),
                fqn: "App\\Test\\Baz".to_string(),
                kind: PhpSymbolKind::Class,
                uri: "file:///test.php".to_string(),
                range: (0, 0, 10, 0),
                selection_range: (0, 6, 0, 9),
                visibility: Visibility::Public,
                modifiers: SymbolModifiers::default(),
                attributes: vec![],
                doc_comment: None,
                signature: None,
                parent_fqn: None,
                extends: vec![],
                implements: vec![],
                traits: vec![],
                templates: vec![],
                template_bindings: vec![],
            },
            SymbolInfo {
                name: "test".to_string(),
                fqn: "App\\Test\\Baz::test".to_string(),
                kind: PhpSymbolKind::Method,
                uri: "file:///test.php".to_string(),
                range: (2, 4, 2, 20),
                selection_range: (2, 13, 2, 17),
                visibility: Visibility::Public,
                modifiers: SymbolModifiers::default(),
                attributes: vec![],
                doc_comment: None,
                signature: Some(Signature {
                    params: vec![],
                    return_type: None,
                }),
                parent_fqn: Some("App\\Test\\Baz".to_string()),
                extends: vec![],
                implements: vec![],
                traits: vec![],
                templates: vec![],
                template_bindings: vec![],
            },
        ],
        ..Default::default()
    };

    let index = WorkspaceIndex::new();
    index.update_file("file:///test.php", file_symbols.clone());

    let ctx = CompletionContext::MemberAccess {
        object_expr: "$baz2".to_string(),
        member_prefix: String::new(),
        class_fqn: Some("App\\Test\\Baz".to_string()),
        access_mode: MemberAccessMode::Read,
    };
    let items = provide_completions(&ctx, &index, &file_symbols);

    assert!(
        items.iter().any(|i| i.label == "test"),
        "Should include members of inferred class"
    );
}

#[test]
fn test_member_completion_filters_static_and_visibility() {
    let file_symbols = FileSymbols {
        namespace: Some("App".to_string()),
        use_statements: vec![],
        symbols: vec![
            make_symbol(
                "Service",
                "App\\Service",
                PhpSymbolKind::Class,
                None,
                Visibility::Public,
                false,
            ),
            make_symbol(
                "name",
                "App\\Service::$name",
                PhpSymbolKind::Property,
                Some("App\\Service"),
                Visibility::Public,
                false,
            ),
            make_symbol(
                "secret",
                "App\\Service::$secret",
                PhpSymbolKind::Property,
                Some("App\\Service"),
                Visibility::Private,
                false,
            ),
            make_symbol(
                "create",
                "App\\Service::create",
                PhpSymbolKind::Method,
                Some("App\\Service"),
                Visibility::Public,
                true,
            ),
        ],
        ..Default::default()
    };
    let index = WorkspaceIndex::new();
    index.update_file("file:///test.php", file_symbols.clone());

    let ctx = CompletionContext::MemberAccess {
        object_expr: "$service".to_string(),
        member_prefix: String::new(),
        class_fqn: Some("App\\Service".to_string()),
        access_mode: MemberAccessMode::Read,
    };
    let items = provide_completions(&ctx, &index, &file_symbols);
    let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();

    assert!(labels.contains(&"name"));
    assert!(
        !labels.contains(&"$name"),
        "instance property should omit `$`"
    );
    assert!(
        !labels.contains(&"secret"),
        "external private member should be hidden"
    );
    assert!(
        !labels.contains(&"create"),
        "static method should be hidden on `->`"
    );
}

#[test]
fn test_member_completion_sorts_methods_before_properties() {
    let file_symbols = FileSymbols {
        namespace: Some("App".to_string()),
        use_statements: vec![],
        symbols: vec![
            make_symbol(
                "Client",
                "App\\Client",
                PhpSymbolKind::Class,
                None,
                Visibility::Public,
                false,
            ),
            make_symbol(
                "requestHeaders",
                "App\\Client::$requestHeaders",
                PhpSymbolKind::Property,
                Some("App\\Client"),
                Visibility::Public,
                false,
            ),
            make_symbol(
                "getRequest",
                "App\\Client::getRequest",
                PhpSymbolKind::Method,
                Some("App\\Client"),
                Visibility::Public,
                false,
            ),
            make_symbol(
                "request",
                "App\\Client::request",
                PhpSymbolKind::Method,
                Some("App\\Client"),
                Visibility::Public,
                false,
            ),
        ],
        ..Default::default()
    };
    let index = WorkspaceIndex::new();
    index.update_file("file:///test.php", file_symbols.clone());

    let ctx = CompletionContext::MemberAccess {
        object_expr: "$client".to_string(),
        member_prefix: "reques".to_string(),
        class_fqn: Some("App\\Client".to_string()),
        access_mode: MemberAccessMode::Read,
    };
    let items = provide_completions(&ctx, &index, &file_symbols);
    let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();

    assert_eq!(labels.first().copied(), Some("request"));
    assert!(
        labels.iter().position(|label| *label == "request").unwrap()
            < labels
                .iter()
                .position(|label| *label == "requestHeaders")
                .unwrap(),
        "methods should sort before properties in member completion"
    );
    assert!(
        labels.iter().position(|label| *label == "request").unwrap()
            < labels
                .iter()
                .position(|label| *label == "getRequest")
                .unwrap(),
        "members starting with typed prefix should sort before substring matches"
    );
}

#[test]
fn test_member_completion_includes_phpdoc_virtual_members() {
    let mut service = make_symbol(
        "Service",
        "App\\Service",
        PhpSymbolKind::Class,
        None,
        Visibility::Public,
        false,
    );
    service.doc_comment = Some(
        "/**\n * @property-read string $slug Service slug\n * @method User owner()\n */"
            .to_string(),
    );
    let file_symbols = FileSymbols {
        namespace: Some("App".to_string()),
        use_statements: vec![],
        symbols: vec![service],
        ..Default::default()
    };
    let index = WorkspaceIndex::new();
    index.update_file("file:///test.php", file_symbols.clone());

    let ctx = CompletionContext::MemberAccess {
        object_expr: "$service".to_string(),
        member_prefix: String::new(),
        class_fqn: Some("App\\Service".to_string()),
        access_mode: MemberAccessMode::Read,
    };
    let items = provide_completions(&ctx, &index, &file_symbols);

    let slug = items
        .iter()
        .find(|item| item.label == "slug")
        .expect("virtual property completion");
    assert_eq!(slug.kind, Some(CompletionItemKind::PROPERTY));
    assert_eq!(slug.detail.as_deref(), Some("@property-read string"));
    assert!(
        slug.data
            .as_ref()
            .and_then(|data| data.get("kind"))
            .and_then(|kind| kind.as_str())
            == Some("phpdoc-virtual-member")
    );

    let owner = items
        .iter()
        .find(|item| item.label == "owner")
        .expect("virtual method completion");
    assert_eq!(owner.kind, Some(CompletionItemKind::METHOD));
    assert_eq!(owner.detail.as_deref(), Some("(): User"));
}

#[test]
fn test_member_completion_filters_phpdoc_properties_by_access_mode() {
    let mut service = make_symbol(
        "Service",
        "App\\Service",
        PhpSymbolKind::Class,
        None,
        Visibility::Public,
        false,
    );
    service.doc_comment = Some(
            "/**\n * @property-read int $version\n * @property-write bool $dirty\n * @property string $label\n */"
                .to_string(),
        );
    let file_symbols = FileSymbols {
        symbols: vec![service],
        ..Default::default()
    };
    let index = WorkspaceIndex::new();
    index.update_file("file:///test.php", file_symbols.clone());

    let read_ctx = CompletionContext::MemberAccess {
        object_expr: "$service".to_string(),
        member_prefix: String::new(),
        class_fqn: Some("App\\Service".to_string()),
        access_mode: MemberAccessMode::Read,
    };
    let read_items = provide_completions(&read_ctx, &index, &file_symbols);
    let read_labels: Vec<&str> = read_items.iter().map(|item| item.label.as_str()).collect();
    assert!(read_labels.contains(&"version"));
    assert!(read_labels.contains(&"label"));
    assert!(
        !read_labels.contains(&"dirty"),
        "write-only virtual properties should be hidden in read completion"
    );

    let write_ctx = CompletionContext::MemberAccess {
        object_expr: "$service".to_string(),
        member_prefix: String::new(),
        class_fqn: Some("App\\Service".to_string()),
        access_mode: MemberAccessMode::Write,
    };
    let write_items = provide_completions(&write_ctx, &index, &file_symbols);
    let write_labels: Vec<&str> = write_items.iter().map(|item| item.label.as_str()).collect();
    assert!(write_labels.contains(&"dirty"));
    assert!(write_labels.contains(&"label"));
    assert!(
        !write_labels.contains(&"version"),
        "read-only virtual properties should be hidden in write completion"
    );
}

#[test]
fn test_member_completion_inherits_phpdoc_virtual_members() {
    let mut base = make_symbol(
        "BaseService",
        "App\\BaseService",
        PhpSymbolKind::Class,
        None,
        Visibility::Public,
        false,
    );
    base.doc_comment = Some("/**\n * @property int $id\n */".to_string());
    let mut service = make_symbol(
        "Service",
        "App\\Service",
        PhpSymbolKind::Class,
        None,
        Visibility::Public,
        false,
    );
    service.extends = vec!["App\\BaseService".to_string()];
    let file_symbols = FileSymbols {
        namespace: Some("App".to_string()),
        use_statements: vec![],
        symbols: vec![base, service],
        ..Default::default()
    };
    let index = WorkspaceIndex::new();
    index.update_file("file:///test.php", file_symbols.clone());

    let ctx = CompletionContext::MemberAccess {
        object_expr: "$service".to_string(),
        member_prefix: String::new(),
        class_fqn: Some("App\\Service".to_string()),
        access_mode: MemberAccessMode::Read,
    };
    let items = provide_completions(&ctx, &index, &file_symbols);

    assert!(items.iter().any(|item| item.label == "id"));
}

#[test]
fn test_static_completion_filters_instance_members() {
    let file_symbols = FileSymbols {
        namespace: Some("App".to_string()),
        use_statements: vec![],
        symbols: vec![
            make_symbol(
                "Service",
                "App\\Service",
                PhpSymbolKind::Class,
                None,
                Visibility::Public,
                false,
            ),
            make_symbol(
                "run",
                "App\\Service::run",
                PhpSymbolKind::Method,
                Some("App\\Service"),
                Visibility::Public,
                false,
            ),
            make_symbol(
                "create",
                "App\\Service::create",
                PhpSymbolKind::Method,
                Some("App\\Service"),
                Visibility::Public,
                true,
            ),
            make_symbol(
                "counter",
                "App\\Service::$counter",
                PhpSymbolKind::Property,
                Some("App\\Service"),
                Visibility::Public,
                true,
            ),
        ],
        ..Default::default()
    };
    let index = WorkspaceIndex::new();
    index.update_file("file:///test.php", file_symbols.clone());

    let ctx = CompletionContext::StaticAccess {
        class_expr: "Service".to_string(),
        member_prefix: String::new(),
        class_fqn: "App\\Service".to_string(),
    };
    let items = provide_completions(&ctx, &index, &file_symbols);
    let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();

    assert!(labels.contains(&"create"));
    assert!(labels.contains(&"$counter"));
    assert!(
        !labels.contains(&"run"),
        "instance method should be hidden on `::`"
    );
}

#[test]
fn test_static_completion_sorts_constants_before_methods_and_properties() {
    let mut service = make_symbol(
        "Service",
        "App\\Service",
        PhpSymbolKind::Class,
        None,
        Visibility::Public,
        false,
    );
    service.doc_comment = Some("/**\n * @method static self zip()\n */".to_string());
    let file_symbols = FileSymbols {
        namespace: Some("App".to_string()),
        use_statements: vec![],
        symbols: vec![
            service,
            make_symbol(
                "make",
                "App\\Service::make",
                PhpSymbolKind::Method,
                Some("App\\Service"),
                Visibility::Public,
                true,
            ),
            make_symbol(
                "counter",
                "App\\Service::$counter",
                PhpSymbolKind::Property,
                Some("App\\Service"),
                Visibility::Public,
                true,
            ),
            make_symbol(
                "CREATE",
                "App\\Service::CREATE",
                PhpSymbolKind::ClassConstant,
                Some("App\\Service"),
                Visibility::Public,
                false,
            ),
            make_symbol(
                "OVERWRITE",
                "App\\Service::OVERWRITE",
                PhpSymbolKind::ClassConstant,
                Some("App\\Service"),
                Visibility::Public,
                false,
            ),
        ],
        ..Default::default()
    };
    let index = WorkspaceIndex::new();
    index.update_file("file:///test.php", file_symbols.clone());

    let ctx = CompletionContext::StaticAccess {
        class_expr: "Service".to_string(),
        member_prefix: String::new(),
        class_fqn: "App\\Service".to_string(),
    };
    let items = provide_completions(&ctx, &index, &file_symbols);
    let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();

    assert_eq!(
        &labels[..6],
        &["CREATE", "OVERWRITE", "class", "make", "zip", "$counter"],
        "static completion should put class constants before other static members: {labels:?}"
    );
}

#[test]
fn test_static_completion_includes_static_phpdoc_virtual_methods() {
    let mut service = make_symbol(
        "Service",
        "App\\Service",
        PhpSymbolKind::Class,
        None,
        Visibility::Public,
        false,
    );
    service.doc_comment =
        Some("/**\n * @method User owner()\n * @method static self make()\n */".to_string());
    let file_symbols = FileSymbols {
        symbols: vec![service],
        ..Default::default()
    };
    let index = WorkspaceIndex::new();
    index.update_file("file:///test.php", file_symbols.clone());

    let ctx = CompletionContext::StaticAccess {
        class_expr: "Service".to_string(),
        member_prefix: String::new(),
        class_fqn: "App\\Service".to_string(),
    };
    let items = provide_completions(&ctx, &index, &file_symbols);
    let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();

    assert!(labels.contains(&"make"));
    assert!(
        !labels.contains(&"owner"),
        "instance @method should not appear in static completion"
    );
}

#[test]
fn test_static_completion_includes_class_pseudo_constant() {
    let file_symbols = FileSymbols {
        namespace: Some("App".to_string()),
        use_statements: vec![],
        symbols: vec![make_symbol(
            "Service",
            "App\\Service",
            PhpSymbolKind::Class,
            None,
            Visibility::Public,
            false,
        )],
        ..Default::default()
    };
    let index = WorkspaceIndex::new();
    index.update_file("file:///test.php", file_symbols.clone());

    let ctx = CompletionContext::StaticAccess {
        class_expr: "Service".to_string(),
        member_prefix: String::new(),
        class_fqn: "App\\Service".to_string(),
    };
    let items = provide_completions(&ctx, &index, &file_symbols);
    let class_item = items
        .iter()
        .find(|item| item.label == "class")
        .expect("static completion should include ::class");

    assert_eq!(class_item.kind, Some(CompletionItemKind::CONSTANT));
}

#[test]
fn test_parent_static_completion_includes_instance_methods() {
    let file_symbols = FileSymbols {
        namespace: Some("App".to_string()),
        use_statements: vec![],
        symbols: vec![
            make_symbol(
                "Base",
                "App\\Base",
                PhpSymbolKind::Class,
                None,
                Visibility::Public,
                false,
            ),
            make_symbol(
                "setUp",
                "App\\Base::setUp",
                PhpSymbolKind::Method,
                Some("App\\Base"),
                Visibility::Protected,
                false,
            ),
            make_symbol(
                "create",
                "App\\Base::create",
                PhpSymbolKind::Method,
                Some("App\\Base"),
                Visibility::Public,
                true,
            ),
        ],
        ..Default::default()
    };
    let index = WorkspaceIndex::new();
    index.update_file("file:///test.php", file_symbols.clone());

    let ctx = CompletionContext::StaticAccess {
        class_expr: "parent".to_string(),
        member_prefix: String::new(),
        class_fqn: "App\\Base".to_string(),
    };
    let items = provide_completions(&ctx, &index, &file_symbols);
    let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();

    assert!(
        labels.contains(&"setUp"),
        "`parent::` should complete inherited instance methods"
    );
    assert!(labels.contains(&"create"));
}

#[test]
fn test_member_completion_uses_cursor_class_for_two_classes() {
    let base = with_range(
        make_symbol(
            "Base",
            "App\\Base",
            PhpSymbolKind::Class,
            None,
            Visibility::Public,
            false,
        ),
        (0, 0, 6, 1),
    );
    let base_secret = make_symbol(
        "baseSecret",
        "App\\Base::baseSecret",
        PhpSymbolKind::Method,
        Some("App\\Base"),
        Visibility::Private,
        false,
    );
    let mut child = with_range(
        make_symbol(
            "Child",
            "App\\Child",
            PhpSymbolKind::Class,
            None,
            Visibility::Public,
            false,
        ),
        (8, 0, 14, 1),
    );
    child.extends = vec!["App\\Base".to_string()];
    let child_secret = make_symbol(
        "childSecret",
        "App\\Child::childSecret",
        PhpSymbolKind::Method,
        Some("App\\Child"),
        Visibility::Private,
        false,
    );
    let file_symbols = FileSymbols {
        namespace: Some("App".to_string()),
        use_statements: vec![],
        symbols: vec![base, base_secret, child, child_secret],
        ..Default::default()
    };
    let index = WorkspaceIndex::new();
    index.update_file("file:///test.php", file_symbols.clone());

    let ctx = CompletionContext::MemberAccess {
        object_expr: "$this".to_string(),
        member_prefix: String::new(),
        class_fqn: Some("App\\Child".to_string()),
        access_mode: MemberAccessMode::Read,
    };
    let items = provide_completions_at_range(&ctx, &index, &file_symbols, (10, 12, 10, 12));
    let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();

    assert!(
        labels.contains(&"childSecret"),
        "$this-> should include private members from the cursor class"
    );
    assert!(
        !labels.contains(&"baseSecret"),
        "$this-> should not expose private members from another class"
    );
}

#[test]
fn test_member_completion_uses_cursor_trait_context() {
    let other = with_range(
        make_symbol(
            "Other",
            "App\\Other",
            PhpSymbolKind::Class,
            None,
            Visibility::Public,
            false,
        ),
        (0, 0, 4, 1),
    );
    let feature = with_range(
        make_symbol(
            "Feature",
            "App\\Feature",
            PhpSymbolKind::Trait,
            None,
            Visibility::Public,
            false,
        ),
        (6, 0, 12, 1),
    );
    let trait_secret = make_symbol(
        "traitSecret",
        "App\\Feature::traitSecret",
        PhpSymbolKind::Method,
        Some("App\\Feature"),
        Visibility::Private,
        false,
    );
    let file_symbols = FileSymbols {
        namespace: Some("App".to_string()),
        use_statements: vec![],
        symbols: vec![other, feature, trait_secret],
        ..Default::default()
    };
    let index = WorkspaceIndex::new();
    index.update_file("file:///test.php", file_symbols.clone());

    let ctx = CompletionContext::MemberAccess {
        object_expr: "$this".to_string(),
        member_prefix: String::new(),
        class_fqn: Some("App\\Feature".to_string()),
        access_mode: MemberAccessMode::Read,
    };
    let items = provide_completions_at_range(&ctx, &index, &file_symbols, (8, 12, 8, 12));
    let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();

    assert!(
        labels.contains(&"traitSecret"),
        "$this-> should use the trait at the cursor for private visibility"
    );
}

#[test]
fn test_member_completion_uses_innermost_anonymous_class_context() {
    let outer = with_range(
        make_symbol(
            "Outer",
            "App\\Outer",
            PhpSymbolKind::Class,
            None,
            Visibility::Public,
            false,
        ),
        (0, 0, 24, 1),
    );
    let outer_secret = make_symbol(
        "outerSecret",
        "App\\Outer::outerSecret",
        PhpSymbolKind::Method,
        Some("App\\Outer"),
        Visibility::Private,
        false,
    );
    let anonymous_fqn = "App\\Outer@anonymous:8";
    let mut anonymous = with_range(
        make_symbol(
            "anonymous",
            anonymous_fqn,
            PhpSymbolKind::Class,
            None,
            Visibility::Public,
            false,
        ),
        (8, 8, 16, 9),
    );
    anonymous.extends = vec!["App\\Outer".to_string()];
    let anonymous_secret = make_symbol(
        "anonymousSecret",
        "App\\Outer@anonymous:8::anonymousSecret",
        PhpSymbolKind::Method,
        Some(anonymous_fqn),
        Visibility::Private,
        false,
    );
    let file_symbols = FileSymbols {
        namespace: Some("App".to_string()),
        use_statements: vec![],
        symbols: vec![outer, outer_secret, anonymous, anonymous_secret],
        ..Default::default()
    };
    let index = WorkspaceIndex::new();
    index.update_file("file:///test.php", file_symbols.clone());

    let ctx = CompletionContext::MemberAccess {
        object_expr: "$this".to_string(),
        member_prefix: String::new(),
        class_fqn: Some(anonymous_fqn.to_string()),
        access_mode: MemberAccessMode::Read,
    };
    let items = provide_completions_at_range(&ctx, &index, &file_symbols, (12, 16, 12, 16));
    let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();

    assert!(
        labels.contains(&"anonymousSecret"),
        "anonymous class private members should be visible inside that class"
    );
    assert!(
        !labels.contains(&"outerSecret"),
        "outer class private members should not leak into an anonymous class"
    );
}

#[test]
fn test_static_completion_uses_cursor_class_for_self_static_and_parent() {
    let base = with_range(
        make_symbol(
            "Base",
            "App\\Base",
            PhpSymbolKind::Class,
            None,
            Visibility::Public,
            false,
        ),
        (0, 0, 6, 1),
    );
    let base_private = make_symbol(
        "basePrivate",
        "App\\Base::basePrivate",
        PhpSymbolKind::Method,
        Some("App\\Base"),
        Visibility::Private,
        true,
    );
    let base_protected = make_symbol(
        "baseProtected",
        "App\\Base::baseProtected",
        PhpSymbolKind::Method,
        Some("App\\Base"),
        Visibility::Protected,
        false,
    );
    let mut child = with_range(
        make_symbol(
            "Child",
            "App\\Child",
            PhpSymbolKind::Class,
            None,
            Visibility::Public,
            false,
        ),
        (8, 0, 14, 1),
    );
    child.extends = vec!["App\\Base".to_string()];
    let child_private = make_symbol(
        "childPrivate",
        "App\\Child::childPrivate",
        PhpSymbolKind::Method,
        Some("App\\Child"),
        Visibility::Private,
        true,
    );
    let file_symbols = FileSymbols {
        namespace: Some("App".to_string()),
        use_statements: vec![],
        symbols: vec![base, base_private, base_protected, child, child_private],
        ..Default::default()
    };
    let index = WorkspaceIndex::new();
    index.update_file("file:///test.php", file_symbols.clone());

    for class_expr in ["self", "static"] {
        let ctx = CompletionContext::StaticAccess {
            class_expr: class_expr.to_string(),
            member_prefix: String::new(),
            class_fqn: "App\\Child".to_string(),
        };
        let items = provide_completions_at_range(&ctx, &index, &file_symbols, (10, 12, 10, 12));
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();

        assert!(
            labels.contains(&"childPrivate"),
            "{class_expr}:: should include private members from the cursor class"
        );
        assert!(
            !labels.contains(&"basePrivate"),
            "{class_expr}:: should not expose private members from another class"
        );
    }

    let ctx = CompletionContext::StaticAccess {
        class_expr: "parent".to_string(),
        member_prefix: String::new(),
        class_fqn: "App\\Base".to_string(),
    };
    let items = provide_completions_at_range(&ctx, &index, &file_symbols, (10, 12, 10, 12));
    let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();

    assert!(
        labels.contains(&"baseProtected"),
        "parent:: should include protected parent instance methods"
    );
    assert!(
        !labels.contains(&"basePrivate"),
        "parent:: should not expose private parent members"
    );
}

#[test]
fn current_file_members_replace_stale_index_generation() {
    let stale_symbols = FileSymbols {
        symbols: vec![
            make_symbol(
                "Subject",
                "App\\Subject",
                PhpSymbolKind::Class,
                None,
                Visibility::Public,
                false,
            ),
            make_symbol(
                "oldMethod",
                "App\\Subject::oldMethod",
                PhpSymbolKind::Method,
                Some("App\\Subject"),
                Visibility::Public,
                false,
            ),
            make_symbol(
                "oldStatic",
                "App\\Subject::oldStatic",
                PhpSymbolKind::Method,
                Some("App\\Subject"),
                Visibility::Public,
                true,
            ),
        ],
        ..Default::default()
    };
    let current_symbols = FileSymbols {
        symbols: vec![
            make_symbol(
                "Subject",
                "App\\Subject",
                PhpSymbolKind::Class,
                None,
                Visibility::Public,
                false,
            ),
            make_symbol(
                "newMethod",
                "App\\Subject::newMethod",
                PhpSymbolKind::Method,
                Some("App\\Subject"),
                Visibility::Public,
                false,
            ),
            make_symbol(
                "newStatic",
                "App\\Subject::newStatic",
                PhpSymbolKind::Method,
                Some("App\\Subject"),
                Visibility::Public,
                true,
            ),
        ],
        ..Default::default()
    };
    let index = WorkspaceIndex::new();
    index.update_file("file:///subject.php", stale_symbols);

    let member_items = provide_completions(
        &CompletionContext::MemberAccess {
            object_expr: "$this".to_string(),
            member_prefix: String::new(),
            class_fqn: Some("App\\Subject".to_string()),
            access_mode: MemberAccessMode::Read,
        },
        &index,
        &current_symbols,
    );
    let member_labels: Vec<_> = member_items
        .iter()
        .map(|item| item.label.as_str())
        .collect();
    assert!(member_labels.contains(&"newMethod"));
    assert!(!member_labels.contains(&"oldMethod"));

    let static_items = provide_completions(
        &CompletionContext::StaticAccess {
            class_fqn: "App\\Subject".to_string(),
            class_expr: "self".to_string(),
            member_prefix: String::new(),
        },
        &index,
        &current_symbols,
    );
    let static_labels: Vec<_> = static_items
        .iter()
        .map(|item| item.label.as_str())
        .collect();
    assert!(static_labels.contains(&"newStatic"));
    assert!(!static_labels.contains(&"oldStatic"));
}
