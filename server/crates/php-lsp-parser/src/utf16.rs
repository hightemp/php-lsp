//! UTF-16 ↔ byte offset conversion utilities.
//!
//! LSP uses UTF-16 code units for `Position.character`.
//! Tree-sitter uses byte offsets for `Point.column`.
//! This module provides conversion between the two.

/// Build a line-indexed lookup table for a source string.
///
/// Each entry in the returned Vec corresponds to a source line and contains
/// a list of (byte_offset_in_line, utf16_offset_in_line) for every UTF-8
/// character boundary. For ASCII-only lines the entry is empty, making the
/// common case essentially free.
///
/// Prefer building this once per file version and reusing it for all position
/// conversions in that file.
pub struct Utf16LineIndex {
    /// For each line, a sorted list of (byte_offset, utf16_offset) at UTF-8
    /// character boundaries. Empty for ASCII-only lines.
    lines: Vec<Vec<(usize, usize)>>,
    /// UTF-8 byte length for each indexed line.
    line_byte_lengths: Vec<usize>,
}

impl Utf16LineIndex {
    /// Build the index from source text.
    pub fn new(source: &str) -> Self {
        let mut lines = Vec::new();
        let mut line_byte_lengths = Vec::new();
        for line_text in source.split('\n') {
            line_byte_lengths.push(line_text.len());
            // Check if line is ASCII-only (fast path)
            if line_text.is_ascii() {
                lines.push(Vec::new());
            } else {
                let mut mappings = Vec::new();
                let mut byte_off = 0usize;
                let mut utf16_off = 0usize;
                for ch in line_text.chars() {
                    let ch_bytes = ch.len_utf8();
                    let ch_utf16 = ch.len_utf16();
                    byte_off += ch_bytes;
                    utf16_off += ch_utf16;
                    mappings.push((byte_off, utf16_off));
                }
                lines.push(mappings);
            }
        }
        Utf16LineIndex {
            lines,
            line_byte_lengths,
        }
    }

    /// Convert a tree-sitter byte column to a UTF-16 column for LSP.
    pub fn byte_col_to_utf16(&self, line: u32, byte_col: u32) -> u32 {
        let line = line as usize;
        if line >= self.lines.len() {
            return byte_col;
        }
        let mappings = &self.lines[line];
        if mappings.is_empty() {
            // ASCII-only line: byte offset == UTF-16 offset
            return byte_col.min(self.line_len_utf8(line));
        }
        let byte_col = byte_col as usize;
        let mut utf16_col = byte_col;
        for &(b, u) in mappings.iter() {
            if byte_col < b {
                break;
            }
            if byte_col == b {
                utf16_col = u;
                break;
            } else {
                utf16_col = u;
            }
        }
        utf16_col as u32
    }

    fn line_len_utf8(&self, line: usize) -> u32 {
        self.line_byte_lengths
            .get(line)
            .copied()
            .unwrap_or(u32::MAX as usize) as u32
    }
}

/// Convert a single tree-sitter byte column to UTF-16 column given source text.
///
/// Use this when you only need a one-off conversion and don't want to build
/// the full index.
pub fn byte_col_to_utf16(source: &str, line: u32, byte_col: u32) -> u32 {
    let line_text = match source.split('\n').nth(line as usize) {
        Some(l) => l,
        None => return byte_col,
    };

    if line_text.is_ascii() {
        return byte_col.min(line_text.len() as u32);
    }

    let byte_col = byte_col as usize;
    let mut byte_off = 0usize;
    let mut utf16_off = 0usize;

    for ch in line_text.chars() {
        if byte_col <= byte_off {
            break;
        }
        if byte_col < byte_off + ch.len_utf8() && ch.len_utf8() != ch.len_utf16() {
            break;
        }
        byte_off += ch.len_utf8();
        utf16_off += ch.len_utf16();
    }

    utf16_off as u32
}

/// Convert a (start_line, start_col, end_line, end_col) range from byte columns
/// to UTF-16 columns.
pub fn range_byte_to_utf16(source: &str, range: (u32, u32, u32, u32)) -> (u32, u32, u32, u32) {
    (
        range.0,
        byte_col_to_utf16(source, range.0, range.1),
        range.2,
        byte_col_to_utf16(source, range.2, range.3),
    )
}

/// Convert a UTF-16 column (from LSP Position.character) to a byte column
/// for use with tree-sitter.
pub fn utf16_col_to_byte(source: &str, line: u32, utf16_col: u32) -> u32 {
    let line_text = match source.split('\n').nth(line as usize) {
        Some(l) => l,
        None => return utf16_col,
    };

    if line_text.is_ascii() {
        return utf16_col.min(line_text.len() as u32);
    }

    let utf16_col = utf16_col as usize;
    let mut byte_off = 0usize;
    let mut utf16_off = 0usize;

    for ch in line_text.chars() {
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
