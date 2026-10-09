use super::*;
fn simple(name: &str) -> TypeInfo {
    TypeInfo::Simple(name.into())
}
fn check(pattern: &TypeInfo, actual: &TypeInfo) -> PatternMatch {
    match_type_pattern(
        pattern,
        actual,
        &HashSet::new(),
        &HashMap::new(),
        &|_, _| None,
    )
}

#[test]
fn an_unbound_template_cannot_prove_its_own_condition() {
    let bindings = HashMap::new();
    assert_eq!(
        match_type_pattern(
            &simple("T"),
            &TypeInfo::LiteralInt("1".into()),
            &HashSet::from(["T".into()]),
            &bindings,
            &|_, _| None
        ),
        PatternMatch::Unknown
    );
    assert!(bindings.is_empty());
}

#[test]
fn a_condition_cannot_infer_templates_from_class_string_union_members() {
    let pattern = TypeInfo::ClassString(Some(Box::new(simple("T"))));
    let actual = TypeInfo::Union(vec![
        TypeInfo::ClassString(Some(Box::new(simple("Foo")))),
        TypeInfo::ClassString(Some(Box::new(simple("Bar")))),
    ]);
    let bindings = HashMap::new();
    assert_eq!(
        match_type_pattern(
            &pattern,
            &actual,
            &HashSet::from(["T".into()]),
            &bindings,
            &|_, _| None
        ),
        PatternMatch::Unknown
    );
    assert!(bindings.is_empty());
}

#[test]
fn nominal_generics_do_not_assume_undeclared_covariance() {
    let pattern = TypeInfo::Generic {
        base: "Box".into(),
        args: vec![simple("Base")],
    };
    let actual = TypeInfo::Generic {
        base: "Box".into(),
        args: vec![simple("Child")],
    };
    assert_eq!(
        match_type_pattern(
            &pattern,
            &actual,
            &HashSet::new(),
            &HashMap::new(),
            &|_, _| Some(true)
        ),
        PatternMatch::Unknown
    );
}

#[test]
fn builtin_array_value_types_can_use_proven_covariance() {
    let pattern = TypeInfo::Generic {
        base: "array".into(),
        args: vec![simple("Base")],
    };
    let actual = TypeInfo::Generic {
        base: "array".into(),
        args: vec![simple("Child")],
    };
    assert_eq!(
        match_type_pattern(
            &pattern,
            &actual,
            &HashSet::new(),
            &HashMap::new(),
            &|_, _| Some(true)
        ),
        PatternMatch::Yes
    );
}

#[test]
fn literals_match_native_families_without_coercing_disjoint_families() {
    for (pattern, actual) in [
        (simple("string"), TypeInfo::LiteralString("'x'".into())),
        (simple("int"), TypeInfo::LiteralInt("1".into())),
        (simple("float"), TypeInfo::LiteralFloat("1.25".into())),
        (simple("bool"), TypeInfo::LiteralBool(false)),
        (simple("integer"), TypeInfo::LiteralInt("1".into())),
    ] {
        assert_eq!(check(&pattern, &actual), PatternMatch::Yes);
    }
    assert_eq!(
        check(&simple("int"), &TypeInfo::LiteralString("'1'".into())),
        PatternMatch::No
    );
}

#[test]
fn literal_value_comparison_handles_quote_and_integer_spelling() {
    assert_eq!(
        check(
            &TypeInfo::LiteralString("'value'".into()),
            &TypeInfo::LiteralString("\"value\"".into())
        ),
        PatternMatch::Yes
    );
    assert_eq!(
        check(
            &TypeInfo::LiteralInt("0x10".into()),
            &TypeInfo::LiteralInt("16".into())
        ),
        PatternMatch::Yes
    );
    assert_eq!(
        check(
            &TypeInfo::LiteralInt("-0b10".into()),
            &TypeInfo::LiteralInt("-2".into())
        ),
        PatternMatch::Yes
    );
    assert_eq!(
        check(
            &TypeInfo::LiteralInt("16".into()),
            &TypeInfo::LiteralInt("17".into())
        ),
        PatternMatch::No
    );
}

#[test]
fn unsupported_refinements_and_missing_class_relations_do_not_select_else() {
    assert_eq!(
        check(&simple("positive-int"), &TypeInfo::LiteralInt("-1".into())),
        PatternMatch::Unknown
    );
    assert_eq!(
        check(&simple("Base"), &simple("Child")),
        PatternMatch::Unknown
    );
    assert_eq!(
        check(&simple("string"), &TypeInfo::Mixed),
        PatternMatch::Unknown
    );
}

#[test]
fn actual_union_members_must_all_prove_the_same_branch() {
    assert_eq!(
        check(
            &simple("string"),
            &TypeInfo::Union(vec![
                TypeInfo::LiteralString("'x'".into()),
                TypeInfo::LiteralString("'y'".into())
            ])
        ),
        PatternMatch::Yes
    );
    assert_eq!(
        check(
            &simple("string"),
            &TypeInfo::Union(vec![
                TypeInfo::LiteralString("'x'".into()),
                TypeInfo::LiteralInt("1".into())
            ])
        ),
        PatternMatch::Unknown
    );
}

#[test]
fn pattern_checks_do_not_publish_failed_partial_bindings() {
    let pattern = TypeInfo::Intersection(vec![simple("T"), simple("int")]);
    let bindings = HashMap::from([("Other".into(), simple("string"))]);
    let original = bindings.clone();
    assert_eq!(
        match_type_pattern(
            &pattern,
            &TypeInfo::LiteralString("'x'".into()),
            &HashSet::from(["T".into()]),
            &bindings,
            &|_, _| None
        ),
        PatternMatch::No
    );
    assert_eq!(bindings, original);
}

#[test]
fn deep_or_wide_matching_stops_with_unknown() {
    let mut deep = simple("string");
    for _ in 0..100 {
        deep = TypeInfo::Nullable(Box::new(deep));
    }
    assert_eq!(
        check(&deep, &TypeInfo::LiteralString("'x'".into())),
        PatternMatch::Unknown
    );
    let pattern = TypeInfo::Union(
        (0..200)
            .map(|i| TypeInfo::LiteralString(format!("'value{i}'")))
            .collect(),
    );
    let actual = TypeInfo::Union(
        (0..200)
            .map(|i| TypeInfo::LiteralInt(i.to_string()))
            .collect(),
    );
    assert_eq!(check(&pattern, &actual), PatternMatch::Unknown);
    let cloned = deep.clone();
    assert_eq!(
        check(&deep, &cloned),
        PatternMatch::Unknown,
        "structural equality must respect the depth budget"
    );
}

#[test]
fn default_values_preserve_literals_and_reject_unresolved_expressions() {
    assert_eq!(
        literal_default_type("'ready'"),
        Some(TypeInfo::LiteralString("'ready'".into()))
    );
    assert_eq!(
        literal_default_type("0x10"),
        Some(TypeInfo::LiteralInt("0x10".into()))
    );
    assert_eq!(literal_default_type("null"), Some(TypeInfo::LiteralNull));
    assert_eq!(literal_default_type("Config::VALUE"), None);
    assert_eq!(literal_default_type("\"$unknown\""), None);
}

#[test]
fn equivalent_unknown_branches_collapse_without_mutating_the_source_type() {
    let ty = TypeInfo::Conditional {
        subject: "$x".into(),
        target: Box::new(simple("string")),
        if_type: Box::new(simple("Same")),
        else_type: Box::new(simple("Same")),
    };
    let original = ty.clone();
    assert_eq!(
        resolve_conditionals(
            &ty,
            &HashMap::new(),
            &HashSet::new(),
            &HashMap::new(),
            &|_, _| None
        ),
        simple("Same")
    );
    assert_eq!(ty, original);
}

#[test]
fn an_unbound_result_template_is_not_emitted_as_a_concrete_class() {
    let ty = TypeInfo::Conditional {
        subject: "$value".into(),
        target: Box::new(simple("string")),
        if_type: Box::new(simple("T")),
        else_type: Box::new(TypeInfo::Mixed),
    };
    let result = resolve_conditionals(
        &ty,
        &HashMap::from([("value".into(), TypeInfo::LiteralString("'x'".into()))]),
        &HashSet::from(["T".into()]),
        &HashMap::new(),
        &|_, _| None,
    );
    assert_eq!(result, TypeInfo::Mixed);
}
