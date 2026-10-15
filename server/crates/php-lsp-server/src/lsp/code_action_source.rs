//! Structural source spans for edits that must preserve neighboring PHP code.
use super::*;

pub(super) struct VisibilityPlan {
    pub modifier: Option<(usize, usize)>,
    pub insert_at: usize,
}
pub(super) fn visibility_plan(
    source: &str,
    symbol: &php_lsp_types::SymbolInfo,
) -> Option<VisibilityPlan> {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let (start, end) = byte_offsets_for_range(source, symbol.range)?;
    let mut node = parser
        .tree()?
        .root_node()
        .descendant_for_byte_range(start, end)?;
    while !(node.start_byte() == start
        && node.end_byte() == end
        && matches!(
            node.kind(),
            "method_declaration"
                | "property_declaration"
                | "property_promotion_parameter"
                | "const_declaration"
        ))
    {
        node = node.parent()?;
    }
    if node.has_error() {
        return None;
    }
    let mut cursor = node.walk();
    let children = node.children(&mut cursor).collect::<Vec<_>>();
    let modifiers = children
        .iter()
        .filter(|n| matches!(n.kind(), "visibility_modifier" | "var_modifier"))
        .collect::<Vec<_>>();
    if modifiers.len() > 1 {
        return None;
    }
    let modifier = if let Some(modifier) = modifiers.first() {
        // Asymmetric/set visibility needs its own contract, not substring edits.
        if !matches!(
            source
                .get(modifier.byte_range())?
                .to_ascii_lowercase()
                .as_str(),
            "public" | "protected" | "private" | "var"
        ) {
            return None;
        }
        Some((modifier.start_byte(), modifier.end_byte()))
    } else {
        None
    };
    let insert_at = children
        .iter()
        .find(|n| !n.is_extra() && n.kind() != "attribute_list")?
        .start_byte();
    Some(VisibilityPlan {
        modifier,
        insert_at,
    })
}

#[cfg(test)]
pub(super) fn remove_import(uri: Uri, source: &str, range: Range) -> Option<WorkspaceEdit> {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    remove_import_from_tree(uri, source, range, parser.tree()?)
}

pub(super) fn remove_import_from_tree(
    uri: Uri,
    source: &str,
    range: Range,
    tree: &tree_sitter::Tree,
) -> Option<WorkspaceEdit> {
    let (start, end) = diagnostic_range_byte_offsets(source, range)?;
    if start > end {
        return None;
    }
    let mut clause = tree.root_node().descendant_for_byte_range(start, end)?;
    while clause.kind() != "namespace_use_clause" {
        clause = clause.parent()?;
    }
    let container = clause.parent()?;
    let declaration = if container.kind() == "namespace_use_group" {
        container.parent()?
    } else {
        container
    };
    if declaration.kind() != "namespace_use_declaration" || declaration.has_error() {
        return None;
    }
    let mut cursor = container.walk();
    let children = container.children(&mut cursor).collect::<Vec<_>>();
    let clauses = children
        .iter()
        .filter(|n| n.kind() == "namespace_use_clause")
        .collect::<Vec<_>>();
    let mut spans = Vec::new();
    if clauses.len() == 1 {
        let (start, end) = (declaration.start_byte(), declaration.end_byte());
        let (line_start, line_end) = line_full_span(source, start, end);
        spans.push(
            if source[line_start..start].trim().is_empty()
                && source[end..line_end].trim().is_empty()
            {
                (line_start, line_end)
            } else {
                (start, end)
            },
        );
    } else {
        let i = children.iter().position(|n| n.id() == clause.id())?;
        let comma = children[i + 1..]
            .iter()
            .find(|n| n.kind() == ",")
            .or_else(|| children[..i].iter().rev().find(|n| n.kind() == ","))?;
        spans.push((clause.start_byte(), clause.end_byte()));
        spans.push((comma.start_byte(), comma.end_byte()));
    }
    Some(workspace_edit_from_text_edits(
        uri,
        spans
            .into_iter()
            .map(|(start, end)| TextEdit {
                range: lsp_range_for_byte_offsets(source, start, end),
                new_text: String::new(),
            })
            .collect(),
    ))
}

pub(super) fn analyzer_ignore_edit(
    uri: Uri,
    source: &str,
    diagnostic: &Diagnostic,
    analyzer: ExternalAnalyzer,
) -> Option<WorkspaceEdit> {
    let (start, end) = diagnostic_range_byte_offsets(source, diagnostic.range)?;
    if start > end {
        return None;
    }
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let mut node = parser
        .tree()?
        .root_node()
        .descendant_for_byte_range(start, end)?;
    loop {
        if node.kind().ends_with("_statement") && node.kind() != "compound_statement"
            || matches!(
                node.kind(),
                "method_declaration"
                    | "function_definition"
                    | "property_declaration"
                    | "const_declaration"
                    | "class_declaration"
                    | "interface_declaration"
                    | "trait_declaration"
                    | "enum_declaration"
            )
        {
            break;
        }
        if matches!(
            node.kind(),
            "program" | "text" | "php_tag" | "namespace_definition"
        ) {
            return None;
        }
        node = node.parent()?;
    }
    if node.has_error()
        || (analyzer == ExternalAnalyzer::PhpStan
            && node.start_position().row as u32 != diagnostic.range.start.line)
    {
        return None;
    }
    let mut anchored = diagnostic.clone();
    anchored.range.start.line = node.start_position().row as u32;
    let newline = if source.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    if let Some(previous) = node.prev_named_sibling().filter(|previous| {
        previous.kind() == "comment"
            && source[previous.end_byte()..node.start_byte()]
                .trim()
                .is_empty()
            && source[previous.byte_range()].starts_with("/**")
    }) {
        // Preserve the attached PHPDoc rather than shadowing it with a second block.
        if analyzer == ExternalAnalyzer::PhpStan {
            return None;
        }
        let code = diagnostic_code_str(diagnostic)?;
        analyzer_ignore_comment(source, &anchored, analyzer)?;
        let insertion = previous.end_byte().checked_sub(2)?;
        let text = format!("{newline} * @psalm-suppress {}{newline} ", code.trim());
        return Some(workspace_edit_from_text_edits(
            uri,
            vec![TextEdit {
                range: lsp_range_for_byte_offsets(source, insertion, insertion),
                new_text: text,
            }],
        ));
    }
    let comment = analyzer_ignore_comment(source, &anchored, analyzer)?.replace('\n', newline);
    let anchor = node.start_byte();
    let line_start = line_start_offset(source, anchor);
    let (insert, new_text) = if source[line_start..anchor].trim().is_empty() {
        (line_start, comment)
    } else {
        (
            anchor,
            format!(
                "{newline}{comment}{}",
                line_indent_at_offset(source, anchor)
            ),
        )
    };
    Some(workspace_edit_from_text_edits(
        uri,
        vec![TextEdit {
            range: lsp_range_for_byte_offsets(source, insert, insert),
            new_text,
        }],
    ))
}

pub(super) fn organizable_block_is_safe(
    tree: &tree_sitter::Tree,
    source: &str,
    file: &php_lsp_types::FileSymbols,
    start_line: u32,
    end_line: u32,
) -> bool {
    let Some(start) = byte_offset_for_line_col(source, start_line, 0) else {
        return false;
    };
    let end = byte_offset_for_line_col(source, end_line, 0).unwrap_or(source.len());
    let mut declarations = Vec::new();
    for import in &file.use_statements {
        let Some((a, b)) = byte_offsets_for_range(source, import.range) else {
            return false;
        };
        let Some(mut node) = tree.root_node().descendant_for_byte_range(a, b) else {
            return false;
        };
        while node.kind() != "namespace_use_declaration" {
            let Some(parent) = node.parent() else {
                return false;
            };
            node = parent;
        }
        if node.has_error() || node.start_byte() < start || node.end_byte() > end {
            return false;
        }
        let mut pending = vec![node];
        while let Some(node) = pending.pop() {
            if node.kind() == "comment" {
                return false;
            }
            let mut cursor = node.walk();
            pending.extend(node.named_children(&mut cursor));
        }
        declarations.push((node.start_byte(), node.end_byte()));
    }
    declarations.sort_unstable();
    declarations.dedup();
    let mut cursor = start;
    for (a, b) in declarations {
        if a < cursor || !source[cursor..a].trim().is_empty() {
            return false;
        }
        cursor = b;
    }
    source[cursor..end].trim().is_empty()
}
