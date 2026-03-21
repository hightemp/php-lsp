//! UTF-16 ↔ byte offset conversion utilities.
//!
//! LSP uses UTF-16 code units for `Position.character`.
//! Tree-sitter uses byte offsets for `Point.column`.
//! This module provides conversion between the two.

/// Build a line-indexed lookup table for a source string.
///
/// Each entry in the returned Vec corresponds to a source line and contains
/// a list of (byte_offset_in_line, utf16_offset_in_line) for every character
/// whose cumulative UTF-16 offset diverges from the byte offset. For ASCII-only
/// lines the entry is empty, making the common case essentially free.
///
/// Prefer building this once per file version and reusing it for all position
/// conversions in that file.
pub struct Utf16LineIndex {
    /// For each line, a sorted list of (byte_offset, utf16_offset) at each
    /// character boundary where they differ. Empty for ASCII-only lines.
    lines: Vec<Vec<(usize, usize)>>,
}

impl Utf16LineIndex {
    /// Build the index from source text.
    pub fn new(source: &str) -> Self {
        let mut lines = Vec::new();
        for line_text in source.split('\n') {
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
                    if byte_off != utf16_off {
                        mappings.push((byte_off, utf16_off));
                    }
                }
                lines.push(mappings);
            }
        }
        Utf16LineIndex { lines }
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
            return byte_col;
        }
        let byte_col = byte_col as usize;
        // Find the last mapping at or before byte_col
        let mut utf16_col = byte_col;
        for &(b, u) in mappings.iter() {
            if b <= byte_col {
                // At this byte offset the UTF-16 offset is `u`.
                // Characters past this point: byte_col - b bytes, which are
                // all after the divergence point, so we extrapolate:
                // But actually `u` is the cumulative utf16 offset AT byte offset `b`.
                // Any remaining bytes from `b` to `byte_col` are within the
                // next character, so the UTF-16 offset at `byte_col` is just `u`
                // plus whatever comes after.
                utf16_col = u + (byte_col - b);
            } else {
                break;
            }
        }
        utf16_col as u32
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
        return byte_col;
    }

    let byte_col = byte_col as usize;
    let mut byte_off = 0usize;
    let mut utf16_off = 0usize;

    for ch in line_text.chars() {
        if byte_off >= byte_col {
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
        return utf16_col;
    }

    let utf16_col = utf16_col as usize;
    let mut byte_off = 0usize;
    let mut utf16_off = 0usize;

    for ch in line_text.chars() {
        if utf16_off >= utf16_col {
            break;
        }
        byte_off += ch.len_utf8();
        utf16_off += ch.len_utf16();
    }

    byte_off as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ascii_only() {
        let source = "hello world\nfoo bar\n";
        assert_eq!(byte_col_to_utf16(source, 0, 5), 5);
        assert_eq!(byte_col_to_utf16(source, 1, 3), 3);
    }

    #[test]
    fn test_cyrillic() {
        // Cyrillic: each char is 2 bytes UTF-8, 1 code unit UTF-16
        let source = "<?php\n$x = 'Тест';\n";
        // Line 1: $x = 'Тест';
        // bytes:  $ x   =   '  Т(2B) е(2B) с(2B) т(2B) '  ;
        // byte:   0 1 2 3 4 5 6   8   10  12  14 15 16
        // utf16:  0 1 2 3 4 5 6   7    8   9  10 11 12
        // The semicolon is at byte 16, utf16 12
        assert_eq!(byte_col_to_utf16(source, 1, 6), 6); // start of Т
        assert_eq!(byte_col_to_utf16(source, 1, 8), 7); // after Т
        assert_eq!(byte_col_to_utf16(source, 1, 14), 10); // after т
    }

    #[test]
    fn test_index_matches_function() {
        let source = "<?php\n$msg = 'Привет мир';\necho $msg;\n";
        let idx = Utf16LineIndex::new(source);
        for line in 0..3u32 {
            for col in 0..30u32 {
                assert_eq!(
                    idx.byte_col_to_utf16(line, col),
                    byte_col_to_utf16(source, line, col),
                    "mismatch at line={}, col={}",
                    line,
                    col
                );
            }
        }
    }
}
