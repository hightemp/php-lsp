//! Ordered selection chains built from one coherent original/virtual document.
use super::*;
use php_lsp_parser::utf16::{utf16_col_to_byte_in_line, Utf16LineIndex};

// SelectionRange serializes and drops recursively. Keep the closest useful
// ranges within a bounded response depth; LSP does not require every ancestor.
const MAX_SELECTION_RANGES: usize = 64;

struct SourcePositions<'a> {
    lines: Vec<&'a str>,
    starts: Vec<usize>,
    utf16: Utf16LineIndex,
}
impl<'a> SourcePositions<'a> {
    fn new(source: &'a str) -> Self {
        let mut offset = 0;
        let mut starts = Vec::new();
        let lines = source
            .split('\n')
            .map(|line| {
                starts.push(offset);
                offset += line.len() + 1;
                line.strip_suffix('\r').unwrap_or(line)
            })
            .collect();
        Self {
            lines,
            starts,
            utf16: Utf16LineIndex::new(source),
        }
    }
    fn normalize(&self, position: Position) -> Option<(Position, usize)> {
        let line = *self.lines.get(position.line as usize)?;
        let byte = utf16_col_to_byte_in_line(line, position.character);
        Some((
            Position::new(
                position.line,
                self.utf16.byte_col_to_utf16(position.line, byte),
            ),
            self.starts[position.line as usize] + byte as usize,
        ))
    }
    fn point(&self, offset: usize) -> tree_sitter::Point {
        let row = self
            .starts
            .partition_point(|start| *start <= offset)
            .saturating_sub(1);
        tree_sitter::Point::new(row, offset - self.starts[row])
    }
    fn position(&self, offset: usize) -> Position {
        let point = self.point(offset);
        let column = point.column.min(self.lines[point.row].len());
        Position::new(
            point.row as u32,
            self.utf16
                .byte_col_to_utf16(point.row as u32, column as u32),
        )
    }
    fn range(&self, start: usize, end: usize) -> Range {
        Range::new(self.position(start), self.position(end))
    }
}

fn selection_ranges(
    tree: &tree_sitter::Tree,
    virtual_source: &str,
    template: Option<&TemplateDocument>,
    positions: &[Position],
) -> Result<Vec<SelectionRange>> {
    let source = template.map_or(virtual_source, TemplateDocument::original_source);
    let original = SourcePositions::new(source);
    let virtual_positions = template.map(|_| SourcePositions::new(virtual_source));
    // Validate the complete batch against original source, including a trailing
    // empty EOF line. Invalid lines never produce a shortened successful array.
    let positions = positions
        .iter()
        .map(|&position| {
            original.normalize(position).ok_or_else(|| {
                tower_lsp::jsonrpc::Error::invalid_params(
                    "Selection position line is outside the document",
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut results = Vec::with_capacity(positions.len());
    for (position, offset) in positions {
        let mapped = template.map_or(Some(offset), |template| {
            template.map_original_byte_to_virtual(offset)
        });
        let coordinates = virtual_positions.as_ref().unwrap_or(&original);
        let mut node = mapped.and_then(|offset| {
            let point = coordinates.point(offset);
            tree.root_node().descendant_for_point_range(point, point)
        });
        let mut ranges: Vec<Range> = Vec::new();
        while let Some(current) = node {
            if current.is_named() && current.kind() != "program" {
                let bytes = template.map_or(
                    Some((current.start_byte(), current.end_byte())),
                    |template| {
                        template.map_virtual_bytes_to_original_exact(
                            current.start_byte(),
                            current.end_byte(),
                        )
                    },
                );
                if let Some((start, end)) = bytes {
                    let range = original.range(start, end);
                    if range.start <= position
                        && position <= range.end
                        && ranges.last().is_none_or(|child| {
                            range != *child && range.start <= child.start && child.end <= range.end
                        })
                    {
                        ranges.push(range);
                        if ranges.len() == MAX_SELECTION_RANGES {
                            break;
                        }
                    }
                }
            }
            node = current.parent();
        }
        let mut parent = None;
        for range in ranges.into_iter().rev() {
            parent = Some(Box::new(SelectionRange { range, parent }));
        }
        results.push(parent.map(|range| *range).unwrap_or(SelectionRange {
            range: Range::new(position, position),
            parent: None,
        }));
    }
    Ok(results)
}

fn selection_ranges_for_open_document(
    open: &DashMap<String, FileParser>,
    templates: &DashMap<String, TemplateDocument>,
    uri: &str,
    positions: &[Position],
    after_lock: impl FnOnce(),
) -> Result<Option<Vec<SelectionRange>>> {
    if positions.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let Some(parser) = open.get(uri) else {
        return Ok(None);
    };
    after_lock();
    let Some(tree) = parser.tree() else {
        return Ok(None);
    };
    let source = parser.source();
    let template = templates.get(uri);
    // Writers hold the parser entry while publishing template state. No await
    // or index access here: every position sees the same document lifetime.
    selection_ranges(tree, &source, template.as_deref(), positions).map(Some)
}

impl PhpLspBackend {
    pub(crate) async fn lsp_selection_range(
        &self,
        params: SelectionRangeParams,
    ) -> Result<Option<Vec<SelectionRange>>> {
        selection_ranges_for_open_document(
            &self.open_files,
            &self.template_documents,
            params.text_document.uri.as_str(),
            &params.positions,
            || {},
        )
    }
}

#[cfg(test)]
#[path = "selection_ranges_tests.rs"]
mod tests;
