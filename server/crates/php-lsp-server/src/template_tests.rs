use super::*;

#[test]
fn blade_echo_maps_original_position_to_virtual_php() {
    let doc = preprocess_blade_template("<div>{{ $user->name }}</div>\n");
    assert!(doc
        .virtual_source()
        .contains("<?php echo  $user->name ; ?>"));

    let original_position = Position::new(0, 8);
    let virtual_position = doc
        .map_original_position_to_virtual(original_position)
        .expect("template expression position should map");
    let virtual_offset = byte_offset_for_position(doc.virtual_source(), virtual_position)
        .expect("virtual position offset");
    assert_eq!(
        doc.virtual_source()
            .get(virtual_offset..virtual_offset + "$user".len()),
        Some("$user")
    );
}

#[test]
fn blade_directives_create_virtual_php_and_semantic_tokens() {
    let doc = preprocess_blade_template(
            "@if ($user)\n{{-- comment --}}\n@foreach ($items as $item)\n{{ $item }}\n@endforeach\n@endif\n",
        );
    assert!(doc.virtual_source().contains("<?php if ($user): ?>"));
    assert!(doc
        .virtual_source()
        .contains("<?php foreach ($items as $item): ?>"));
    assert!(doc.virtual_source().contains("<?php endforeach; ?>"));
    assert!(doc.virtual_source().contains("<?php endif; ?>"));

    let tokens = doc.map_semantic_tokens_to_original(Vec::new());
    assert!(
        tokens.iter().any(|token| token.token_type == TOKEN_KEYWORD),
        "expected directive keyword semantic tokens"
    );
    assert!(
        tokens.iter().any(|token| token.token_type == TOKEN_COMMENT),
        "expected comment semantic tokens"
    );
}

#[test]
fn blade_range_mapping_suppresses_unmapped_generated_php() {
    let doc = preprocess_blade_template("<div>{{ $user }}</div>");
    let generated_prefix = Range::new(Position::new(0, 0), Position::new(0, 5));
    assert!(doc
        .map_virtual_range_to_original(generated_prefix)
        .is_none());

    let user_virtual = doc
        .map_original_position_to_virtual(Position::new(0, 8))
        .expect("mapped user position");
    let user_range = Range::new(
        user_virtual,
        Position::new(user_virtual.line, user_virtual.character + 5),
    );
    let original = doc
        .map_virtual_range_to_original(user_range)
        .expect("mapped variable range");
    assert_eq!(original.start, Position::new(0, 8));
    assert_eq!(original.end, Position::new(0, 13));
}

#[test]
fn twig_echo_maps_variable_and_member_chain_to_virtual_php() {
    let doc = preprocess_twig_template(
        "<h1>{{ user.name }}</h1>\n",
        &[TemplateVariableType {
            name: "user".to_string(),
            type_text: "App\\Entity\\User".to_string(),
            shape_definitions: Vec::new(),
        }],
    );
    assert!(doc.virtual_source().contains("$user->name"));

    let original_position = Position::new(0, 7);
    let virtual_position = doc
        .map_original_position_to_virtual(original_position)
        .expect("Twig variable should map to virtual PHP variable");
    let virtual_offset = byte_offset_for_position(doc.virtual_source(), virtual_position)
        .expect("virtual position offset");
    assert_eq!(
        doc.virtual_source()
            .get(virtual_offset..virtual_offset + "$user".len()),
        Some("$user")
    );
}

#[test]
fn twig_whitespace_control_maps_expression_and_block_content() {
    let doc = preprocess_twig_template(
            "<h1>{{- user.name -}}</h1>\n{%- for item in users -%}\n{{- item.name -}}\n{%- endfor -%}\n",
            &[TemplateVariableType {
                name: "user".to_string(),
                type_text: "App\\Entity\\User".to_string(),
            shape_definitions: Vec::new(),
            }],
        );
    assert!(doc.virtual_source().contains("$user->name"));
    assert!(doc.virtual_source().contains("foreach ($users as $item)"));
    assert!(doc.virtual_source().contains("$item->name"));
    assert!(doc.syntax_diagnostics().is_empty());

    let original_position = Position::new(0, 8);
    let virtual_position = doc
        .map_original_position_to_virtual(original_position)
        .expect("Twig whitespace-control variable should map");
    let virtual_offset = byte_offset_for_position(doc.virtual_source(), virtual_position)
        .expect("virtual position offset");
    assert_eq!(
        doc.virtual_source()
            .get(virtual_offset..virtual_offset + "$user".len()),
        Some("$user")
    );
}

#[test]
fn twig_verbatim_blocks_skip_inner_twig_syntax() {
    let doc = preprocess_twig_template(
        "{% verbatim %}{{ user.name }{% endverbatim %}\n{{ user.name }}\n",
        &[TemplateVariableType {
            name: "user".to_string(),
            type_text: "App\\Entity\\User".to_string(),
            shape_definitions: Vec::new(),
        }],
    );
    assert!(doc.syntax_diagnostics().is_empty());
    assert_eq!(doc.virtual_source().matches("$user->name").count(), 1);

    let tokens = doc.map_semantic_tokens_to_original(Vec::new());
    assert!(
        tokens.iter().any(|token| token.token_type == TOKEN_KEYWORD),
        "expected verbatim keyword semantic tokens"
    );
}

#[test]
fn twig_verbatim_finds_end_tag_after_literal_broken_tag_opener() {
    let doc = preprocess_twig_template(
        "{% verbatim %}{% broken {% endverbatim %}\n{{ user.name }}\n",
        &[TemplateVariableType {
            name: "user".to_string(),
            type_text: "App\\Entity\\User".to_string(),
            shape_definitions: Vec::new(),
        }],
    );
    assert!(doc.syntax_diagnostics().is_empty());
    assert_eq!(doc.virtual_source().matches("$user->name").count(), 1);
}

#[test]
fn twig_comments_ignore_quotes_while_finding_close_delimiter() {
    let doc = preprocess_twig_template(
        "{# don't map {{ broken } #}\n{{ user.name }}\n",
        &[TemplateVariableType {
            name: "user".to_string(),
            type_text: "App\\Entity\\User".to_string(),
            shape_definitions: Vec::new(),
        }],
    );
    assert!(doc.syntax_diagnostics().is_empty());
    assert_eq!(doc.virtual_source().matches("$user->name").count(), 1);

    let tokens = doc.map_semantic_tokens_to_original(Vec::new());
    assert!(
        tokens.iter().any(|token| token.token_type == TOKEN_COMMENT),
        "expected quoted Twig comment semantic token"
    );
}

#[test]
fn twig_macro_blocks_are_valid_syntax_but_not_converted_to_php() {
    let doc = preprocess_twig_template(
        "{% macro input(name) %}{{ name }}{% endmacro %}\n{{ user.name }}\n",
        &[TemplateVariableType {
            name: "user".to_string(),
            type_text: "App\\Entity\\User".to_string(),
            shape_definitions: Vec::new(),
        }],
    );
    assert!(doc.syntax_diagnostics().is_empty());
    assert!(!doc.virtual_source().contains("function input"));
    assert!(!doc.virtual_source().contains("$name"));
    assert!(doc.virtual_source().contains("$user->name"));
}

#[test]
fn twig_macro_body_is_still_checked_for_syntax_errors() {
    let doc = preprocess_twig_template("{% macro input(name) %}{{ name }{% endmacro %}\n", &[]);
    let messages: Vec<_> = doc
        .syntax_diagnostics()
        .into_iter()
        .map(|diagnostic| diagnostic.message)
        .collect();
    assert!(
        messages
            .iter()
            .any(|message| message == "Unclosed Twig expression"),
        "expected macro body expression diagnostic, got {messages:?}"
    );
}

#[test]
fn twig_unknown_custom_paired_tags_do_not_report_syntax_errors() {
    let doc = preprocess_twig_template(
        "{% custom %}{{ user.name }}{% endcustom %}\n",
        &[TemplateVariableType {
            name: "user".to_string(),
            type_text: "App\\Entity\\User".to_string(),
            shape_definitions: Vec::new(),
        }],
    );
    assert!(doc.syntax_diagnostics().is_empty());
    assert!(doc.virtual_source().contains("$user->name"));
}

#[test]
fn twig_control_blocks_and_template_paths_are_detected() {
    let doc = preprocess_twig_template(
            "{% for item in users %}\n{{ item.name }}\n{% endfor %}\n{% include 'shared/card.html.twig' %}\n",
            &[],
        );
    assert!(doc.virtual_source().contains("foreach ($users as $item)"));
    assert!(doc.virtual_source().contains("$item->name"));

    let context = doc
        .twig_template_path_context_at_position(Position::new(3, 23))
        .expect("include path context");
    assert_eq!(context.key, "shared/card.html.twig");
    assert_eq!(context.prefix, "shared/card");

    let tokens = doc.map_semantic_tokens_to_original(Vec::new());
    assert!(
        tokens.iter().any(|token| token.token_type == TOKEN_KEYWORD),
        "expected Twig keyword semantic tokens"
    );
}

#[test]
fn twig_unsupported_expression_classifier_covers_backlog_constructs() {
    let macro_aliases = HashSet::new();
    for (source, expected) in [
        ("user.name|upper", UnsupportedTwigExpression::Filter),
        ("user is defined", UnsupportedTwigExpression::Test),
        ("user.id in ids", UnsupportedTwigExpression::InOperator),
        ("path('dashboard')", UnsupportedTwigExpression::FunctionCall),
        (
            "user.active ? 'yes' : 'no'",
            UnsupportedTwigExpression::Ternary,
        ),
        (
            "user.name ?? 'n/a'",
            UnsupportedTwigExpression::NullCoalescing,
        ),
        (
            "user['name']",
            UnsupportedTwigExpression::ComplexAttributeAccess,
        ),
        ("1..5", UnsupportedTwigExpression::ComplexAttributeAccess),
    ] {
        assert_eq!(
            unsupported_twig_expression(source, 0, source.len(), &macro_aliases),
            Some(expected),
            "expected unsupported kind for `{source}`"
        );
    }

    let mut macro_aliases = HashSet::new();
    macro_aliases.insert("forms".to_string());
    assert_eq!(
        unsupported_twig_expression(
            "forms.input(user)",
            0,
            "forms.input(user)".len(),
            &macro_aliases
        ),
        Some(UnsupportedTwigExpression::MacroCall)
    );
    assert_eq!(
        unsupported_twig_expression(
            "_self.input(user)",
            0,
            "_self.input(user)".len(),
            &HashSet::new()
        ),
        Some(UnsupportedTwigExpression::MacroCall)
    );
    assert_eq!(
        unsupported_twig_expression(
            "user.setAge(123)",
            0,
            "user.setAge(123)".len(),
            &HashSet::new()
        ),
        None,
        "plain object method calls remain best-effort PHP mappings"
    );
}

#[test]
fn twig_unsupported_complex_expressions_emit_unmapped_placeholders() {
    let source = concat!(
        "{% import 'forms.html.twig' as forms %}\n",
        "{{ user.name|upper }}\n",
        "{% if user is defined %}visible{% endif %}\n",
        "{% for item in users|filter(u => u.active) %}{{ item.name }}{% endfor %}\n",
        "{% set label = attribute(user, dynamic_name) %}\n",
        "{{ forms.input(user) }}\n",
        "{{ _self.card(user) }}\n",
        "{{ user.name ?? 'n/a' }}\n",
        "{{ user['name'] }}\n",
    );
    let doc = preprocess_twig_template(
        source,
        &[TemplateVariableType {
            name: "user".to_string(),
            type_text: "App\\Entity\\User".to_string(),
            shape_definitions: Vec::new(),
        }],
    );

    assert!(doc.virtual_source().contains("<?php echo null; ?>"));
    assert!(doc.virtual_source().contains("<?php if (true): ?>"));
    assert!(doc.virtual_source().contains("<?php $user; ?>"));
    assert!(doc
        .virtual_source()
        .contains("<?php foreach ($users as $item): ?>"));
    assert!(doc.virtual_source().contains("$label = null"));

    for needle in ["user is defined", "users|filter", "user['name']"] {
        let original_offset = source.find(needle).expect("fixture needle");
        let original_position = position_for_byte_offset(source, original_offset);
        assert!(
            doc.map_original_position_to_virtual(original_position)
                .is_some(),
            "unsupported Twig expression root variable `{needle}` should map to no-op virtual PHP"
        );
    }

    for needle in ["upper", "attribute", "forms.input", "_self.card", "??"] {
        let original_offset = source.find(needle).expect("fixture needle");
        let original_position = position_for_byte_offset(source, original_offset);
        assert!(
            doc.map_original_position_to_virtual(original_position)
                .is_none(),
            "unsupported Twig expression `{needle}` should not map to virtual PHP"
        );
    }
}

#[test]
fn twig_unsupported_expressions_map_inner_member_chains_to_virtual_php() {
    let source = concat!(
        "{{ user.name|upper }}\n",
        "{% if user.items is iterable and user.items|length > 0 %}visible{% endif %}\n",
        "{% set shown = user.items|slice(0, 5) %}\n",
        "{{ path('profile', {'id': user.profile.id}) }}\n",
        "{{ path('profile', {'id': user.}) }}\n",
        "{{ user.createdAt|date('d.m.Y') }}\n",
    );
    let doc = preprocess_twig_template(
        source,
        &[TemplateVariableType {
            name: "user".to_string(),
            type_text: "App\\Entity\\User".to_string(),
            shape_definitions: Vec::new(),
        }],
    );

    for expected in [
        "$shown = $user->items;",
        "$user->name;",
        "$user->items;",
        "$user->profile->id;",
        "$user->;",
        "$user->createdAt;",
    ] {
        assert!(
            doc.virtual_source().contains(expected),
            "expected partial Twig member chain `{expected}` in virtual PHP, got: {}",
            doc.virtual_source()
        );
    }
    assert_eq!(
        doc.virtual_source().matches("$user->items;").count(),
        3,
        "slice base should be mapped by the assignment without a duplicate no-op fragment, got: {}",
        doc.virtual_source()
    );

    for needle in [
        "user.name",
        "user.items",
        "user.profile.id",
        "user.createdAt",
    ] {
        let original_offset = source.find(needle).expect("fixture member chain");
        let original_position = position_for_byte_offset(source, original_offset);
        let virtual_position = doc
            .map_original_position_to_virtual(original_position)
            .unwrap_or_else(|| panic!("member chain `{needle}` should map"));
        let virtual_offset = byte_offset_for_position(doc.virtual_source(), virtual_position)
            .expect("virtual position offset");
        assert_eq!(
            doc.virtual_source()
                .get(virtual_offset..virtual_offset + "$user".len()),
            Some("$user"),
            "member chain `{needle}` should map to a virtual PHP variable"
        );
    }
    let trailing_dot_offset = source
        .find("user.})")
        .map(|offset| offset + "user.".len())
        .expect("fixture trailing member access");
    let trailing_dot_position = position_for_byte_offset(source, trailing_dot_offset);
    let trailing_virtual_position = doc
        .map_original_position_to_virtual(trailing_dot_position)
        .expect("trailing member access cursor should map");
    let trailing_virtual_offset =
        byte_offset_for_position(doc.virtual_source(), trailing_virtual_position)
            .expect("trailing virtual offset");
    assert_eq!(
        doc.virtual_source()
            .get(trailing_virtual_offset.saturating_sub("->".len())..trailing_virtual_offset),
        Some("->"),
        "cursor after trailing Twig dot should map after virtual PHP member arrow"
    );

    for needle in ["upper", "path", "date"] {
        let original_offset = source.find(needle).expect("fixture unsupported token");
        let original_position = position_for_byte_offset(source, original_offset);
        assert!(
            doc.map_original_position_to_virtual(original_position)
                .is_none(),
            "unsupported Twig token `{needle}` should stay unmapped"
        );
    }
}

#[test]
fn twig_generated_context_diagnostics_are_unmapped() {
    let doc = preprocess_twig_template(
        "{{ user.name }}",
        &[TemplateVariableType {
            name: "user".to_string(),
            type_text: "App\\Entity\\User".to_string(),
            shape_definitions: Vec::new(),
        }],
    );
    let generated_prelude = Range::new(Position::new(1, 0), Position::new(1, 5));
    assert!(doc
        .map_virtual_range_to_original(generated_prelude)
        .is_none());
}

#[test]
fn safe_template_diagnostics_map_exact_unknown_members_and_suppress_syntax() {
    let source = "<div>{{ (new User())->missing() }}</div>";
    let doc = preprocess_blade_template(source);
    let original_start = source.find("missing").expect("missing member in fixture");
    let original_end = original_start + "missing".len();
    let virtual_start = doc
        .source_map
        .original_to_virtual(original_start)
        .expect("member start should map");
    let virtual_end = doc
        .source_map
        .original_to_virtual(original_end)
        .expect("member end should map");
    let virtual_range = range_for_byte_offsets(doc.virtual_source(), virtual_start, virtual_end);

    let unknown_member = Diagnostic {
        range: virtual_range,
        source: Some("php-lsp".to_string()),
        code: Some(NumberOrString::String("php-lsp.members".to_string())),
        message: "Unknown method: User::missing".to_string(),
        ..Default::default()
    };
    let mapped = doc.map_safe_diagnostics_to_original(vec![unknown_member]);
    assert_eq!(mapped.len(), 1);
    assert_eq!(
        mapped[0].range,
        range_for_byte_offsets(source, original_start, original_end)
    );

    let syntax = Diagnostic {
        range: virtual_range,
        source: Some("php-lsp".to_string()),
        message: "Syntax error".to_string(),
        ..Default::default()
    };
    assert!(doc
        .map_safe_diagnostics_to_original(vec![syntax])
        .is_empty());
}

#[test]
fn safe_template_diagnostics_require_full_source_map_coverage() {
    let doc = preprocess_blade_template("{{ $user }}");
    let generated_prefix = Range::new(Position::new(0, 0), Position::new(0, 5));
    let diagnostic = Diagnostic {
        range: generated_prefix,
        source: Some("php-lsp".to_string()),
        code: Some(NumberOrString::String("php-lsp.members".to_string())),
        message: "Unknown property: User::$name".to_string(),
        ..Default::default()
    };

    assert!(doc
        .map_safe_diagnostics_to_original(vec![diagnostic])
        .is_empty());
}

#[test]
fn safe_template_diagnostics_suppress_unknown_properties() {
    let source = "{{ (new User())->missing }}";
    let doc = preprocess_blade_template(source);
    let original_start = source.find("missing").expect("property in fixture");
    let original_end = original_start + "missing".len();
    let virtual_start = doc
        .source_map
        .original_to_virtual(original_start)
        .expect("property start should map");
    let virtual_end = doc
        .source_map
        .original_to_virtual(original_end)
        .expect("property end should map");
    let diagnostic = Diagnostic {
        range: range_for_byte_offsets(doc.virtual_source(), virtual_start, virtual_end),
        source: Some("php-lsp".to_string()),
        code: Some(NumberOrString::String("php-lsp.members".to_string())),
        message: "Unknown property: User::$missing".to_string(),
        ..Default::default()
    };

    assert!(doc
        .map_safe_diagnostics_to_original(vec![diagnostic])
        .is_empty());
}

#[test]
fn twig_safe_template_diagnostics_suppress_undefined_variables() {
    let source = "{{ standaloneVariable }}";
    let doc = preprocess_twig_template(source, &[]);
    let original_start = source
        .find("standaloneVariable")
        .expect("variable in fixture");
    let original_end = original_start + "standaloneVariable".len();
    let virtual_start = doc
        .source_map
        .original_to_virtual(original_start)
        .expect("variable start should map");
    let virtual_end = doc
        .source_map
        .original_to_virtual(original_end)
        .expect("variable end should map");
    let diagnostic = Diagnostic {
        range: range_for_byte_offsets(doc.virtual_source(), virtual_start, virtual_end),
        source: Some("php-lsp".to_string()),
        code: Some(NumberOrString::String(
            "php-lsp.undefinedVariable".to_string(),
        )),
        message: "Undefined variable: $standaloneVariable".to_string(),
        ..Default::default()
    };

    assert!(doc
        .map_safe_diagnostics_to_original(vec![diagnostic])
        .is_empty());
}

#[test]
fn twig_copied_expression_tokens_map_for_type_diagnostics() {
    let source = "{{ user.setAge(123) }}";
    let doc = preprocess_twig_template(
        source,
        &[TemplateVariableType {
            name: "user".to_string(),
            type_text: "App\\Entity\\User".to_string(),
            shape_definitions: Vec::new(),
        }],
    );
    let original_start = source.find("123").expect("numeric literal in fixture");
    let original_end = original_start + "123".len();
    let virtual_start = doc
        .source_map
        .original_to_virtual(original_start)
        .expect("literal start should map");
    let virtual_end = doc
        .source_map
        .original_to_virtual(original_end)
        .expect("literal end should map");
    let diagnostic = Diagnostic {
        range: range_for_byte_offsets(doc.virtual_source(), virtual_start, virtual_end),
        source: Some("php-lsp".to_string()),
        code: Some(NumberOrString::String(
            "php-lsp.typeCompatibility".to_string(),
        )),
        message:
            "Type mismatch for App\\Entity\\User::setAge argument $age: expected string, got int"
                .to_string(),
        ..Default::default()
    };

    let mapped = doc.map_safe_diagnostics_to_original(vec![diagnostic]);
    assert_eq!(mapped.len(), 1);
    assert_eq!(
        mapped[0].range,
        range_for_byte_offsets(source, original_start, original_end)
    );
}
