use super::*;
fn simple(name: &str) -> TypeInfo {
    TypeInfo::Simple(name.to_string())
}

#[test]
fn absence_of_closure_inheritance_does_not_disprove_callability() {
    let no_closure_inheritance = |_: &str, _: &str| Some(false);
    assert_eq!(
        phpdoc_refines_native_with(
            &simple("Invokable"),
            &simple("callable"),
            &no_closure_inheritance
        ),
        TypeRefinement::Unknown
    );
}

#[test]
fn structural_refinements_keep_native_value_domains() {
    use TypeRefinement::*;
    for (doc, native, expected) in [
        (simple("Wrong"), simple("int"), Incompatible),
        (TypeInfo::Mixed, simple("int"), Incompatible),
        (
            TypeInfo::Nullable(Box::new(simple("int"))),
            simple("int"),
            Incompatible,
        ),
        (
            simple("int"),
            TypeInfo::Nullable(Box::new(simple("int"))),
            Compatible,
        ),
        (simple("positive-int"), simple("int"), Compatible),
        (simple("int"), simple("positive-int"), Incompatible),
        (TypeInfo::LiteralBool(false), simple("bool"), Compatible),
        (
            TypeInfo::LiteralString("'x'".into()),
            simple("int"),
            Incompatible,
        ),
        (simple("Child"), simple("Base"), Unknown),
        (simple("Child"), simple("object"), Compatible),
        (simple("string"), simple("object"), Incompatible),
        (TypeInfo::Never, simple("object"), Compatible),
        (simple("object"), TypeInfo::Never, Incompatible),
    ] {
        assert_eq!(
            phpdoc_refines_native(&doc, &native),
            expected,
            "{doc} vs {native}"
        );
    }
}

#[test]
fn every_doc_union_and_conditional_alternative_must_fit() {
    let native = TypeInfo::Union(vec![simple("int"), simple("string")]);
    assert_eq!(
        phpdoc_refines_native(&simple("int"), &native),
        TypeRefinement::Compatible
    );
    assert_eq!(
        phpdoc_refines_native(
            &TypeInfo::Union(vec![simple("int"), simple("Wrong")]),
            &native
        ),
        TypeRefinement::Incompatible
    );
    let conditional = TypeInfo::Conditional {
        subject: "$value".into(),
        target: Box::new(simple("bool")),
        if_type: Box::new(simple("int")),
        else_type: Box::new(simple("Wrong")),
    };
    assert_eq!(
        phpdoc_refines_native(&conditional, &simple("int")),
        TypeRefinement::Incompatible
    );
}

#[test]
fn class_relations_and_template_bounds_need_evidence() {
    let resolver = |doc: &str, native: &str| match (doc, native) {
        ("Child", "Base") => Some(true),
        ("Other", "Base") => Some(false),
        _ => None,
    };
    assert_eq!(
        phpdoc_refines_native_with(&simple("Child"), &simple("Base"), &resolver),
        TypeRefinement::Compatible
    );
    assert_eq!(
        phpdoc_refines_native_with(&simple("Other"), &simple("Base"), &resolver),
        TypeRefinement::Incompatible
    );
    let templates = [TemplateParam {
        name: "T".into(),
        bound: Some(simple("Child")),
        variance: crate::TemplateVariance::Invariant,
    }];
    assert_eq!(
        phpdoc_refines_native_with_templates(&simple("T"), &simple("Base"), &resolver, &templates),
        TypeRefinement::Compatible
    );
    assert_eq!(
        phpdoc_refines_native_with_templates(&simple("T"), &simple("int"), &resolver, &templates),
        TypeRefinement::Incompatible
    );
}

#[test]
fn a_class_identity_callback_cannot_weaken_native_static() {
    let same_owner = |_: &str, _: &str| Some(true);
    assert_eq!(
        phpdoc_refines_native_with(&TypeInfo::Self_, &TypeInfo::Static_, &same_owner),
        TypeRefinement::Incompatible
    );
    assert_eq!(
        phpdoc_refines_native_with(&simple("Base"), &TypeInfo::Static_, &same_owner),
        TypeRefinement::Incompatible
    );
}

#[test]
fn scalar_generic_operators_cannot_refine_native_object() {
    let doc = TypeInfo::Generic {
        base: "key-of".into(),
        args: vec![simple("Foo::KEYS")],
    };
    assert_eq!(
        phpdoc_refines_native(&doc, &simple("object")),
        TypeRefinement::Incompatible
    );
}

#[test]
fn key_domains_need_constant_evidence_before_rejecting_string_or_int() {
    let doc = TypeInfo::Generic {
        base: "key-of".into(),
        args: vec![simple("Foo::KEYS")],
    };
    for native in ["string", "int"] {
        assert_eq!(
            phpdoc_refines_native(&doc, &simple(native)),
            TypeRefinement::Unknown
        );
    }
}
