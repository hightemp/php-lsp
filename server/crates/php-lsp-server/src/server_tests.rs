use super::lsp::diagnostics::{
    current_class_fqn_at_range, parse_phpstan_json_diagnostics, parse_psalm_json_diagnostics,
    run_diagnostics_blocking, type_info_accepts_inferred_type, InferredExprType,
};
use super::lsp::document_symbols::{workspace_symbol_candidates, workspace_symbol_lsp_range};
use super::*;
use php_lsp_types::*;
use std::cell::Cell;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn make_symbol(
    name: &str,
    fqn: &str,
    kind: PhpSymbolKind,
    range: (u32, u32, u32, u32),
    parent_fqn: Option<&str>,
) -> SymbolInfo {
    SymbolInfo {
        name: name.to_string(),
        fqn: fqn.to_string(),
        kind,
        uri: "file:///test.php".to_string(),
        range,
        selection_range: range,
        visibility: Visibility::Public,
        modifiers: SymbolModifiers::default(),
        doc_comment: None,
        signature: None,
        parent_fqn: parent_fqn.map(|s| s.to_string()),
        extends: vec![],
        implements: vec![],
        traits: vec![],
        templates: vec![],
        template_bindings: vec![],
    }
}

fn make_symbol_for_uri(
    uri: &str,
    name: &str,
    fqn: &str,
    kind: PhpSymbolKind,
    range: (u32, u32, u32, u32),
    parent_fqn: Option<&str>,
) -> SymbolInfo {
    let mut symbol = make_symbol(name, fqn, kind, range, parent_fqn);
    symbol.uri = uri.to_string();
    symbol
}

fn offset_at(source: &str, line: u32, col: u32) -> usize {
    let mut current_line = 0u32;
    let mut line_start = 0usize;
    for (idx, ch) in source.char_indices() {
        if current_line == line {
            return line_start + col as usize;
        }
        if ch == '\n' {
            current_line += 1;
            line_start = idx + 1;
        }
    }
    line_start + col as usize
}

fn parse_and_index_php_file(index: &WorkspaceIndex, uri: &str, code: &str) -> FileParser {
    let mut parser = FileParser::new();
    parser.parse_full(code);
    let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
    index.update_file(uri, symbols);
    parser
}

fn unique_server_temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "php-lsp-server-test-{}-{}-{}",
        name,
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn test_current_class_fqn_at_range_uses_innermost_class_like() {
    let file_symbols = FileSymbols {
        namespace: Some("App".to_string()),
        use_statements: vec![],
        symbols: vec![
            make_symbol(
                "Outer",
                "App\\Outer",
                PhpSymbolKind::Class,
                (0, 0, 24, 1),
                None,
            ),
            make_symbol(
                "anonymous",
                "App\\Outer@anonymous:8",
                PhpSymbolKind::Class,
                (8, 8, 16, 9),
                None,
            ),
        ],
        ..Default::default()
    };

    assert_eq!(
        current_class_fqn_at_range(&file_symbols, (12, 16, 12, 16)).as_deref(),
        Some("App\\Outer@anonymous:8")
    );
    assert_eq!(
        current_class_fqn_at_range(&file_symbols, (20, 8, 20, 8)).as_deref(),
        Some("App\\Outer")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_diagnostics_blocking_compute_yields_current_thread_runtime() {
    let started = Instant::now();
    let handle = tokio::spawn(run_diagnostics_blocking(
        "file:///test/SlowDiagnostics.php".to_string(),
        Some(1),
        || {
            std::thread::sleep(Duration::from_millis(400));
            Vec::new()
        },
    ));

    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    assert!(
        started.elapsed() < Duration::from_millis(300),
        "diagnostics compute should not block the async runtime"
    );
    assert!(handle.await.unwrap().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn test_file_io_blocking_yields_current_thread_runtime() {
    let started = Instant::now();
    let handle = tokio::spawn(run_file_io_blocking(
        "synthetic file IO",
        "/tmp/php-lsp-slow-io.php".to_string(),
        || {
            std::thread::sleep(Duration::from_millis(400));
            7usize
        },
    ));

    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    assert!(
        started.elapsed() < Duration::from_millis(300),
        "file IO helper should not block the async runtime"
    );
    assert_eq!(handle.await.unwrap().unwrap(), 7);
}

fn diagnostic_messages(diagnostics: &[Diagnostic]) -> Vec<String> {
    diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.clone())
        .collect()
}

fn assert_no_diagnostic_containing(messages: &[String], unexpected: &str) {
    assert!(
        !messages.iter().any(|message| message.contains(unexpected)),
        "Did not expect `{}` in diagnostics, got: {:?}",
        unexpected,
        messages
    );
}

fn inferred_expr(display: &str, comparable: &str) -> InferredExprType {
    InferredExprType {
        display: display.to_string(),
        comparable: comparable.to_string(),
        range: (0, 0, 0, 1),
    }
}

#[test]
fn test_request_type_cache_reuses_same_expression_context() {
    let cache = RequestTypeCache::new("file:///test.php", Some(7));
    let calls = Cell::new(0usize);

    let first = cache.cached_type_info((3, 4, 3, 10), "completion-type-info", "$user", || {
        calls.set(calls.get() + 1);
        Some(TypeInfo::Simple("App\\User".to_string()))
    });
    let second = cache.cached_type_info((3, 4, 3, 10), "completion-type-info", "$user", || {
        calls.set(calls.get() + 1);
        Some(TypeInfo::Simple("App\\Other".to_string()))
    });

    assert_eq!(calls.get(), 1);
    assert_eq!(first, Some(TypeInfo::Simple("App\\User".to_string())));
    assert_eq!(second, first);
}

#[test]
fn test_request_type_cache_stores_negative_results() {
    let cache = RequestTypeCache::new("file:///test.php", Some(7));
    let calls = Cell::new(0usize);

    let first = cache.cached_string((0, 0, 0, 0), "member-type", "App\\User::missing", || {
        calls.set(calls.get() + 1);
        None
    });
    let second = cache.cached_string((0, 0, 0, 0), "member-type", "App\\User::missing", || {
        calls.set(calls.get() + 1);
        Some("App\\Never".to_string())
    });

    assert_eq!(calls.get(), 1);
    assert_eq!(first, None);
    assert_eq!(second, None);
}

#[test]
fn test_request_type_cache_separates_context_and_document_version() {
    let first_cache = RequestTypeCache::new("file:///test.php", Some(7));
    let second_cache = RequestTypeCache::new("file:///test.php", Some(8));
    let calls = Cell::new(0usize);

    let first =
        first_cache.cached_type_info((3, 4, 3, 10), "completion-type-info", "$user", || {
            calls.set(calls.get() + 1);
            Some(TypeInfo::Simple("App\\User".to_string()))
        });
    let different_context =
        first_cache.cached_type_info((3, 4, 3, 10), "call-site-argument-type", "$user", || {
            calls.set(calls.get() + 1);
            Some(TypeInfo::Simple("App\\Request".to_string()))
        });
    let different_version =
        second_cache.cached_type_info((3, 4, 3, 10), "completion-type-info", "$user", || {
            calls.set(calls.get() + 1);
            Some(TypeInfo::Simple("App\\UserV2".to_string()))
        });

    assert_eq!(calls.get(), 3);
    assert_ne!(first, different_context);
    assert_ne!(first, different_version);
}

#[test]
fn test_infer_new_expression_type_from_parenthesized_expression() {
    let file_symbols = FileSymbols {
        namespace: Some("App".to_string()),
        use_statements: vec![UseStatement {
            fqn: "Symfony\\Component\\Form\\Guess\\TypeGuess".to_string(),
            alias: None,
            kind: UseKind::Class,
            range: (0, 0, 0, 0),
        }],
        symbols: vec![],
        ..Default::default()
    };

    assert_eq!(
        infer_new_expression_type("(new \\ReflectionClass($v))", &file_symbols).as_deref(),
        Some("ReflectionClass")
    );
    assert_eq!(
        infer_new_expression_type("((new TypeGuess(Foo::class)))", &file_symbols).as_deref(),
        Some("Symfony\\Component\\Form\\Guess\\TypeGuess")
    );
}

#[test]
fn test_infer_static_call_expression_type_with_resolver() {
    let file_symbols = FileSymbols {
        namespace: Some("App\\Models".to_string()),
        use_statements: vec![UseStatement {
            fqn: "App\\Database\\UserBuilder".to_string(),
            alias: None,
            kind: UseKind::Class,
            range: (0, 0, 0, 0),
        }],
        symbols: vec![],
        ..Default::default()
    };
    let source = "<?php\nUser::query();\n";
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let tree = parser.tree().unwrap();
    let context_node = tree.root_node();

    let inferred = infer_static_call_expression_type(
        "User::query()",
        &file_symbols,
        source,
        context_node,
        |class_fqn, method_name| {
            assert_eq!(class_fqn, "App\\Models\\User");
            assert_eq!(method_name, "query");
            Some("App\\Database\\UserBuilder".to_string())
        },
    );

    assert_eq!(inferred.as_deref(), Some("App\\Database\\UserBuilder"));
    assert!(infer_static_call_expression_type(
        "User::class",
        &file_symbols,
        source,
        context_node,
        |_, _| { Some("never".to_string()) }
    )
    .is_none());
}

#[test]
fn test_infer_static_call_expression_type_resolves_scope_names() {
    fn infer_marker_expression(code_with_marker: &str, expr: &str) -> Option<String> {
        let marker = "/*caret*/";
        let marker_offset = code_with_marker
            .find(marker)
            .expect("test code should contain marker");
        let code = code_with_marker.replace(marker, "");
        let prefix = &code[..marker_offset];
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
        let line_start = prefix.rfind('\n').map(|idx| idx + 1).unwrap_or(0);
        let byte_col = (prefix.len() - line_start) as u32;

        let mut parser = FileParser::new();
        parser.parse_full(&code);
        let tree = parser.tree().unwrap();
        let file_symbols = extract_file_symbols(tree, &code, "file:///scope-chain.php");
        let point = tree_sitter::Point::new(line as usize, byte_col as usize);
        let mut context_node = tree
            .root_node()
            .descendant_for_point_range(point, point)
            .expect("context node");
        while !context_node.is_named() {
            context_node = context_node.parent().expect("named context node");
        }

        infer_static_call_expression_type(
            expr,
            &file_symbols,
            &code,
            context_node,
            |class_fqn, method_name| Some(format!("{class_fqn}::{method_name}")),
        )
    }

    let code = r#"<?php
namespace App;
class BaseFactory {
    public static function makeParent(): void {}
}
class ChildFactory extends BaseFactory {
    public static function makeSelf(): void {}
    public static function makeStatic(): void {}
    public function run(): void {
        self::makeSelf()/*caret*/;
        static::makeStatic();
        parent::makeParent();
    }
}
"#;
    assert_eq!(
        infer_marker_expression(code, "self::makeSelf()").as_deref(),
        Some("App\\ChildFactory::makeSelf")
    );

    let code = code.replacen("self::makeSelf()/*caret*/", "self::makeSelf()", 1);
    let code = code.replacen("static::makeStatic()", "static::makeStatic()/*caret*/", 1);
    assert_eq!(
        infer_marker_expression(&code, "static::makeStatic()").as_deref(),
        Some("App\\ChildFactory::makeStatic")
    );

    let code = code.replacen("static::makeStatic()/*caret*/", "static::makeStatic()", 1);
    let code = code.replacen("parent::makeParent()", "parent::makeParent()/*caret*/", 1);
    assert_eq!(
        infer_marker_expression(&code, "parent::makeParent()").as_deref(),
        Some("App\\BaseFactory::makeParent")
    );
}

#[test]
fn test_framework_string_key_context_detection() {
    let source = "<?php\nconfig('app.na');\nroute('dashboard.home');\n__('messages.welcome');\nview('users.show');\nRoute::get('/')->name('admin.index');\n";

    let config =
        framework_string_key_context_at_position(source, 1, 14).expect("config string key context");
    assert_eq!(config.domain, "config");
    assert_eq!(config.prefix, "app.na");
    assert_eq!(config.key, "app.na");

    let route =
        framework_string_key_context_at_position(source, 2, 11).expect("route string key context");
    assert_eq!(route.domain, "route");
    assert_eq!(route.prefix, "dash");
    assert_eq!(route.key, "dashboard.home");

    let translation = framework_string_key_context_at_position(source, 3, 13)
        .expect("translation string key context");
    assert_eq!(translation.domain, "translation");
    assert_eq!(translation.prefix, "messages.");

    let view =
        framework_string_key_context_at_position(source, 4, 12).expect("view string key context");
    assert_eq!(view.domain, "view");
    assert_eq!(view.key, "users.show");

    let route_name = framework_string_key_context_at_position(source, 5, 29)
        .expect("route declaration name context");
    assert_eq!(route_name.domain, "route");
    assert_eq!(route_name.key, "admin.index");
}

#[test]
fn test_twig_direct_string_key_context_detection() {
    let source = "<input value=\"email/timer_expired.html.twig\">\n{{ path('app_debug_email') }}\n{{ url('app_debug_logs', {level: 'error'}) }}\n";
    let position = |needle: &str| -> (u32, u32) {
        let offset = source.find(needle).expect("needle should exist");
        let line = source[..offset]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count() as u32;
        let line_start = source[..offset].rfind('\n').map(|idx| idx + 1).unwrap_or(0);
        (line, (offset - line_start) as u32)
    };

    let (line, byte_col) = position("email/timer_expired");
    let template_context = twig_static_template_path_context_at_position(source, line, byte_col)
        .expect("static HTML template string should be detected");
    assert_eq!(template_context.domain, "twig");
    assert_eq!(template_context.key, "email/timer_expired.html.twig");
    assert!(
        twig_route_key_context_at_position(source, line, byte_col).is_none(),
        "HTML string values should not be treated as route keys"
    );

    let (line, byte_col) = position("app_debug_email");
    let route_context = twig_route_key_context_at_position(source, line, byte_col)
        .expect("Twig path() route key should be detected");
    assert_eq!(route_context.domain, "route");
    assert_eq!(route_context.key, "app_debug_email");

    let (line, byte_col) = position("app_debug_logs");
    let url_context = twig_route_key_context_at_position(source, line, byte_col)
        .expect("Twig url() route key should be detected");
    assert_eq!(url_context.domain, "route");
    assert_eq!(url_context.key, "app_debug_logs");
}

#[test]
fn test_phpdoc_extra_markdown_sections_include_virtual_members() {
    let phpdoc = parse_phpdoc(
            "/**\n * @property-read string $slug Service slug\n * @method User owner()\n * @throws \\RuntimeException\n */",
        );
    let sections = phpdoc_extra_markdown_sections(&phpdoc).join("\n");

    assert!(sections.contains("**Throws:**"));
    assert!(sections.contains("`\\RuntimeException`"));
    assert!(sections.contains("`@property-read string $slug` - Service slug"));
    assert!(sections.contains("`@method User owner()`"));
}

#[test]
fn test_phpdoc_virtual_member_range_points_to_tag_name() {
    let source = "<?php\n/**\n * @property-read string $slug Service slug\n */\nclass Service {}\n";
    let doc_start = source.find("/**").expect("doc comment start");
    let doc_end = source.find("*/").expect("doc comment end") + 2;
    let doc_comment = &source[doc_start..doc_end];
    let mut owner = make_symbol(
        "Service",
        "App\\Service",
        PhpSymbolKind::Class,
        (4, 6, 4, 13),
        None,
    );
    owner.doc_comment = Some(doc_comment.to_string());
    let member = PhpDocVirtualMember {
        owner: Arc::new(owner),
        name: "slug".to_string(),
        kind: PhpDocVirtualMemberKind::Property,
        type_info: Some(TypeInfo::Simple("string".to_string())),
        access: Some(PhpDocPropertyAccess::ReadOnly),
        return_type: None,
        params: Vec::new(),
        description: Some("Service slug".to_string()),
        is_static: false,
    };

    let range = phpdoc_virtual_member_range(source, doc_comment, doc_start, &member)
        .expect("virtual member range");
    let start = offset_at(source, range.0, range.1);
    let end = offset_at(source, range.2, range.3);

    assert_eq!(&source[start..end], "slug");
}

#[test]
fn test_php_kind_to_lsp() {
    assert_eq!(php_kind_to_lsp(PhpSymbolKind::Class), SymbolKind::CLASS);
    assert_eq!(
        php_kind_to_lsp(PhpSymbolKind::Function),
        SymbolKind::FUNCTION
    );
    assert_eq!(php_kind_to_lsp(PhpSymbolKind::Method), SymbolKind::METHOD);
    assert_eq!(
        php_kind_to_lsp(PhpSymbolKind::Property),
        SymbolKind::PROPERTY
    );
    assert_eq!(
        php_kind_to_lsp(PhpSymbolKind::EnumCase),
        SymbolKind::ENUM_MEMBER
    );
    assert_eq!(
        php_kind_to_lsp(PhpSymbolKind::Namespace),
        SymbolKind::NAMESPACE
    );
}

#[test]
fn test_document_symbol_hierarchy() {
    // Simulate file with namespace → class → methods
    let file_symbols = FileSymbols {
        namespace: Some("App\\Service".to_string()),
        use_statements: vec![],
        symbols: vec![
            make_symbol(
                "App\\Service",
                "App\\Service",
                PhpSymbolKind::Namespace,
                (0, 0, 20, 0),
                None,
            ),
            make_symbol(
                "UserService",
                "App\\Service\\UserService",
                PhpSymbolKind::Class,
                (2, 0, 18, 1),
                None,
            ),
            make_symbol(
                "getUser",
                "App\\Service\\UserService::getUser",
                PhpSymbolKind::Method,
                (4, 4, 8, 5),
                Some("App\\Service\\UserService"),
            ),
            make_symbol(
                "$name",
                "App\\Service\\UserService::$name",
                PhpSymbolKind::Property,
                (3, 4, 3, 30),
                Some("App\\Service\\UserService"),
            ),
        ],
        ..Default::default()
    };

    // Index file
    let index = WorkspaceIndex::new();
    index.update_file("file:///test.php", file_symbols);

    // Retrieve and verify structure
    let fs = index.file_symbols.get("file:///test.php").unwrap();
    let symbols = &fs.symbols;

    // Should have 4 symbols total
    assert_eq!(symbols.len(), 4);

    // Verify the class has proper kind
    let class = symbols
        .iter()
        .find(|s| s.kind == PhpSymbolKind::Class)
        .unwrap();
    assert_eq!(class.name, "UserService");

    // Verify members belong to the class
    let members: Vec<_> = symbols
        .iter()
        .filter(|s| s.parent_fqn.as_deref() == Some("App\\Service\\UserService"))
        .collect();
    assert_eq!(members.len(), 2); // getUser + $name
}

#[test]
fn test_workspace_symbol_search() {
    let index = WorkspaceIndex::new();
    let file_symbols = FileSymbols {
        namespace: Some("App".to_string()),
        use_statements: vec![],
        symbols: vec![
            make_symbol(
                "FooController",
                "App\\FooController",
                PhpSymbolKind::Class,
                (0, 0, 10, 0),
                None,
            ),
            make_symbol(
                "BarService",
                "App\\BarService",
                PhpSymbolKind::Class,
                (12, 0, 20, 0),
                None,
            ),
            make_symbol(
                "helper_foo",
                "App\\helper_foo",
                PhpSymbolKind::Function,
                (22, 0, 25, 0),
                None,
            ),
        ],
        ..Default::default()
    };
    index.update_file("file:///app.php", file_symbols);

    // Search for "foo" should find FooController + helper_foo
    let results = index.search("foo");
    assert_eq!(results.len(), 2);

    // Search for "Service" should find BarService
    let results = index.search("Service");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "BarService");

    // Search for "xyz" should find nothing
    let results = index.search("xyz");
    assert!(results.is_empty());
}

#[test]
fn test_workspace_symbol_candidates_rank_filters_and_members() {
    let index = WorkspaceIndex::new();
    let file_symbols = FileSymbols {
        namespace: Some("App\\Service".to_string()),
        use_statements: vec![],
        symbols: vec![
            make_symbol(
                "UserService",
                "App\\Service\\UserService",
                PhpSymbolKind::Class,
                (0, 0, 10, 0),
                None,
            ),
            make_symbol(
                "buildUser",
                "App\\Service\\UserService::buildUser",
                PhpSymbolKind::Method,
                (2, 4, 4, 5),
                Some("App\\Service\\UserService"),
            ),
            make_symbol(
                "UserServiceFactory",
                "App\\Factory\\UserServiceFactory",
                PhpSymbolKind::Class,
                (20, 0, 25, 0),
                None,
            ),
        ],
        ..Default::default()
    };
    index.update_file("file:///app.php", file_symbols);

    let candidates = workspace_symbol_candidates(&index, "usrsvc");
    let names: Vec<_> = candidates
        .iter()
        .map(|candidate| candidate.symbol.name.as_str())
        .collect();
    assert!(
        names.starts_with(&["UserService"]),
        "fuzzy query should rank the closest type first, got: {:?}",
        names
    );

    let method_candidates = workspace_symbol_candidates(&index, "method:build");
    assert_eq!(method_candidates.len(), 1);
    assert_eq!(method_candidates[0].symbol.name, "buildUser");
    assert_eq!(method_candidates[0].symbol.kind, PhpSymbolKind::Method);

    let class_candidates = workspace_symbol_candidates(&index, "class:build");
    assert!(
        class_candidates.is_empty(),
        "kind filter should exclude method-only matches"
    );
}

#[test]
fn test_workspace_symbol_lsp_range_converts_byte_columns_to_utf16() {
    let source = "<?php\n$привет = 1; class Demo {}\n";
    let range = workspace_symbol_lsp_range(source, (1, 19, 1, 24));

    assert_eq!(range.start, Position::new(1, 13));
    assert_eq!(range.end, Position::new(1, 18));
}

#[test]
fn test_workspace_reindex_keeps_vendor_and_stub_symbols() {
    let index = WorkspaceIndex::new();
    let workspace_uri = "file:///tmp/project/src/Foo.php";
    index.update_file(
        workspace_uri,
        FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![make_symbol_for_uri(
                workspace_uri,
                "Foo",
                "App\\Foo",
                PhpSymbolKind::Class,
                (0, 0, 1, 0),
                None,
            )],
            ..Default::default()
        },
    );
    let vendor_uri = "file:///tmp/project/vendor/acme/pkg/Bar.php";
    index.update_file(
        vendor_uri,
        FileSymbols {
            namespace: Some("Vendor\\Pkg".to_string()),
            use_statements: vec![],
            symbols: vec![make_symbol_for_uri(
                vendor_uri,
                "Bar",
                "Vendor\\Pkg\\Bar",
                PhpSymbolKind::Class,
                (0, 0, 1, 0),
                None,
            )],
            ..Default::default()
        },
    );
    let stub_uri = "phpstub://Core/Core.php";
    index.update_file(
        stub_uri,
        FileSymbols {
            namespace: None,
            use_statements: vec![],
            symbols: vec![make_symbol_for_uri(
                stub_uri,
                "stdClass",
                "stdClass",
                PhpSymbolKind::Class,
                (0, 0, 1, 0),
                None,
            )],
            ..Default::default()
        },
    );

    let removed = remove_indexed_file_symbols(&index, &[PathBuf::from("/tmp/project")]);

    assert_eq!(removed, 1);
    assert!(index.resolve_fqn("App\\Foo").is_none());
    assert!(index.resolve_fqn("Vendor\\Pkg\\Bar").is_some());
    assert!(index.resolve_fqn("stdClass").is_some());
}

#[test]
fn test_workspace_index_reads_non_utf8_php_lossily() {
    let tmp = std::env::temp_dir().join(format!("php-lsp-non-utf8-index-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let file = tmp.join("Legacy.php");
    std::fs::write(
        &file,
        b"<?php\nclass Legacy {\n    public const VALUE = \"\xff\";\n}\n",
    )
    .unwrap();

    let parsed = parse_workspace_file_for_index(file);

    assert!(
        parsed.error.is_none(),
        "got parse error: {:?}",
        parsed.error
    );
    assert!(parsed
        .file_symbols
        .as_ref()
        .is_some_and(|symbols| symbols.symbols.iter().any(|sym| sym.fqn == "Legacy")));

    std::fs::remove_dir_all(tmp).unwrap();
}

#[test]
fn test_workspace_index_parallel_updates_are_safe() {
    let index = Arc::new(WorkspaceIndex::new());
    let mut handles = Vec::new();

    for i in 0..32 {
        let index = index.clone();
        handles.push(std::thread::spawn(move || {
            let uri = format!("file:///tmp/project/src/Foo{}.php", i);
            let fqn = format!("App\\Foo{}", i);
            index.update_file(
                &uri,
                FileSymbols {
                    namespace: Some("App".to_string()),
                    use_statements: vec![],
                    symbols: vec![make_symbol(
                        &format!("Foo{}", i),
                        &fqn,
                        PhpSymbolKind::Class,
                        (0, 0, 1, 0),
                        None,
                    )],
                    ..Default::default()
                },
            );
        }));
    }

    for handle in handles {
        handle.join().unwrap();
    }

    for i in 0..32 {
        assert!(index.resolve_fqn(&format!("App\\Foo{}", i)).is_some());
    }
}

#[test]
fn test_document_version_ordering_accepts_only_newer_versions() {
    assert!(document_version_is_newer(None, 1));
    assert!(document_version_is_newer(Some(1), 2));
    assert!(!document_version_is_newer(Some(2), 2));
    assert!(!document_version_is_newer(Some(3), 2));
}

#[test]
fn test_cache_configs_use_separate_namespaces() {
    let root = Path::new("/tmp/project");
    let workspace_config =
        workspace_index_cache_config(Some(root), PhpVersion::DEFAULT, &[], &[], None, None);
    let stub_extensions = ["Core".to_string()];
    let stubs_config = stubs_index_cache_config(
        Path::new("/tmp/project/stubs"),
        PhpVersion::DEFAULT,
        Some(&stub_extensions),
    );
    let vendor_config = vendor_index_cache_config(root, PhpVersion::DEFAULT, &[]);

    assert_eq!(workspace_config.namespace, CacheNamespace::Workspace);
    assert_eq!(stubs_config.namespace, CacheNamespace::Stubs);
    assert_eq!(vendor_config.namespace, CacheNamespace::Vendor);
    assert_ne!(workspace_config.config_hash(), stubs_config.config_hash());
    assert_ne!(workspace_config.config_hash(), vendor_config.config_hash());
}

#[test]
fn test_effective_stub_extensions_distinguishes_defaults_from_explicit_empty() {
    assert!(effective_stub_extensions(None).contains(&"Core".to_string()));

    let disabled: Vec<String> = Vec::new();
    assert!(effective_stub_extensions(Some(&disabled)).is_empty());

    let custom = ["Core".to_string()];
    assert_eq!(
        effective_stub_extensions(Some(&custom)),
        vec!["Core".to_string()]
    );
}

#[test]
fn test_vendor_file_lru_evicts_old_index_entries() {
    let index = WorkspaceIndex::new();
    let uri1 = "file:///tmp/project/vendor/acme/pkg/One.php";
    let uri2 = "file:///tmp/project/vendor/acme/pkg/Two.php";
    index.update_file(
        uri1,
        FileSymbols {
            namespace: Some("Vendor\\Pkg".to_string()),
            use_statements: vec![],
            symbols: vec![make_symbol_for_uri(
                uri1,
                "One",
                "Vendor\\Pkg\\One",
                PhpSymbolKind::Class,
                (0, 0, 1, 0),
                None,
            )],
            ..Default::default()
        },
    );
    index.update_file(
        uri2,
        FileSymbols {
            namespace: Some("Vendor\\Pkg".to_string()),
            use_statements: vec![],
            symbols: vec![make_symbol_for_uri(
                uri2,
                "Two",
                "Vendor\\Pkg\\Two",
                PhpSymbolKind::Class,
                (0, 0, 1, 0),
                None,
            )],
            ..Default::default()
        },
    );

    let mut lru = VendorFileLru::with_capacity(1);
    assert!(lru.touch(uri1.to_string()).is_empty());
    let evicted = lru.touch(uri2.to_string());
    for uri in evicted {
        index.remove_file(&uri);
    }

    assert!(index.resolve_fqn("Vendor\\Pkg\\One").is_none());
    assert!(index.resolve_fqn("Vendor\\Pkg\\Two").is_some());
}

#[test]
fn test_vendor_autoload_map_parses_psr4_and_files() {
    let tmp = std::env::temp_dir().join(format!(
        "php-lsp-vendor-autoload-map-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let vendor_dir = tmp.join("vendor");
    let composer_dir = vendor_dir.join("composer");
    std::fs::create_dir_all(&composer_dir).unwrap();

    let installed_json = serde_json::json!({
        "packages": [
            {
                "name": "acme/library",
                "install-path": "../acme/library",
                "autoload": {
                    "psr-4": {
                        "Acme\\Library\\": ["src/", "generated/"]
                    },
                    "files": ["bootstrap.php"]
                }
            }
        ]
    });
    std::fs::write(
        composer_dir.join("installed.json"),
        serde_json::to_string(&installed_json).unwrap(),
    )
    .unwrap();

    let map = parse_vendor_autoload_map(&vendor_dir).unwrap();
    let paths = resolve_vendor_paths_from_map("Acme\\Library\\Http\\Client", &map).unwrap();

    assert_eq!(paths.len(), 2);
    assert!(
        paths
            .iter()
            .any(|path| path.to_string_lossy().ends_with("src/Http/Client.php")),
        "Expected src PSR-4 path, got: {:?}",
        paths
    );
    assert!(
        map.files
            .iter()
            .any(|path| path.to_string_lossy().ends_with("bootstrap.php")),
        "Expected autoload file path, got: {:?}",
        map.files
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test(flavor = "current_thread")]
async fn test_lazy_index_class_returns_false_when_psr4_file_contains_different_class() {
    let root = unique_server_temp_dir("lazy-wrong-class");
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("Foo.php"),
        "<?php\nnamespace App;\nclass Different {}\n",
    )
    .unwrap();

    let (service, _socket) = tower_lsp::LspService::new(PhpLspBackend::new);
    let backend = service.inner();
    *backend.workspace_configs.lock().await = vec![WorkspaceRootConfig {
        root: root.clone(),
        namespace_map: Some(NamespaceMap {
            psr4: vec![("App\\".to_string(), vec![src])],
            ..Default::default()
        }),
    }];

    assert!(!backend.lazy_index_class("App\\Foo").await);
    assert!(backend.index.resolve_fqn("App\\Foo").is_none());
    assert!(backend.index.resolve_fqn("App\\Different").is_some());

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_lazy_indexed_vendor_symbol_survives_restart_cache_load() {
    let root = unique_server_temp_dir("lazy-vendor-cache");
    let vendor_src = root.join("vendor/acme/package/src");
    std::fs::create_dir_all(&vendor_src).unwrap();
    let vendor_file = vendor_src.join("Foo.php");
    std::fs::write(
        &vendor_file,
        "<?php\nnamespace Vendor\\Package;\nclass Foo {}\n",
    )
    .unwrap();

    let cache_path = cache::cache_file_path_for_namespace(&root, CacheNamespace::Vendor);
    let _ = std::fs::remove_file(&cache_path);

    let (service, _socket) = tower_lsp::LspService::new(PhpLspBackend::new);
    let backend = service.inner();
    *backend.workspace_configs.lock().await = vec![WorkspaceRootConfig {
        root: root.clone(),
        namespace_map: Some(NamespaceMap {
            psr4: vec![("Vendor\\Package\\".to_string(), vec![vendor_src])],
            ..Default::default()
        }),
    }];

    assert!(backend.lazy_index_class("Vendor\\Package\\Foo").await);
    assert!(
        cache_path.is_file(),
        "expected vendor cache at {:?}",
        cache_path
    );

    let restarted_index = WorkspaceIndex::new();
    let cache_config = vendor_index_cache_config(&root, PhpVersion::DEFAULT, &[]);
    assert!(load_cached_vendor_file(
        &restarted_index,
        &root,
        &vendor_file,
        &cache_config
    ));
    assert!(restarted_index
        .resolve_fqn("Vendor\\Package\\Foo")
        .is_some());

    if let Some(cache_dir) = cache_path.parent() {
        let _ = std::fs::remove_dir_all(cache_dir);
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_watched_composer_metadata_change_ignores_vendor_package_composer_files() {
    let roots = vec![PathBuf::from("/workspace")];
    let package_composer = PathBuf::from("/workspace/vendor/acme/package/composer.json");
    let temp_package_composer =
        PathBuf::from("/workspace/vendor/composer/75f4db74/package/composer.json");
    let package_lock = PathBuf::from("/workspace/vendor/acme/package/composer.lock");
    let installed_json = PathBuf::from("/workspace/vendor/composer/installed.json");
    let autoload_static = PathBuf::from("/workspace/vendor/composer/autoload_static.php");

    assert_eq!(
        composer_metadata_change_for_path(&PathBuf::from("/workspace/composer.json")),
        Some(ComposerMetadataChange::ProjectAutoload)
    );
    assert_eq!(
        composer_metadata_change_for_path(&PathBuf::from("/workspace/composer.lock")),
        Some(ComposerMetadataChange::VendorAutoload)
    );
    assert_eq!(
        composer_metadata_change_for_path(&package_composer),
        Some(ComposerMetadataChange::ProjectAutoload)
    );
    assert_eq!(
        composer_metadata_change_for_path(&temp_package_composer),
        Some(ComposerMetadataChange::ProjectAutoload)
    );
    assert_eq!(
        composer_metadata_change_for_path(&package_lock),
        Some(ComposerMetadataChange::VendorAutoload)
    );
    assert_eq!(
        composer_metadata_change_for_path(&installed_json),
        Some(ComposerMetadataChange::VendorAutoload)
    );
    assert_eq!(
        composer_metadata_change_for_path(&autoload_static),
        Some(ComposerMetadataChange::VendorAutoload)
    );
    assert!(should_ignore_vendor_package_composer_metadata_change(
        &package_composer,
        &roots
    ));
    assert!(should_ignore_vendor_package_composer_metadata_change(
        &temp_package_composer,
        &roots
    ));
    assert!(should_ignore_vendor_package_composer_metadata_change(
        &package_lock,
        &roots
    ));
    assert!(!should_ignore_vendor_package_composer_metadata_change(
        &installed_json,
        &roots
    ));
    assert!(!should_ignore_vendor_package_composer_metadata_change(
        &autoload_static,
        &roots
    ));
    assert!(!should_ignore_vendor_package_composer_metadata_change(
        &PathBuf::from("/vendor/workspace/composer.json"),
        &[PathBuf::from("/vendor/workspace")]
    ));
}

#[test]
fn test_compute_diagnostics_reports_duplicate_workspace_symbols() {
    let uri1 = "file:///one.php";
    let uri2 = "file:///two.php";
    let code1 = "<?php\nnamespace App;\nclass Duplicate {}\n";
    let code2 = "<?php\nnamespace App;\nclass Duplicate {}\n";

    let mut parser1 = FileParser::new();
    parser1.parse_full(code1);
    let mut parser2 = FileParser::new();
    parser2.parse_full(code2);

    let index = WorkspaceIndex::new();
    let symbols1 = extract_file_symbols(parser1.tree().unwrap(), code1, uri1);
    let symbols2 = extract_file_symbols(parser2.tree().unwrap(), code2, uri2);
    index.update_file(uri1, symbols1);
    index.update_file(uri2, symbols2);

    let diagnostics = compute_diagnostics(
        uri1,
        &parser1,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );

    assert!(
        diagnostics
            .iter()
            .any(|diag| diag.message == "Duplicate symbol: App\\Duplicate"),
        "Expected duplicate workspace symbol diagnostic, got: {:?}",
        diagnostics
    );
}

#[test]
fn test_compute_diagnostics_deduplicates_same_file_duplicate_symbols() {
    let uri = "file:///same-file-duplicates.php";
    let code = "<?php\nnamespace App;\nclass Duplicate {}\nclass Duplicate {}\n";

    let index = WorkspaceIndex::new();
    let parser = parse_and_index_php_file(&index, uri, code);

    let diagnostics = compute_diagnostics(
        uri,
        &parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );
    let duplicates: Vec<_> = diagnostics
        .iter()
        .filter(|diag| diag.message == "Duplicate symbol: App\\Duplicate")
        .collect();

    assert_eq!(
        duplicates.len(),
        2,
        "Expected one diagnostic per duplicate declaration, got: {:?}",
        diagnostics
    );
}

#[test]
fn test_type_compatibility_approximation_rules_are_explicit() {
    let file_symbols = FileSymbols::default();
    let index = WorkspaceIndex::new();

    let generic_array = TypeInfo::Generic {
        base: "array".to_string(),
        args: vec![
            TypeInfo::Simple("int".to_string()),
            TypeInfo::Simple("string".to_string()),
        ],
    };
    assert!(type_info_accepts_inferred_type(
        &generic_array,
        &inferred_expr("array", "array"),
        &file_symbols,
        &index
    ));
    assert!(!type_info_accepts_inferred_type(
        &generic_array,
        &inferred_expr("string", "string"),
        &file_symbols,
        &index
    ));

    let shape = TypeInfo::ArrayShape(vec![ArrayShapeItem {
        key: Some("id".to_string()),
        optional: false,
        value: TypeInfo::Simple("int".to_string()),
    }]);
    assert!(type_info_accepts_inferred_type(
        &shape,
        &inferred_expr("array", "array"),
        &file_symbols,
        &index
    ));

    let union = TypeInfo::Union(vec![
        TypeInfo::Simple("string".to_string()),
        TypeInfo::Simple("int".to_string()),
    ]);
    assert!(type_info_accepts_inferred_type(
        &union,
        &inferred_expr("string", "'ok'"),
        &file_symbols,
        &index
    ));
    assert!(!type_info_accepts_inferred_type(
        &union,
        &inferred_expr("bool", "false"),
        &file_symbols,
        &index
    ));

    let intersection = TypeInfo::Intersection(vec![
        TypeInfo::Simple("App\\A".to_string()),
        TypeInfo::Simple("App\\B".to_string()),
    ]);
    assert!(type_info_accepts_inferred_type(
        &intersection,
        &inferred_expr("App\\A", "App\\A"),
        &file_symbols,
        &index
    ));

    for relative_type in [TypeInfo::Self_, TypeInfo::Static_, TypeInfo::Parent_] {
        assert!(type_info_accepts_inferred_type(
            &relative_type,
            &inferred_expr("App\\Child", "App\\Child"),
            &file_symbols,
            &index
        ));
    }

    assert!(type_info_accepts_inferred_type(
        &TypeInfo::LiteralString("'ok'".to_string()),
        &inferred_expr("string", "'ok'"),
        &file_symbols,
        &index
    ));
    assert!(type_info_accepts_inferred_type(
        &TypeInfo::LiteralInt("1".to_string()),
        &inferred_expr("int", "1"),
        &file_symbols,
        &index
    ));
    assert!(type_info_accepts_inferred_type(
        &TypeInfo::LiteralBool(true),
        &inferred_expr("true", "true"),
        &file_symbols,
        &index
    ));
    assert!(type_info_accepts_inferred_type(
        &TypeInfo::LiteralNull,
        &inferred_expr("null", "null"),
        &file_symbols,
        &index
    ));
    assert!(!type_info_accepts_inferred_type(
        &TypeInfo::LiteralInt("2".to_string()),
        &inferred_expr("int", "1"),
        &file_symbols,
        &index
    ));
}

#[test]
fn test_compute_diagnostics_reports_member_access_errors() {
    let uri = "file:///members.php";
    let code = r#"<?php
namespace App;

class Service {
    public string $name;
    public static string $count;
    protected object $request;
    private function hidden(): void {}
    public static function stat(): void {}
    public function inst(): void {}
    public function fluent(): static { return $this; }
    public function request(): void {}
    public const OK = 'ok';
}

class Demo {
    public function run(Service $service): void {
        $service->missing();
        echo $service->missingProp;
        echo Service::MISSING;
        $service->stat();
        Service::inst();
        $service->fluent();
        $service->request();
        echo $service->count;
        echo Service::$name;
        $service->hidden();
    }
}
"#;

    let mut parser = FileParser::new();
    parser.parse_full(code);

    let index = WorkspaceIndex::new();
    let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
    index.update_file(uri, symbols);

    let diagnostics = compute_diagnostics(
        uri,
        &parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );
    let messages: Vec<_> = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();

    for expected in [
        "Unknown method: App\\Service::missing",
        "Unknown property: App\\Service::$missingProp",
        "Unknown class constant: App\\Service::MISSING",
        "Static method called as instance method: App\\Service::stat",
        "Instance method called statically: App\\Service::inst",
        "Static property accessed as instance property: App\\Service::$count",
        "Instance property accessed statically: App\\Service::$name",
        "Private member is not accessible here: App\\Service::hidden",
    ] {
        assert!(
            messages.contains(&expected),
            "Expected `{}` in diagnostics, got: {:?}",
            expected,
            messages
        );
    }

    assert!(
        !messages.contains(&"Static method called as instance method: App\\Service::fluent"),
        "Method returning `static` must not be treated as a static method: {:?}",
        messages
    );
    assert!(
        !messages.contains(&"Protected member is not accessible here: App\\Service::$request"),
        "Method calls must not resolve to same-named properties: {:?}",
        messages
    );
}

#[test]
fn test_compute_diagnostics_reports_unknown_method_on_imported_typed_parameter() {
    let entity_uri = "file:///src/Domain/ImportedEntity.php";
    let handler_uri = "file:///src/Application/ImportedEntityHandler.php";
    let entity_code = r#"<?php
namespace App\Domain;

class ImportedEntity {
    public function existingMethod(): array { return []; }
}
"#;
    let handler_code = r#"<?php
namespace App\Application;

use App\Domain\ImportedEntity;

class ImportedEntityHandler {
    private function handle(ImportedEntity $entity): void
    {
        $result = $entity->existingMethod();
        $entity->missingMethod($result);
    }
}
"#;

    let index = WorkspaceIndex::new();
    parse_and_index_php_file(&index, entity_uri, entity_code);
    let parser = parse_and_index_php_file(&index, handler_uri, handler_code);

    let diagnostics = compute_diagnostics(
        handler_uri,
        &parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );
    let messages: Vec<_> = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();

    assert!(
        messages.contains(&"Unknown method: App\\Domain\\ImportedEntity::missingMethod"),
        "Expected unknown method diagnostic for imported typed parameter, got: {:?}",
        messages
    );
    assert!(
        !messages.contains(&"Unknown method: App\\Domain\\ImportedEntity::existingMethod"),
        "Existing imported method must not be reported as unknown: {:?}",
        messages
    );
}

#[test]
fn test_compute_diagnostics_allows_nullsafe_member_access() {
    let uri = "file:///nullsafe-members.php";
    let code = r#"<?php
namespace App;

class Session {
    public string $id;
    public function get(string $key): string { return ''; }
}

class Demo {
    public function run(?Session $session): void {
        $session?->get('token');
        echo $session?->id;
    }
}
"#;

    let mut parser = FileParser::new();
    parser.parse_full(code);

    let index = WorkspaceIndex::new();
    let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
    index.update_file(uri, symbols);

    let diagnostics = compute_diagnostics(
        uri,
        &parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );
    let messages = diagnostic_messages(&diagnostics);

    assert_no_diagnostic_containing(&messages, "Unknown method: App\\Session::get");
    assert_no_diagnostic_containing(&messages, "Unknown property: App\\Session::$id");
}

#[test]
fn test_compute_diagnostics_skips_members_on_unindexed_imported_types() {
    let uri = "file:///external-client.php";
    let code = r#"<?php
namespace App;

use Vendor\Package\Client;

class Demo {
    public function run(Client $client): void {
        $client->send();
    }
}
"#;

    let mut parser = FileParser::new();
    parser.parse_full(code);

    let index = WorkspaceIndex::new();
    let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
    index.update_file(uri, symbols);

    let diagnostics = compute_diagnostics(
        uri,
        &parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );
    let messages: Vec<_> = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();

    assert!(
        !messages.contains(&"Unknown method: Vendor\\Package\\Client::send"),
        "Unindexed imported types should not get guessed member diagnostics: {:?}",
        messages
    );
}

#[test]
fn test_compute_diagnostics_allows_framework_heavy_dynamic_patterns() {
    let uri = "file:///framework-heavy.php";
    let code = r#"<?php
namespace Symfony\Bundle\FrameworkBundle\Controller;
abstract class AbstractController {}

namespace Illuminate\Database\Eloquent;
class Model {}
class Builder {}

namespace App\Models;
class User extends \Illuminate\Database\Eloquent\Model {}

namespace App\Controller;

use App\Models\User;
use Symfony\Bundle\FrameworkBundle\Controller\AbstractController;

final class DashboardController extends AbstractController
{
    public function index(User $user): void
    {
        $this->render('dashboard.html.twig');
        $this->json(['ok' => true]);
        $this->redirectToRoute('dashboard');

        echo $user->email;
        User::whereEmail('demo@example.com')->firstOrFail();
    }
}
"#;

    let mut parser = FileParser::new();
    parser.parse_full(code);

    let index = WorkspaceIndex::new();
    let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
    index.update_file(uri, symbols);

    let diagnostics = compute_diagnostics(
        uri,
        &parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );
    let messages: Vec<_> = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();

    for unexpected in [
        "Unknown method: App\\Controller\\DashboardController::render",
        "Unknown method: App\\Controller\\DashboardController::json",
        "Unknown method: App\\Controller\\DashboardController::redirectToRoute",
        "Unknown property: App\\Models\\User::$email",
        "Unknown method: App\\Models\\User::whereEmail",
    ] {
        assert!(
            !messages.iter().any(|message| message.contains(unexpected)),
            "Did not expect `{}` in diagnostics, got: {:?}",
            unexpected,
            messages
        );
    }
}

#[test]
fn test_compute_diagnostics_allows_promoted_properties_on_self_typed_parameter() {
    let uri = "file:///promoted-self-defaults.php";
    let code = r#"<?php
namespace App\Diagnostics;

final class PromotedSelfDefaults
{
    public function __construct(
        public ?string $objectManager = null,
        public ?array $mapping = null,
    ) {
    }

    public function withDefaults(self $defaults): static
    {
        $clone = clone $this;
        $clone->objectManager ??= $defaults->objectManager;
        $clone->mapping ??= $defaults->mapping ?? [];

        return $clone;
    }
}
"#;

    let mut parser = FileParser::new();
    parser.parse_full(code);

    let index = WorkspaceIndex::new();
    let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
    index.update_file(uri, symbols);

    let diagnostics = compute_diagnostics(
        uri,
        &parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );
    let messages: Vec<_> = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();

    for unexpected in [
        "Unknown property: App\\Diagnostics\\PromotedSelfDefaults::$objectManager",
        "Unknown property: App\\Diagnostics\\PromotedSelfDefaults::$mapping",
        "Unknown property: self::$objectManager",
        "Unknown property: self::$mapping",
    ] {
        assert!(
            !messages.contains(&unexpected),
            "Did not expect `{}` in diagnostics, got: {:?}",
            unexpected,
            messages
        );
    }
}

#[test]
fn test_compute_diagnostics_applies_category_severity_controls() {
    let uri = "file:///severity-controls.php";
    let code = r#"<?php
namespace App;

class Service {}

function run(Service $service): void
{
    $unused = 1;
    $service->missing();
    new MissingClass();
}
"#;

    let mut parser = FileParser::new();
    parser.parse_full(code);

    let index = WorkspaceIndex::new();
    let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
    index.update_file(uri, symbols);

    let mut severity = DiagnosticSeverityConfig::default();
    severity.set(DiagnosticCategory::Members, DiagnosticLevel(None));
    severity.set(
        DiagnosticCategory::UnknownSymbols,
        DiagnosticLevel(Some(DiagnosticSeverity::INFORMATION)),
    );
    severity.set(
        DiagnosticCategory::Unused,
        DiagnosticLevel(Some(DiagnosticSeverity::HINT)),
    );

    let diagnostics = compute_diagnostics_with_config(
        uri,
        &parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        severity,
        PhpVersion::DEFAULT,
    );

    assert!(
        !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "Unknown method: App\\Service::missing"),
        "Member category is off, got diagnostics: {:?}",
        diagnostics
    );

    let unknown_class = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message == "Unknown class: App\\MissingClass")
        .expect("Expected unknown class diagnostic");
    assert_eq!(
        unknown_class.severity,
        Some(DiagnosticSeverity::INFORMATION)
    );
    assert_eq!(
        unknown_class.code,
        Some(NumberOrString::String("php-lsp.unknownClass".to_string()))
    );

    let unused_variable = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message == "Unused variable: $unused")
        .expect("Expected unused variable diagnostic");
    assert_eq!(unused_variable.severity, Some(DiagnosticSeverity::HINT));
    assert_eq!(
        unused_variable.code,
        Some(NumberOrString::String("php-lsp.unusedVariable".to_string()))
    );
}

#[test]
fn test_compute_diagnostics_allows_magic_class_and_late_bound_self_calls() {
    let uri = "file:///phpunit-patterns.php";
    let code = r#"<?php
namespace App;

class Foo {}

class Base {
    protected function once(): void {}
    protected static function createStub(string $type): object { return new Foo(); }
    public static function callback(callable $callback): bool { return true; }
}

class Demo extends Base {
    public function run(): void {
        echo Foo::class;
        self::once();
        self::callback(static fn (): bool => true);
        $this->createStub(Foo::class);
    }
}
"#;

    let mut parser = FileParser::new();
    parser.parse_full(code);

    let index = WorkspaceIndex::new();
    let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
    index.update_file(uri, symbols);

    let diagnostics = compute_diagnostics(
        uri,
        &parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );
    let messages: Vec<_> = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();

    for unexpected in [
        "Unknown class constant: App\\Foo::class",
        "Instance method called statically: App\\Base::once",
        "Static method called as instance method: App\\Base::createStub",
    ] {
        assert!(
            !messages.contains(&unexpected),
            "Did not expect `{}` in diagnostics, got: {:?}",
            unexpected,
            messages
        );
    }
}

#[test]
fn test_compute_diagnostics_allows_phpunit_stub_api_on_typed_properties() {
    let uri = "file:///phpunit-stub-api.php";
    let code = r#"<?php
namespace PHPUnit\Framework;
class TestCase {}

namespace Symfony\Component\Console\Tester;
class CommandTester {}

namespace App\Tests\Command;

use PHPUnit\Framework\TestCase;
use Symfony\Component\Console\Tester\CommandTester;

class UserRepository {}

final class ChangeUserPasswordCommandTest extends TestCase
{
    private UserRepository $userRepo;
    private CommandTester $commandTester;

    protected function setUp(): void
    {
        $this->userRepo = $this->createStub(UserRepository::class);
        $this->commandTester = new CommandTester();
    }

    public function testUserNotFoundByEmail(): void
    {
        $this->userRepo->method('findOneBy')->willReturn(null);
        self::assertSame(1, 1);
    }
}
"#;

    let mut parser = FileParser::new();
    parser.parse_full(code);

    let index = WorkspaceIndex::new();
    let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
    index.update_file(uri, symbols);

    let diagnostics = compute_diagnostics(
        uri,
        &parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );
    let messages: Vec<_> = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();

    for unexpected in [
            "Unknown method: App\\Tests\\Command\\ChangeUserPasswordCommandTest::createStub",
            "Unknown method: App\\Tests\\Command\\UserRepository::method",
            "Unknown method: App\\Tests\\Command\\ChangeUserPasswordCommandTest::assertSame",
            "Property assignment type mismatch for App\\Tests\\Command\\ChangeUserPasswordCommandTest::$commandTester",
        ] {
            assert!(
                !messages.iter().any(|message| message.contains(unexpected)),
                "Did not expect `{}` in diagnostics, got: {:?}",
                unexpected,
                messages
            );
        }
}

#[test]
fn test_compute_diagnostics_allows_trait_member_visibility_and_stdclass_properties() {
    let uri = "file:///trait-members.php";
    let code = r#"<?php
namespace App\Tests;

enum TimerType: string {
    case Test = 'test';
}

trait HelperTestTrait {
    protected int $count;
    protected function protectedHelper(): void {}
    private function privateHelper(): void {}
}

final class HelperConsumerTest {
    use HelperTestTrait;

    public function run(\stdClass $payload, object $response, TimerType $type): void {
        $this->count = 1;
        $this->protectedHelper();
        $this->privateHelper();
        echo $payload->PortMessages;
        echo $response->getContent();
        echo $response->headers;
        echo $type->name;
        echo $type->value;
    }
}
"#;

    let mut parser = FileParser::new();
    parser.parse_full(code);

    let index = WorkspaceIndex::new();
    let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
    index.update_file(uri, symbols);

    let diagnostics = compute_diagnostics(
        uri,
        &parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );
    let messages: Vec<_> = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();

    for unexpected in [
        "Protected member is not accessible here: App\\Tests\\HelperTestTrait::$count",
        "Protected member is not accessible here: App\\Tests\\HelperTestTrait::protectedHelper",
        "Private member is not accessible here: App\\Tests\\HelperTestTrait::privateHelper",
        "Unknown property: stdClass::$PortMessages",
        "Unknown method: object::getContent",
        "Unknown property: object::$headers",
        "Unknown property: App\\Tests\\TimerType::$name",
        "Unknown property: App\\Tests\\TimerType::$value",
    ] {
        assert!(
            !messages.iter().any(|message| {
                message.contains(unexpected)
                    || (unexpected.contains("object::getContent")
                        && message.ends_with("object::getContent"))
                    || (unexpected.contains("object::$headers")
                        && message.ends_with("object::$headers"))
            }),
            "Did not expect `{}` in diagnostics, got: {:?}",
            unexpected,
            messages
        );
    }
}

#[test]
fn test_compute_diagnostics_skips_anonymous_class_body_member_checks() {
    let uri = "file:///anonymous-class.php";
    let code = r#"<?php
namespace App\Tests;

final class Factory
{
    public function make(): object
    {
        return new class('demo') {
            private string $name;

            public function __construct(string $name)
            {
                $this->name = $name;
            }

            public function getName(): string
            {
                return $this->name;
            }

            public function getDate(): ?\DateTime
            {
                return null;
            }
        };
    }
}
"#;

    let mut parser = FileParser::new();
    parser.parse_full(code);

    let index = WorkspaceIndex::new();
    let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
    index.update_file(uri, symbols);

    let diagnostics = compute_diagnostics(
        uri,
        &parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );
    let messages: Vec<_> = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();

    for unexpected in [
        "Unknown property: App\\Tests\\Factory::$name",
        "Return type mismatch in App\\Tests\\Factory::make: expected object, got null",
    ] {
        assert!(
            !messages.iter().any(|message| message.contains(unexpected)),
            "Did not expect `{}` in diagnostics, got: {:?}",
            unexpected,
            messages
        );
    }
}

fn compute_member_heavy_diagnostics(diagnostic_budget: DiagnosticBudgetConfig) -> Vec<Diagnostic> {
    let uri = "file:///large-member-heavy.php";
    let mut code = String::from(
        r#"<?php
namespace App;

class Service {}

function configure(Service $service): void
{
"#,
    );
    for index in 0..=DEFAULT_MEMBER_TYPE_DIAGNOSTIC_NODE_BUDGET {
        code.push_str(&format!("    $service->missing{}();\n", index));
    }
    code.push_str("}\n");

    let mut parser = FileParser::new();
    parser.parse_full(&code);

    let index = WorkspaceIndex::new();
    let symbols = extract_file_symbols(parser.tree().unwrap(), &code, uri);
    index.update_file(uri, symbols);

    compute_diagnostics_with_runtime_config(
        uri,
        &parser,
        &index,
        DiagnosticsRuntimeConfig {
            mode: DiagnosticsMode::BasicSemantic,
            budget: diagnostic_budget,
            ..DiagnosticsRuntimeConfig::default()
        },
        None,
    )
}

#[test]
fn test_compute_diagnostics_default_budget_covers_moderate_member_heavy_file() {
    let uri = "file:///moderate-members.php";
    let mut code = String::from(
        r#"<?php
namespace App;

class Service {
    public function ping(): void {}
}

function configure(Service $service): void
{
"#,
    );
    for _ in 0..80 {
        code.push_str("    $service->ping();\n");
    }
    code.push_str("    $service->missing();\n}\n");

    let mut parser = FileParser::new();
    parser.parse_full(&code);

    let index = WorkspaceIndex::new();
    let symbols = extract_file_symbols(parser.tree().unwrap(), &code, uri);
    index.update_file(uri, symbols);

    let diagnostics = compute_diagnostics(
        uri,
        &parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );
    let messages: Vec<_> = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();

    assert!(
        messages.contains(&"Unknown method: App\\Service::missing"),
        "Default budget should still run member diagnostics for moderate files, got: {:?}",
        messages
    );
    assert!(
        !messages.iter().any(|message| message
            .contains("php-lsp skipped member and type diagnostics because this file exceeded")),
        "Moderate file should not emit partial-analysis diagnostic, got: {:?}",
        messages
    );
}

#[test]
fn test_compute_diagnostics_skips_member_type_checks_above_default_node_budget() {
    let diagnostics = compute_member_heavy_diagnostics(DiagnosticBudgetConfig::default());
    let messages: Vec<_> = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();

    assert!(
        !messages
            .iter()
            .any(|message| message.contains("Unknown method: App\\Service::missing")),
        "Member diagnostics should be skipped above budget, got: {:?}",
        messages
    );
    let expected_partial = format!(
        "php-lsp skipped member and type diagnostics because this file exceeded the diagnostics budget of {} relevant syntax nodes",
        DEFAULT_MEMBER_TYPE_DIAGNOSTIC_NODE_BUDGET
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains(&expected_partial)),
        "Expected a partial-analysis diagnostic, got: {:?}",
        messages
    );
    let partial = diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic
                .message
                .contains("php-lsp skipped member and type diagnostics because this file exceeded")
        })
        .expect("expected partial-analysis diagnostic");
    assert_eq!(partial.severity, Some(DiagnosticSeverity::INFORMATION));
    assert_eq!(
        partial.code,
        Some(NumberOrString::String("partial-analysis".to_string()))
    );
}

#[test]
fn test_compute_diagnostics_runs_member_type_checks_with_higher_node_budget() {
    let diagnostic_budget = DiagnosticBudgetConfig {
        member_type_node_budget: Some(10_000),
        ..DiagnosticBudgetConfig::default()
    };
    let diagnostics = compute_member_heavy_diagnostics(diagnostic_budget);
    let messages: Vec<_> = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();

    assert!(
        messages
            .iter()
            .any(|message| message.contains("Unknown method: App\\Service::missing")),
        "Member diagnostics should run with higher budget, got: {:?}",
        messages
    );
    assert!(
        !messages.iter().any(|message| message
            .contains("php-lsp skipped member and type diagnostics because this file exceeded")),
        "Partial-analysis diagnostic should not be emitted when budget is not exceeded, got: {:?}",
        messages
    );
}

#[test]
fn test_compute_diagnostics_can_disable_member_type_node_budget() {
    let diagnostic_budget = DiagnosticBudgetConfig {
        member_type_node_budget: None,
        ..DiagnosticBudgetConfig::default()
    };
    let diagnostics = compute_member_heavy_diagnostics(diagnostic_budget);
    let messages: Vec<_> = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();

    assert!(
        messages
            .iter()
            .any(|message| message.contains("Unknown method: App\\Service::missing")),
        "Member diagnostics should run when the budget cap is disabled, got: {:?}",
        messages
    );
    assert!(
        !messages.iter().any(|message| message
            .contains("php-lsp skipped member and type diagnostics because this file exceeded")),
        "Partial-analysis diagnostic should not be emitted when budget cap is disabled, got: {:?}",
        messages
    );
}

#[test]
fn test_compute_diagnostics_can_hide_partial_analysis_budget_message() {
    let diagnostic_budget = DiagnosticBudgetConfig {
        partial_analysis_diagnostic: false,
        ..DiagnosticBudgetConfig::default()
    };
    let diagnostics = compute_member_heavy_diagnostics(diagnostic_budget);
    let messages: Vec<_> = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();

    assert!(
        !messages
            .iter()
            .any(|message| message.contains("Unknown method: App\\Service::missing")),
        "Member diagnostics should still be skipped above budget, got: {:?}",
        messages
    );
    assert!(
        !messages.iter().any(|message| message
            .contains("php-lsp skipped member and type diagnostics because this file exceeded")),
        "Partial-analysis diagnostic should be hidden by config, got: {:?}",
        messages
    );
}

#[test]
fn test_diagnostic_budget_config_parses_nested_settings_and_zero_budget() {
    let config = diagnostic_budget_config_from_settings(&serde_json::json!({
        "diagnostics": {
            "memberTypeNodeBudget": 256,
            "partialAnalysisDiagnostic": false
        }
    }));
    assert_eq!(config.member_type_node_budget, Some(256));
    assert!(!config.partial_analysis_diagnostic);

    let disabled = diagnostic_budget_config_from_settings(&serde_json::json!({
        "phpLsp": {
            "diagnostics": {
                "memberTypeNodeBudget": 0
            }
        }
    }));
    assert_eq!(disabled.member_type_node_budget, None);
    assert!(disabled.partial_analysis_diagnostic);
}

#[test]
fn test_compute_diagnostics_allows_phpunit_helpers_in_framework_tests_and_test_traits() {
    let deps_uri = "file:///phpunit-deps.php";
    let deps_code = r#"<?php
namespace PHPUnit\Framework;
class TestCase {}

namespace Symfony\Bundle\FrameworkBundle\Test;
class WebTestCase extends \PHPUnit\Framework\TestCase {}
"#;

    let test_uri = "file:///framework-test.php";
    let test_code = r#"<?php
namespace App\Tests\Controller;

use Symfony\Bundle\FrameworkBundle\Test\WebTestCase;

final class FlowTest extends WebTestCase
{
    protected function setUp(): void
    {
        parent::setUp();
    }

    protected function tearDown(): void
    {
        parent::tearDown();
    }

    public function run(): void
    {
        self::assertSame(1, 1);
        $this->anything();
        $this->stringContains('needle');
    }
}
"#;

    let trait_uri = "file:///outbound-test-trait.php";
    let trait_code = r#"<?php
namespace App\Tests\Soap\Outbound;

trait OutboundTestTrait
{
    protected function helper(): void
    {
        $this->createStub(\stdClass::class);
    }
}
"#;

    let mut deps_parser = FileParser::new();
    deps_parser.parse_full(deps_code);
    let mut test_parser = FileParser::new();
    test_parser.parse_full(test_code);
    let mut trait_parser = FileParser::new();
    trait_parser.parse_full(trait_code);

    let index = WorkspaceIndex::new();
    index.update_file(
        deps_uri,
        extract_file_symbols(deps_parser.tree().unwrap(), deps_code, deps_uri),
    );
    index.update_file(
        test_uri,
        extract_file_symbols(test_parser.tree().unwrap(), test_code, test_uri),
    );
    index.update_file(
        trait_uri,
        extract_file_symbols(trait_parser.tree().unwrap(), trait_code, trait_uri),
    );

    let test_diagnostics = compute_diagnostics(
        test_uri,
        &test_parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );
    let trait_diagnostics = compute_diagnostics(
        trait_uri,
        &trait_parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );
    let messages: Vec<_> = test_diagnostics
        .iter()
        .chain(trait_diagnostics.iter())
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();

    for unexpected in [
        "Unknown method: App\\Tests\\Controller\\FlowTest::assertSame",
        "Unknown method: App\\Tests\\Controller\\FlowTest::anything",
        "Unknown method: App\\Tests\\Controller\\FlowTest::stringContains",
        "Unknown method: parent::setUp",
        "Unknown method: parent::tearDown",
        "Unknown method: App\\Tests\\Soap\\Outbound\\OutboundTestTrait::createStub",
    ] {
        assert!(
            !messages.iter().any(|message| message.contains(unexpected)),
            "Did not expect `{}` in diagnostics, got: {:?}",
            unexpected,
            messages
        );
    }
}

#[test]
fn test_compute_diagnostics_reports_basic_type_mismatches() {
    let uri = "file:///types.php";
    let code = r#"<?php
namespace App;

function takesInt(int $value): void {}

function returnsInt(): int {
    return "bad";
}

class Box {
    public int $count;

    public function set(string $name): void {}
}

function run(Box $box): void {
    takesInt("bad");
    $box->set(123);
    $box->count = "bad";
}
"#;

    let mut parser = FileParser::new();
    parser.parse_full(code);

    let index = WorkspaceIndex::new();
    let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
    index.update_file(uri, symbols);

    let diagnostics = compute_diagnostics(
        uri,
        &parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );
    let messages: Vec<_> = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();

    for expected in [
        "Type mismatch for App\\takesInt argument $value: expected int, got string",
        "Return type mismatch in App\\returnsInt: expected int, got string",
        "Type mismatch for App\\Box::set argument $name: expected string, got int",
        "Property assignment type mismatch for App\\Box::$count: expected int, got string",
    ] {
        assert!(
            messages.contains(&expected),
            "Expected `{}` in diagnostics, got: {:?}",
            expected,
            messages
        );
    }
}

#[test]
fn test_compute_diagnostics_allows_phpdoc_array_suffix_argument_type() {
    let uri = "file:///phpdoc-array-suffix.php";
    let code = r#"<?php
namespace App;

/**
 * @param mixed[] $context
 */
function logInfo(array $context = []): void {}

function run(string $soapRequest): void {
    logInfo([$soapRequest]);
}
"#;

    let mut parser = FileParser::new();
    parser.parse_full(code);

    let index = WorkspaceIndex::new();
    let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
    index.update_file(uri, symbols);

    let diagnostics = compute_diagnostics(
        uri,
        &parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );
    let messages: Vec<_> = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();

    assert!(
        !messages
            .iter()
            .any(|message| message.contains("Type mismatch")),
        "PHPDoc T[] should accept array literals, got: {:?}",
        messages
    );
}

#[test]
fn test_compute_diagnostics_expands_multiline_phpdoc_shape_alias_for_arguments() {
    let uri = "file:///phpdoc-multiline-shape-alias.php";
    let code = r#"<?php
namespace App;

/**
 * @param RowShape $row
 */
function acceptsRow(array $row): void {}

function run(): void {
    acceptsRow(['id' => 1, 'name' => 'Ada']);
}

/**
 * @phpstan-type RowShape array{
 *   id: int,
 *   name?: string,
 * }
 */
"#;

    let mut parser = FileParser::new();
    parser.parse_full(code);

    let index = WorkspaceIndex::new();
    let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
    index.update_file(uri, symbols);

    let diagnostics = compute_diagnostics(
        uri,
        &parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );
    let messages: Vec<_> = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();

    assert!(
        !messages
            .iter()
            .any(|message| message.contains("Type mismatch for App\\acceptsRow")),
        "multi-line shape alias should be expanded before argument diagnostics, got: {:?}",
        messages
    );
}

#[test]
fn test_compute_diagnostics_allows_psr_logger_context_array_suffix_type() {
    let uri = "file:///logger-context.php";
    let code = r#"<?php
namespace Psr\Log;

interface LoggerInterface
{
    /**
     * @param mixed[] $context
     */
    public function info(string $message, array $context = []): void;
}

namespace App;

use Psr\Log\LoggerInterface;

final class DeactivateConfirmService
{
    public function __construct(private LoggerInterface $logger) {}

    public function run(string $soapRequest): void
    {
        $this->logger->info('Prepared Deactivate Confirm SOAP request', [$soapRequest]);
    }
}
"#;

    let mut parser = FileParser::new();
    parser.parse_full(code);

    let index = WorkspaceIndex::new();
    let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
    index.update_file(uri, symbols);

    let diagnostics = compute_diagnostics(
        uri,
        &parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );
    let messages: Vec<_> = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();

    for unexpected in [
        "Type mismatch for Psr\\Log\\LoggerInterface::info argument $context",
        "Unknown method: Psr\\Log\\LoggerInterface::info",
    ] {
        assert!(
            !messages.iter().any(|message| message.contains(unexpected)),
            "Did not expect `{}` in diagnostics, got: {:?}",
            unexpected,
            messages
        );
    }
}

#[test]
fn test_compute_diagnostics_skips_uncertain_ternary_return_type() {
    let uri = "file:///ternary-return.php";
    let code = r#"<?php
namespace App;

class RemoteFileService {}

class Controller {
    private RemoteFileService $primaryFileService;
    private RemoteFileService $secondaryFileService;

    private function getService(string $name): RemoteFileService {
        return 'primary' === $name ? $this->primaryFileService : $this->secondaryFileService;
    }
}
"#;

    let mut parser = FileParser::new();
    parser.parse_full(code);

    let index = WorkspaceIndex::new();
    let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
    index.update_file(uri, symbols);

    let diagnostics = compute_diagnostics(
        uri,
        &parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );
    let messages: Vec<_> = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();

    assert!(
        !messages.iter().any(|message| {
            message.contains("Return type mismatch in App\\Controller::getService")
        }),
        "Uncertain ternary return should not be inferred from its condition, got: {:?}",
        messages
    );
}

#[test]
fn test_compute_diagnostics_reports_override_and_php_version_errors() {
    let uri = "file:///override.php";
    let code = r#"<?php
namespace App;

class Base {
    public function value(int $id): string {
        return "";
    }
}

class Child extends Base {
    public function value(string $id): int {
        return 1;
    }
}

function nullableUnion(): string|null {
    return null;
}
"#;

    let mut parser = FileParser::new();
    parser.parse_full(code);

    let index = WorkspaceIndex::new();
    let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
    index.update_file(uri, symbols);

    let diagnostics = compute_diagnostics(
        uri,
        &parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion { major: 7, minor: 4 },
    );
    let messages: Vec<_> = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();

    for expected in [
        "Incompatible override signature: App\\Child::value differs from App\\Base::value",
        "Type is not supported by PHP 7.4: string|null",
    ] {
        assert!(
            messages.contains(&expected),
            "Expected `{}` in diagnostics, got: {:?}",
            expected,
            messages
        );
    }
}

#[test]
fn test_compute_diagnostics_applies_class_variance_to_override_signatures() {
    let uri = "file:///override-variance.php";
    let code = r#"<?php
namespace App;

class Animal {}
class Dog extends Animal {}
class ServiceDog extends Dog {}

class Base {
    public function adopt(Dog $dog): Animal {
        return $dog;
    }
}

class GoodChild extends Base {
    public function adopt(Animal $dog): Dog {
        return new Dog();
    }
}

class BadParamChild extends Base {
    public function adopt(ServiceDog $dog): Animal {
        return $dog;
    }
}

class ReturnBase {
    public function make(): Dog {
        return new Dog();
    }
}

class BadReturnChild extends ReturnBase {
    public function make(): Animal {
        return new Animal();
    }
}
"#;

    let index = WorkspaceIndex::new();
    let parser = parse_and_index_php_file(&index, uri, code);

    let diagnostics = compute_diagnostics(
        uri,
        &parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );
    let messages = diagnostic_messages(&diagnostics);

    assert_no_diagnostic_containing(
        &messages,
        "Incompatible override signature: App\\GoodChild::adopt",
    );
    assert!(
        messages.iter().any(|message| {
            message == "Incompatible override signature: App\\BadParamChild::adopt differs from App\\Base::adopt"
        }),
        "Expected narrowed parameter override diagnostic, got: {:?}",
        messages
    );
    assert!(
        messages.iter().any(|message| {
            message == "Incompatible override signature: App\\BadReturnChild::make differs from App\\ReturnBase::make"
        }),
        "Expected widened return override diagnostic, got: {:?}",
        messages
    );
}

#[test]
fn test_compute_diagnostics_allows_named_arguments() {
    let uri = "file:///named-args.php";
    let code = r#"<?php
namespace Symfony\Component\Validator\Constraints;

class NotBlank {
    public function __construct(?array $options = null, ?string $message = null) {}
}

class Length {
    public function __construct(?array $options = null, ?int $min = null, ?int $max = null, ?string $minMessage = null, ?string $maxMessage = null) {}
}

namespace App;

use Symfony\Component\Validator\Constraints\Length;
use Symfony\Component\Validator\Constraints\NotBlank;

function run(): void {
    new NotBlank(message: 'Required');
    new Length(max: 255, maxMessage: 'Too long');
}
"#;

    let mut parser = FileParser::new();
    parser.parse_full(code);

    let index = WorkspaceIndex::new();
    let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
    index.update_file(uri, symbols);

    let diagnostics = compute_diagnostics(
        uri,
        &parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );
    let messages: Vec<_> = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();

    assert!(
        !messages
            .iter()
            .any(|message| message.contains("Type mismatch")),
        "Named arguments should be matched by parameter name, got: {:?}",
        messages
    );
}

#[test]
fn test_compute_diagnostics_accepts_positive_int_literal_phpdoc_type() {
    let uri = "file:///positive-int-literal.php";
    let code = r#"<?php
namespace Symfony\Component\Validator\Constraints;

class Length {
    /**
     * @param positive-int|null $max
     */
    public function __construct(?int $max = null) {}
}

namespace App;

use Symfony\Component\Validator\Constraints\Length;

function build(): void {
    new Length(max: 255);
}
"#;

    let index = WorkspaceIndex::new();
    let parser = parse_and_index_php_file(&index, uri, code);

    let diagnostics = compute_diagnostics(
        uri,
        &parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );
    let messages = diagnostic_messages(&diagnostics);

    assert_no_diagnostic_containing(&messages, "Type mismatch");
    assert_no_diagnostic_containing(&messages, "positive-int");
}

#[test]
fn test_compute_diagnostics_resolves_phpdoc_method_tags() {
    let uri = "file:///phpdoc-method-call.php";
    let code = r#"<?php
namespace Symfony\Component\HttpFoundation;

class Request {}

namespace SymfonyCasts\Bundle\VerifyEmail;

/**
 * @method void validateEmailConfirmationFromRequest(Request $request, string $userId, string $userEmail)
 */
interface VerifyEmailHelperInterface {}

namespace App;

use Symfony\Component\HttpFoundation\Request;
use SymfonyCasts\Bundle\VerifyEmail\VerifyEmailHelperInterface;

final class EmailVerifier
{
    public function __construct(private VerifyEmailHelperInterface $helper) {}

    public function handle(Request $request): void
    {
        $this->helper->validateEmailConfirmationFromRequest($request, '1', 'a@example.com');
    }
}
"#;

    let index = WorkspaceIndex::new();
    let parser = parse_and_index_php_file(&index, uri, code);

    let diagnostics = compute_diagnostics(
        uri,
        &parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );
    let messages = diagnostic_messages(&diagnostics);

    assert_no_diagnostic_containing(
            &messages,
            "Unknown method: SymfonyCasts\\Bundle\\VerifyEmail\\VerifyEmailHelperInterface::validateEmailConfirmationFromRequest",
        );
}

#[test]
fn test_compute_diagnostics_ignores_phpdoc_method_tags_for_override_checks() {
    let uri = "file:///phpdoc-method-override-noise.php";
    let code = r#"<?php
namespace Vendor;

class Entity {}

class BaseRepository
{
    public function find(mixed $id): object|null
    {
        return null;
    }
}

namespace App;

use Vendor\BaseRepository;
use Vendor\Entity;

/**
 * @method Entity|null find($id)
 */
final class EntityRepository extends BaseRepository
{
}
"#;

    let index = WorkspaceIndex::new();
    let parser = parse_and_index_php_file(&index, uri, code);

    let diagnostics = compute_diagnostics(
        uri,
        &parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );
    let messages = diagnostic_messages(&diagnostics);

    assert_no_diagnostic_containing(
            &messages,
            "Incompatible override signature: App\\EntityRepository::find differs from Vendor\\BaseRepository::find",
        );
}

#[test]
fn test_compute_diagnostics_allows_simplexml_dynamic_properties() {
    let stub_uri = "phpstub://SimpleXML/SimpleXML.php";
    let stub_code = "<?php\nclass SimpleXMLElement { /** @return static */ private function __get($name) {} }\n";
    let uri = "file:///simplexml-dynamic-properties.php";
    let code = r#"<?php
namespace App;

function status(\SimpleXMLElement $result): void {
    $statusCode = (string) $result->StatusCode;
    echo $statusCode;
}
"#;

    let index = WorkspaceIndex::new();
    let _stub_parser = parse_and_index_php_file(&index, stub_uri, stub_code);
    let parser = parse_and_index_php_file(&index, uri, code);

    let diagnostics = compute_diagnostics(
        uri,
        &parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );
    let messages = diagnostic_messages(&diagnostics);

    assert_no_diagnostic_containing(&messages, "Unknown property: SimpleXMLElement::$StatusCode");
}

#[test]
fn test_compute_diagnostics_accepts_non_empty_string_literals() {
    let uri = "file:///non-empty-string-literal.php";
    let code = r#"<?php
namespace App;

final class RailsClient
{
    /**
     * @param non-empty-string $path
     * @param array<string,mixed> $payload
     */
    public function post(string $path, array $payload): array
    {
        return [];
    }

    public function log(string $message): void {}
}

function run(RailsClient $client, string $suffix): void
{
    $client->post('/v1/billing/crm/get-personal-data', []);
    $client->log('Rails API HTTP error: ' . $suffix);
}
"#;

    let index = WorkspaceIndex::new();
    let parser = parse_and_index_php_file(&index, uri, code);

    let diagnostics = compute_diagnostics(
        uri,
        &parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );
    let messages = diagnostic_messages(&diagnostics);

    assert_no_diagnostic_containing(
        &messages,
        "Type mismatch for App\\RailsClient::post argument $path",
    );
    assert_no_diagnostic_containing(
        &messages,
        "Type mismatch for App\\RailsClient::log argument $message",
    );
}

#[test]
fn test_compute_diagnostics_allows_enum_builtin_methods_and_parent_constructor() {
    let uri = "file:///enum-parent.php";
    let code = r#"<?php
namespace App;

enum TimerType: string {
    case Tccp = 'tccp';
}

class BaseCommand {
    public function __construct(?string $name = null) {}
}

class SendCommand extends BaseCommand {
    public function __construct(private TimerType $timerType) {
        parent::__construct();
    }

    public function run(): void {
        TimerType::cases();
        TimerType::tryFrom('tccp');
    }
}
"#;

    let mut parser = FileParser::new();
    parser.parse_full(code);

    let index = WorkspaceIndex::new();
    let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
    index.update_file(uri, symbols);

    let diagnostics = compute_diagnostics(
        uri,
        &parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );
    let messages: Vec<_> = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();

    for unexpected in [
            "Unknown method: App\\TimerType::cases",
            "Unknown method: App\\TimerType::tryFrom",
            "Unknown method: parent::__construct",
            "Incompatible override signature: App\\SendCommand::__construct differs from App\\BaseCommand::__construct",
        ] {
            assert!(
                !messages.contains(&unexpected),
                "Did not expect `{}` in diagnostics, got: {:?}",
                unexpected,
                messages
            );
        }
}

#[test]
fn test_compute_diagnostics_allows_alias_and_mixed_override_signatures() {
    let scheduler_uri = "file:///scheduler-overrides.php";
    let scheduler_code = r#"<?php
namespace Symfony\Component\Scheduler;

class Schedule {}

interface ScheduleProviderInterface {
    public function getSchedule(): Schedule;
}
"#;

    let voter_uri = "file:///voter-overrides.php";
    let voter_code = r#"<?php
namespace Symfony\Component\Security\Core\Authorization\Voter;

abstract class Voter {
    protected function supports(string $attribute, mixed $subject): bool {
        echo $attribute;
        echo $subject;
        return true;
    }
}
"#;

    let app_uri = "file:///app-overrides.php";
    let app_code = r#"<?php
namespace App;

use Symfony\Component\Scheduler\Schedule as SymfonySchedule;
use Symfony\Component\Scheduler\ScheduleProviderInterface;
use Symfony\Component\Security\Core\Authorization\Voter\Voter;

class Schedule implements ScheduleProviderInterface {
    public function getSchedule(): SymfonySchedule {
        return new SymfonySchedule();
    }
}

class UserVoter extends Voter {
    protected function supports(string $attribute, $subject): bool {
        echo $attribute;
        echo $subject;
        return true;
    }
}
"#;

    let mut scheduler_parser = FileParser::new();
    scheduler_parser.parse_full(scheduler_code);
    let mut voter_parser = FileParser::new();
    voter_parser.parse_full(voter_code);
    let mut app_parser = FileParser::new();
    app_parser.parse_full(app_code);

    let index = WorkspaceIndex::new();
    index.update_file(
        scheduler_uri,
        extract_file_symbols(
            scheduler_parser.tree().unwrap(),
            scheduler_code,
            scheduler_uri,
        ),
    );
    index.update_file(
        voter_uri,
        extract_file_symbols(voter_parser.tree().unwrap(), voter_code, voter_uri),
    );
    index.update_file(
        app_uri,
        extract_file_symbols(app_parser.tree().unwrap(), app_code, app_uri),
    );

    let diagnostics = compute_diagnostics(
        app_uri,
        &app_parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );
    let messages: Vec<_> = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();

    for unexpected in [
            "Incompatible override signature: App\\Schedule::getSchedule differs from Symfony\\Component\\Scheduler\\ScheduleProviderInterface::getSchedule",
            "Incompatible override signature: App\\UserVoter::supports differs from Symfony\\Component\\Security\\Core\\Authorization\\Voter\\Voter::supports",
        ] {
            assert!(
                !messages.contains(&unexpected),
                "Did not expect `{}` in diagnostics, got: {:?}",
                unexpected,
                messages
            );
        }
}

#[test]
fn test_compute_diagnostics_allows_template_phpdoc_override_signature() {
    let framework_uri = "file:///security-voter-framework.php";
    let framework_code = r#"<?php
namespace Symfony\Component\Security\Core\Authentication\Token;

interface TokenInterface {}

namespace Symfony\Component\Security\Core\Authorization\Voter;

use Symfony\Component\Security\Core\Authentication\Token\TokenInterface;

final class Vote {}

/**
 * @template TAttribute of string
 * @template TSubject
 */
abstract class Voter
{
    /**
     * @param TAttribute $attribute
     * @param TSubject $subject
     */
    abstract protected function voteOnAttribute(string $attribute, mixed $subject, TokenInterface $token, ?Vote $vote = null): bool;
}
"#;
    let app_uri = "file:///security-voter-app.php";
    let app_code = r#"<?php
namespace App;

use Symfony\Component\Security\Core\Authentication\Token\TokenInterface;
use Symfony\Component\Security\Core\Authorization\Voter\Vote;
use Symfony\Component\Security\Core\Authorization\Voter\Voter;

final class UserVoter extends Voter
{
    protected function voteOnAttribute(string $attribute, mixed $subject, TokenInterface $token, ?Vote $vote = null): bool
    {
        return true;
    }
}
"#;

    let index = WorkspaceIndex::new();
    let _framework_parser = parse_and_index_php_file(&index, framework_uri, framework_code);
    let parser = parse_and_index_php_file(&index, app_uri, app_code);

    let diagnostics = compute_diagnostics(
        app_uri,
        &parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );
    let messages = diagnostic_messages(&diagnostics);

    assert_no_diagnostic_containing(
            &messages,
            "Incompatible override signature: App\\UserVoter::voteOnAttribute differs from Symfony\\Component\\Security\\Core\\Authorization\\Voter\\Voter::voteOnAttribute",
        );
}

#[test]
fn test_compute_diagnostics_allows_phpdoc_refined_array_override_signature() {
    let uri = "file:///billing-payload-overrides.php";
    let code = r#"<?php
namespace App;

interface BillingPayloadProcessor
{
    /**
     * @param array<int,array<string,mixed>> $payload
     * @return array<int,array<string,mixed>>
     */
    public function processWithBillingPayload(array $payload): array;
}

final class NpDataResponseService implements BillingPayloadProcessor
{
    public function processWithBillingPayload(array $payload): array
    {
        return $payload;
    }
}
"#;

    let index = WorkspaceIndex::new();
    let parser = parse_and_index_php_file(&index, uri, code);

    let diagnostics = compute_diagnostics(
        uri,
        &parser,
        &index,
        DiagnosticsMode::BasicSemantic,
        PhpVersion::DEFAULT,
    );
    let messages = diagnostic_messages(&diagnostics);

    assert_no_diagnostic_containing(
            &messages,
            "Incompatible override signature: App\\NpDataResponseService::processWithBillingPayload differs from App\\BillingPayloadProcessor::processWithBillingPayload",
        );
}

#[test]
fn test_formatting_provider_none_disables_stale_command() {
    let config =
        FormattingConfig::from_options(Some("none"), Some("vendor/bin/php-cs-fixer"), None);
    assert!(config.command_template().is_none());

    let custom =
        FormattingConfig::from_options(Some("custom"), Some("vendor/bin/fmt {file}"), None);
    assert_eq!(
        custom.command_template().as_deref(),
        Some("vendor/bin/fmt {file}")
    );
}

#[test]
fn test_framework_string_key_cache_evicts_lru_entries() {
    fn key(root: &str, domain: &str) -> FrameworkStringKeyCacheKey {
        FrameworkStringKeyCacheKey {
            root: PathBuf::from(root),
            domain: domain.to_string(),
        }
    }

    fn value(name: &str) -> crate::framework::FrameworkStringKey {
        crate::framework::FrameworkStringKey {
            key: name.to_string(),
            detail: None,
            provider_ids: Vec::new(),
            sources: Vec::new(),
        }
    }

    let mut cache = FrameworkStringKeyCache {
        capacity: 2,
        ..Default::default()
    };
    let first = key("/workspace-a", "config");
    let second = key("/workspace-b", "route");
    let third = key("/workspace-c", "view");

    cache.insert(first.clone(), vec![value("app.name")]);
    cache.insert(second.clone(), vec![value("home")]);
    assert!(cache.get(&first).is_some());
    cache.insert(third.clone(), vec![value("users.show")]);

    assert!(cache.get(&second).is_none());
    assert!(cache.get(&first).is_some());
    assert!(cache.get(&third).is_some());
}

#[test]
fn test_twig_context_disk_cache_evicts_lru_entries() {
    fn key(root: &str, template_name: &str) -> TwigContextDiskCacheKey {
        TwigContextDiskCacheKey {
            root: PathBuf::from(root),
            template_name: template_name.to_string(),
        }
    }

    fn value(uri: &str, name: &str, type_text: &str) -> TwigContextFileVariables {
        TwigContextFileVariables {
            uri: uri.to_string(),
            variables: vec![TemplateVariableType {
                name: name.to_string(),
                type_text: type_text.to_string(),
                shape_definitions: Vec::new(),
            }],
        }
    }

    let mut cache = TwigContextDiskCache {
        capacity: 2,
        ..Default::default()
    };
    let first = key("/workspace", "one.html.twig");
    let second = key("/workspace", "two.html.twig");
    let third = key("/workspace", "three.html.twig");

    cache.insert(
        first.clone(),
        vec![value("file:///one.php", "user", "User")],
    );
    cache.insert(
        second.clone(),
        vec![value("file:///two.php", "team", "Team")],
    );
    assert!(cache.get(&first).is_some());
    cache.insert(
        third.clone(),
        vec![value("file:///three.php", "post", "Post")],
    );

    assert!(cache.get(&second).is_none());
    assert!(cache.get(&first).is_some());
    assert!(cache.get(&third).is_some());
}

#[test]
fn test_twig_context_disk_cache_evicts_entries_for_source_uri() {
    fn key(root: &str, template_name: &str) -> TwigContextDiskCacheKey {
        TwigContextDiskCacheKey {
            root: PathBuf::from(root),
            template_name: template_name.to_string(),
        }
    }

    fn value(uri: &str, name: &str, type_text: &str) -> TwigContextFileVariables {
        TwigContextFileVariables {
            uri: uri.to_string(),
            variables: vec![TemplateVariableType {
                name: name.to_string(),
                type_text: type_text.to_string(),
                shape_definitions: Vec::new(),
            }],
        }
    }

    let mut cache = TwigContextDiskCache {
        capacity: 4,
        ..Default::default()
    };
    let controller_uri = "file:///workspace/src/Controller/DashboardController.php";
    let dashboard = key("/workspace", "dashboard/show.html.twig");
    let profile = key("/workspace", "profile/show.html.twig");
    let unrelated = key("/workspace", "blog/show.html.twig");

    cache.insert(
        dashboard.clone(),
        vec![
            value(controller_uri, "user", "App\\Entity\\User"),
            value(
                "file:///workspace/src/Controller/TeamController.php",
                "team",
                "Team",
            ),
        ],
    );
    cache.insert(
        profile.clone(),
        vec![value(controller_uri, "profile", "App\\Entity\\Profile")],
    );
    cache.insert(
        unrelated.clone(),
        vec![value(
            "file:///workspace/src/Controller/BlogController.php",
            "post",
            "App\\Entity\\Post",
        )],
    );

    assert_eq!(cache.evict_entries_for_source_uri(controller_uri), 2);
    assert!(cache.get(&dashboard).is_none());
    assert!(cache.get(&profile).is_none());
    assert!(cache.get(&unrelated).is_some());
    assert_eq!(cache.len(), 1);
    assert_eq!(cache.evict_entries_for_source_uri(controller_uri), 0);
}

#[tokio::test]
async fn test_request_fs_cache_invalidation_clears_framework_and_twig_caches() {
    let framework_cache = Arc::new(Mutex::new(FrameworkStringKeyCache {
        capacity: 4,
        ..Default::default()
    }));
    framework_cache.lock().await.insert(
        FrameworkStringKeyCacheKey {
            root: PathBuf::from("/workspace"),
            domain: "config".to_string(),
        },
        vec![crate::framework::FrameworkStringKey {
            key: "app.name".to_string(),
            detail: None,
            provider_ids: Vec::new(),
            sources: Vec::new(),
        }],
    );

    let twig_cache = Arc::new(Mutex::new(TwigContextDiskCache {
        capacity: 4,
        ..Default::default()
    }));
    twig_cache.lock().await.insert(
        TwigContextDiskCacheKey {
            root: PathBuf::from("/workspace"),
            template_name: "users/show.html.twig".to_string(),
        },
        vec![TwigContextFileVariables {
            uri: "file:///workspace/src/Controller/UserController.php".to_string(),
            variables: vec![TemplateVariableType {
                name: "user".to_string(),
                type_text: "App\\Entity\\User".to_string(),
                shape_definitions: Vec::new(),
            }],
        }],
    );

    assert_eq!(framework_cache.lock().await.len(), 1);
    assert_eq!(twig_cache.lock().await.len(), 1);

    clear_request_fs_caches(&framework_cache, &twig_cache).await;

    assert_eq!(framework_cache.lock().await.len(), 0);
    assert_eq!(twig_cache.lock().await.len(), 0);
}

#[test]
fn test_formatting_auto_detects_project_tools_from_composer_metadata() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let tmp = std::env::temp_dir().join(format!(
        "php-lsp-format-detect-test-{}-{}",
        std::process::id(),
        nanos
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(
        tmp.join("composer.json"),
        r#"{
                "require-dev": {
                    "friendsofphp/php-cs-fixer": "^3.0",
                    "squizlabs/php_codesniffer": "^3.0"
                }
            }"#,
    )
    .unwrap();

    let config = FormattingConfig::default().resolve_for_workspace(Some(&tmp));
    assert_eq!(config.provider, "php-cs-fixer");
    assert_eq!(
        config.command_template().as_deref(),
        Some("vendor/bin/php-cs-fixer fix --using-cache=no --quiet {file}")
    );

    std::fs::write(
        tmp.join("composer.json"),
        r#"{
                "require-dev": {
                    "laravel/pint": "^1.0",
                    "friendsofphp/php-cs-fixer": "^3.0"
                }
            }"#,
    )
    .unwrap();
    let config = FormattingConfig::default().resolve_for_workspace(Some(&tmp));
    assert_eq!(config.provider, "pint");
    assert_eq!(
        config.command_template().as_deref(),
        Some("vendor/bin/pint --quiet {file}")
    );

    let disabled =
        FormattingConfig::from_options(Some("none"), None, None).resolve_for_workspace(Some(&tmp));
    assert!(disabled.command_template().is_none());

    let _ = std::fs::remove_dir_all(tmp);
}

#[test]
fn test_parse_phpstan_json_diagnostics_maps_messages() {
    let file_path = PathBuf::from("/tmp/php-lsp-phpstan/src/Foo.php");
    let output = serde_json::json!({
        "totals": { "errors": 0, "file_errors": 1 },
        "files": {
            (file_path.to_string_lossy().to_string()): {
                "errors": 1,
                "messages": [
                    {
                        "message": "Call to an undefined method App\\Foo::missing().",
                        "line": 7,
                        "identifier": "method.notFound",
                        "tip": "Check the object type."
                    }
                ]
            }
        },
        "errors": []
    })
    .to_string();

    let diagnostics = parse_phpstan_json_diagnostics(&output, &file_path).unwrap();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].range.start.line, 6);
    assert_eq!(diagnostics[0].source.as_deref(), Some("phpstan"));
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(
        diagnostics[0].code,
        Some(NumberOrString::String("method.notFound".to_string()))
    );
    assert!(
        diagnostics[0]
            .message
            .contains("Call to an undefined method App\\Foo::missing()."),
        "unexpected message: {}",
        diagnostics[0].message
    );
    assert!(
        diagnostics[0].message.contains("Check the object type."),
        "tip should be appended to diagnostic message"
    );
}

#[tokio::test]
async fn test_run_phpstan_for_file_accepts_nonzero_json_output() {
    if cfg!(windows) {
        return;
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let tmp = std::env::temp_dir().join(format!(
        "php-lsp-phpstan-test-{}-{}",
        std::process::id(),
        nanos
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let file_path = tmp.join("Subject.php");
    std::fs::write(&file_path, "<?php\nclass Subject {}\n").unwrap();

    let output = serde_json::json!({
        "totals": { "errors": 0, "file_errors": 1 },
        "files": {
            (file_path.to_string_lossy().to_string()): {
                "errors": 1,
                "messages": [
                    {
                        "message": "PHPStan reported a test error.",
                        "line": 2,
                        "identifier": "test.identifier"
                    }
                ]
            }
        },
        "errors": []
    })
    .to_string();

    let script_path = tmp.join("phpstan-fake.sh");
    std::fs::write(
        &script_path,
        format!("#!/bin/sh\ncat <<'JSON'\n{}\nJSON\nexit 1\n", output),
    )
    .unwrap();

    let config = PhpStanConfig {
        enabled: true,
        command: format!(
            "sh {} {{file}}",
            shell_escape(&script_path.to_string_lossy())
        ),
        timeout_ms: 5_000,
        memory_limit: None,
    };
    let diagnostics = run_phpstan_for_file(config, file_path, Some(tmp.clone()), None)
        .await
        .unwrap();

    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].source.as_deref(), Some("phpstan"));
    assert_eq!(diagnostics[0].message, "PHPStan reported a test error.");

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn test_run_shell_command_with_timeout_respects_cancellation() {
    if cfg!(windows) {
        return;
    }

    let token = OperationCancellationToken::new();
    let cancel_token = token.clone();
    let run = tokio::spawn(async move {
        run_shell_command_with_timeout("Test", "sleep 5", None, 10_000, Some(token)).await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel_token.cancel();

    let error = run.await.unwrap().unwrap_err();
    assert_eq!(error, "Test command cancelled");
}

#[tokio::test]
async fn test_external_analyzers_timeout_without_hanging() {
    if cfg!(windows) {
        return;
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let tmp = std::env::temp_dir().join(format!(
        "php-lsp-analyzer-timeout-test-{}-{}",
        std::process::id(),
        nanos
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let file_path = tmp.join("Subject.php");
    std::fs::write(&file_path, "<?php\nclass Subject {}\n").unwrap();
    let script_path = tmp.join("slow-analyzer.sh");
    std::fs::write(&script_path, "#!/bin/sh\nsleep 5\n").unwrap();
    let command = format!(
        "sh {} {{file}}",
        shell_escape(&script_path.to_string_lossy())
    );

    let phpstan = tokio::time::timeout(
        Duration::from_secs(1),
        run_phpstan_for_file(
            PhpStanConfig {
                enabled: true,
                command: command.clone(),
                timeout_ms: 50,
                memory_limit: None,
            },
            file_path.clone(),
            Some(tmp.clone()),
            None,
        ),
    )
    .await
    .expect("PHPStan timeout path should not hang")
    .unwrap_err();
    assert!(
        phpstan.contains("PHPStan command timed out after 50ms"),
        "unexpected PHPStan timeout error: {}",
        phpstan
    );

    let psalm = tokio::time::timeout(
        Duration::from_secs(1),
        run_psalm_for_file(
            PsalmConfig {
                enabled: true,
                command,
                timeout_ms: 50,
            },
            file_path,
            Some(tmp.clone()),
            None,
        ),
    )
    .await
    .expect("Psalm timeout path should not hang")
    .unwrap_err();
    assert!(
        psalm.contains("Psalm command timed out after 50ms"),
        "unexpected Psalm timeout error: {}",
        psalm
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn test_external_analyzers_malformed_json_without_hanging() {
    if cfg!(windows) {
        return;
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let tmp = std::env::temp_dir().join(format!(
        "php-lsp-analyzer-malformed-json-test-{}-{}",
        std::process::id(),
        nanos
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let file_path = tmp.join("Subject.php");
    std::fs::write(&file_path, "<?php\nclass Subject {}\n").unwrap();
    let script_path = tmp.join("malformed-analyzer.sh");
    std::fs::write(&script_path, "#!/bin/sh\nprintf '{not-json'\nexit 0\n").unwrap();
    let command = format!(
        "sh {} {{file}}",
        shell_escape(&script_path.to_string_lossy())
    );

    let phpstan = tokio::time::timeout(
        Duration::from_secs(1),
        run_phpstan_for_file(
            PhpStanConfig {
                enabled: true,
                command: command.clone(),
                timeout_ms: 5_000,
                memory_limit: None,
            },
            file_path.clone(),
            Some(tmp.clone()),
            None,
        ),
    )
    .await
    .expect("PHPStan malformed JSON path should not hang")
    .unwrap_err();
    assert!(
        phpstan.contains("invalid PHPStan JSON"),
        "unexpected PHPStan malformed JSON error: {}",
        phpstan
    );

    let psalm = tokio::time::timeout(
        Duration::from_secs(1),
        run_psalm_for_file(
            PsalmConfig {
                enabled: true,
                command,
                timeout_ms: 5_000,
            },
            file_path,
            Some(tmp.clone()),
            None,
        ),
    )
    .await
    .expect("Psalm malformed JSON path should not hang")
    .unwrap_err();
    assert!(
        psalm.contains("invalid Psalm JSON"),
        "unexpected Psalm malformed JSON error: {}",
        psalm
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn test_parse_psalm_json_diagnostics_maps_issues() {
    let file_path = PathBuf::from("/tmp/php-lsp-psalm/src/Foo.php");
    let output = serde_json::json!([
        {
            "severity": "error",
            "line_from": 4,
            "line_to": 4,
            "type": "UndefinedMethod",
            "message": "Method App\\Foo::missing does not exist",
            "file_name": file_path.to_string_lossy().to_string(),
            "file_path": file_path.to_string_lossy().to_string(),
            "column_from": 12,
            "column_to": 19
        }
    ])
    .to_string();

    let diagnostics = parse_psalm_json_diagnostics(&output, &file_path).unwrap();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].range.start.line, 3);
    assert_eq!(diagnostics[0].range.start.character, 11);
    assert_eq!(diagnostics[0].range.end.character, 18);
    assert_eq!(diagnostics[0].source.as_deref(), Some("psalm"));
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(
        diagnostics[0].code,
        Some(NumberOrString::String("UndefinedMethod".to_string()))
    );
    assert_eq!(
        diagnostics[0].message,
        "Method App\\Foo::missing does not exist"
    );
}

#[tokio::test]
async fn test_run_psalm_for_file_accepts_nonzero_json_output() {
    if cfg!(windows) {
        return;
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let tmp = std::env::temp_dir().join(format!(
        "php-lsp-psalm-test-{}-{}",
        std::process::id(),
        nanos
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let file_path = tmp.join("Subject.php");
    std::fs::write(&file_path, "<?php\nclass Subject {}\n").unwrap();

    let output = serde_json::json!([
        {
            "severity": "info",
            "line_from": 2,
            "line_to": 2,
            "type": "PossiblyUnusedMethod",
            "message": "Psalm reported a test issue.",
            "file_path": file_path.to_string_lossy().to_string()
        }
    ])
    .to_string();

    let script_path = tmp.join("psalm-fake.sh");
    std::fs::write(
        &script_path,
        format!("#!/bin/sh\ncat <<'JSON'\n{}\nJSON\nexit 1\n", output),
    )
    .unwrap();

    let config = PsalmConfig {
        enabled: true,
        command: format!(
            "sh {} {{file}}",
            shell_escape(&script_path.to_string_lossy())
        ),
        timeout_ms: 5_000,
    };
    let diagnostics = run_psalm_for_file(config, file_path, Some(tmp.clone()), None)
        .await
        .unwrap();

    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].source.as_deref(), Some("psalm"));
    assert_eq!(
        diagnostics[0].severity,
        Some(DiagnosticSeverity::INFORMATION)
    );
    assert_eq!(diagnostics[0].message, "Psalm reported a test issue.");

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn test_untrusted_project_config_strips_executable_settings() {
    let mut settings = serde_json::json!({
        "allowProjectCommands": true,
        "phpVersion": "8.3",
        "diagnostics": { "mode": "syntax-only" },
        "includePaths": ["src"],
        "formatting": {
            "provider": "php-cs-fixer",
            "command": "sh -c 'touch /tmp/php-lsp-owned' {file}",
            "timeoutMs": 1000
        },
        "phpstan": {
            "enabled": true,
            "command": "sh -c 'touch /tmp/php-lsp-owned' {file}",
            "timeoutMs": 1000,
            "memory_limit": "1G"
        },
        "psalm": {
            "enabled": true,
            "command": "sh -c 'touch /tmp/php-lsp-owned' {file}",
            "timeoutMs": 1000
        }
    });

    let message = sanitize_project_settings_for_command_trust(
        &mut settings,
        Path::new("/workspace/.php-lsp.toml"),
        false,
    )
    .expect("expected executable project settings to be ignored");

    assert_eq!(settings["phpVersion"], "8.3");
    assert_eq!(settings["diagnostics"]["mode"], "syntax-only");
    assert_eq!(settings["includePaths"][0], "src");
    assert!(settings.get("allowProjectCommands").is_none());
    assert!(settings["formatting"].get("provider").is_none());
    assert!(settings["formatting"].get("command").is_none());
    assert_eq!(settings["formatting"]["timeoutMs"], 1000);
    assert!(settings["phpstan"].get("enabled").is_none());
    assert!(settings["phpstan"].get("command").is_none());
    assert_eq!(settings["phpstan"]["timeoutMs"], 1000);
    assert_eq!(settings["phpstan"]["memory_limit"], "1G");
    assert!(settings["psalm"].get("enabled").is_none());
    assert!(settings["psalm"].get("command").is_none());
    assert_eq!(settings["psalm"]["timeoutMs"], 1000);
    assert!(message.contains("formatting.command"));
    assert!(message.contains("phpstan.enabled"));
    assert!(message.contains("psalm.command"));
}

#[test]
fn test_trusted_project_config_keeps_executable_settings_but_not_self_trust() {
    let mut settings = serde_json::json!({
        "allowProjectCommands": true,
        "formatting": {
            "provider": "pint",
            "command": "vendor/bin/pint --quiet {file}"
        },
        "phpstan": {
            "enabled": true,
            "command": "vendor/bin/phpstan analyse --error-format=json {file}"
        },
        "psalm": {
            "enabled": true,
            "command": "vendor/bin/psalm --output-format=json {file}"
        }
    });

    let message = sanitize_project_settings_for_command_trust(
        &mut settings,
        Path::new("/workspace/.php-lsp.toml"),
        true,
    );

    assert!(message.is_none());
    assert!(settings.get("allowProjectCommands").is_none());
    assert_eq!(settings["formatting"]["provider"], "pint");
    assert_eq!(
        settings["formatting"]["command"],
        "vendor/bin/pint --quiet {file}"
    );
    assert_eq!(settings["phpstan"]["enabled"], true);
    assert_eq!(settings["psalm"]["enabled"], true);
}

#[test]
fn test_client_project_command_trust_overrides_global_config() {
    let global = serde_json::json!({ "allowProjectCommands": true });
    let client = serde_json::json!({ "allowProjectCommands": false });
    assert!(!project_commands_are_trusted(&global, &client));

    let client = serde_json::json!({});
    assert!(project_commands_are_trusted(&global, &client));

    let client = serde_json::json!({ "allowProjectCommands": true });
    assert!(project_commands_are_trusted(
        &serde_json::json!({}),
        &client
    ));
}

#[test]
fn test_path_is_excluded_matches_relative_directory() {
    let root = PathBuf::from("/project");
    let exclude_paths = normalize_config_paths(vec!["var/cache".to_string()]);

    assert!(path_is_excluded(
        Path::new("/project/var/cache/Generated.php"),
        &root,
        &exclude_paths
    ));
    assert!(!path_is_excluded(
        Path::new("/project/src/Service.php"),
        &root,
        &exclude_paths
    ));
}

#[test]
fn test_collect_php_files_uses_include_paths_and_excludes() {
    let tmp = std::env::temp_dir().join(format!(
        "php-lsp-include-exclude-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let src = tmp.join("src");
    let extra = tmp.join("extra");
    let generated = extra.join("generated");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&generated).unwrap();
    std::fs::write(src.join("App.php"), "<?php class App {}").unwrap();
    std::fs::write(extra.join("Helper.php"), "<?php function helper() {}").unwrap();
    std::fs::write(generated.join("Generated.php"), "<?php class Generated {}").unwrap();

    let include_paths = vec![PathBuf::from("src"), PathBuf::from("extra")];
    let exclude_paths = normalize_config_paths(vec!["extra/generated".to_string()]);
    let mut files = collect_php_files(&include_paths, &tmp, &exclude_paths);
    files.sort();

    assert_eq!(files, vec![extra.join("Helper.php"), src.join("App.php")]);

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn test_resolve_vendor_paths() {
    // Create temp dir with fake vendor/composer/installed.json
    let tmp = std::env::temp_dir().join("php-lsp-test-vendor");
    let vendor_dir = tmp.join("vendor");
    let composer_dir = vendor_dir.join("composer");
    std::fs::create_dir_all(&composer_dir).unwrap();

    let installed_json = serde_json::json!({
        "packages": [
            {
                "name": "acme/library",
                "install-path": "../acme/library",
                "autoload": {
                    "psr-4": {
                        "Acme\\Library\\": "src/"
                    }
                }
            }
        ]
    });

    std::fs::write(
        composer_dir.join("installed.json"),
        serde_json::to_string(&installed_json).unwrap(),
    )
    .unwrap();

    // Test resolving a FQN
    let paths = resolve_vendor_paths("Acme\\Library\\Http\\Client", &vendor_dir);
    assert!(paths.is_some());
    let paths = paths.unwrap();
    assert_eq!(paths.len(), 1);
    // The path should resolve to vendor/composer/../acme/library/src/Http/Client.php
    let expected_end = "src/Http/Client.php";
    assert!(
        paths[0].to_string_lossy().ends_with(expected_end),
        "Expected path to end with {}, got: {}",
        expected_end,
        paths[0].display()
    );

    // Test FQN that doesn't match any prefix
    let no_match = resolve_vendor_paths("Other\\Namespace\\Foo", &vendor_dir);
    // Should return Some(empty vec) or None — no paths match
    assert!(no_match.is_none() || no_match.unwrap().is_empty());

    // Cleanup
    let _ = std::fs::remove_dir_all(&tmp);
}
