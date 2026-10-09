use super::*;

fn simple(name: &str) -> TypeInfo {
    TypeInfo::Simple(name.into())
}
fn conditional(subject: &str, target: TypeInfo, yes: TypeInfo, no: TypeInfo) -> TypeInfo {
    TypeInfo::Conditional {
        subject: subject.into(),
        target: Box::new(target),
        if_type: Box::new(yes),
        else_type: Box::new(no),
    }
}

#[test]
fn conditional_branches_bind_union_and_intersection_before_outer_condition() {
    let expected = conditional(
        "$x",
        simple("Foo"),
        TypeInfo::Union(vec![simple("A"), simple("B")]),
        TypeInfo::Intersection(vec![simple("C"), simple("D")]),
    );
    for text in [
        "$x is Foo ? A|B : C&D",
        "($x is Foo ? A|B : C&D)",
        "(($x is Foo ? A|B : C&D))",
    ] {
        assert_eq!(parse_type_string(text), expected, "{text}");
    }
}

#[test]
fn parenthesized_conditionals_still_allow_an_outer_union_and_array_suffix() {
    let cond = conditional("$x", simple("Foo"), simple("A"), simple("B"));
    assert_eq!(
        parse_type_string("($x is Foo ? A : B)|null"),
        TypeInfo::Union(vec![cond.clone(), TypeInfo::LiteralNull])
    );
    assert_eq!(
        parse_type_string("($x is Foo ? A : B)[]"),
        TypeInfo::Generic {
            base: "array".into(),
            args: vec![cond]
        }
    );
}

#[test]
fn nested_conditional_colons_pair_with_their_own_question_mark() {
    let inner = conditional("$y", simple("Bar"), simple("A"), simple("B"));
    assert_eq!(
        parse_type_string("$x is Foo ? $y is Bar ? A : B : C"),
        conditional("$x", simple("Foo"), inner.clone(), simple("C"))
    );
    assert_eq!(
        parse_type_string("$x is Foo ? C : $y is Bar ? A : B"),
        conditional("$x", simple("Foo"), simple("C"), inner)
    );
}

#[test]
fn nullable_target_and_branches_are_not_ternary_delimiters() {
    let expected = conditional(
        "$x",
        TypeInfo::Nullable(Box::new(simple("Foo"))),
        TypeInfo::Nullable(Box::new(simple("A"))),
        TypeInfo::Union(vec![simple("B"), simple("C")]),
    );
    assert_eq!(parse_type_string("($x is ?Foo ? ?A : B|C)"), expected);
}

#[test]
fn conditional_types_parse_inside_generics_shapes_and_callable_returns() {
    let cond = conditional(
        "$x",
        simple("Foo"),
        TypeInfo::Union(vec![simple("A"), simple("B")]),
        simple("C"),
    );
    assert_eq!(
        parse_type_string("array<$x is Foo ? A|B : C>"),
        TypeInfo::Generic {
            base: "array".into(),
            args: vec![cond.clone()]
        }
    );
    assert_eq!(
        parse_type_string("array{result: ($x is Foo ? A|B : C)}"),
        TypeInfo::ArrayShape(vec![ArrayShapeItem {
            key: Some("result".into()),
            optional: false,
            value: cond.clone()
        }])
    );
    assert_eq!(
        parse_type_string("callable(): ($x is Foo ? A|B : C)"),
        TypeInfo::Callable {
            params: vec![],
            return_type: Some(Box::new(cond))
        }
    );
}

#[test]
fn negated_condition_swaps_semantic_branches_without_a_new_type_variant() {
    assert_eq!(
        parse_type_string("($x is not null ? A : B)"),
        conditional("$x", TypeInfo::LiteralNull, simple("B"), simple("A"))
    );
}

#[test]
fn conditional_delimiters_ignore_literal_shape_and_utf8_punctuation() {
    let text = "($x is '?:|& is ' ? array{'ключ?:': A|B} : C)";
    let expected = conditional(
        "$x",
        TypeInfo::LiteralString("'?:|& is '".into()),
        TypeInfo::ArrayShape(vec![ArrayShapeItem {
            key: Some("ключ?:".into()),
            optional: false,
            value: TypeInfo::Union(vec![simple("A"), simple("B")]),
        }]),
        simple("C"),
    );
    assert_eq!(parse_type_string(text), expected);
    for malformed in ["$x is Foo ? : B", "$x is ? A : B", "$x is Foo ? A :"] {
        assert!(
            !matches!(parse_type_string(malformed), TypeInfo::Conditional { .. }),
            "{malformed}"
        );
    }
}

#[test]
fn callable_return_colons_are_not_conditional_branch_separators() {
    let callable = TypeInfo::Callable {
        params: vec![],
        return_type: Some(Box::new(simple("A"))),
    };
    assert_eq!(
        parse_type_string("($x is Foo ? callable(): A : B)"),
        conditional("$x", simple("Foo"), callable, simple("B"))
    );
}
