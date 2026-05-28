use php_lsp_parser::utf16::{range_byte_to_utf16, utf16_col_to_byte};
use tower_lsp::ls_types::{Position, Range};

pub(crate) fn range_from_tuple(range: (u32, u32, u32, u32)) -> Range {
    Range {
        start: Position::new(range.0, range.1),
        end: Position::new(range.2, range.3),
    }
}

pub(crate) fn range_from_byte_range(source: &str, range: (u32, u32, u32, u32)) -> Range {
    range_from_tuple(range_byte_to_utf16(source, range))
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
mod tests {
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
}
