//! FileParser: tree-sitter + ropey::Rope for incremental PHP parsing.

use crate::line_index::LineIndex;
use crate::parsed::ParsedSnapshot;
use ropey::Rope;
use std::{fmt, sync::Arc};
use tree_sitter::{InputEdit, Parser, Point, Tree};

/// An invalid incremental edit that could not be applied safely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyEditError {
    /// The LSP range ends before it starts.
    ReversedRange { start: (u32, u32), end: (u32, u32) },
}

impl fmt::Display for ApplyEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReversedRange { start, end } => write!(
                formatter,
                "incremental edit range ends before it starts: {}:{} -> {}:{}",
                start.0, start.1, end.0, end.1
            ),
        }
    }
}

impl std::error::Error for ApplyEditError {}

/// Manages parsing state for a single PHP file.
pub struct FileParser {
    parser: Parser,
    rope: Rope,
    snapshot: Arc<ParsedSnapshot>,
}

impl FileParser {
    /// Create a new FileParser with tree-sitter-php language.
    pub fn new() -> Self {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_php::LANGUAGE_PHP.into())
            .expect("Failed to set tree-sitter PHP language");

        FileParser {
            parser,
            rope: Rope::new(),
            snapshot: Arc::new(ParsedSnapshot::new(
                None,
                Arc::from(""),
                Arc::new(LineIndex::new("")),
            )),
        }
    }

    /// Full parse of a source string (used on didOpen).
    pub fn parse_full(&mut self, source: &str) {
        #[cfg(any(test, feature = "test-instrumentation"))]
        crate::analysis_metrics::record(crate::analysis_metrics::Work::Parse, source);
        self.rope = Rope::from_str(source);
        let tree = self.parser.parse(source.as_bytes(), None);
        self.snapshot = Arc::new(ParsedSnapshot::new(
            tree,
            Arc::from(source),
            Arc::new(LineIndex::new(source)),
        ));
    }

    /// Apply an incremental edit from LSP didChange and reparse.
    ///
    /// LSP positions use (line, character) where `character` is measured in
    /// **UTF-16 code units**. Convert to source bytes for Tree-sitter and to
    /// Rope character offsets for editing; CST rows advance only at LF.
    pub fn apply_edit(
        &mut self,
        start_line: u32,
        start_char: u32,
        end_line: u32,
        end_char: u32,
        new_text: &str,
    ) -> Result<(), ApplyEditError> {
        self.apply_edits(&[SourceEdit {
            range: Some(((start_line, start_char), (end_line, end_char))),
            text: new_text,
        }])
    }

    /// Apply all ranges against successive text states, then parse exactly once.
    /// Staging preserves previously published trees and rejects invalid batches atomically.
    pub fn apply_edits(&mut self, changes: &[SourceEdit<'_>]) -> Result<(), ApplyEditError> {
        for change in changes {
            if let Some((start, end)) = change.range {
                if end < start {
                    return Err(ApplyEditError::ReversedRange { start, end });
                }
            }
        }
        if changes.is_empty() {
            return Ok(());
        }
        let mut rope = self.rope.clone();
        let mut tree = self.snapshot.tree.clone();
        for (position, change) in changes.iter().enumerate() {
            let Some((start, end)) = change.range else {
                rope = Rope::from_str(change.text);
                tree = None;
                continue;
            };
            let (start_byte, old_end_byte, start_position, old_end_position) = if position == 0 {
                let lines = &self.snapshot.lines;
                let start_byte = lines
                    .position_to_offset(start.0, start.1)
                    .unwrap_or(lines.len_bytes());
                let old_end_byte = lines
                    .position_to_offset(end.0, end.1)
                    .unwrap_or(lines.len_bytes());
                let a = lines.offset_to_point(start_byte);
                let b = lines.offset_to_point(old_end_byte);
                (
                    start_byte,
                    old_end_byte,
                    Point::new(a.0 as usize, a.1 as usize),
                    Point::new(b.0 as usize, b.1 as usize),
                )
            } else {
                let a = rope_position_to_byte(&rope, start);
                let b = rope_position_to_byte(&rope, end);
                (
                    a,
                    b,
                    rope_byte_to_point(&rope, a),
                    rope_byte_to_point(&rope, b),
                )
            };
            let start_char = rope.byte_to_char(start_byte);
            let end_char = rope.byte_to_char(old_end_byte);
            rope.remove(start_char..end_char);
            rope.insert(start_char, change.text);
            let new_end_byte = start_byte + change.text.len();
            if let Some(tree) = &mut tree {
                tree.edit(&InputEdit {
                    start_byte,
                    old_end_byte,
                    new_end_byte,
                    start_position,
                    old_end_position,
                    new_end_position: Point::new(
                        start_position.row
                            + change.text.bytes().filter(|byte| *byte == b'\n').count(),
                        change
                            .text
                            .rfind('\n')
                            .map_or(start_position.column + change.text.len(), |offset| {
                                change.text.len() - offset - 1
                            }),
                    ),
                });
            }
        }
        let source: Arc<str> = Arc::from(rope.to_string());
        #[cfg(any(test, feature = "test-instrumentation"))]
        {
            crate::analysis_metrics::record(crate::analysis_metrics::Work::Source, &source);
            crate::analysis_metrics::record(crate::analysis_metrics::Work::Parse, &source);
        }
        let parsed = self.parser.parse_with(
            &mut |offset, _| {
                if offset >= rope.len_bytes() {
                    return &[][..];
                }
                let (chunk, start, _, _) = rope.chunk_at_byte(offset);
                &chunk.as_bytes()[offset - start..]
            },
            tree.as_ref(),
        );
        let lines = Arc::new(LineIndex::new(&source));
        self.snapshot = Arc::new(ParsedSnapshot::new(parsed, source, lines));
        self.rope = rope;
        Ok(())
    }

    pub fn snapshot(&self) -> Arc<ParsedSnapshot> {
        self.snapshot.clone()
    }

    /// Get the current tree-sitter Tree (if parsed successfully).
    pub fn tree(&self) -> Option<&Tree> {
        self.snapshot.tree()
    }

    /// Get the current source as a String.
    pub fn source(&self) -> String {
        let source = self.snapshot.source.to_string();
        #[cfg(any(test, feature = "test-instrumentation"))]
        crate::analysis_metrics::record(crate::analysis_metrics::Work::Source, &source);
        source
    }

    /// Get the current rope.
    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    pub fn line_index(&self) -> &LineIndex {
        &self.snapshot.lines
    }

    /// Legacy column-only API. Full protocol conversions use `line_index()`.
    #[deprecated(note = "Use line_index().point_to_position for full coordinate conversion")]
    pub fn byte_col_to_utf16(&self, line: u32, byte_col: u32) -> u32 {
        self.snapshot.lines.point_to_position(line, byte_col).1
    }
}

/// A full replacement (`range: None`) or one LSP UTF-16 ranged edit.
pub struct SourceEdit<'a> {
    pub range: Option<((u32, u32), (u32, u32))>,
    pub text: &'a str,
}

fn rope_position_to_byte(rope: &Rope, (line, column): (u32, u32)) -> usize {
    // Rope is compiled with cr_lines and without unicode_lines, matching LSP.
    if line as usize >= rope.len_lines() {
        return rope.len_bytes();
    }
    let mut bytes = rope.line_to_byte(line as usize);
    let mut units = 0;
    for ch in rope.line(line as usize).chars() {
        if matches!(ch, '\r' | '\n') || units + ch.len_utf16() > column as usize {
            break;
        }
        units += ch.len_utf16();
        bytes += ch.len_utf8();
    }
    bytes
}
fn rope_byte_to_point(rope: &Rope, byte: usize) -> Point {
    let mut row = 0;
    let mut line_start = 0;
    for (offset, ch) in rope.byte_slice(..byte).bytes().enumerate() {
        if ch == b'\n' {
            row += 1;
            line_start = offset + 1;
        }
    }
    Point::new(row, byte - line_start)
}

impl Default for FileParser {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[path = "parser_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "document_line_tests.rs"]
mod document_line_tests;
