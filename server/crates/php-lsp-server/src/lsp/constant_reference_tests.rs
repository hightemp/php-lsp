use super::*;

#[test]
fn enum_case_queries_match_constant_syntax_but_preserve_owner_name_and_kind() {
    use php_lsp_types::{PhpSymbolKind, SymbolReference, SymbolReferenceReceiver};
    let index = WorkspaceIndex::new();
    let reference = SymbolReference {
        target_fqn: "E::FLAG".into(),
        target_kind: PhpSymbolKind::ClassConstant,
        range: (0, 0, 0, 4),
        is_declaration: false,
        starts_with_dollar: false,
        allows_global_fallback: false,
        rename_range: None,
        preserve_spelling_on_rename: false,
        is_import_target: false,
        call_site: None,
        receiver: SymbolReferenceReceiver::StaticClass {
            class_fqn: "E".into(),
        },
    };
    assert!(symbol_reference_matches(
        &index,
        &reference,
        "E::FLAG",
        PhpSymbolKind::EnumCase,
        true
    ));
    for (target, kind) in [
        ("F::FLAG", PhpSymbolKind::EnumCase),
        ("E::flag", PhpSymbolKind::EnumCase),
        ("E::FLAG", PhpSymbolKind::GlobalConstant),
        ("E::FLAG", PhpSymbolKind::Method),
        ("FLAG", PhpSymbolKind::GlobalConstant),
    ] {
        assert!(
            !symbol_reference_matches(&index, &reference, target, kind, true),
            "{target}: {kind:?}"
        );
    }
}
