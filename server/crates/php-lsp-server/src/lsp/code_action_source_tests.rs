use super::*;

const URI: &str = "file:///p240.php";
fn parse(source: &str) -> (FileParser, php_lsp_types::FileSymbols) {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    assert!(
        !parser.tree().unwrap().root_node().has_error(),
        "invalid fixture: {source}"
    );
    let symbols = extract_file_symbols(parser.tree().unwrap(), source, URI);
    (parser, symbols)
}
fn apply(source: &str, edit: WorkspaceEdit) -> String {
    let mut text = source.to_string();
    let mut edits = edit
        .changes
        .unwrap()
        .remove(&URI.parse().unwrap())
        .unwrap_or_default();
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.range.start));
    let mut previous = source.len();
    for edit in edits {
        let start = lsp_position_to_byte(source, edit.range.start).unwrap();
        let end = lsp_position_to_byte(source, edit.range.end).unwrap();
        assert!(start <= end && end <= previous, "overlapping edits");
        previous = start;
        text.replace_range(start..end, &edit.new_text);
    }
    parse(&text);
    text
}
fn diagnostic(source: &str, needle: &str, analyzer: &str) -> Diagnostic {
    let start = source.find(needle).unwrap();
    Diagnostic {
        range: lsp_range_for_byte_offsets(source, start, start + needle.len()),
        source: Some(analyzer.into()),
        code: Some(NumberOrString::String("InvalidArgument".into())),
        message: "fixture".into(),
        ..Default::default()
    }
}

#[test]
fn p240_visibility_changes_only_the_declaration_modifier() {
    for (source, expected) in [
        (
            "<?php class Box { function run(){ echo 'public'; } }",
            "<?php class Box { private function run(){ echo 'public'; } }",
        ),
        (
            "<?php class Box { protected function run(){ echo 'public'; } }",
            "<?php class Box { private function run(){ echo 'public'; } }",
        ),
        (
            "<?php class Box { #[Mark('public')] function run(){} }",
            "<?php class Box { #[Mark('public')] private function run(){} }",
        ),
        (
            "<?php class Box { var $value = 'public'; }",
            "<?php class Box { private $value = 'public'; }",
        ),
    ] {
        let (_, symbols) = parse(source);
        let symbol = symbols
            .symbols
            .iter()
            .find(|s| {
                matches!(
                    s.kind,
                    php_lsp_types::PhpSymbolKind::Method | php_lsp_types::PhpSymbolKind::Property
                )
            })
            .unwrap();
        let edit = change_visibility_edit(
            URI.parse().unwrap(),
            &WorkspaceIndex::new(),
            &symbols,
            source,
            symbol,
            php_lsp_types::Visibility::Private,
        )
        .unwrap();
        assert_eq!(apply(source, edit), expected);
    }
}

#[test]
fn p240_remove_import_preserves_other_inline_statements() {
    for (source, expected) in [
        ("<?php use Vendor\\Unused; echo 1;", "<?php  echo 1;"),
        (
            "<?php\r\nuse Vendor\\Unused; // keep 😀\r\necho 1;\r\n",
            "<?php\r\n // keep 😀\r\necho 1;\r\n",
        ),
    ] {
        let (_, symbols) = parse(source);
        let range = range_from_byte_range(source, symbols.use_statements[0].range);
        let edit = remove_unused_import_edit(URI.parse().unwrap(), source, range).unwrap();
        assert_eq!(apply(source, edit), expected);
    }
}

#[test]
fn p240_remove_import_selects_one_group_or_comma_clause() {
    for source in [
        "<?php use Vendor\\{KeepA, Unused, KeepB}; new KeepA; new KeepB;",
        "<?php use Vendor\\KeepA, Vendor\\Unused, Vendor\\KeepB; new KeepA; new KeepB;",
        "<?php use Vendor\\{function keepA, function unused, const KEEP_B}; keepA(); echo KEEP_B;",
    ] {
        let (_, symbols) = parse(source);
        let import = symbols
            .use_statements
            .iter()
            .find(|s| s.fqn.to_ascii_lowercase().ends_with("unused"))
            .unwrap();
        let edit = remove_unused_import_edit(
            URI.parse().unwrap(),
            source,
            range_from_byte_range(source, import.range),
        )
        .unwrap();
        let edited = apply(source, edit);
        let (_, after) = parse(&edited);
        let remaining = after
            .use_statements
            .iter()
            .map(|s| s.fqn.as_str())
            .collect::<Vec<_>>();
        assert_eq!(remaining.len(), 2, "lost neighboring imports: {edited}");
        assert!(!edited.to_ascii_lowercase().contains("unused"));
        assert!(
            edited.ends_with(source.split_once(';').unwrap().1),
            "changed following statements: {edited}"
        );
    }
}

#[test]
fn p240_psalm_ignore_is_outside_multiline_literal_and_attached_to_statement() {
    let source = "<?php\n$value = \"first\nproblem\nlast\";\necho $value;\n";
    let diagnostic = diagnostic(source, "problem", "psalm");
    let CodeActionOrCommand::CodeAction(action) = build_ignore_external_analyzer_action(
        URI.parse().unwrap(),
        source,
        &diagnostic,
        ExternalAnalyzer::Psalm,
    )
    .unwrap() else {
        panic!()
    };
    let edited = apply(source, action.edit.unwrap());
    assert!(
        edited.contains("\"first\nproblem\nlast\""),
        "literal changed: {edited}"
    );
    assert!(
        edited.starts_with("<?php\n/** @psalm-suppress InvalidArgument */\n$value"),
        "wrong anchor: {edited}"
    );
}

#[test]
fn p240_phpstan_ignore_does_not_claim_to_suppress_an_interior_literal_line() {
    let source = "<?php\n$value = \"first\nproblem\nlast\";\n";
    let diagnostic = diagnostic(source, "problem", "phpstan");
    assert!(build_ignore_external_analyzer_action(
        URI.parse().unwrap(),
        source,
        &diagnostic,
        ExternalAnalyzer::PhpStan
    )
    .is_none());
}

#[test]
fn p240_organize_imports_does_not_delete_a_following_statement_on_the_same_line() {
    let source = "<?php\nuse Vendor\\Unused; echo 'keep';\n";
    let (parser, symbols) = parse(source);
    if let Some(edit) = build_organize_imports_edit(
        URI.parse().unwrap(),
        source,
        parser.tree().unwrap(),
        &symbols,
    ) {
        assert!(apply(source, edit).contains("echo 'keep';"));
    }
}

#[test]
fn p240_psalm_ignore_preserves_existing_docblock_contract() {
    let source = "<?php\n/** @var string $value */\n$value = missing();\n";
    let diagnostic = diagnostic(source, "missing", "psalm");
    let CodeActionOrCommand::CodeAction(action) = build_ignore_external_analyzer_action(
        URI.parse().unwrap(),
        source,
        &diagnostic,
        ExternalAnalyzer::Psalm,
    )
    .unwrap() else {
        panic!()
    };
    let edited = apply(source, action.edit.unwrap());
    assert_eq!(
        edited.matches("/**").count(),
        1,
        "detached original PHPDoc: {edited}"
    );
    assert!(edited.contains("@var string $value"));
    assert!(edited.contains("@psalm-suppress InvalidArgument"));
}

#[test]
fn p240_stale_unused_import_diagnostic_cannot_remove_a_now_used_binding() {
    let source = "<?php use Vendor\\Thing; new Thing();";
    let (parser, file) = parse(source);
    let mut diagnostic = diagnostic(source, "Vendor\\Thing", "php-lsp");
    diagnostic.range = range_from_byte_range(source, file.use_statements[0].range);
    diagnostic.code = Some(NumberOrString::String("php-lsp.unusedImport".into()));
    diagnostic.message = "Unused import: Vendor\\Thing".into();
    let references = collect_symbol_references_in_file(parser.tree().unwrap(), source, &file);
    assert!(build_remove_unused_import_action(
        URI.parse().unwrap(),
        source,
        parser.tree().unwrap(),
        &file,
        (
            &references,
            &action_phpdoc::source_docs(parser.tree().unwrap(), source)
        ),
        &diagnostic,
        false
    )
    .is_none());
}

fn unused_action(source: &str) -> Option<CodeActionOrCommand> {
    let (parser, file) = parse(source);
    let import = &file.use_statements[0];
    let mut diagnostic = diagnostic(source, &import.fqn, "php-lsp");
    diagnostic.range = range_from_byte_range(source, import.range);
    diagnostic.code = Some(NumberOrString::String("php-lsp.unusedImport".into()));
    diagnostic.message = format!("Unused import: {}", import.fqn);
    let refs = collect_symbol_references_in_file(parser.tree().unwrap(), source, &file);
    build_remove_unused_import_action(
        URI.parse().unwrap(),
        source,
        parser.tree().unwrap(),
        &file,
        (
            &refs,
            &action_phpdoc::source_docs(parser.tree().unwrap(), source),
        ),
        &diagnostic,
        false,
    )
}

#[test]
fn p240_namespace_import_is_used_by_qualified_function_and_constant() {
    for usage in ["U\\run();", "echo U\\FLAG;"] {
        let source = format!("<?php\nuse Vendor\\Thing as U;\n{usage}\n");
        assert!(
            unused_action(&source).is_none(),
            "used namespace import: {source}"
        );
        let (parser, file) = parse(&source);
        if let Some(edit) = build_organize_imports_edit(
            URI.parse().unwrap(),
            &source,
            parser.tree().unwrap(),
            &file,
        ) {
            assert!(apply(&source, edit).contains("use Vendor\\Thing as U;"));
        }
    }
}

#[test]
fn p240_phpdoc_subject_and_class_constant_use_the_imported_type() {
    for doc in [
        "/** @return (U is int ? string : bool) */",
        "/** @return U::VALUE */",
    ] {
        let source = format!("<?php\nuse Vendor\\Thing as U;\n{doc}\nfunction run(){{}}\n");
        assert!(
            unused_action(&source).is_none(),
            "used PHPDoc import: {source}"
        );
    }
}

#[test]
fn p240_local_and_prefixed_phpdoc_uses_are_distinct_from_prose_or_strings() {
    for usage in [
        "/** @phpstan-return U */ function run(){}",
        "function run(){ /** @var U $value */ $value = null; }",
    ] {
        let source = format!("<?php\nuse Vendor\\Thing as U;\n{usage}\n");
        assert!(
            unused_action(&source).is_none(),
            "used PHPDoc import: {source}"
        );
    }
    for usage in [
        "/** U appears in prose. */ function run(){}",
        "$text = '/** @var U */';",
    ] {
        let source = format!("<?php\nuse Vendor\\Thing as U;\n{usage}\n");
        assert!(
            unused_action(&source).is_some(),
            "inert text counted as use: {source}"
        );
    }
}

#[test]
fn p240_visibility_supports_uppercase_modifiers_and_implicit_constants() {
    for (source, needle, expected) in [
        (
            "<?php class Box {PUBLIC function run() {echo 'public';}}",
            "run",
            "<?php class Box {private function run() {echo 'public';}}",
        ),
        (
            "<?php class Box {const VALUE = 'public';}",
            "VALUE",
            "<?php class Box {private const VALUE = 'public';}",
        ),
    ] {
        let (_, file) = parse(source);
        let symbol = file.symbols.iter().find(|s| s.name == needle).unwrap();
        let edit = change_visibility_edit(
            URI.parse().unwrap(),
            &WorkspaceIndex::new(),
            &file,
            source,
            symbol,
            php_lsp_types::Visibility::Private,
        )
        .expect("valid declaration");
        assert_eq!(apply(source, edit), expected);
    }
}

#[test]
fn p240_import_clause_edges_preserve_comments_unicode_and_crlf() {
    for (source, expected_count) in [
        (
            "<?php /* 😀 */ use V\\{Unused, /* keep */ Kept}; new Kept;",
            1,
        ),
        ("<?php\r\nuse V\\{Kept, /* keep */ Unused};\r\nnew Kept;", 1),
        ("<?php use V\\{Unused}; echo '😀';", 0),
    ] {
        let (_, file) = parse(source);
        let import = file
            .use_statements
            .iter()
            .find(|i| i.fqn.ends_with("Unused"))
            .unwrap();
        let edit = remove_unused_import_edit(
            URI.parse().unwrap(),
            source,
            range_from_byte_range(source, import.range),
        )
        .unwrap();
        let edited = apply(source, edit);
        let (_, file) = parse(&edited);
        assert_eq!(file.use_statements.len(), expected_count, "{edited}");
        if source.contains("/* keep */") {
            assert!(edited.contains("/* keep */"));
        }
        if source.contains('\r') {
            assert_eq!(
                source.matches("\r\n").count(),
                edited.matches("\r\n").count()
            );
        }
        assert!(edited.ends_with(if expected_count == 0 {
            "echo '😀';"
        } else {
            "new Kept;"
        }));
    }
}

#[test]
fn p240_analyzer_markers_preserve_inline_php_and_reject_comment_injection() {
    let source = "<?php /* 😀 */ fail();\r\n";
    for analyzer in [ExternalAnalyzer::Psalm, ExternalAnalyzer::PhpStan] {
        let mut diagnostic = diagnostic(source, "fail", "psalm");
        let edit = structural_source::analyzer_ignore_edit(
            URI.parse().unwrap(),
            source,
            &diagnostic,
            analyzer,
        )
        .unwrap();
        let edited = apply(source, edit);
        assert!(edited.starts_with("<?php /* 😀 */ \r\n"), "{edited}");
        assert!(edited.ends_with("fail();\r\n"), "{edited}");
        if analyzer == ExternalAnalyzer::Psalm {
            diagnostic.code = Some(NumberOrString::String("Oops */ die(); /*".into()));
            assert!(structural_source::analyzer_ignore_edit(
                URI.parse().unwrap(),
                source,
                &diagnostic,
                analyzer
            )
            .is_none());
        }
    }
}

#[test]
fn line_model_refactor_helpers_preserve_cr_attributes_and_member_indentation() {
    for eol in ["\n", "\r\n", "\r"] {
        let source = format!("<?php{eol}    class Box {{{eol}        #[Mark]{eol}        public int $value;{eol}        function run() {{return 42;}}{eol}    }}");
        let (parser, file) = parse(&source);
        let declaration = source.find("public").unwrap();
        assert_eq!(
            adjacent_attribute_start(&source, declaration),
            Some(source.find("        #[").unwrap()),
            "{eol:?}"
        );
        let class = file.symbols.iter().find(|s| s.name == "Box").unwrap();
        let insertion = class_constant_insertion(&source, class).unwrap();
        assert_eq!(insertion.position, Position::new(2, 0), "{eol:?}");
        assert_eq!(insertion.member_indent, "        ");
        assert!(!insertion.needs_leading_newline);
        let start = source.find("42").unwrap();
        let a = parser.line_index().offset_to_point(start);
        let b = parser.line_index().offset_to_point(start + 2);
        let edit = extract_constant_edit(
            URI.parse().unwrap(),
            parser.tree().unwrap(),
            &source,
            &file,
            (a.0, a.1, b.0, b.1),
            "VALUE",
        )
        .unwrap();
        let edited = apply(&source, edit);
        assert!(
            edited.contains("        private const VALUE = 42;"),
            "{edited:?}"
        );
        assert!(
            edited.contains(&format!("        #[Mark]{eol}        public int $value;")),
            "{edited:?}"
        );
        assert!(edited.contains("return self::VALUE;"));
    }
}

#[test]
fn line_model_external_ignore_actions_reject_unadapted_bare_cr_ranges() {
    for eol in ["\n", "\r\n", "\r"] {
        let source = format!("<?php{eol}/* a\u{2028}b */ fail();{eol}");
        for analyzer in [ExternalAnalyzer::Psalm, ExternalAnalyzer::PhpStan] {
            let diagnostic = diagnostic(&source, "fail", "psalm");
            let edit = structural_source::analyzer_ignore_edit(
                URI.parse().unwrap(),
                &source,
                &diagnostic,
                analyzer,
            );
            assert_eq!(edit.is_some(), eol != "\r", "{eol:?}: {analyzer:?}");
            if let Some(edit) = edit {
                assert!(apply(&source, edit).contains("fail();"));
            }
        }
    }
}

#[test]
fn line_model_framework_string_completion_stops_at_each_line_ending() {
    for eol in ["\n", "\r\n", "\r"] {
        let source = format!("unfinished{eol} next'");
        assert_eq!(
            crate::server::lsp::completion_helpers::find_unescaped_quote(&source, 0, '\''),
            None,
            "{eol:?}"
        );
    }
}
