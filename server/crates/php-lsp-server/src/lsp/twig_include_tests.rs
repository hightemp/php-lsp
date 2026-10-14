use super::*;

fn caller_variables() -> Vec<TemplateVariableType> {
    vec![
        TemplateVariableType {
            name: "item".into(),
            type_text: "array{field: string}".into(),
            shape_definitions: vec![TemplateShapeKeyDefinition {
                target: TemplateShapeDefinitionTarget::Direct,
                path: vec!["field".into()],
                uri: "file:///test/Caller.php".into(),
                range: (1, 3, 1, 8),
            }],
        },
        TemplateVariableType {
            name: "keep".into(),
            type_text: "App\\Kept".into(),
            shape_definitions: Vec::new(),
        },
        TemplateVariableType {
            name: "other".into(),
            type_text: "App\\Other".into(),
            shape_definitions: Vec::new(),
        },
    ]
}

fn contexts(source: &str) -> HashMap<String, Vec<TemplateVariableType>> {
    evaluate_twig_include_calls(
        source,
        &twig_include_calls(source, &|| false),
        &caller_variables(),
        &|| false,
    )
}

fn named<'a>(
    groups: &'a HashMap<String, Vec<TemplateVariableType>>,
    target: &str,
    name: &str,
) -> Option<&'a TemplateVariableType> {
    groups.get(target)?.iter().find(|value| value.name == name)
}

#[test]
fn p234_plain_include_inherits_context_and_shape_definition_provenance() {
    let result = contexts("{% include 'child.twig' %}");
    for variable in caller_variables() {
        assert_eq!(
            named(&result, "child.twig", &variable.name),
            Some(&variable)
        );
    }
}

#[test]
fn p234_with_overrides_one_variable_and_preserves_other_caller_variables() {
    let result = contexts("{% include 'child.twig' with { item: other, renamed: item } %}");
    assert_eq!(
        named(&result, "child.twig", "keep").map(|v| v.type_text.as_str()),
        Some("App\\Kept")
    );
    let item = named(&result, "child.twig", "item").unwrap();
    assert_eq!(item.type_text, "App\\Other");
    assert!(item.shape_definitions.is_empty());
    assert_eq!(
        named(&result, "child.twig", "renamed")
            .unwrap()
            .shape_definitions,
        caller_variables()[0].shape_definitions
    );
}

#[test]
fn p234_only_isolates_child_but_with_values_still_resolve_in_caller_context() {
    let result = contexts("{% include 'child.twig' with { renamed: item } only %}");
    assert_eq!(result["child.twig"].len(), 1);
    let renamed = named(&result, "child.twig", "renamed").unwrap();
    assert_eq!(renamed.type_text, caller_variables()[0].type_text);
    assert_eq!(
        renamed.shape_definitions,
        caller_variables()[0].shape_definitions
    );
}

#[test]
fn p234_only_without_with_keeps_an_empty_include_edge() {
    let result = contexts("{% include 'child.twig' only %}");
    assert!(result.contains_key("child.twig"));
    assert!(result["child.twig"].is_empty());
}

#[test]
fn p234_empty_with_inherits_unless_only_is_present() {
    let result =
        contexts("{% include 'inherit.twig' with {} %}{% include 'isolated.twig' with {} only %}");
    assert_eq!(result["inherit.twig"].len(), caller_variables().len());
    assert!(result["isolated.twig"].is_empty());
}

#[test]
fn p234_duplicate_explicit_keys_replace_types_and_old_definitions() {
    let result = contexts("{% include 'child.twig' with { value: item, value: other } only %}");
    let value = named(&result, "child.twig", "value").unwrap();
    assert_eq!(value.type_text, "App\\Other");
    assert!(value.shape_definitions.is_empty());
}

#[test]
fn p234_scalar_and_unknown_overrides_do_not_retain_inherited_shapes() {
    for (expression, expected) in [
        ("null", "null"),
        ("17", "int"),
        ("'text'", "string"),
        ("missing_variable", "mixed"),
    ] {
        let result = contexts(&format!(
            "{{% include 'child.twig' with {{ item: {expression} }} %}}"
        ));
        let value = named(&result, "child.twig", "item").unwrap();
        assert_eq!(value.type_text, expected);
        assert!(value.shape_definitions.is_empty());
    }
}

#[test]
fn p234_per_call_overrides_do_not_mutate_the_next_include_context() {
    let result =
        contexts("{% include 'first.twig' with { item: other } %}{% include 'next.twig' %}");
    assert_eq!(
        named(&result, "first.twig", "item").unwrap().type_text,
        "App\\Other"
    );
    assert_eq!(
        named(&result, "next.twig", "item"),
        Some(&caller_variables()[0])
    );
}

#[test]
fn p234_keywords_inside_with_values_are_not_include_modifiers() {
    let result = contexts("{%- include 'child.twig' ignore missing with { label: 'only with %} text', payload: item } -%}");
    assert_eq!(
        named(&result, "child.twig", "keep").map(|v| v.type_text.as_str()),
        Some("App\\Kept")
    );
    assert_eq!(
        named(&result, "child.twig", "label").unwrap().type_text,
        "string"
    );
    assert_eq!(
        named(&result, "child.twig", "payload")
            .unwrap()
            .shape_definitions,
        caller_variables()[0].shape_definitions
    );
}

#[test]
fn p234_inert_comment_verbatim_and_string_includes_do_not_create_edges() {
    let include = "{% include 'phantom.twig' with { value: item } %}";
    for source in [
        format!("{{# {include} #}}"),
        format!("{{% verbatim %}}{include}{{% endverbatim %}}"),
        format!("{{{{ \"{include}\" }}}}"),
    ] {
        let source = format!("{source} {{% include 'real.twig' with {{ value: item }} only %}}");
        let result = contexts(&source);
        assert!(
            !result.contains_key("phantom.twig"),
            "inert include: {source}"
        );
        assert_eq!(result["real.twig"].len(), 1);
    }
}

#[test]
fn p234_unknown_with_expression_does_not_claim_inherited_types() {
    let result = contexts("{% include 'child.twig' with dynamic_map %}");
    assert!(result.get("child.twig").is_none_or(Vec::is_empty));
}

#[test]
fn p234_transforming_filters_do_not_copy_old_shape_types_or_definitions() {
    for expression in [
        "item|length",
        "item|json_encode",
        "item.field|length",
        "item|slice(0, 2)|length",
    ] {
        let result = contexts(&format!(
            "{{% include 'child.twig' with {{ value: {expression} }} only %}}"
        ));
        let value = named(&result, "child.twig", "value").unwrap();
        assert_eq!(value.type_text, "mixed", "{expression}");
        assert!(value.shape_definitions.is_empty(), "{expression}");
    }
}

#[test]
fn p234_local_assignment_invalidates_inherited_and_explicit_alias_types() {
    let result = contexts("{% set item = other %}{% include 'child.twig' %}{% include 'alias.twig' with { value: item } only %}");
    let item = named(&result, "child.twig", "item").unwrap();
    assert_eq!(item.type_text, "mixed");
    assert!(item.shape_definitions.is_empty());
    assert_eq!(
        named(&result, "child.twig", "keep").unwrap().type_text,
        "App\\Kept"
    );
    let alias = named(&result, "alias.twig", "value").unwrap();
    assert_eq!(alias.type_text, "mixed");
    assert!(alias.shape_definitions.is_empty());
}

#[test]
fn p234_local_loop_bindings_do_not_reuse_same_named_render_variables() {
    let result = contexts("{% for other, item in rows %}{% include 'child.twig' %}{% endfor %}");
    for name in ["other", "item"] {
        let value = named(&result, "child.twig", name).unwrap();
        assert_eq!(value.type_text, "mixed", "{name}");
        assert!(value.shape_definitions.is_empty());
    }
}

#[test]
fn p234_unmodelled_context_blocks_do_not_claim_original_render_types() {
    for tag in [
        "with {item: other}",
        "with only",
        "embed 'base.twig' with {item: other}",
    ] {
        let result = contexts(&format!("{{% {tag} %}}{{% include 'child.twig' %}}"));
        assert!(
            result
                .get("child.twig")
                .is_none_or(|vars| vars.iter().all(|var| var.type_text == "mixed")),
            "{tag}: {result:?}"
        );
    }
}

#[test]
fn p234_compound_quoted_expressions_are_not_guessed_to_be_string_literals() {
    for expression in [
        "'a' == 'b'",
        "'a' matches '/x/'",
        "'a' ~ 'b'",
        "\"a\" != \"b\"",
    ] {
        let result = contexts(&format!(
            "{{% include 'child.twig' with {{ item: {expression} }} only %}}"
        ));
        let value = named(&result, "child.twig", "item").unwrap();
        assert_eq!(value.type_text, "mixed", "{expression}");
        assert!(value.shape_definitions.is_empty());
    }
}

#[test]
fn p234_tilde_trim_and_ignore_missing_keep_the_same_include_flags() {
    let calls = twig_include_calls(
        "{%~ include 'child.twig' ignore missing with { value: item } only ~%}",
        &|| false,
    );
    assert_eq!(calls.len(), 1);
    assert!(calls[0].only);
    assert!(calls[0].with_range.is_some());
}

#[test]
fn p234_local_write_deltas_apply_at_the_correct_include_and_stay_linear() {
    let result = contexts("{% include 'before.twig' %}{% set item = other %}{% include 'after.twig' only %}{% include 'later.twig' %}");
    assert_eq!(
        named(&result, "before.twig", "item"),
        Some(&caller_variables()[0])
    );
    assert!(result["after.twig"].is_empty());
    assert_eq!(
        named(&result, "later.twig", "item").unwrap().type_text,
        "mixed"
    );
    let source = (0..256)
        .map(|n| format!("{{% set variable{n} = 1 %}}{{% include 'child{n}.twig' only %}}"))
        .collect::<String>();
    let calls = twig_include_calls(&source, &|| false);
    assert_eq!(calls.len(), 256);
    assert_eq!(
        calls
            .iter()
            .map(|call| call.invalidated_variables.len())
            .sum::<usize>(),
        256,
        "syntax facts copied a growing context per include"
    );
}

#[test]
fn p234_macro_bodies_are_inert_and_import_aliases_do_not_keep_render_types() {
    let source = "{% macro output() %}{% include 'phantom.twig' with {x:item} %}{% endmacro %}{% import 'macros.twig' as item %}{% include 'child.twig' %}";
    let result = contexts(source);
    assert!(!result.contains_key("phantom.twig"));
    assert_eq!(
        named(&result, "child.twig", "item").unwrap().type_text,
        "mixed"
    );
    assert!(named(&result, "child.twig", "item")
        .unwrap()
        .shape_definitions
        .is_empty());
    assert_eq!(
        named(&result, "child.twig", "keep").unwrap().type_text,
        "App\\Kept"
    );
}

#[test]
fn p234_dynamic_or_malformed_include_syntax_does_not_poison_valid_neighbors() {
    for invalid in [
        "{% include 'bad.twig' ~ suffix with {value:item} %}",
        "{% include variable with {value:item} %}",
        "{% include \"#{dynamic}.twig\" with {value:item} %}",
        "{% include 'bad.twig' only with {value:item} %}",
        "{% include ignore missing 'bad.twig' with {value:item} %}",
        "{% include 'bad.twig' with {value:item %}",
        "{% include 'bad.twig' with {(unknown_key):item} %}",
    ] {
        let result = contexts(&format!(
            "{invalid} {{% include 'good.twig' with {{value:other}} only %}}"
        ));
        assert!(
            result.get("bad.twig").is_none_or(Vec::is_empty),
            "{invalid}: {result:?}"
        );
        assert_eq!(result["good.twig"].len(), 1);
        assert_eq!(
            named(&result, "good.twig", "value").unwrap().type_text,
            "App\\Other"
        );
    }
}

#[test]
fn p234_quoted_pipes_and_escaped_quotes_are_still_complete_literals() {
    for literal in ["'only | length'", "'don\\'t'", "\"escaped \\\"quote\\\"\""] {
        let result = contexts(&format!(
            "{{% include 'child.twig' with {{value:{literal}}} only %}}"
        ));
        assert_eq!(
            named(&result, "child.twig", "value").unwrap().type_text,
            "string",
            "{literal}"
        );
    }
}

#[test]
fn p234_cancelled_include_evaluation_does_not_return_partial_context() {
    let source = "{% include 'first.twig' %}{% include 'second.twig' with {value:item} only %}";
    let calls = twig_include_calls(source, &|| false);
    let checks = std::cell::Cell::new(0);
    let cancelled = || {
        let count = checks.get() + 1;
        checks.set(count);
        count >= 7
    };
    let result = evaluate_twig_include_calls(source, &calls, &caller_variables(), &cancelled);
    assert!(result.is_empty());
    assert_eq!(checks.get(), 7);
}

#[test]
fn p234_cancelled_last_with_map_does_not_return_previous_targets() {
    let source = "{% include 'first.twig' %}{% include 'second.twig' with {value:item} only %}";
    let calls = twig_include_calls(source, &|| false);
    let checks = std::cell::Cell::new(0);
    let cancelled = || {
        let count = checks.get() + 1;
        checks.set(count);
        count >= 12
    };
    let result = evaluate_twig_include_calls(source, &calls, &caller_variables(), &cancelled);
    assert!(
        result.is_empty(),
        "cancelled evaluation returned partial targets: {result:?}"
    );
}
