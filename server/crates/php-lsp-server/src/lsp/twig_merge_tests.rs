use super::*;

fn variable(name: &str, ty: &str) -> TemplateVariableType {
    TemplateVariableType {
        name: name.into(),
        type_text: ty.into(),
        shape_definitions: Vec::new(),
    }
}
fn merged(types: &[&str]) -> String {
    merge_twig_context_variables(types.iter().map(|ty| vec![variable("item", ty)]))[0]
        .type_text
        .clone()
}

#[test]
fn p235_distinct_caller_types_form_an_order_independent_union() {
    for types in [
        ["User", "Admin", "User"],
        ["Admin", "User", "User"],
        ["User", "User", "Admin"],
    ] {
        assert_eq!(merged(&types), "Admin|User");
    }
}
#[test]
fn p235_nullable_and_existing_union_branches_are_flattened_and_deduplicated() {
    for types in [
        ["?User", "Admin|User"],
        ["Admin|User", "?User"],
        ["User|Admin|null", "User"],
    ] {
        assert_eq!(merged(&types), "Admin|User|null");
    }
}
#[test]
fn p235_nested_union_tokens_remain_inside_shapes_generics_and_callables() {
    for (a, b) in [
        ("array{key: int|string}", "array{key: bool}"),
        ("array<'a|b', User|Admin>", "array<int, Admin>"),
        ("callable(int|string): bool", "callable(): User"),
        ("(A&B)", "C"),
    ] {
        let left = merged(&[a, b]);
        let right = merged(&[b, a]);
        assert_eq!(left, right, "{a}, {b}");
        let actual = twig_context_type_text_to_type_info(&left).unwrap();
        let php_lsp_types::TypeInfo::Union(parts) = actual else {
            panic!("not a union: {left}")
        };
        assert_eq!(parts.len(), 2, "damaged nested type: {left}");
        for ty in [a, b] {
            assert!(
                parts.contains(&twig_context_type_text_to_type_info(ty).unwrap()),
                "lost {ty}: {left}"
            );
        }
    }
}
#[test]
fn p235_mixed_caller_is_not_silently_narrowed_to_another_callers_type() {
    for types in [["mixed", "User"], ["User", "mixed"], ["null", "mixed"]] {
        assert_eq!(merged(&types), "mixed");
    }
}
#[test]
fn p235_broad_array_caller_is_not_replaced_by_an_unrelated_shape() {
    assert_eq!(
        merged(&["array", "array{key: string}"]),
        "array|array{key: string}"
    );
}
#[test]
fn p235_merge_keeps_all_shape_origins_with_stable_order_and_no_duplicates() {
    let mut a = variable("item", "array{a: string}");
    a.shape_definitions.push(TemplateShapeKeyDefinition {
        target: TemplateShapeDefinitionTarget::Direct,
        path: vec!["a".into()],
        uri: "file:///a.php".into(),
        range: (0, 1, 0, 2),
    });
    let mut b = variable("item", "array{b: int}");
    b.shape_definitions.push(TemplateShapeKeyDefinition {
        uri: "file:///b.php".into(),
        path: vec!["b".into()],
        ..a.shape_definitions[0].clone()
    });
    let left = merge_twig_context_variables([vec![a.clone()], vec![b.clone()], vec![a.clone()]]);
    let right = merge_twig_context_variables([vec![b], vec![a]]);
    assert_eq!(left, right);
    assert_eq!(left[0].shape_definitions.len(), 2);
}
#[test]
fn p235_mixed_merge_clears_unproven_shape_navigation() {
    let mut a = variable("item", "array{key: string}");
    a.shape_definitions.push(TemplateShapeKeyDefinition {
        target: TemplateShapeDefinitionTarget::Direct,
        path: vec!["key".into()],
        uri: "file:///a.php".into(),
        range: (0, 1, 0, 4),
    });
    let result = merge_twig_context_variables([vec![a], vec![variable("item", "mixed")]]);
    assert_eq!(result[0].type_text, "mixed");
    assert!(result[0].shape_definitions.is_empty());
}
#[test]
fn p235_render_sites_in_one_file_and_include_sites_keep_all_types() {
    let source="<?php function page() { $this->render('caller.twig', ['item'=>new User()]); $this->render('caller.twig', ['item'=>new Admin()]); }";
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let symbols = extract_file_symbols(parser.tree().unwrap(), source, "file:///controller.php");
    let result = evaluate_twig_render_calls(
        "file:///controller.php",
        source,
        &symbols,
        &twig_render_calls(source, &|| false),
        &WorkspaceIndex::new(),
        &TwigContextPhpSourceResolver {
            lookup: &|_| None,
            cancelled: &|| false,
        },
    );
    assert_eq!(result["caller.twig"][0].type_text, "Admin|User");
    let caller="{% include 'partial.twig' with {item:first} only %}{% include 'partial.twig' with {item:second} only %}";
    let result = evaluate_twig_include_calls(
        caller,
        &twig_include_calls(caller, &|| false),
        &[variable("first", "User"), variable("second", "Admin")],
        &|| false,
    );
    assert_eq!(result["partial.twig"][0].type_text, "Admin|User");
}

struct Fixture {
    root: PathBuf,
    files: Arc<DashMap<String, FileParser>>,
    templates: Arc<DashMap<String, TemplateDocument>>,
    index: Arc<WorkspaceIndex>,
    cache: Arc<Mutex<TwigContextDiskCache>>,
}
impl Fixture {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("php-lsp-p235-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(root.join("templates")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        Self {
            root,
            files: Arc::default(),
            templates: Arc::default(),
            index: Arc::new(WorkspaceIndex::new()),
            cache: Arc::default(),
        }
    }
    fn write(&self, path: &str, source: &str) {
        let path = self.root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    async fn context(&self) -> TwigContextResult {
        twig_variable_types_for_template_state(
            &path_to_uri(&self.root.join("templates/page.twig")).unwrap(),
            &self.files,
            &self.templates,
            &self.index,
            std::slice::from_ref(&self.root),
            &self.cache,
            TraversalLimits::default(),
            &[],
            None,
            None,
        )
        .await
        .unwrap()
    }
    async fn invalidate(&self, path: &str) {
        self.cache
            .lock()
            .await
            .evict_entries_for_source_uri(&path_to_uri(&self.root.join(path)).unwrap());
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn has(result: &TwigContextResult, name: &str) -> bool {
    result.variables.iter().any(|v| v.name == name)
}

#[tokio::test(flavor = "current_thread")]
async fn p235_standalone_twig_has_no_symfony_globals() {
    let fixture = Fixture::new();
    fixture.write("templates/page.twig", "{{ app }}");
    let result = fixture.context().await;
    assert!(result.variables.is_empty(), "{:?}", result.variables);
}
#[tokio::test(flavor = "current_thread")]
async fn p235_laravel_and_generic_twig_metadata_do_not_enable_symfony_globals() {
    for metadata in [
        r#"{"require":{"laravel/framework":"*","twig/twig":"*","symfony/http-foundation":"*"}}"#,
        r#"{"suggest":{"symfony/twig-bundle":"*"}}"#,
        r#"{"require":{"symfony/twig-bundle":false}}"#,
        "broken json",
    ] {
        let fixture = Fixture::new();
        fixture.write("composer.json", metadata);
        fixture.write("symfony.lock", "{}");
        fixture.write("bin/console", "marker");
        assert!(fixture.context().await.variables.is_empty(), "{metadata}");
    }
}
#[tokio::test(flavor = "current_thread")]
async fn p235_confirmed_symfony_metadata_keeps_fallbacks_and_warm_cache() {
    for (path, source) in [
        (
            "composer.json",
            r#"{"require":{"symfony/twig-bundle":"^7"}}"#,
        ),
        (
            "composer.json",
            r#"{"require-dev":{"symfony/twig-bridge":"^7"}}"#,
        ),
        (
            "composer.lock",
            r#"{"packages":[{"name":"symfony/twig-bundle","version":"v7.0"}]}"#,
        ),
        (
            "vendor/composer/installed.json",
            r#"{"packages":[{"name":"symfony/twig-bridge","version":"v7.0"}]}"#,
        ),
        (
            "vendor/composer/installed.json",
            r#"[{"name":"symfony/twig-bundle","version":"v7.0"}]"#,
        ),
    ] {
        let fixture = Fixture::new();
        fixture.write(path, source);
        let result = fixture.context().await;
        for name in ["app", "error", "errors"] {
            assert!(has(&result, name), "{path} {name}");
        }
        let cold = source_work(&fixture.root);
        fixture.context().await;
        assert_eq!(source_work(&fixture.root), cold);
    }
}
#[tokio::test(flavor = "current_thread")]
async fn p235_explicit_caller_globals_win_even_when_unknown_or_null() {
    let fixture = Fixture::new();
    fixture.write(
        "composer.json",
        r#"{"require":{"symfony/twig-bundle":"*"}}"#,
    );
    fixture.write("src/Controller.php","<?php function page(string $message) { $this->render('page.twig', ['app'=>null, 'error'=>$message, 'errors'=>$unknown]); }");
    let result = fixture.context().await;
    for (name, ty) in [("app", "null"), ("error", "string"), ("errors", "mixed")] {
        assert_eq!(
            result
                .variables
                .iter()
                .find(|v| v.name == name)
                .unwrap()
                .type_text,
            ty,
            "{name}"
        );
    }
}
#[tokio::test(flavor = "current_thread")]
async fn p235_metadata_changes_replace_cached_eligibility_and_reject_old_views() {
    let fixture = Fixture::new();
    let before = fixture.context().await;
    assert!(!has(&before, "app"));
    fixture.write(
        "composer.json",
        r#"{"require":{"symfony/twig-bundle":"*"}}"#,
    );
    fixture.invalidate("composer.json").await;
    let enabled = fixture.context().await;
    assert!(has(&enabled, "app"));
    assert!(before.context.commit_if_current(|| true).is_none());
    fixture.write("composer.json", "{}");
    fixture.invalidate("composer.json").await;
    assert!(!has(&fixture.context().await, "app"));
    assert!(enabled.context.commit_if_current(|| true).is_none());
}

#[test]
fn p235_nested_intersection_grouping_survives_caller_merge() {
    for ty in [
        "((A|B)&C)",
        "(?(A|B)&C)",
        "array{key: (A|B)&C}",
        "callable((A|B)&C): (D|E)&F",
    ] {
        let text = merged(&[ty, "Other"]);
        let expected = twig_context_type_text_to_type_info(ty).unwrap();
        let actual = twig_context_type_text_to_type_info(&text).unwrap();
        let php_lsp_types::TypeInfo::Union(parts) = actual else {
            panic!("{text}")
        };
        assert!(parts.contains(&expected), "lost grouping {ty}: {text}");
    }
}
#[test]
fn p235_duplicate_render_keys_replace_within_each_site_before_caller_union() {
    let source="<?php function page() { $this->render('page.twig', ['item'=>new First(), 'item'=>new Second()]); $this->render('page.twig', ['item'=>new Third()]); }";
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let symbols = extract_file_symbols(parser.tree().unwrap(), source, "file:///controller.php");
    let result = evaluate_twig_render_calls(
        "file:///controller.php",
        source,
        &symbols,
        &twig_render_calls(source, &|| false),
        &WorkspaceIndex::new(),
        &TwigContextPhpSourceResolver {
            lookup: &|_| None,
            cancelled: &|| false,
        },
    );
    assert_eq!(result["page.twig"][0].type_text, "Second|Third");
}

#[tokio::test(flavor = "current_thread")]
async fn p235_unowned_template_does_not_borrow_the_first_workspace_context() {
    let symfony = Fixture::new();
    let plain = Fixture::new();
    symfony.write(
        "composer.json",
        r#"{"require":{"symfony/twig-bundle":"*"}}"#,
    );
    let unowned = Fixture::new();
    let uri = path_to_uri(&unowned.root.join("templates/page.twig")).unwrap();
    for roots in [
        [symfony.root.clone(), plain.root.clone()],
        [plain.root.clone(), symfony.root.clone()],
    ] {
        let context = twig_variable_types_for_template_state(
            &uri,
            &symfony.files,
            &symfony.templates,
            &symfony.index,
            &roots,
            &symfony.cache,
            TraversalLimits::default(),
            &[],
            None,
            None,
        )
        .await;
        assert!(
            context.is_none(),
            "unowned template inherited an arbitrary root"
        );
    }
}

#[test]
fn p235_many_callers_parse_only_incoming_type_bytes_not_growing_unions() {
    let names = (0..2000)
        .map(|number| format!("Class{number:04}"))
        .collect::<Vec<_>>();
    let expected = names.join("|");
    let input_bytes = names.iter().map(String::len).sum::<usize>();
    for reverse in [false, true] {
        let mut names = names.clone();
        if reverse {
            names.reverse();
        }
        twig_context_merge::PARSED_TYPE_BYTES.with(|count| count.set(0));
        let result =
            merge_twig_context_variables(names.iter().map(|name| vec![variable("item", name)]));
        let parsed = twig_context_merge::PARSED_TYPE_BYTES.with(|count| count.get());
        assert_eq!(result[0].type_text, expected);
        assert!(
            parsed <= input_bytes * 2,
            "reparsed growing unions: {parsed} bytes for {input_bytes} input bytes"
        );
    }
}
