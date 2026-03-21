//! FileParser: tree-sitter + ropey::Rope for incremental PHP parsing.

use ropey::Rope;
use tree_sitter::{InputEdit, Parser, Point, Tree};

/// Manages parsing state for a single PHP file.
pub struct FileParser {
    parser: Parser,
    tree: Option<Tree>,
    rope: Rope,
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
        }
    }

    /// Full parse of a source string (used on didOpen).
    pub fn parse_full(&mut self, source: &str) {
        self.rope = Rope::from_str(source);
        let source_bytes = source.as_bytes();
        self.tree = self.parser.parse(source_bytes, None);
    }

    /// Apply an incremental edit from LSP didChange and reparse.
    ///
    /// LSP positions use (line, character) where `character` is measured in
    /// **UTF-16 code units**.  Tree-sitter and ropey work in bytes, so we must
    /// convert before applying the edit.
    pub fn apply_edit(
        &mut self,
        start_line: u32,
        start_char: u32,
        end_line: u32,
        end_char: u32,
        new_text: &str,
    ) {
        let start_line = start_line as usize;
        let start_char = start_char as usize;
        let end_line = end_line as usize;
        let end_char = end_char as usize;

        // Calculate byte offsets from rope (UTF-16 → byte).
        let start_byte = self.utf16_position_to_byte(start_line, start_char);
        let old_end_byte = self.utf16_position_to_byte(end_line, end_char);

        // Tree-sitter Points need byte columns (byte offset from line start).
        let start_byte_col = start_byte - self.line_start_byte(start_line);
        let old_end_byte_col = old_end_byte - self.line_start_byte(end_line);

        let start_point = Point::new(start_line, start_byte_col);
        let old_end_point = Point::new(end_line, old_end_byte_col);

        // Apply the edit to the rope
        let start_char_idx = self.rope.byte_to_char(start_byte);
        let end_char_idx = self.rope.byte_to_char(old_end_byte);
        self.rope.remove(start_char_idx..end_char_idx);
        self.rope.insert(start_char_idx, new_text);

        // Calculate new end position
        let new_end_byte = start_byte + new_text.len();
        let new_end_line = self.rope.byte_to_line(new_end_byte.min(self.rope.len_bytes()));
        let new_end_byte_col = new_end_byte - self.line_start_byte(new_end_line);
        let new_end_point = Point::new(new_end_line, new_end_byte_col);

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
        self.tree = self.parser.parse(source.as_bytes(), self.tree.as_ref());
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

    /// Convert (line, utf16_character) to byte offset in the rope.
    ///
    /// LSP `Position.character` is measured in UTF-16 code units.
    /// For ASCII this is the same as byte offset, but for multi-byte characters
    /// (Cyrillic, CJK, emoji, etc.) it differs.
    fn utf16_position_to_byte(&self, line: usize, utf16_char: usize) -> usize {
        if line >= self.rope.len_lines() {
            return self.rope.len_bytes();
        }
        let line_start = self.rope.line_to_byte(line);
        let line_slice = self.rope.line(line);

        let mut utf16_offset = 0;
        let mut byte_offset = 0;

        for ch in line_slice.chars() {
            if utf16_offset >= utf16_char {
                break;
            }
            byte_offset += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }

        line_start + byte_offset
    }

    /// Get the byte offset of the start of a line.
    fn line_start_byte(&self, line: usize) -> usize {
        if line >= self.rope.len_lines() {
            return self.rope.len_bytes();
        }
        self.rope.line_to_byte(line)
    }

    /// Convert (line, byte_column) to (line, utf16_character) for LSP.
    ///
    /// Tree-sitter positions use byte columns; LSP uses UTF-16 code unit columns.
    pub fn byte_col_to_utf16(&self, line: u32, byte_col: u32) -> u32 {
        let line = line as usize;
        if line >= self.rope.len_lines() {
            return byte_col;
        }
        let line_slice = self.rope.line(line);
        let byte_col = byte_col as usize;

        let mut utf16_offset: usize = 0;
        let mut byte_offset: usize = 0;

        for ch in line_slice.chars() {
            if byte_offset >= byte_col {
                break;
            }
            byte_offset += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }

        utf16_offset as u32
    }
}

impl Default for FileParser {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_full_simple_class() {
        let mut parser = FileParser::new();
        parser.parse_full("<?php\nclass Foo {\n    public function bar(): void {}\n}\n");

        let tree = parser.tree().expect("Should have a tree");
        let root = tree.root_node();
        assert_eq!(root.kind(), "program");
        assert!(!root.has_error());
    }

    #[test]
    fn test_parse_full_with_error() {
        let mut parser = FileParser::new();
        parser.parse_full("<?php\nfunction foo( {\n}\n");

        let tree = parser.tree().expect("Should have a tree");
        let root = tree.root_node();
        assert_eq!(root.kind(), "program");
        // Tree should have an error node but still parse
        assert!(root.has_error());
    }

    #[test]
    fn test_incremental_edit() {
        let mut parser = FileParser::new();
        parser.parse_full("<?php\nclass Foo {}\n");

        // Change "Foo" to "Bar" (line 1, chars 6-9)
        parser.apply_edit(1, 6, 1, 9, "Bar");

        let source = parser.source();
        assert!(source.contains("class Bar {}"));

        let tree = parser.tree().expect("Should have a tree after edit");
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn test_parse_empty_php() {
        let mut parser = FileParser::new();
        parser.parse_full("<?php\n");

        let tree = parser.tree().expect("Should have a tree");
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn test_parse_mixed_html_php() {
        let mut parser = FileParser::new();
        parser.parse_full("<html><body><?php echo 'hello'; ?></body></html>");

        let tree = parser.tree().expect("Should have a tree");
        assert_eq!(tree.root_node().kind(), "program");
        // Mixed PHP/HTML should parse without errors
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn test_parse_self_param_and_static_return_type() {
        let mut parser = FileParser::new();
        parser.parse_full(
            "<?php\nclass Demo {\n    public function withSelf(self $arg): static\n    {\n        return $this;\n    }\n}\n",
        );

        let tree = parser.tree().expect("Should have a tree");
        assert!(
            !tree.root_node().has_error(),
            "Valid self/static type-hint syntax should parse without errors"
        );
    }
}
