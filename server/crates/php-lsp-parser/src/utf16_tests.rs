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
