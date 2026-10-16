//! FileParser: tree-sitter + ropey::Rope for incremental PHP parsing.

use crate::line_index::LineIndex;
use ropey::Rope;
use std::fmt;
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
    tree: Option<Tree>,
    rope: Rope,
    lines: LineIndex,
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
            tree: None,
            rope: Rope::new(),
            lines: LineIndex::new(""),
        }
    }

    /// Full parse of a source string (used on didOpen).
    pub fn parse_full(&mut self, source: &str) {
        self.rope = Rope::from_str(source);
        self.lines = LineIndex::new(source);
        let source_bytes = source.as_bytes();
        self.tree = self.parser.parse(source_bytes, None);
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
        if (end_line, end_char) < (start_line, start_char) {
            return Err(ApplyEditError::ReversedRange {
                start: (start_line, start_char),
                end: (end_line, end_char),
            });
        }

        let start_byte = self
            .lines
            .position_to_offset(start_line, start_char)
            .unwrap_or(self.lines.len_bytes());
        let old_end_byte = self
            .lines
            .position_to_offset(end_line, end_char)
            .unwrap_or(self.lines.len_bytes());
        debug_assert!(start_byte <= old_end_byte);
        let start = self.lines.offset_to_point(start_byte);
        let old_end = self.lines.offset_to_point(old_end_byte);
        let start_point = Point::new(start.0 as usize, start.1 as usize);
        let old_end_point = Point::new(old_end.0 as usize, old_end.1 as usize);

        // Apply the edit to the rope
        let start_char_idx = self.rope.byte_to_char(start_byte);
        let end_char_idx = self.rope.byte_to_char(old_end_byte);
        self.rope.remove(start_char_idx..end_char_idx);
        self.rope.insert(start_char_idx, new_text);

        // Calculate new end position
        let new_end_byte = start_byte + new_text.len();
        let newlines = new_text.bytes().filter(|byte| *byte == b'\n').count();
        let new_column = match new_text.rfind('\n') {
            Some(i) => new_text.len() - i - 1,
            None => start_point.column + new_text.len(),
        };
        let new_end_point = Point::new(start_point.row + newlines, new_column);

        // Apply edit to tree-sitter tree for incremental reparsing
        if let Some(tree) = &mut self.tree {
            tree.edit(&InputEdit {
                start_byte,
                old_end_byte,
                new_end_byte,
                start_position: start_point,
                old_end_position: old_end_point,
                new_end_position: new_end_point,
            });
        }

        // Reparse incrementally
        let source = self.rope.to_string();
        self.lines = LineIndex::new(&source);
        self.tree = self.parser.parse(source.as_bytes(), self.tree.as_ref());
        Ok(())
    }

    /// Get the current tree-sitter Tree (if parsed successfully).
    pub fn tree(&self) -> Option<&Tree> {
        self.tree.as_ref()
    }

    /// Get the current source as a String.
    pub fn source(&self) -> String {
        self.rope.to_string()
    }

    /// Get the current rope.
    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    pub fn line_index(&self) -> &LineIndex {
        &self.lines
    }

    /// Legacy column-only API. Full protocol conversions use `line_index()`.
    #[deprecated(note = "Use line_index().point_to_position for full coordinate conversion")]
    pub fn byte_col_to_utf16(&self, line: u32, byte_col: u32) -> u32 {
        self.lines.point_to_position(line, byte_col).1
    }
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
