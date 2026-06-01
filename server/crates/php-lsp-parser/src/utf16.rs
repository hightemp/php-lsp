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
mod tests {
    use super::*;

    fn reference_byte_to_utf16(line_text: &str, byte_col: u32) -> u32 {
        let byte_col = byte_col as usize;
        let mut byte_off = 0usize;
        let mut utf16_off = 0usize;
        for ch in line_text.chars() {
            if byte_col <= byte_off {
                break;
            }
            if byte_col < byte_off + ch.len_utf8() {
                break;
            }
            byte_off += ch.len_utf8();
            utf16_off += ch.len_utf16();
        }
        utf16_off as u32
    }

    fn reference_utf16_to_byte(line_text: &str, utf16_col: u32) -> u32 {
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

    fn line_text(source: &str, line: u32) -> &str {
        source.split('\n').nth(line as usize).unwrap_or("")
    }

    fn assert_line_conversions(source: &str, line: u32) {
        let idx = Utf16LineIndex::new(source);
        let text = line_text(source, line);
        let max_byte = text.len() as u32 + 4;
        let max_utf16 = text.encode_utf16().count() as u32 + 4;

        for byte_col in 0..=max_byte {
            let expected = reference_byte_to_utf16(text, byte_col);
            assert_eq!(
                byte_col_to_utf16(source, line, byte_col),
                expected,
                "one-off byte->utf16 mismatch for line {line}, byte_col {byte_col}, text {text:?}"
            );
            assert_eq!(
                idx.byte_col_to_utf16(line, byte_col),
                expected,
                "indexed byte->utf16 mismatch for line {line}, byte_col {byte_col}, text {text:?}"
            );
        }

        for utf16_col in 0..=max_utf16 {
            assert_eq!(
                utf16_col_to_byte(source, line, utf16_col),
                reference_utf16_to_byte(text, utf16_col),
                "utf16->byte mismatch for line {line}, utf16_col {utf16_col}, text {text:?}"
            );
        }
    }

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

    #[test]
    fn test_index_clamps_inside_multibyte_character() {
        let source = "<?php\n$x = '😀';\n";
        let idx = Utf16LineIndex::new(source);

        assert_eq!(byte_col_to_utf16(source, 1, 7), 6);
        assert_eq!(byte_col_to_utf16(source, 1, 8), 6);
        assert_eq!(byte_col_to_utf16(source, 1, 9), 6);
        for col in 0..16 {
            assert_eq!(
                idx.byte_col_to_utf16(1, col),
                byte_col_to_utf16(source, 1, col),
                "mismatch at byte col {col}"
            );
        }
    }

    #[test]
    fn test_exhaustive_unicode_line_conversions() {
        let cases = [
            "plain ascii",
            "кириллица",
            "Greek αβγ and Hebrew שלום",
            "latin combining e\u{0301} a\u{0308}",
            "precomposed é ü ñ",
            "emoji 😀😇🚀",
            "american flag 🇺🇸",
            "skin tones 👍🏽👩🏾",
            "zwj family 👨\u{200d}👩\u{200d}👧\u{200d}👦",
            "variation heart ♥\u{fe0f} text ♥",
            "mixed $var = Привет😀e\u{0301}👩\u{200d}💻;",
            "tabs\tand\tunicode\t😀",
        ];

        for case in cases {
            let source = format!("<?php\n// {case}\n$value = '{case}';\n");
            assert_line_conversions(&source, 1);
            assert_line_conversions(&source, 2);
        }
    }

    #[test]
    fn test_crlf_empty_lines_and_eof_conversions() {
        let source = "<?php\r\n\r\n$emoji = '😀';\r\n";

        assert_line_conversions(source, 0);
        assert_line_conversions(source, 1);
        assert_line_conversions(source, 2);
        assert_eq!(byte_col_to_utf16(source, 99, 7), 7);
        assert_eq!(utf16_col_to_byte(source, 99, 7), 7);
    }

    #[test]
    fn test_range_byte_to_utf16_multiline_unicode() {
        let source = "<?php\n$one = 'Привет';\n$two = '😀';\n";

        assert_eq!(range_byte_to_utf16(source, (1, 8, 2, 11)), (1, 8, 2, 8));
    }
}
