use super::*;

#[test]
fn test_type_info_display() {
    assert_eq!(TypeInfo::Simple("string".into()).to_string(), "string");
    assert_eq!(TypeInfo::Void.to_string(), "void");
    assert_eq!(
        TypeInfo::Union(vec![
            TypeInfo::Simple("string".into()),
            TypeInfo::Simple("int".into()),
        ])
        .to_string(),
        "string|int"
    );
    assert_eq!(
        TypeInfo::Nullable(Box::new(TypeInfo::Simple("Foo".into()))).to_string(),
        "?Foo"
    );
    assert_eq!(
        TypeInfo::Generic {
            base: "array".into(),
            args: vec![
                TypeInfo::Simple("int".into()),
                TypeInfo::Simple("User".into())
            ],
        }
        .to_string(),
        "array<int, User>"
    );
    assert_eq!(
        TypeInfo::ClassString(Some(Box::new(TypeInfo::Simple("User".into())))).to_string(),
        "class-string<User>"
    );
    assert_eq!(
        TypeInfo::Conditional {
            subject: "$class".into(),
            target: Box::new(TypeInfo::ClassString(Some(Box::new(TypeInfo::Simple(
                "T".into()
            ))))),
            if_type: Box::new(TypeInfo::Simple("T".into())),
            else_type: Box::new(TypeInfo::Simple("object".into())),
        }
        .to_string(),
        "($class is class-string<T> ? T : object)"
    );
    assert_eq!(
        TypeInfo::Callable {
            params: vec![TypeInfo::Simple("A".into())],
            return_type: Some(Box::new(TypeInfo::Simple("B".into()))),
        }
        .to_string(),
        "callable(A): B"
    );
}

#[test]
fn test_symbol_kind_to_lsp() {
    assert_eq!(
        PhpSymbolKind::Class.to_lsp_symbol_kind(),
        lsp_types::SymbolKind::CLASS
    );
    assert_eq!(
        PhpSymbolKind::Function.to_lsp_symbol_kind(),
        lsp_types::SymbolKind::FUNCTION
    );
}

#[test]
fn test_global_constant_fqn_namespace_is_case_insensitive_only() {
    assert!(symbol_fqn_eq(
        r"Vendor\Package\FLAG",
        r"vendor\package\FLAG",
        PhpSymbolKind::GlobalConstant,
    ));
    assert!(!symbol_fqn_eq(
        r"Vendor\Package\FLAG",
        r"vendor\package\flag",
        PhpSymbolKind::GlobalConstant,
    ));
    assert_eq!(
        global_constant_fqn_key(r"\Vendor\Package\FLAG"),
        r"vendor\package\FLAG"
    );
}

#[test]
fn test_member_fqn_casing_follows_php_lookup_rules() {
    assert!(symbol_fqn_eq(
        r"App\Service::doWork",
        r"app\service::DOWORK",
        PhpSymbolKind::Method,
    ));
    assert!(symbol_fqn_eq(
        r"App\Service::$value",
        r"app\service::$value",
        PhpSymbolKind::Property,
    ));
    assert!(!symbol_fqn_eq(
        r"App\Service::$value",
        r"app\service::$VALUE",
        PhpSymbolKind::Property,
    ));
}

#[test]
fn final_namespace_scope_includes_cursor_at_file_end_only() {
    let file_symbols = FileSymbols {
        namespace: Some("First".to_string()),
        namespace_scopes: vec![
            NamespaceScope {
                namespace: Some("First".to_string()),
                range: (0, 0, 3, 0),
            },
            NamespaceScope {
                namespace: Some("Second".to_string()),
                range: (3, 0, 6, 3),
            },
        ],
        use_statements: vec![
            UseStatement {
                fqn: r"Vendor\One".to_string(),
                alias: Some("Shared".to_string()),
                kind: UseKind::Class,
                namespace: Some("First".to_string()),
                range: (1, 0, 1, 22),
            },
            UseStatement {
                fqn: r"Vendor\Two".to_string(),
                alias: Some("Shared".to_string()),
                kind: UseKind::Class,
                namespace: Some("Second".to_string()),
                range: (4, 0, 4, 22),
            },
        ],
        ..FileSymbols::default()
    };

    let boundary = file_symbols
        .namespace_scope_at_byte_position(3, 0)
        .expect("adjacent boundary belongs to the following scope");
    assert_eq!(boundary.namespace.as_deref(), Some("Second"));

    let scoped = file_symbols.scoped_at_byte_position(6, 3);
    assert_eq!(scoped.namespace.as_deref(), Some("Second"));
    assert_eq!(scoped.use_statements.len(), 1);
    assert_eq!(scoped.use_statements[0].fqn, r"Vendor\Two");
}

#[test]
fn scoped_alias_lookup_does_not_expose_other_sections_outside_a_namespace() {
    let first = NamespaceScope {
        namespace: Some("App".to_string()),
        range: (1, 0, 3, 1),
    };
    let second = NamespaceScope {
        namespace: Some("App".to_string()),
        range: (5, 0, 7, 1),
    };
    let file_symbols = FileSymbols {
        namespace_scopes: vec![first.clone(), second.clone()],
        type_aliases: vec![
            PhpDocTypeAlias {
                name: "Shared".to_string(),
                type_info: TypeInfo::Simple("First".to_string()),
                scope: Some(first),
            },
            PhpDocTypeAlias {
                name: "Shared".to_string(),
                type_info: TypeInfo::Simple("Second".to_string()),
                scope: Some(second),
            },
        ],
        ..Default::default()
    };
    let first_scope = file_symbols.scoped_at_byte_position(2, 0);
    assert_eq!(first_scope.type_aliases.len(), 1);
    assert_eq!(
        first_scope.type_aliases[0].type_info,
        TypeInfo::Simple("First".into())
    );
    let second_scope = file_symbols.scoped_at_byte_position(6, 0);
    assert_eq!(second_scope.type_aliases.len(), 1);
    assert_eq!(
        second_scope.type_aliases[0].type_info,
        TypeInfo::Simple("Second".into())
    );
    let between = file_symbols.scoped_at_byte_position(4, 0);
    assert!(
        between.type_aliases.is_empty(),
        "aliases leaked outside sections: {between:?}"
    );
}
