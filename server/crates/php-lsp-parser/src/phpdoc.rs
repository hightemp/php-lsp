//! PHPDoc comment parser.
//!
//! Extracts type information and documentation from PHPDoc comments.
//! Supports: @param, @return, @var, @throws, @deprecated, @property, @method.

use php_lsp_types::{
    PhpDoc, PhpDocMethod, PhpDocParam, PhpDocProperty, PhpDocPropertyAccess, TypeInfo,
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
    if let Some(rest) = line.strip_prefix("@param") {
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

    if is_callable_signature(s) {
        return TypeInfo::Simple(s.to_string());
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

    match s.to_lowercase().as_str() {
        "void" => TypeInfo::Void,
        "never" => TypeInfo::Never,
        "mixed" => TypeInfo::Mixed,
        "self" => TypeInfo::Self_,
        "static" => TypeInfo::Static_,
        "parent" => TypeInfo::Parent_,
        _ => TypeInfo::Simple(s.to_string()),
    }
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

fn is_callable_signature(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    let prefix_len = if lower.starts_with("callable") {
        "callable".len()
    } else if lower.starts_with("\\closure") {
        "\\closure".len()
    } else if lower.starts_with("closure") {
        "closure".len()
    } else {
        return false;
    };

    let after_prefix = &s[prefix_len..];
    let leading_ws = after_prefix
        .char_indices()
        .find(|(_, ch)| !ch.is_whitespace())
        .map(|(idx, _)| idx)
        .unwrap_or(after_prefix.len());
    let open_paren = prefix_len + leading_ws;
    if !s[open_paren..].starts_with('(') {
        return false;
    }

    let Some(close_paren) = find_matching_paren(s, open_paren) else {
        return false;
    };

    s[close_paren + 1..].trim_start().starts_with(':')
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
        assert_eq!(
            doc.params[0].type_info,
            Some(TypeInfo::Simple("array<int, User>".to_string()))
        );
        assert_eq!(doc.params[0].description.as_deref(), Some("The users"));
    }

    #[test]
    fn test_parse_nested_generic_type_with_spaces() {
        let doc =
            parse_phpdoc("/**\n * @param array<int, array<string, User>> $users Nested users\n */");
        assert_eq!(
            doc.params[0].type_info,
            Some(TypeInfo::Simple(
                "array<int, array<string, User>>".to_string()
            ))
        );
        assert_eq!(doc.params[0].description.as_deref(), Some("Nested users"));
    }

    #[test]
    fn test_parse_return_list_type() {
        let doc = parse_phpdoc("/**\n * @return list<User> Users\n */");
        assert_eq!(
            doc.return_type,
            Some(TypeInfo::Simple("list<User>".to_string()))
        );
    }

    #[test]
    fn test_parse_var_class_string_with_variable() {
        let doc = parse_phpdoc("/** @var class-string<T> $class */");
        assert_eq!(
            doc.var_type,
            Some(TypeInfo::Simple("class-string<T>".to_string()))
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
        assert_eq!(parts[1], TypeInfo::Simple("null".to_string()));
    }

    #[test]
    fn test_parse_callable_return_type() {
        let doc = parse_phpdoc("/**\n * @return callable(A): B Handler\n */");
        assert_eq!(
            doc.return_type,
            Some(TypeInfo::Simple("callable(A): B".to_string()))
        );
    }

    #[test]
    fn test_parse_param_callable_ignores_nested_variable_token() {
        let doc = parse_phpdoc("/**\n * @param callable($value): string $callback Callback\n */");
        assert_eq!(doc.params.len(), 1);
        assert_eq!(doc.params[0].name, "callback");
        assert_eq!(
            doc.params[0].type_info,
            Some(TypeInfo::Simple("callable($value): string".to_string()))
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
            Some(TypeInfo::Simple("callable(A): B".to_string()))
        );
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
