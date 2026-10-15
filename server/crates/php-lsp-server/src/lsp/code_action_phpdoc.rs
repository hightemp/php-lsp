//! PHPDoc facts shared by safe import removal and inherited contract comparison.
use super::*;

pub(crate) struct ScopedDoc {
    line: u32,
    column: u32,
    doc: php_lsp_types::PhpDoc,
    opaque_annotation: Option<String>,
}

pub(super) fn class_annotation(tag: &str) -> bool {
    let Some(name) = tag.strip_prefix('@') else {
        return false;
    };
    name.contains('\\') || name.chars().next().is_some_and(|c| c.is_uppercase())
}

pub(super) fn base_tag(tag: &str) -> &str {
    tag.strip_prefix("@phpstan-")
        .or_else(|| tag.strip_prefix("@psalm-"))
        .unwrap_or_else(|| tag.trim_start_matches('@'))
}

pub(super) fn blocks(doc: &str) -> Vec<(String, String)> {
    let mut blocks: Vec<(String, String)> = Vec::new();
    for line in phpdoc_content_lines(doc) {
        let trimmed = line.trim();
        if trimmed.starts_with('@') {
            let (tag, rest) = trimmed
                .split_once(char::is_whitespace)
                .unwrap_or((trimmed, ""));
            blocks.push((tag.into(), rest.trim().into()));
        } else if let Some((_, rest)) = blocks.last_mut() {
            rest.push(' ');
            rest.push_str(trimmed);
        }
    }
    blocks
}

pub(super) fn source_docs(tree: &tree_sitter::Tree, source: &str) -> Vec<ScopedDoc> {
    let mut docs = Vec::new();
    let mut cursor = tree.root_node().walk();
    loop {
        let node = cursor.node();
        if node.kind() == "comment" && source[node.byte_range()].starts_with("/**") {
            for (tag, rest) in blocks(&source[node.byte_range()]) {
                let opaque_annotation = class_annotation(&tag).then(|| format!("{tag} {rest}"));
                let base = base_tag(&tag);
                let canonical = if matches!(
                    base,
                    "param"
                        | "return"
                        | "var"
                        | "throws"
                        | "template"
                        | "template-covariant"
                        | "template-contravariant"
                ) {
                    format!("@{base}")
                } else if matches!(
                    base,
                    "param-out"
                        | "param-closure-this"
                        | "assert"
                        | "assert-if-true"
                        | "assert-if-false"
                ) {
                    "@var".into()
                } else {
                    tag
                };
                let parsed = parse_phpdoc(&format!("/** {canonical} {rest} */"));
                docs.push(ScopedDoc {
                    line: node.start_position().row as u32,
                    column: node.start_position().column as u32,
                    doc: parsed,
                    opaque_annotation,
                });
            }
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return docs;
            }
        }
    }
}

pub(super) fn uses_import(docs: &[ScopedDoc], import: &OrganizableImport) -> bool {
    import.kind == ImportKind::Class
        && docs.iter().any(|fact| {
            import_scope_contains(import, fact.line, fact.column)
                && (phpdoc_uses_import(&fact.doc, import)
                    || fact.opaque_annotation.as_deref().is_some_and(|text| {
                        text.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '\\'))
                            .any(|name| phpdoc_name_uses_import(name, import))
                    }))
        })
}

pub(super) fn contract_identity(doc: &str) -> Option<Vec<String>> {
    let mut result = Vec::new();
    for (tag, rest) in blocks(doc) {
        let base = base_tag(&tag);
        if matches!(base, "param" | "return" | "var" | "throws") {
            let end = consume_phpdoc_type_expr(&rest)?;
            let mut identity = format!("{tag} {}", &rest[..end]);
            if base == "param" {
                let suffix = rest[end..].trim_start();
                let (a, b) = find_phpdoc_variable_token_span(suffix)?;
                identity.push(' ');
                identity.push_str(&suffix[a..b]);
            }
            result.push(identity);
        } else if !matches!(
            base,
            "deprecated" | "since" | "author" | "version" | "copyright" | "license" | "link"
        ) {
            result.push(format!("{tag} {rest}"));
        }
    }
    Some(result)
}
