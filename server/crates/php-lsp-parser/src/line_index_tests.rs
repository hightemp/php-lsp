use super::*;

#[test]
fn line_model_invalid_cst_rows_cannot_become_valid_lsp_rows() {
    let index = LineIndex::new("a\rb");
    let invalid = index.point_to_position(1, 0);
    assert!(
        invalid.0 as usize >= index.line_count(),
        "invalid CST point became valid LSP position: {invalid:?}"
    );
    assert!(index.position_to_offset(2, 0).is_none());
}

#[test]
fn line_model_segments_never_emit_empty_utf16_tokens() {
    let index = LineIndex::new("😀\r\n😀");
    for start in 0..11 {
        for end in start..11 {
            for (_, a, b) in index.segments(start, end) {
                assert!(b > a, "{start}..{end} -> {a}..{b}");
            }
        }
    }
}

#[test]
fn line_model_offsets_roundtrip_on_all_representable_boundaries() {
    for source in [
        "",
        "x",
        "\r",
        "\n",
        "\r\n",
        "😀\ra\r\n世\nb\u{2028}\u{85}\u{b}\u{c}\u{2029}",
        "\r\r\n\n",
    ] {
        let index = LineIndex::new(source);
        for offset in 0..=source.len() {
            if !source.is_char_boundary(offset) {
                continue;
            }
            let a = index.offset_to_position(offset);
            let b = index.position_to_offset(a.0, a.1).unwrap();
            let expected = if offset > 0
                && source.as_bytes().get(offset) == Some(&b'\n')
                && source.as_bytes()[offset - 1] == b'\r'
            {
                offset - 1
            } else {
                offset
            };
            assert_eq!(b, expected, "{source:?} at {offset}");
            let point = index.offset_to_point(offset);
            assert_eq!(index.point_to_offset(point.0, point.1), Some(offset));
        }
        for row in 0..index.line_count() {
            let span = index.line_range(row as u32).unwrap();
            assert_eq!(
                index.position_to_offset(row as u32, u32::MAX),
                Some(span.end)
            );
        }
    }
}

#[test]
fn line_model_surrogate_halves_floor_within_the_requested_logical_line() {
    let index = LineIndex::new("x\r😀y\r\n");
    assert_eq!(index.position_to_offset(1, 1), Some(2));
    assert_eq!(index.position_to_point(1, 1), (0, 2));
    assert_eq!(index.position_to_point(1, 2), (0, 6));
    assert_eq!(index.point_to_position(0, 3), (1, 0));
    assert_eq!(index.point_to_position(0, 6), (1, 2));
}

#[test]
fn line_model_batch_extraction_reuses_one_position_index_for_many_nodes() {
    let mut source = "<?php\r".to_string();
    for i in 0..1000 {
        source.push_str(&format!("function f{i}():int {{return 1;}}\r"));
    }
    let mut parser = crate::parser::FileParser::new();
    parser.parse_full(&source);
    let tree = parser.tree().unwrap();
    counters::BUILDS.with(|c| c.set(0));
    let symbols = crate::symbols::extract_file_symbols(tree, &source, "file:///line-batch.php");
    assert_eq!(counters::BUILDS.with(|c| c.get()), 1);
    assert_eq!(symbols.symbols.len(), 1000);
    counters::BUILDS.with(|c| c.set(0));
    let refs = crate::references::collect_symbol_references_in_file(tree, &source, &symbols);
    assert_eq!(counters::BUILDS.with(|c| c.get()), 1);
    assert_eq!(refs.len(), 1000);
    assert_eq!(refs.last().unwrap().range.0, 1000);
    counters::BUILDS.with(|c| c.set(0));
    let tokens = crate::semantic_tokens::extract_semantic_tokens(tree, &source);
    assert_eq!(counters::BUILDS.with(|c| c.get()), 1);
    assert!(tokens.len() > 1000);
}
