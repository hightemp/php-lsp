use super::*;

#[tokio::test]
async fn test_rename_scans_open_file_before_index_commit() {
    let uri = "file:///staged-open-rename.php";
    let source = r#"<?php
namespace {
function stagedTarget(): void {}
}
namespace App {
function consume(): void {
    STAGEDTARGET();
    namespace\STAGEDTARGET();
}
}
"#;
    let (service, _socket) = tower_lsp::LspService::new(PhpLspBackend::new);
    let backend = service.inner();

    let mut parser = FileParser::new();
    parser.parse_full(source);
    backend.open_files.insert(uri.to_string(), parser);
    assert!(!backend.index.file_references.contains_key(uri));

    let edit = backend
        .lsp_rename(RenameParams {
            text_document_position: TextDocumentPositionParams::new(
                TextDocumentIdentifier::new(uri.parse().expect("staged URI")),
                Position::new(6, 8),
            ),
            new_name: "renamedTarget".to_string(),
            work_done_progress_params: WorkDoneProgressParams::default(),
        })
        .await
        .expect("rename request")
        .expect("workspace edit");
    let changes = edit.changes.expect("rename changes");
    let uri: Uri = uri.parse().expect("staged URI");
    let edits = changes.get(&uri).expect("staged open-file edits");

    assert_eq!(edits.len(), 2);
    assert!(edits.iter().all(|edit| edit.new_text == "renamedTarget"));
    let mut edited_lines: Vec<_> = edits.iter().map(|edit| edit.range.start.line).collect();
    edited_lines.sort_unstable();
    assert_eq!(edited_lines, vec![2, 6]);
}

#[test]
fn test_symbol_rename_name_validation_by_kind() {
    let cases = [
        (
            php_lsp_types::PhpSymbolKind::Class,
            "NewClass",
            Some("NewClass"),
        ),
        (
            php_lsp_types::PhpSymbolKind::Interface,
            "_Contract2",
            Some("_Contract2"),
        ),
        (
            php_lsp_types::PhpSymbolKind::Trait,
            "TraitName",
            Some("TraitName"),
        ),
        (php_lsp_types::PhpSymbolKind::Enum, "Status", Some("Status")),
        (
            php_lsp_types::PhpSymbolKind::Function,
            "calculate_2",
            Some("calculate_2"),
        ),
        (
            php_lsp_types::PhpSymbolKind::Method,
            "__invoke",
            Some("__invoke"),
        ),
        (
            php_lsp_types::PhpSymbolKind::ClassConstant,
            "MAX_SIZE",
            Some("MAX_SIZE"),
        ),
        (
            php_lsp_types::PhpSymbolKind::GlobalConstant,
            "_GLOBAL_LIMIT",
            Some("_GLOBAL_LIMIT"),
        ),
        (
            php_lsp_types::PhpSymbolKind::EnumCase,
            "Ready2",
            Some("Ready2"),
        ),
        (
            php_lsp_types::PhpSymbolKind::Property,
            "$displayName",
            Some("displayName"),
        ),
        (
            php_lsp_types::PhpSymbolKind::Property,
            "displayName",
            Some("displayName"),
        ),
    ];

    for (kind, new_name, expected) in cases {
        assert_eq!(
            normalize_symbol_new_name(kind, new_name).as_deref(),
            expected
        );
    }
}

#[test]
fn test_symbol_rename_rejects_invalid_names_by_kind() {
    let cases = [
        (php_lsp_types::PhpSymbolKind::Class, "123"),
        (php_lsp_types::PhpSymbolKind::Interface, "$Contract"),
        (php_lsp_types::PhpSymbolKind::Trait, "foo-bar"),
        (php_lsp_types::PhpSymbolKind::Enum, "enum"),
        (php_lsp_types::PhpSymbolKind::Function, "return"),
        (php_lsp_types::PhpSymbolKind::Method, "foo bar"),
        (php_lsp_types::PhpSymbolKind::ClassConstant, "MAX-VALUE"),
        (php_lsp_types::PhpSymbolKind::GlobalConstant, "$LIMIT"),
        (php_lsp_types::PhpSymbolKind::EnumCase, "case"),
        (php_lsp_types::PhpSymbolKind::Property, "123"),
    ];

    for (kind, new_name) in cases {
        assert!(
            normalize_symbol_new_name(kind, new_name).is_none(),
            "{kind:?} should reject {new_name:?}"
        );
    }
}

#[test]
fn test_variable_rename_name_validation_and_normalization() {
    assert_eq!(
        normalize_variable_new_name("localName").as_deref(),
        Some("$localName")
    );
    assert_eq!(
        normalize_variable_new_name("$localName").as_deref(),
        Some("$localName")
    );

    for new_name in ["", " local", "123", "$123", "local-name", "$this"] {
        assert!(
            normalize_variable_new_name(new_name).is_none(),
            "variable rename should reject {new_name:?}"
        );
    }
}
