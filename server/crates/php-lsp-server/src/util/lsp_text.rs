use php_lsp_parser::utf16::{range_byte_to_utf16, utf16_col_to_byte};
use tower_lsp::ls_types::{Position, Range};

pub(crate) fn range_from_lsp_tuple(range: (u32, u32, u32, u32)) -> Range {
    Range {
        start: Position::new(range.0, range.1),
        end: Position::new(range.2, range.3),
    }
}

pub(crate) fn range_from_byte_range(source: &str, range: (u32, u32, u32, u32)) -> Range {
    range_from_lsp_tuple(range_byte_to_utf16(source, range))
}

/// Convert a parser/tree-sitter byte line and column to a source byte offset.
/// Oversized columns stop at the requested line's end instead of crossing into
/// following lines.
pub(crate) fn byte_offset_for_line_col(source: &str, line: u32, byte_col: u32) -> Option<usize> {
    let mut current_line = 0u32;
    let mut line_start = 0usize;

    for (offset, byte) in source.bytes().enumerate() {
        if current_line == line && byte == b'\n' {
            let line_len = offset - line_start;
            return Some(line_start + (byte_col as usize).min(line_len));
        }
        if byte == b'\n' {
            current_line += 1;
            line_start = offset + 1;
        }
    }

    (current_line == line).then(|| {
        let line_len = source.len() - line_start;
        line_start + (byte_col as usize).min(line_len)
    })
}

/// Convert an LSP UTF-16 position to a byte offset in `source`.
pub(crate) fn lsp_position_to_byte(source: &str, position: Position) -> Option<usize> {
    let byte_col = utf16_col_to_byte(source, position.line, position.character) as usize;
    let mut offset = 0usize;

    for (current_line, row) in source.split_inclusive('\n').enumerate() {
        if current_line as u32 == position.line {
            return Some(offset + byte_col.min(row.len()));
        }
        offset += row.len();
    }

    if position.line as usize == source.lines().count() {
        Some(source.len())
    } else {
        None
    }
}

/// Return the text covered by an LSP UTF-16 range.
pub(crate) fn text_at_lsp_range(source: &str, range: Range) -> Option<&str> {
    let start = lsp_position_to_byte(source, range.start)?;
    let end = lsp_position_to_byte(source, range.end)?;
    source.get(start..end)
}

#[cfg(test)]
#[path = "lsp_text_tests.rs"]
mod tests;
