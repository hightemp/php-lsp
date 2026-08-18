use super::*;

#[test]
fn byte_offset_for_line_col_clamps_to_the_requested_line() {
    let source = "first\nβeta\r\nlast";
    let first_end = source.find('\n').unwrap();
    let second_start = first_end + 1;
    let second_end = second_start + source[second_start..].find('\n').unwrap();

    assert_eq!(
        byte_offset_for_line_col(source, 0, u32::MAX),
        Some(first_end)
    );
    assert_eq!(
        byte_offset_for_line_col(source, 1, u32::MAX),
        Some(second_end),
        "CRLF line should clamp before LF instead of entering the next line"
    );
    assert_eq!(
        byte_offset_for_line_col(source, 1, "β".len() as u32),
        Some(second_start + "β".len()),
        "columns are parser byte columns, not UTF-16 columns"
    );
    assert_eq!(
        byte_offset_for_line_col(source, 2, u32::MAX),
        Some(source.len())
    );
    assert_eq!(byte_offset_for_line_col(source, 3, 0), None);
    assert_eq!(byte_offset_for_line_col("first\n", 1, u32::MAX), Some(6));
    assert_eq!(byte_offset_for_line_col("", 0, u32::MAX), Some(0));
}

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
