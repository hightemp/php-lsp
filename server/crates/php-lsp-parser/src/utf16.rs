//! UTF-16 ↔ byte offset conversion utilities.
//!
//! LSP uses UTF-16 code units for `Position.character`.
//! Tree-sitter uses byte offsets for `Point.column`.
//! This module provides conversion between the two.

pub use crate::line_index::LineIndex as Utf16LineIndex;

/// Convert a full Tree-sitter point to an LSP position.
pub fn byte_point_to_lsp(source: &str, row: u32, byte_col: u32) -> (u32, u32) {
    Utf16LineIndex::new(source).point_to_position(row, byte_col)
}

/// Convert a full LSP position to a Tree-sitter byte point.
pub fn lsp_position_to_byte_point(source: &str, line: u32, character: u32) -> (u32, u32) {
    Utf16LineIndex::new(source).position_to_point(line, character)
}

/// Legacy scalar-column API; use a full point/position for protocol coordinates.
#[deprecated(note = "Use byte_point_to_lsp: bare CR can change both row and column")]
pub fn byte_col_to_utf16(source: &str, line: u32, byte_col: u32) -> u32 {
    byte_point_to_lsp(source, line, byte_col).1
}

pub fn range_byte_to_utf16(source: &str, range: (u32, u32, u32, u32)) -> (u32, u32, u32, u32) {
    Utf16LineIndex::new(source).range_to_lsp(range)
}

/// Byte column within an LSP content line, not a Tree-sitter point.
#[deprecated(note = "Use lsp_position_to_byte_point for CST queries")]
pub fn utf16_col_to_byte(source: &str, line: u32, utf16_col: u32) -> u32 {
    let index = Utf16LineIndex::new(source);
    index
        .position_to_offset(line, utf16_col)
        .zip(index.line_range(line))
        .map_or(utf16_col, |(offset, range)| (offset - range.start) as u32)
}

/// Convert within one line, clamping to content before LF or CRLF. A position
/// inside a surrogate pair resolves to the start of that Unicode scalar.
pub fn utf16_col_to_byte_in_line(line_text: &str, utf16_col: u32) -> u32 {
    let line_text = line_text.split(['\r', '\n']).next().unwrap_or(line_text);

    if line_text.is_ascii() {
        return utf16_col.min(line_text.len() as u32);
    }

    utf16_col_to_byte_chars(line_text.chars(), utf16_col)
}

/// Shared iterator form for RopeSlice lines, avoiding a full line allocation.
pub(crate) fn utf16_col_to_byte_chars(chars: impl Iterator<Item = char>, utf16_col: u32) -> u32 {
    if utf16_col == 0 {
        return 0;
    }
    let utf16_col = utf16_col as usize;
    let mut byte_off = 0usize;
    let mut utf16_off = 0usize;

    for ch in chars {
        if matches!(ch, '\r' | '\n') {
            break;
        }
        if utf16_col <= utf16_off {
            break;
        }
        if utf16_col < utf16_off + ch.len_utf16() {
            break;
        }
        byte_off += ch.len_utf8();
        utf16_off += ch.len_utf16();
    }

    byte_off as u32
}

#[cfg(test)]
#[path = "utf16_tests.rs"]
mod tests;
