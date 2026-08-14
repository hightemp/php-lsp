use super::*;

#[test]
fn test_parse_summary() {
    let doc = parse_phpdoc("/** This is a summary. */");
    assert_eq!(doc.summary.as_deref(), Some("This is a summary."));
}

#[test]
fn test_parse_multiline_summary() {
    let doc = parse_phpdoc("/**\n * First line.\n * Second line.\n */");
    assert_eq!(doc.summary.as_deref(), Some("First line. Second line."));
}

#[test]
fn test_parse_param() {
    let doc = parse_phpdoc("/**\n * @param string $name The name\n * @param int $age\n */");
    assert_eq!(doc.params.len(), 2);
    assert_eq!(doc.params[0].name, "name");
    assert_eq!(
        doc.params[0].type_info,
        Some(TypeInfo::Simple("string".to_string()))
    );
    assert_eq!(doc.params[0].description.as_deref(), Some("The name"));
    assert_eq!(doc.params[1].name, "age");
}

#[test]
fn test_parse_return() {
    let doc = parse_phpdoc("/**\n * @return string|null\n */");
    assert!(matches!(doc.return_type, Some(TypeInfo::Union(_))));
}

#[test]
fn test_parse_conditional_return_type() {
    let doc = parse_phpdoc("/**\n * @return ($class is class-string<T> ? T : object)\n */");
    let Some(TypeInfo::Conditional {
        subject,
        target,
        if_type,
        else_type,
    }) = doc.return_type
    else {
        panic!("expected conditional return type");
    };
    assert_eq!(subject, "$class");
    assert_eq!(
        *target,
        TypeInfo::ClassString(Some(Box::new(TypeInfo::Simple("T".to_string()))))
    );
    assert_eq!(*if_type, TypeInfo::Simple("T".to_string()));
    assert_eq!(*else_type, TypeInfo::Simple("object".to_string()));
}

#[test]
fn test_parse_var() {
    let doc = parse_phpdoc("/** @var int */");
    assert_eq!(doc.var_type, Some(TypeInfo::Simple("int".to_string())));
}

#[test]
fn test_parse_throws() {
    let doc = parse_phpdoc(
        "/**\n * @throws \\RuntimeException\n * @throws \\InvalidArgumentException\n */",
    );
    assert_eq!(doc.throws.len(), 2);
}

#[test]
fn test_parse_deprecated() {
    let doc = parse_phpdoc("/**\n * @deprecated Use newMethod() instead\n */");
    assert_eq!(doc.deprecated.as_deref(), Some("Use newMethod() instead"));
}

#[test]
fn test_parse_deprecated_no_message() {
    let doc = parse_phpdoc("/**\n * @deprecated\n */");
    assert_eq!(doc.deprecated.as_deref(), Some("Deprecated"));
}

#[test]
fn test_parse_property() {
    let doc =
        parse_phpdoc("/**\n * @property string $name The name\n * @property-read int $id\n */");
    assert_eq!(doc.properties.len(), 2);
    assert_eq!(doc.properties[0].name, "name");
    assert_eq!(doc.properties[0].access, PhpDocPropertyAccess::ReadWrite);
    assert_eq!(doc.properties[1].name, "id");
    assert_eq!(doc.properties[1].access, PhpDocPropertyAccess::ReadOnly);
}

#[test]
fn test_parse_property_access_modes() {
    let doc = parse_phpdoc(
            "/**\n * @property string $name\n * @property-read int $id\n * @property-write bool $enabled\n */",
        );
    assert_eq!(doc.properties.len(), 3);
    assert_eq!(doc.properties[0].access, PhpDocPropertyAccess::ReadWrite);
    assert!(doc.properties[0].access.is_readable());
    assert!(doc.properties[0].access.is_writable());
    assert_eq!(doc.properties[1].access, PhpDocPropertyAccess::ReadOnly);
    assert!(doc.properties[1].access.is_readable());
    assert!(!doc.properties[1].access.is_writable());
    assert_eq!(doc.properties[2].access, PhpDocPropertyAccess::WriteOnly);
    assert!(!doc.properties[2].access.is_readable());
    assert!(doc.properties[2].access.is_writable());
}

#[test]
fn test_parse_method() {
    let doc = parse_phpdoc("/**\n * @method string getName()\n * @method static Foo create()\n */");
    assert_eq!(doc.methods.len(), 2);
    assert_eq!(doc.methods[0].name, "getName");
    assert!(!doc.methods[0].is_static);
    assert_eq!(doc.methods[1].name, "create");
    assert!(doc.methods[1].is_static);
}

#[test]
fn test_similar_tags_do_not_parse_as_base_tags() {
    let doc = parse_phpdoc(
            "/**\n * @param-out string $name\n * @returnFoo int\n * @var-something User $user\n * @methodFoo string bad()\n */",
        );

    assert!(doc.params.is_empty());
    assert!(doc.return_type.is_none());
    assert!(doc.var_type.is_none());
    assert!(doc.methods.is_empty());
}

#[test]
fn test_parse_method_params_flags_defaults_and_description() {
    let doc = parse_phpdoc(
            "/**\n * @method static Foo create(string &$name, ?int ...$ids, [bool $active], array<string, int> $map = []) Build it\n */",
        );

    assert_eq!(doc.methods.len(), 1);
    let method = &doc.methods[0];
    assert_eq!(method.name, "create");
    assert!(method.is_static);
    assert_eq!(
        method.return_type,
        Some(TypeInfo::Simple("Foo".to_string()))
    );
    assert_eq!(method.description.as_deref(), Some("Build it"));
    assert_eq!(method.params.len(), 4);

    assert_eq!(method.params[0].name, "name");
    assert!(method.params[0].is_by_ref);
    assert!(!method.params[0].is_variadic);
    assert_eq!(
        method.params[0].type_info,
        Some(TypeInfo::Simple("string".to_string()))
    );

    assert_eq!(method.params[1].name, "ids");
    assert!(method.params[1].is_variadic);
    assert_eq!(
        method.params[1].type_info,
        Some(TypeInfo::Nullable(Box::new(TypeInfo::Simple(
            "int".to_string()
        ))))
    );

    assert_eq!(method.params[2].name, "active");
    assert_eq!(method.params[2].default_value.as_deref(), Some("null"));
    assert_eq!(
        method.params[2].type_info,
        Some(TypeInfo::Simple("bool".to_string()))
    );

    assert_eq!(method.params[3].name, "map");
    assert_eq!(method.params[3].default_value.as_deref(), Some("[]"));
    assert_eq!(
        method.params[3].type_info.as_ref().map(ToString::to_string),
        Some("array<string, int>".to_string())
    );
}

#[test]
fn test_parse_method_ignores_parentheses_in_description() {
    let doc = parse_phpdoc(
            "/**\n * @method bool isSameYear(DateTimeInterface|string $date) Checks if the date is in the same year. If null passed, compare to now (with the same timezone).\n */",
        );

    assert_eq!(doc.methods.len(), 1);
    let method = &doc.methods[0];
    assert_eq!(method.name, "isSameYear");
    assert!(!method.is_static);
    assert_eq!(
        method.return_type,
        Some(TypeInfo::Simple("bool".to_string()))
    );
    assert_eq!(method.params.len(), 1);
    assert_eq!(method.params[0].name, "date");
}

#[test]
fn test_parse_nullable_type() {
    let doc = parse_phpdoc("/**\n * @param ?string $name\n */");
    assert!(matches!(
        doc.params[0].type_info,
        Some(TypeInfo::Nullable(_))
    ));
}

#[test]
fn test_parse_param_generic_type_with_spaces() {
    let doc = parse_phpdoc("/**\n * @param array<int, User> $users The users\n */");
    assert_eq!(doc.params.len(), 1);
    assert_eq!(doc.params[0].name, "users");
    let Some(TypeInfo::Generic { base, args }) = &doc.params[0].type_info else {
        panic!("expected generic type");
    };
    assert_eq!(base, "array");
    assert_eq!(args.len(), 2);
    assert_eq!(args[0], TypeInfo::Simple("int".to_string()));
    assert_eq!(args[1], TypeInfo::Simple("User".to_string()));
    assert_eq!(
        doc.params[0].type_info.as_ref().unwrap().to_string(),
        "array<int, User>"
    );
    assert_eq!(doc.params[0].description.as_deref(), Some("The users"));
}

#[test]
fn test_parse_phpdoc_array_suffix_type() {
    let doc = parse_phpdoc("/**\n * @param mixed[] $context\n * @return User[][]\n */");
    assert_eq!(
        doc.params[0].type_info,
        Some(TypeInfo::Generic {
            base: "array".to_string(),
            args: vec![TypeInfo::Mixed],
        })
    );
    assert_eq!(
        doc.params[0].type_info.as_ref().unwrap().to_string(),
        "array<mixed>"
    );
    assert_eq!(
        doc.return_type,
        Some(TypeInfo::Generic {
            base: "array".to_string(),
            args: vec![TypeInfo::Generic {
                base: "array".to_string(),
                args: vec![TypeInfo::Simple("User".to_string())],
            }],
        })
    );
}

#[test]
fn test_parse_nested_generic_type_with_spaces() {
    let doc =
        parse_phpdoc("/**\n * @param array<int, array<string, User>> $users Nested users\n */");
    assert!(matches!(
        doc.params[0].type_info,
        Some(TypeInfo::Generic { .. })
    ));
    assert_eq!(
        doc.params[0].type_info.as_ref().unwrap().to_string(),
        "array<int, array<string, User>>"
    );
    assert_eq!(doc.params[0].description.as_deref(), Some("Nested users"));
}

#[test]
fn test_parse_return_list_type() {
    let doc = parse_phpdoc("/**\n * @return list<User> Users\n */");
    assert_eq!(
        doc.return_type,
        Some(TypeInfo::Generic {
            base: "list".to_string(),
            args: vec![TypeInfo::Simple("User".to_string())],
        })
    );
}

#[test]
fn test_parse_var_class_string_with_variable() {
    let doc = parse_phpdoc("/** @var class-string<T> $class */");
    assert_eq!(
        doc.var_type,
        Some(TypeInfo::ClassString(Some(Box::new(TypeInfo::Simple(
            "T".to_string()
        )))))
    );
}

#[test]
fn test_parse_parenthesized_intersection_union_type() {
    let doc = parse_phpdoc("/**\n * @return (A&B)|null\n */");
    let Some(TypeInfo::Union(parts)) = doc.return_type else {
        panic!("expected union type");
    };
    assert_eq!(parts.len(), 2);
    assert!(matches!(parts[0], TypeInfo::Intersection(_)));
    assert_eq!(parts[1], TypeInfo::LiteralNull);
}

#[test]
fn test_parse_callable_return_type() {
    let doc = parse_phpdoc("/**\n * @return callable(A): B Handler\n */");
    assert_eq!(
        doc.return_type,
        Some(TypeInfo::Callable {
            params: vec![TypeInfo::Simple("A".to_string())],
            return_type: Some(Box::new(TypeInfo::Simple("B".to_string()))),
        })
    );
}

#[test]
fn test_parse_param_callable_ignores_nested_variable_token() {
    let doc = parse_phpdoc("/**\n * @param callable($value): string $callback Callback\n */");
    assert_eq!(doc.params.len(), 1);
    assert_eq!(doc.params[0].name, "callback");
    assert_eq!(
        doc.params[0].type_info,
        Some(TypeInfo::Callable {
            params: vec![TypeInfo::Simple("$value".to_string())],
            return_type: Some(Box::new(TypeInfo::Simple("string".to_string()))),
        })
    );
    assert_eq!(doc.params[0].description.as_deref(), Some("Callback"));
}

#[test]
fn test_parse_method_callable_return_type() {
    let doc = parse_phpdoc("/**\n * @method callable(A): B handle()\n */");
    assert_eq!(doc.methods.len(), 1);
    assert_eq!(doc.methods[0].name, "handle");
    assert_eq!(
        doc.methods[0].return_type,
        Some(TypeInfo::Callable {
            params: vec![TypeInfo::Simple("A".to_string())],
            return_type: Some(Box::new(TypeInfo::Simple("B".to_string()))),
        })
    );
}

#[test]
fn test_parse_array_shape_and_literal_types() {
    let doc = parse_phpdoc(
        "/**\n * @return array{'status': 'ok', \"count\"?: 1, active: true, ratio: 1.5}\n */",
    );
    let Some(TypeInfo::ArrayShape(items)) = doc.return_type else {
        panic!("expected array shape");
    };
    assert_eq!(items.len(), 4);
    assert_eq!(items[0].key.as_deref(), Some("status"));
    assert_eq!(items[0].value, TypeInfo::LiteralString("'ok'".to_string()));
    assert_eq!(items[1].key.as_deref(), Some("count"));
    assert!(items[1].optional);
    assert_eq!(items[1].value, TypeInfo::LiteralInt("1".to_string()));
    assert_eq!(items[2].value, TypeInfo::LiteralBool(true));
    assert_eq!(items[3].value, TypeInfo::LiteralFloat("1.5".to_string()));
}

#[test]
fn test_parse_numeric_literal_type_forms() {
    let accepted = [
        ("0", TypeInfo::LiteralInt("0".to_string())),
        ("123", TypeInfo::LiteralInt("123".to_string())),
        ("1_234_567", TypeInfo::LiteralInt("1_234_567".to_string())),
        ("0123", TypeInfo::LiteralInt("0123".to_string())),
        ("0_123", TypeInfo::LiteralInt("0_123".to_string())),
        ("0o7_1", TypeInfo::LiteralInt("0o7_1".to_string())),
        ("0O71", TypeInfo::LiteralInt("0O71".to_string())),
        ("0x1_A", TypeInfo::LiteralInt("0x1_A".to_string())),
        ("0X1A", TypeInfo::LiteralInt("0X1A".to_string())),
        ("0b10_10", TypeInfo::LiteralInt("0b10_10".to_string())),
        ("0B1010", TypeInfo::LiteralInt("0B1010".to_string())),
        ("-0x1A", TypeInfo::LiteralInt("-0x1A".to_string())),
        ("1.5", TypeInfo::LiteralFloat("1.5".to_string())),
        ("1.", TypeInfo::LiteralFloat("1.".to_string())),
        (".5", TypeInfo::LiteralFloat(".5".to_string())),
        ("-.5", TypeInfo::LiteralFloat("-.5".to_string())),
        ("1_234.567", TypeInfo::LiteralFloat("1_234.567".to_string())),
        ("1.2e3", TypeInfo::LiteralFloat("1.2e3".to_string())),
        ("7E-10", TypeInfo::LiteralFloat("7E-10".to_string())),
        ("1e+5", TypeInfo::LiteralFloat("1e+5".to_string())),
        ("1_2e3_4", TypeInfo::LiteralFloat("1_2e3_4".to_string())),
        ("1.e2", TypeInfo::LiteralFloat("1.e2".to_string())),
        (".5e2", TypeInfo::LiteralFloat(".5e2".to_string())),
    ];

    for (raw, expected) in accepted {
        assert_eq!(parse_literal_type(raw), Some(expected), "literal {raw}");
    }
}

#[test]
fn test_reject_malformed_numeric_literal_type_forms() {
    for raw in [
        "", "-", "+1", "+1.2", "_1", "1_", "1__2", "09", "0_9", "0x", "0x_1", "0x1_", "0b102",
        "0o128", ".", "-.", "1..2", "1_.2", "1._2", "1e", "1e+", "1e_2", "1__2e3", "1e2.3",
    ] {
        assert_eq!(parse_literal_type(raw), None, "literal {raw}");
    }
}

#[test]
fn test_parse_object_shape() {
    let doc = parse_phpdoc("/**\n * @var object{user: User, id?: int} $row\n */");
    let Some(TypeInfo::ObjectShape(items)) = doc.var_type else {
        panic!("expected object shape");
    };
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].key.as_deref(), Some("user"));
    assert_eq!(items[0].value, TypeInfo::Simple("User".to_string()));
    assert_eq!(items[1].key.as_deref(), Some("id"));
    assert!(items[1].optional);
}

#[test]
fn test_parse_template_tags_with_bounds_and_variance() {
    let doc = parse_phpdoc(
            "/**\n * @template T of Entity\n * @template-covariant TItem as object\n * @template-contravariant TConsumer\n */",
        );

    assert_eq!(doc.templates.len(), 3);
    assert_eq!(doc.templates[0].name, "T");
    assert_eq!(doc.templates[0].variance, TemplateVariance::Invariant);
    assert_eq!(
        doc.templates[0].bound,
        Some(TypeInfo::Simple("Entity".to_string()))
    );
    assert_eq!(doc.templates[1].name, "TItem");
    assert_eq!(doc.templates[1].variance, TemplateVariance::Covariant);
    assert_eq!(
        doc.templates[1].bound,
        Some(TypeInfo::Simple("object".to_string()))
    );
    assert_eq!(doc.templates[2].variance, TemplateVariance::Contravariant);
}

#[test]
fn test_parse_template_binding_tags() {
    let doc = parse_phpdoc(
            "/**\n * @extends Repository<int, User>\n * @implements IteratorAggregate<int, User>\n * @use Auditable<User>\n * @mixin Builder<User>\n */",
        );

    assert_eq!(doc.template_bindings.len(), 4);
    assert_eq!(doc.template_bindings[0].kind, TemplateBindingKind::Extends);
    assert_eq!(doc.template_bindings[0].target, "Repository");
    assert_eq!(
        doc.template_bindings[0].args,
        vec![
            TypeInfo::Simple("int".to_string()),
            TypeInfo::Simple("User".to_string())
        ]
    );
    assert_eq!(
        doc.template_bindings[1].kind,
        TemplateBindingKind::Implements
    );
    assert_eq!(doc.template_bindings[2].kind, TemplateBindingKind::Use);
    assert_eq!(doc.template_bindings[3].kind, TemplateBindingKind::Mixin);
}

#[test]
fn test_parse_type_alias_tags() {
    let doc = parse_phpdoc(
            "/**\n * @phpstan-type UserShape array{id: int, name?: string}\n * @psalm-type IdList = list<int>\n */",
        );

    assert_eq!(doc.type_aliases.len(), 2);
    assert_eq!(doc.type_aliases[0].name, "UserShape");
    let TypeInfo::ArrayShape(items) = &doc.type_aliases[0].type_info else {
        panic!("expected array shape alias");
    };
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].key.as_deref(), Some("id"));
    assert_eq!(items[0].value, TypeInfo::Simple("int".to_string()));
    assert_eq!(
        doc.type_aliases[1].type_info,
        TypeInfo::Generic {
            base: "list".to_string(),
            args: vec![TypeInfo::Simple("int".to_string())],
        }
    );
}

#[test]
fn test_parse_multiline_type_alias_shape() {
    let doc = parse_phpdoc(
            "/**\n * @phpstan-type UserShape array{\n *   'user-id': int,\n *   \"name\"?: string,\n *   meta: array{\n *     city: string,\n *   },\n * }\n */",
        );

    assert_eq!(doc.type_aliases.len(), 1);
    assert_eq!(doc.type_aliases[0].name, "UserShape");
    let TypeInfo::ArrayShape(items) = &doc.type_aliases[0].type_info else {
        panic!("expected array shape alias");
    };
    assert_eq!(items.len(), 3);
    assert_eq!(items[0].key.as_deref(), Some("user-id"));
    assert_eq!(items[0].value, TypeInfo::Simple("int".to_string()));
    assert_eq!(items[1].key.as_deref(), Some("name"));
    assert!(items[1].optional);
    let TypeInfo::ArrayShape(meta_items) = &items[2].value else {
        panic!("expected nested array shape");
    };
    assert_eq!(meta_items.len(), 1);
    assert_eq!(meta_items[0].key.as_deref(), Some("city"));
    assert_eq!(
        doc.type_aliases[0].type_info.to_string(),
        "array{'user-id': int, name?: string, meta: array{city: string}}"
    );
}

#[test]
fn test_parse_type_alias_import_tags() {
    let doc = parse_phpdoc(
            "/**\n * @phpstan-import-type UserShape from UserTypes\n * @psalm-import-type AddressShape from \\App\\Types as LocalAddress\n */",
        );

    assert_eq!(doc.type_alias_imports.len(), 2);
    assert_eq!(doc.type_alias_imports[0].name, "UserShape");
    assert_eq!(doc.type_alias_imports[0].source_alias, "UserShape");
    assert_eq!(doc.type_alias_imports[0].source_type, "UserTypes");
    assert_eq!(doc.type_alias_imports[1].name, "LocalAddress");
    assert_eq!(doc.type_alias_imports[1].source_alias, "AddressShape");
    assert_eq!(doc.type_alias_imports[1].source_type, "\\App\\Types");
}

#[test]
fn test_malformed_tags_are_ignored() {
    let doc = parse_phpdoc("/**\n * @param array<int, User>\n * @property string\n * @return\n */");
    assert!(doc.params.is_empty());
    assert!(doc.properties.is_empty());
    assert!(doc.return_type.is_none());
}

#[test]
fn test_full_phpdoc() {
    let doc = parse_phpdoc(
        r#"/**
             * Create a new user.
             *
             * @param string $name The user name
             * @param int $age The age
             * @return User
             * @throws \InvalidArgumentException
             * @deprecated Use createUser() instead
             */"#,
    );
    assert_eq!(doc.summary.as_deref(), Some("Create a new user."));
    assert_eq!(doc.params.len(), 2);
    assert_eq!(doc.return_type, Some(TypeInfo::Simple("User".to_string())));
    assert_eq!(doc.throws.len(), 1);
    assert!(doc.deprecated.is_some());
}
