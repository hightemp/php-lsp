//! PHPDoc comment parser.
//!
//! Extracts type information and documentation from PHPDoc comments.
//! Supports: @param, @return, @var, @throws, @deprecated, @property, @method,
//! @template, @extends, @implements, @use, and @mixin.

use php_lsp_types::{
    ArrayShapeItem, PhpDoc, PhpDocMethod, PhpDocParam, PhpDocProperty, PhpDocPropertyAccess,
    TemplateBinding, TemplateBindingKind, TemplateParam, TemplateVariance, TypeInfo,
};

/// Parse a PHPDoc comment string into structured data.
///
/// Expects the full comment including `/**` and `*/`.
pub fn parse_phpdoc(comment: &str) -> PhpDoc {
    let mut doc = PhpDoc::default();

    // Strip /** and */ and leading * from each line
    let lines = strip_comment_markers(comment);

    let mut summary_lines: Vec<String> = Vec::new();
    let mut in_summary = true;

    for line in &lines {
        let trimmed = line.trim();

        if trimmed.is_empty() {
            if in_summary && !summary_lines.is_empty() {
                in_summary = false;
            }
            continue;
        }

        if trimmed.starts_with('@') {
            in_summary = false;
            parse_tag(trimmed, &mut doc);
        } else if in_summary {
            summary_lines.push(trimmed.to_string());
        }
    }

    if !summary_lines.is_empty() {
        doc.summary = Some(summary_lines.join(" "));
    }

    doc
}

fn strip_comment_markers(comment: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for line in comment.lines() {
        let trimmed = line.trim();
        // Remove leading /** or */
        let mut stripped = if let Some(rest) = trimmed.strip_prefix("/**") {
            rest.trim()
        } else if trimmed.starts_with("*/") {
            continue;
        } else if let Some(rest) = trimmed.strip_prefix('*') {
            rest.trim_start()
        } else {
            trimmed
        };
        // Remove trailing */
        if stripped.ends_with("*/") {
            stripped = stripped[..stripped.len() - 2].trim_end();
        }
        if !stripped.is_empty() {
            lines.push(stripped.to_string());
        }
    }
    lines
}

fn parse_tag(line: &str, doc: &mut PhpDoc) {
    if let Some((rest, variance)) = template_tag(line) {
        parse_template_tag(rest, variance, doc);
    } else if let Some((rest, kind)) = template_binding_tag(line) {
        parse_template_binding_tag(rest, kind, doc);
    } else if let Some(rest) = line.strip_prefix("@param") {
        parse_param_tag(rest.trim(), doc);
    } else if let Some(rest) = line.strip_prefix("@return") {
        let rest = rest.trim();
        if let Some((type_str, _)) = split_type_prefix(rest) {
            doc.return_type = Some(parse_type_string(type_str));
        }
    } else if let Some(rest) = line.strip_prefix("@var") {
        parse_var_tag(rest.trim(), doc);
    } else if let Some(rest) = line.strip_prefix("@throws") {
        let rest = rest.trim();
        if let Some((type_str, _)) = split_type_prefix(rest) {
            doc.throws.push(parse_type_string(type_str));
        }
    } else if let Some(rest) = line.strip_prefix("@deprecated") {
        let rest = rest.trim();
        doc.deprecated = Some(if rest.is_empty() {
            "Deprecated".to_string()
        } else {
            rest.to_string()
        });
    } else if let Some(rest) = line.strip_prefix("@property-read") {
        parse_property_tag(rest.trim(), doc, PhpDocPropertyAccess::ReadOnly);
    } else if let Some(rest) = line.strip_prefix("@property-write") {
        parse_property_tag(rest.trim(), doc, PhpDocPropertyAccess::WriteOnly);
    } else if let Some(rest) = line.strip_prefix("@property") {
        parse_property_tag(rest.trim(), doc, PhpDocPropertyAccess::ReadWrite);
    } else if let Some(rest) = line.strip_prefix("@method") {
        parse_method_tag(rest.trim(), doc);
    }
}

fn strip_exact_tag<'a>(line: &'a str, tag: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(tag)?;
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        Some(rest.trim())
    } else {
        None
    }
}

fn template_tag(line: &str) -> Option<(&str, TemplateVariance)> {
    for tag in [
        "@template-covariant",
        "@phpstan-template-covariant",
        "@psalm-template-covariant",
    ] {
        if let Some(rest) = strip_exact_tag(line, tag) {
            return Some((rest, TemplateVariance::Covariant));
        }
    }

    for tag in [
        "@template-contravariant",
        "@phpstan-template-contravariant",
        "@psalm-template-contravariant",
    ] {
        if let Some(rest) = strip_exact_tag(line, tag) {
            return Some((rest, TemplateVariance::Contravariant));
        }
    }

    for tag in ["@template", "@phpstan-template", "@psalm-template"] {
        if let Some(rest) = strip_exact_tag(line, tag) {
            return Some((rest, TemplateVariance::Invariant));
        }
    }

    None
}

fn template_binding_tag(line: &str) -> Option<(&str, TemplateBindingKind)> {
    for tag in ["@extends", "@phpstan-extends", "@psalm-extends"] {
        if let Some(rest) = strip_exact_tag(line, tag) {
            return Some((rest, TemplateBindingKind::Extends));
        }
    }

    for tag in ["@implements", "@phpstan-implements", "@psalm-implements"] {
        if let Some(rest) = strip_exact_tag(line, tag) {
            return Some((rest, TemplateBindingKind::Implements));
        }
    }

    for tag in ["@use", "@phpstan-use", "@psalm-use"] {
        if let Some(rest) = strip_exact_tag(line, tag) {
            return Some((rest, TemplateBindingKind::Use));
        }
    }

    for tag in ["@mixin", "@phpstan-mixin", "@psalm-mixin"] {
        if let Some(rest) = strip_exact_tag(line, tag) {
            return Some((rest, TemplateBindingKind::Mixin));
        }
    }

    None
}

fn strip_param_prefix(s: &str) -> &str {
    s.strip_prefix("&...$")
        .or_else(|| s.strip_prefix("...$"))
        .or_else(|| s.strip_prefix("&$"))
        .or_else(|| s.strip_prefix('$'))
        .unwrap_or(s)
}

fn parse_param_tag(rest: &str, doc: &mut PhpDoc) {
    let Some((type_str, name_str, desc)) = split_type_variable_description(rest) else {
        return;
    };
    let name = strip_param_prefix(name_str).to_string();

    doc.params.push(PhpDocParam {
        name,
        type_info: type_str.map(parse_type_string),
        description: desc,
    });
}

fn parse_template_tag(rest: &str, variance: TemplateVariance, doc: &mut PhpDoc) {
    let rest = rest.trim();
    let Some((name, remaining)) = split_first_word(rest) else {
        return;
    };

    let name = name.trim_start_matches('$').trim();
    if !is_valid_template_name(name) {
        return;
    }

    let remaining = remaining.trim_start();
    let bound = remaining
        .strip_prefix("of ")
        .or_else(|| remaining.strip_prefix("as "))
        .and_then(|bound| {
            split_type_prefix(bound).map(|(type_str, _)| parse_type_string(type_str))
        });

    doc.templates.push(TemplateParam {
        name: name.to_string(),
        bound,
        variance,
    });
}

fn split_first_word(s: &str) -> Option<(&str, &str)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    for (idx, ch) in s.char_indices() {
        if ch.is_whitespace() {
            return Some((&s[..idx], &s[idx + ch.len_utf8()..]));
        }
    }
    Some((s, ""))
}

fn is_valid_template_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn parse_template_binding_tag(rest: &str, kind: TemplateBindingKind, doc: &mut PhpDoc) {
    let Some((type_str, _)) = split_type_prefix(rest) else {
        return;
    };

    let type_info = parse_type_string(type_str);
    if let Some(binding) = template_binding_from_type(type_info, kind) {
        doc.template_bindings.push(binding);
    }
}

fn template_binding_from_type(
    type_info: TypeInfo,
    kind: TemplateBindingKind,
) -> Option<TemplateBinding> {
    match type_info {
        TypeInfo::Generic { base, args } => Some(TemplateBinding {
            kind,
            target: base,
            args,
        }),
        TypeInfo::Simple(target) => Some(TemplateBinding {
            kind,
            target,
            args: Vec::new(),
        }),
        _ => None,
    }
}

fn parse_var_tag(rest: &str, doc: &mut PhpDoc) {
    if let Some((type_str, _, _)) = split_type_variable_description(rest) {
        if let Some(type_str) = type_str {
            doc.var_type = Some(parse_type_string(type_str));
        }
        return;
    }

    if let Some((type_str, _)) = split_type_prefix(rest) {
        doc.var_type = Some(parse_type_string(type_str));
    }
}

fn parse_property_tag(rest: &str, doc: &mut PhpDoc, access: PhpDocPropertyAccess) {
    let Some((Some(type_str), name_str, desc)) = split_type_variable_description(rest) else {
        return;
    };
    let name = strip_param_prefix(name_str).to_string();

    doc.properties.push(PhpDocProperty {
        name,
        type_info: Some(parse_type_string(type_str)),
        access,
        description: desc,
    });
}

fn parse_method_tag(rest: &str, doc: &mut PhpDoc) {
    let rest = rest.trim();

    // Check for static
    let (is_static, rest) = if let Some(r) = rest.strip_prefix("static") {
        (true, r.trim_start())
    } else {
        (false, rest)
    };

    // Format: [ReturnType] name([params]) [description]
    // Simple parsing: find method name (word before '(')
    let paren_pos = match find_last_top_level_open_paren(rest) {
        Some(pos) => pos,
        None => return,
    };

    let Some((return_type, name)) = split_method_return_and_name(rest[..paren_pos].trim()) else {
        return;
    };

    doc.methods.push(PhpDocMethod {
        name,
        return_type,
        params: vec![], // Simplified — not parsing method params in PHPDoc
        is_static,
        description: None,
    });
}

fn split_type_prefix(rest: &str) -> Option<(&str, Option<String>)> {
    let rest = rest.trim();
    let end = consume_type_expr(rest)?;
    let type_str = rest[..end].trim();
    if type_str.is_empty() {
        return None;
    }
    let description = rest[end..].trim();
    Some((
        type_str,
        (!description.is_empty()).then(|| description.to_string()),
    ))
}

fn split_type_variable_description(rest: &str) -> Option<(Option<&str>, &str, Option<String>)> {
    let rest = rest.trim();
    let (name_start, name_end) = find_phpdoc_variable_token(rest)?;
    let type_str = rest[..name_start].trim();
    let name_str = &rest[name_start..name_end];
    let description = rest[name_end..].trim();

    Some((
        (!type_str.is_empty()).then_some(type_str),
        name_str,
        (!description.is_empty()).then(|| description.to_string()),
    ))
}

fn split_method_return_and_name(before_paren: &str) -> Option<(Option<TypeInfo>, String)> {
    let before_paren = before_paren.trim();
    if before_paren.is_empty() {
        return None;
    }

    let name_end = before_paren.len();
    let mut name_start = name_end;
    for (idx, ch) in before_paren.char_indices().rev() {
        if is_php_identifier_char(ch) {
            name_start = idx;
        } else {
            break;
        }
    }

    if name_start == name_end {
        return None;
    }

    let name = before_paren[name_start..name_end].to_string();
    let return_type = before_paren[..name_start].trim();
    let return_type = (!return_type.is_empty()).then(|| parse_type_string(return_type));

    Some((return_type, name))
}

fn find_last_top_level_open_paren(s: &str) -> Option<usize> {
    let mut result = None;
    let mut paren_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for (idx, ch) in s.char_indices() {
        if let Some(quote_ch) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote_ch {
                quote = None;
            }
            continue;
        }

        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }

        let nested = paren_depth > 0 || angle_depth > 0 || bracket_depth > 0 || brace_depth > 0;
        if ch == '(' && !nested {
            result = Some(idx);
        }

        match ch {
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '<' => angle_depth += 1,
            '>' => angle_depth = angle_depth.saturating_sub(1),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }
    }

    result
}

fn consume_type_expr(rest: &str) -> Option<usize> {
    let mut paren_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut last_significant: Option<char> = None;
    let mut end = 0usize;

    for (idx, ch) in rest.char_indices() {
        let ch_end = idx + ch.len_utf8();

        if let Some(quote_ch) = quote {
            end = ch_end;
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote_ch {
                quote = None;
            }
            continue;
        }

        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            last_significant = Some(ch);
            end = ch_end;
            continue;
        }

        let nested = paren_depth > 0 || angle_depth > 0 || bracket_depth > 0 || brace_depth > 0;
        if ch.is_whitespace() && !nested {
            let next = next_non_whitespace(rest, ch_end);
            if matches!(next, Some('|') | Some('&'))
                || matches!(last_significant, Some('|') | Some('&') | Some(':'))
            {
                end = ch_end;
                continue;
            }
            break;
        }

        match ch {
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '<' => angle_depth += 1,
            '>' => angle_depth = angle_depth.saturating_sub(1),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        if !ch.is_whitespace() {
            last_significant = Some(ch);
        }
        end = ch_end;
    }

    (end > 0).then_some(end)
}

fn find_phpdoc_variable_token(rest: &str) -> Option<(usize, usize)> {
    let mut paren_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for (idx, ch) in rest.char_indices() {
        if let Some(quote_ch) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote_ch {
                quote = None;
            }
            continue;
        }

        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }

        let nested = paren_depth > 0 || angle_depth > 0 || bracket_depth > 0 || brace_depth > 0;
        if ch == '$' && !nested {
            let mut name_end = idx + ch.len_utf8();
            let mut has_name = false;
            for (offset, name_ch) in rest[name_end..].char_indices() {
                if is_php_identifier_char(name_ch) {
                    has_name = true;
                    name_end = idx + ch.len_utf8() + offset + name_ch.len_utf8();
                } else {
                    break;
                }
            }

            if !has_name {
                continue;
            }

            let prefix = &rest[..idx];
            let name_start = if prefix.ends_with("&...") {
                idx - 4
            } else if prefix.ends_with("...") {
                idx - 3
            } else if prefix.ends_with('&') {
                idx - 1
            } else {
                idx
            };

            return Some((name_start, name_end));
        }

        match ch {
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '<' => angle_depth += 1,
            '>' => angle_depth = angle_depth.saturating_sub(1),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }
    }

    None
}

/// Parse a PHPDoc type string into TypeInfo.
fn parse_type_string(s: &str) -> TypeInfo {
    let s = s.trim();

    if let Some(callable) = parse_callable_signature(s) {
        return callable;
    }

    if let Some(parts) = split_top_level(s, '|') {
        let parts: Vec<TypeInfo> = parts.into_iter().map(parse_type_string).collect();
        return TypeInfo::Union(parts);
    }

    if let Some(parts) = split_top_level(s, '&') {
        let parts: Vec<TypeInfo> = parts.into_iter().map(parse_type_string).collect();
        return TypeInfo::Intersection(parts);
    }

    if let Some(inner) = s.strip_prefix('?') {
        return TypeInfo::Nullable(Box::new(parse_type_string(inner)));
    }

    if let Some(inner) = strip_enclosing_parentheses(s) {
        return parse_type_string(inner);
    }

    if let Some(shape) = parse_array_shape_type(s) {
        return shape;
    }

    if let Some(generic) = parse_generic_type(s) {
        return generic;
    }

    if let Some(literal) = parse_literal_type(s) {
        return literal;
    }

    match s.to_lowercase().as_str() {
        "void" => TypeInfo::Void,
        "never" => TypeInfo::Never,
        "mixed" => TypeInfo::Mixed,
        "self" => TypeInfo::Self_,
        "static" => TypeInfo::Static_,
        "parent" => TypeInfo::Parent_,
        "class-string" => TypeInfo::ClassString(None),
        _ => TypeInfo::Simple(s.to_string()),
    }
}

fn parse_generic_type(s: &str) -> Option<TypeInfo> {
    let (base, args) = split_generic_type(s)?;
    let parsed_args: Vec<TypeInfo> = split_top_level(args, ',')
        .unwrap_or_else(|| vec![args.trim()])
        .into_iter()
        .filter(|arg| !arg.trim().is_empty())
        .map(parse_type_string)
        .collect();

    if parsed_args.is_empty() {
        return None;
    }

    if base.eq_ignore_ascii_case("class-string") {
        return Some(TypeInfo::ClassString(
            parsed_args.into_iter().next().map(Box::new),
        ));
    }

    Some(TypeInfo::Generic {
        base: base.to_string(),
        args: parsed_args,
    })
}

fn split_generic_type(s: &str) -> Option<(&str, &str)> {
    let open = find_top_level_char(s, '<')?;
    if !s.ends_with('>') {
        return None;
    }

    let base = s[..open].trim();
    if base.is_empty() {
        return None;
    }

    let mut angle_depth = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for (idx, ch) in s[open..].char_indices() {
        let idx = open + idx;
        if let Some(quote_ch) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote_ch {
                quote = None;
            }
            continue;
        }

        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }

        match ch {
            '<' => angle_depth += 1,
            '>' => {
                angle_depth = angle_depth.saturating_sub(1);
                if angle_depth == 0 && idx + ch.len_utf8() != s.len() {
                    return None;
                }
            }
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }
    }

    if angle_depth == 0 && paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 {
        Some((base, s[open + 1..s.len() - 1].trim()))
    } else {
        None
    }
}

fn parse_array_shape_type(s: &str) -> Option<TypeInfo> {
    let body = s
        .strip_prefix("array{")
        .and_then(|body| body.strip_suffix('}'))?;
    let items = split_top_level(body, ',')
        .unwrap_or_else(|| vec![body.trim()])
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .map(parse_array_shape_item)
        .collect::<Vec<_>>();

    Some(TypeInfo::ArrayShape(items))
}

fn parse_array_shape_item(s: &str) -> ArrayShapeItem {
    let s = s.trim();
    if let Some(colon) = find_top_level_char(s, ':') {
        let mut key = s[..colon].trim().to_string();
        let optional = key.ends_with('?');
        if optional {
            key.pop();
            key = key.trim_end().to_string();
        }
        ArrayShapeItem {
            key: (!key.is_empty()).then_some(key),
            optional,
            value: parse_type_string(&s[colon + 1..]),
        }
    } else {
        ArrayShapeItem {
            key: None,
            optional: false,
            value: parse_type_string(s),
        }
    }
}

fn parse_callable_signature(s: &str) -> Option<TypeInfo> {
    let lower = s.to_ascii_lowercase();
    let prefix_len = if lower.starts_with("callable") {
        "callable".len()
    } else if lower.starts_with("\\closure") {
        "\\closure".len()
    } else if lower.starts_with("closure") {
        "closure".len()
    } else {
        return None;
    };

    let after_prefix = &s[prefix_len..];
    let leading_ws = after_prefix
        .char_indices()
        .find(|(_, ch)| !ch.is_whitespace())
        .map(|(idx, _)| idx)
        .unwrap_or(after_prefix.len());
    let open_paren = prefix_len + leading_ws;
    if !s[open_paren..].starts_with('(') {
        return None;
    }

    let close_paren = find_matching_paren(s, open_paren)?;
    let after_paren = s[close_paren + 1..].trim_start();
    let return_type = after_paren
        .strip_prefix(':')
        .map(str::trim)
        .filter(|return_type| !return_type.is_empty())
        .map(parse_type_string)
        .map(Box::new);
    if return_type.is_none() && !after_paren.is_empty() {
        return None;
    }

    let params_body = &s[open_paren + 1..close_paren];
    let params = split_top_level(params_body, ',')
        .unwrap_or_else(|| {
            if params_body.trim().is_empty() {
                Vec::new()
            } else {
                vec![params_body.trim()]
            }
        })
        .into_iter()
        .filter_map(parse_callable_param_type)
        .collect();

    Some(TypeInfo::Callable {
        params,
        return_type,
    })
}

fn parse_callable_param_type(s: &str) -> Option<TypeInfo> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Some((name_start, _)) = find_phpdoc_variable_token(s) {
        let type_part = s[..name_start].trim();
        if !type_part.is_empty() {
            return Some(parse_type_string(type_part));
        }
    }
    Some(parse_type_string(s))
}

fn parse_literal_type(s: &str) -> Option<TypeInfo> {
    let lower = s.to_ascii_lowercase();
    match lower.as_str() {
        "true" => return Some(TypeInfo::LiteralBool(true)),
        "false" => return Some(TypeInfo::LiteralBool(false)),
        "null" => return Some(TypeInfo::LiteralNull),
        _ => {}
    }

    if is_quoted_literal(s) {
        return Some(TypeInfo::LiteralString(s.to_string()));
    }
    if is_int_literal(s) {
        return Some(TypeInfo::LiteralInt(s.to_string()));
    }
    if is_float_literal(s) {
        return Some(TypeInfo::LiteralFloat(s.to_string()));
    }

    None
}

fn split_top_level(s: &str, delimiter: char) -> Option<Vec<&str>> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut found = false;
    let mut paren_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for (idx, ch) in s.char_indices() {
        if let Some(quote_ch) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote_ch {
                quote = None;
            }
            continue;
        }

        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }

        let nested = paren_depth > 0 || angle_depth > 0 || bracket_depth > 0 || brace_depth > 0;
        if ch == delimiter && !nested {
            let part = s[start..idx].trim();
            if part.is_empty() {
                return None;
            }
            parts.push(part);
            start = idx + ch.len_utf8();
            found = true;
            continue;
        }

        match ch {
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '<' => angle_depth += 1,
            '>' => angle_depth = angle_depth.saturating_sub(1),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }
    }

    if !found {
        return None;
    }

    let part = s[start..].trim();
    if part.is_empty() {
        return None;
    }
    parts.push(part);
    Some(parts)
}

fn find_top_level_char(s: &str, needle: char) -> Option<usize> {
    let mut paren_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for (idx, ch) in s.char_indices() {
        if let Some(quote_ch) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote_ch {
                quote = None;
            }
            continue;
        }

        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }

        let nested = paren_depth > 0 || angle_depth > 0 || bracket_depth > 0 || brace_depth > 0;
        if ch == needle && !nested {
            return Some(idx);
        }

        match ch {
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '<' => angle_depth += 1,
            '>' => angle_depth = angle_depth.saturating_sub(1),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }
    }

    None
}

fn strip_enclosing_parentheses(s: &str) -> Option<&str> {
    if !s.starts_with('(') || !s.ends_with(')') {
        return None;
    }

    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for (idx, ch) in s.char_indices() {
        if let Some(quote_ch) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote_ch {
                quote = None;
            }
            continue;
        }

        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }

        match ch {
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    if idx + ch.len_utf8() == s.len() {
                        return Some(s[1..idx].trim());
                    }
                    return None;
                }
            }
            _ => {}
        }
    }

    None
}

fn find_matching_paren(s: &str, open_paren: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for (idx, ch) in s[open_paren..].char_indices() {
        let idx = open_paren + idx;

        if let Some(quote_ch) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote_ch {
                quote = None;
            }
            continue;
        }

        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }

        match ch {
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }

    None
}

fn next_non_whitespace(s: &str, start: usize) -> Option<char> {
    s[start..].chars().find(|ch| !ch.is_whitespace())
}

fn is_php_identifier_char(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric() || !ch.is_ascii()
}

fn is_quoted_literal(s: &str) -> bool {
    if s.len() < 2 {
        return false;
    }
    (s.starts_with('\'') && s.ends_with('\'')) || (s.starts_with('"') && s.ends_with('"'))
}

fn is_int_literal(s: &str) -> bool {
    let digits = s.strip_prefix('-').unwrap_or(s);
    !digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit())
}

fn is_float_literal(s: &str) -> bool {
    let digits = s.strip_prefix('-').unwrap_or(s);
    if digits.is_empty() || !digits.contains('.') {
        return false;
    }
    let mut dot_seen = false;
    let mut digit_seen = false;
    for ch in digits.chars() {
        if ch == '.' && !dot_seen {
            dot_seen = true;
        } else if ch.is_ascii_digit() {
            digit_seen = true;
        } else {
            return false;
        }
    }
    dot_seen && digit_seen
}

#[cfg(test)]
mod tests {
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
        let doc =
            parse_phpdoc("/**\n * @method string getName()\n * @method static Foo create()\n */");
        assert_eq!(doc.methods.len(), 2);
        assert_eq!(doc.methods[0].name, "getName");
        assert!(!doc.methods[0].is_static);
        assert_eq!(doc.methods[1].name, "create");
        assert!(doc.methods[1].is_static);
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
            "/**\n * @return array{status: 'ok', count?: 1, active: true, ratio: 1.5}\n */",
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
    fn test_malformed_tags_are_ignored() {
        let doc =
            parse_phpdoc("/**\n * @param array<int, User>\n * @property string\n * @return\n */");
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
}
