//! PHPDoc comment parser.
//!
//! Extracts type information and documentation from PHPDoc comments.
//! Supports: @param, @return, @var, @throws, @deprecated, @property, @method.

use php_lsp_types::{PhpDoc, PhpDocMethod, PhpDocParam, PhpDocProperty, TypeInfo};

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
        if !rest.is_empty() {
            doc.return_type = Some(parse_type_string(first_word(rest)));
        }
    } else if let Some(rest) = line.strip_prefix("@var") {
        let rest = rest.trim();
        if !rest.is_empty() {
            doc.var_type = Some(parse_type_string(first_word(rest)));
        }
    } else if let Some(rest) = line.strip_prefix("@throws") {
        let rest = rest.trim();
        if !rest.is_empty() {
            doc.throws.push(parse_type_string(first_word(rest)));
        }
    } else if let Some(rest) = line.strip_prefix("@deprecated") {
        let rest = rest.trim();
        doc.deprecated = Some(if rest.is_empty() {
            "Deprecated".to_string()
        } else {
            rest.to_string()
        });
    } else if let Some(rest) = line.strip_prefix("@property-read") {
        parse_property_tag(rest.trim(), doc);
    } else if let Some(rest) = line.strip_prefix("@property-write") {
        parse_property_tag(rest.trim(), doc);
    } else if let Some(rest) = line.strip_prefix("@property") {
        parse_property_tag(rest.trim(), doc);
    } else if let Some(rest) = line.strip_prefix("@method") {
        parse_method_tag(rest.trim(), doc);
    }
}

fn parse_param_tag(rest: &str, doc: &mut PhpDoc) {
    let parts: Vec<&str> = rest.splitn(3, char::is_whitespace).collect();
    if parts.is_empty() {
        return;
    }

    let (type_str, name_str, desc) = if parts[0].starts_with('$') {
        // @param $name — no type
        (None, parts[0], parts.get(1).map(|s| s.to_string()))
    } else if parts.len() >= 2 && parts[1].starts_with('$') {
        // @param Type $name [description]
        (
            Some(parts[0]),
            parts[1],
            parts.get(2).map(|s| s.to_string()),
        )
    } else {
        return;
    };

    let name = name_str.strip_prefix('$').unwrap_or(name_str).to_string();

    doc.params.push(PhpDocParam {
        name,
        type_info: type_str.map(parse_type_string),
        description: desc,
    });
}

fn parse_property_tag(rest: &str, doc: &mut PhpDoc) {
    let parts: Vec<&str> = rest.splitn(3, char::is_whitespace).collect();
    if parts.len() < 2 {
        return;
    }

    let type_str = parts[0];
    let name_str = parts[1];
    let desc = parts.get(2).map(|s| s.to_string());
    let name = name_str.strip_prefix('$').unwrap_or(name_str).to_string();

    doc.properties.push(PhpDocProperty {
        name,
        type_info: Some(parse_type_string(type_str)),
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
    let paren_pos = match rest.find('(') {
        Some(pos) => pos,
        None => return,
    };

    let before_paren = rest[..paren_pos].trim();
    let parts: Vec<&str> = before_paren.rsplitn(2, char::is_whitespace).collect();

    let (return_type, name) = if parts.len() == 2 {
        (Some(parse_type_string(parts[1])), parts[0].to_string())
    } else {
        (None, parts[0].to_string())
    };

    doc.methods.push(PhpDocMethod {
        name,
        return_type,
        params: vec![], // Simplified — not parsing method params in PHPDoc
        is_static,
        description: None,
    });
}

/// Parse a simple type string into TypeInfo.
fn parse_type_string(s: &str) -> TypeInfo {
    let s = s.trim();

    if s.contains('|') {
        let parts: Vec<TypeInfo> = s.split('|').map(|p| parse_type_string(p.trim())).collect();
        return TypeInfo::Union(parts);
    }

    if s.contains('&') && !s.contains("&$") {
        let parts: Vec<TypeInfo> = s.split('&').map(|p| parse_type_string(p.trim())).collect();
        return TypeInfo::Intersection(parts);
    }

    if let Some(inner) = s.strip_prefix('?') {
        return TypeInfo::Nullable(Box::new(parse_type_string(inner)));
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

/// Get the first whitespace-delimited word from a string.
fn first_word(s: &str) -> &str {
    s.split_whitespace().next().unwrap_or(s)
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
        assert_eq!(doc.properties[1].name, "id");
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
