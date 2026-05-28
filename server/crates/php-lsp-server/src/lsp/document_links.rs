//! Document Links LSP handlers extracted from `server.rs`.

use crate::util::lsp_text::range_from_byte_range;

use super::super::*;
use std::path::{Path, PathBuf};

fn is_document_link_include_expression(kind: &str) -> bool {
    matches!(
        kind,
        "include_expression"
            | "include_once_expression"
            | "require_expression"
            | "require_once_expression"
    )
}

pub(super) fn is_static_string_literal_node(node: tree_sitter::Node) -> bool {
    if !matches!(node.kind(), "string" | "encapsed_string") {
        return false;
    }

    let mut cursor = node.walk();
    let is_static = node
        .named_children(&mut cursor)
        .all(|child| matches!(child.kind(), "string_content" | "escape_sequence"));
    is_static
}

fn unescape_static_php_string(content: &str, quote: char) -> String {
    let mut result = String::with_capacity(content.len());
    let mut chars = content.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            result.push(ch);
            continue;
        }

        let Some(escaped) = chars.next() else {
            result.push('\\');
            break;
        };

        if quote == '\'' {
            match escaped {
                '\\' | '\'' => result.push(escaped),
                other => {
                    result.push('\\');
                    result.push(other);
                }
            }
            continue;
        }

        match escaped {
            'n' => result.push('\n'),
            'r' => result.push('\r'),
            't' => result.push('\t'),
            'v' => result.push('\u{000b}'),
            'e' => result.push('\u{001b}'),
            'f' => result.push('\u{000c}'),
            '\\' | '$' | '"' => result.push(escaped),
            other => {
                result.push('\\');
                result.push(other);
            }
        }
    }
    result
}

fn static_string_literal_value(source: &str, node: tree_sitter::Node) -> Option<String> {
    if !is_static_string_literal_node(node) {
        return None;
    }

    let raw = node_text(source, node).trim();
    let mut chars = raw.char_indices();
    let (start_idx, first) = chars.next()?;
    let (quote_start, quote) = if matches!(first, 'b' | 'B') {
        let (idx, ch) = chars.next()?;
        (idx, ch)
    } else {
        (start_idx, first)
    };

    if !matches!(quote, '\'' | '"') || !raw.ends_with(quote) {
        return None;
    }

    let content_start = quote_start + quote.len_utf8();
    let content_end = raw.len().checked_sub(quote.len_utf8())?;
    if content_start > content_end {
        return None;
    }

    Some(unescape_static_php_string(
        &raw[content_start..content_end],
        quote,
    ))
}

fn binary_expression_is_concat(source: &str, node: tree_sitter::Node) -> bool {
    let Some(left) = node
        .child_by_field_name("left")
        .or_else(|| node.named_child(0))
    else {
        return false;
    };
    let Some(right) = node
        .child_by_field_name("right")
        .or_else(|| node.named_child(1))
    else {
        return false;
    };

    source
        .get(left.end_byte()..right.start_byte())
        .is_some_and(|operator| operator.contains('.'))
}

fn first_call_argument_node(node: tree_sitter::Node) -> Option<tree_sitter::Node> {
    let arguments = node.child_by_field_name("arguments").or_else(|| {
        let mut cursor = node.walk();
        let arguments = node
            .named_children(&mut cursor)
            .find(|child| child.kind() == "arguments");
        arguments
    })?;

    let mut cursor = arguments.walk();
    let first = arguments.named_children(&mut cursor).find_map(|argument| {
        argument
            .child_by_field_name("value")
            .or_else(|| argument.named_child(0))
            .or(Some(argument))
    });
    first
}

fn static_include_expression_value(
    source: &str,
    node: tree_sitter::Node,
    file_path: &Path,
    file_dir: &Path,
) -> Option<String> {
    match node.kind() {
        "string" | "encapsed_string" => static_string_literal_value(source, node),
        "binary_expression" if binary_expression_is_concat(source, node) => {
            let left = node
                .child_by_field_name("left")
                .or_else(|| node.named_child(0))?;
            let right = node
                .child_by_field_name("right")
                .or_else(|| node.named_child(1))?;
            let mut value = static_include_expression_value(source, left, file_path, file_dir)?;
            value.push_str(&static_include_expression_value(
                source, right, file_path, file_dir,
            )?);
            Some(value)
        }
        "parenthesized_expression" => {
            let inner = node.named_child(0)?;
            static_include_expression_value(source, inner, file_path, file_dir)
        }
        "function_call_expression" => {
            let function = node
                .child_by_field_name("function")
                .or_else(|| node.named_child(0))?;
            if !node_text(source, function).eq_ignore_ascii_case("dirname") {
                return None;
            }
            let argument = first_call_argument_node(node)?;
            let value = static_include_expression_value(source, argument, file_path, file_dir)?;
            Path::new(&value)
                .parent()
                .map(|parent| parent.to_string_lossy().into_owned())
        }
        _ => {
            let raw = node_text(source, node).trim();
            if raw.eq_ignore_ascii_case("__DIR__") {
                Some(file_dir.to_string_lossy().into_owned())
            } else if raw.eq_ignore_ascii_case("__FILE__") {
                Some(file_path.to_string_lossy().into_owned())
            } else {
                None
            }
        }
    }
}

fn document_link_target_path(
    source: &str,
    expression: tree_sitter::Node,
    file_path: &Path,
    file_dir: &Path,
) -> Option<PathBuf> {
    let raw_path = static_include_expression_value(source, expression, file_path, file_dir)?;
    let path = PathBuf::from(raw_path);
    let path = if path.is_absolute() {
        path
    } else {
        file_dir.join(path)
    };
    path.is_file().then_some(path)
}

fn collect_document_links(
    node: tree_sitter::Node,
    source: &str,
    file_path: &Path,
    file_dir: &Path,
    links: &mut Vec<DocumentLink>,
) {
    if is_document_link_include_expression(node.kind()) {
        if let Some(expression) = node.named_child(0) {
            if let Some(target_path) =
                document_link_target_path(source, expression, file_path, file_dir)
            {
                if let Ok(target) = path_to_uri(&target_path).parse::<Uri>() {
                    links.push(DocumentLink {
                        range: range_from_byte_range(source, node_byte_range(expression)),
                        target: Some(target),
                        tooltip: Some(target_path.display().to_string()),
                        data: None,
                    });
                }
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_document_links(child, source, file_path, file_dir, links);
    }
}

fn document_links_for_source(
    source: &str,
    tree: &tree_sitter::Tree,
    file_path: &Path,
) -> Vec<DocumentLink> {
    let Some(file_dir) = file_path.parent() else {
        return Vec::new();
    };

    let mut links = Vec::new();
    collect_document_links(tree.root_node(), source, file_path, file_dir, &mut links);
    links
}

impl PhpLspBackend {
    pub(crate) async fn lsp_document_link(
        &self,
        params: DocumentLinkParams,
    ) -> Result<Option<Vec<DocumentLink>>> {
        let uri_str = params.text_document.uri.as_str().to_string();
        let Some(file_path) = uri_to_path(&uri_str) else {
            return Ok(None);
        };

        let links = if let Some(parser) = self.open_files.get(&uri_str) {
            let Some(tree) = parser.tree() else {
                return Ok(None);
            };
            document_links_for_source(&parser.source(), tree, &file_path)
        } else {
            let Ok(source) =
                read_file_to_string_blocking(file_path.clone(), "documentLink source read").await
            else {
                return Ok(None);
            };
            let mut parser = FileParser::new();
            parser.parse_full(&source);
            let Some(tree) = parser.tree() else {
                return Ok(None);
            };
            document_links_for_source(&source, tree, &file_path)
        };

        if links.is_empty() {
            Ok(None)
        } else {
            Ok(Some(links))
        }
    }
}
