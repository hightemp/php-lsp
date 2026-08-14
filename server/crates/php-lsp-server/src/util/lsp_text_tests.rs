use super::*;

#[test]
fn lsp_position_to_byte_handles_utf16_columns() {
    let source = "<?php\n$привет = 1;\n";
    let line = 1;
    let byte_after_variable = "$привет".len() as u32;
    let utf16_after_variable = "$привет".encode_utf16().count() as u32;

    assert_eq!(
        lsp_position_to_byte(source, Position::new(line, utf16_after_variable)),
        Some("<?php\n".len() + byte_after_variable as usize)
    );
}

#[test]
fn text_at_lsp_range_handles_utf16_columns() {
    let source = "<?php\n$привет = 1;\n";
    let start = Position::new(1, 0);
    let end = Position::new(1, "$привет".encode_utf16().count() as u32);

    assert_eq!(
        text_at_lsp_range(source, Range::new(start, end)),
        Some("$привет")
    );
}

#[test]
fn text_at_lsp_range_handles_emoji_and_crlf() {
    let source = "<?php\r\n$emoji = \"😀\"; $target = 1;\r\n";

    assert_eq!(
        text_at_lsp_range(
            source,
            Range::new(Position::new(1, 15), Position::new(1, 22))
        ),
        Some("$target")
    );
}

#[test]
fn range_from_byte_range_converts_after_emoji() {
    let source = "<?php\n$emoji = \"😀\"; $target = 1;\n";
    let start = source.lines().nth(1).unwrap().find("$target").unwrap() as u32;
    let end = start + "$target".len() as u32;

    assert_eq!(
        range_from_byte_range(source, (1, start, 1, end)),
        Range::new(Position::new(1, 15), Position::new(1, 22))
    );
}
