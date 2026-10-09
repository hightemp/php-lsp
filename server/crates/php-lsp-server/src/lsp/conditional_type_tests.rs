use super::*;
use php_lsp_types::TypeInfo;

fn simple(name: &str) -> TypeInfo {
    TypeInfo::Simple(name.into())
}
fn conditional(target: TypeInfo) -> TypeInfo {
    TypeInfo::Conditional {
        subject: "$value".into(),
        target: Box::new(target),
        if_type: Box::new(simple("Yes")),
        else_type: Box::new(simple("No")),
    }
}
fn resolve(target: TypeInfo, actual: TypeInfo) -> TypeInfo {
    resolve_call_site_type_info(
        &conditional(target),
        &HashMap::from([("value".into(), actual)]),
        &HashSet::new(),
        &HashMap::new(),
        &|_, _| None,
    )
}
fn both() -> TypeInfo {
    TypeInfo::Union(vec![simple("Yes"), simple("No")])
}

#[test]
fn literal_arguments_match_their_primitive_supertypes() {
    for (target, actual) in [
        ("string", TypeInfo::LiteralString("'value'".into())),
        ("int", TypeInfo::LiteralInt("42".into())),
        ("float", TypeInfo::LiteralFloat("1.25".into())),
        ("bool", TypeInfo::LiteralBool(true)),
        ("bool", TypeInfo::LiteralBool(false)),
        ("integer", TypeInfo::LiteralInt("42".into())),
        ("boolean", TypeInfo::LiteralBool(true)),
        ("double", TypeInfo::LiteralFloat("1.25".into())),
    ] {
        assert_eq!(resolve(simple(target), actual), simple("Yes"), "{target}");
    }
}

#[test]
fn disjoint_literal_types_select_else() {
    assert_eq!(
        resolve(simple("int"), TypeInfo::LiteralString("'value'".into())),
        simple("No")
    );
    assert_eq!(
        resolve(TypeInfo::LiteralBool(true), TypeInfo::LiteralBool(false)),
        simple("No")
    );
}

#[test]
fn mixed_partial_unions_nullable_and_broad_literal_subjects_keep_both_branches() {
    for (target, actual) in [
        (simple("string"), TypeInfo::Mixed),
        (
            simple("string"),
            TypeInfo::Union(vec![
                TypeInfo::LiteralString("'x'".into()),
                TypeInfo::LiteralInt("1".into()),
            ]),
        ),
        (
            simple("string"),
            TypeInfo::Nullable(Box::new(simple("string"))),
        ),
        (TypeInfo::LiteralString("'x'".into()), simple("string")),
        (simple("ParentType"), simple("UnknownChild")),
    ] {
        assert_eq!(
            resolve(target.clone(), actual.clone()),
            both(),
            "{target:?}: {actual:?}"
        );
    }
}

#[test]
fn every_member_of_an_actual_union_can_match_a_primitive_pattern() {
    let actual = TypeInfo::Union(vec![
        TypeInfo::LiteralString("'x'".into()),
        TypeInfo::LiteralString("'y'".into()),
    ]);
    assert_eq!(resolve(simple("string"), actual), simple("Yes"));
}

#[test]
fn failed_intersection_does_not_leak_partial_template_bindings_into_else() {
    let mut ty = conditional(TypeInfo::Intersection(vec![simple("T"), simple("int")]));
    if let TypeInfo::Conditional { else_type, .. } = &mut ty {
        **else_type = simple("T");
    }
    let result = resolve_call_site_type_info(
        &ty,
        &HashMap::from([("value".into(), TypeInfo::LiteralString("'x'".into()))]),
        &HashSet::from(["T".into()]),
        &HashMap::new(),
        &|_, _| None,
    );
    assert_eq!(result, TypeInfo::Mixed);
}

#[test]
fn repeated_template_bindings_must_be_consistent() {
    let ty = conditional(TypeInfo::Generic {
        base: "array".into(),
        args: vec![simple("T"), simple("T")],
    });
    let actual = TypeInfo::Generic {
        base: "array".into(),
        args: vec![simple("int"), simple("string")],
    };
    let result = resolve_call_site_type_info(
        &ty,
        &HashMap::from([("value".into(), actual)]),
        &HashSet::from(["T".into()]),
        &HashMap::from([("T".into(), simple("int"))]),
        &|_, _| None,
    );
    assert_eq!(result, simple("No"));
}

#[test]
fn declared_template_subject_uses_its_call_site_substitution() {
    let mut ty = conditional(simple("int"));
    if let TypeInfo::Conditional { subject, .. } = &mut ty {
        *subject = "T".into();
    }
    let result = resolve_call_site_type_info(
        &ty,
        &HashMap::new(),
        &HashSet::from(["T".into()]),
        &HashMap::from([("T".into(), TypeInfo::LiteralInt("1".into()))]),
        &|_, _| None,
    );
    assert_eq!(result, simple("Yes"));
}

#[test]
fn conditional_results_inside_containers_are_resolved_recursively() {
    let ty = TypeInfo::Generic {
        base: "array".into(),
        args: vec![conditional(simple("string"))],
    };
    let result = resolve_call_site_type_info(
        &ty,
        &HashMap::from([("value".into(), TypeInfo::LiteralString("'x'".into()))]),
        &HashSet::new(),
        &HashMap::new(),
        &|_, _| None,
    );
    assert_eq!(
        result,
        TypeInfo::Generic {
            base: "array".into(),
            args: vec![simple("Yes")]
        }
    );
}

#[test]
fn function_resolver_metadata_preserves_template_identity_in_a_namespace() {
    let source="<?php namespace Ns; /**\n * @template T\n * @param class-string<T> $value\n * @return ($value is class-string<T> ? T : mixed)\n */ function choose($value){}";
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let symbols = extract_file_symbols(
        parser.tree().unwrap(),
        source,
        "file:///template-metadata.php",
    );
    let index = WorkspaceIndex::new();
    index.update_file("file:///template-metadata.php", symbols);
    let metadata = resolve_function_type_from_index(&index, r"Ns\choose").unwrap();
    assert!(metadata.template_names.contains("T"));
    let ret = parse_phpdoc(&format!("/** @return {} */", metadata.type_text))
        .return_type
        .unwrap();
    let TypeInfo::Conditional {
        target, if_type, ..
    } = ret
    else {
        panic!("lost conditional: {}", metadata.type_text);
    };
    assert_eq!(
        *target,
        TypeInfo::ClassString(Some(Box::new(simple("T")))),
        "template qualified as a class"
    );
    assert_eq!(
        *if_type,
        simple("T"),
        "result template qualified as a class"
    );
}
