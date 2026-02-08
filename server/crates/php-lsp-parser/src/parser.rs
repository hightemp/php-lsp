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
    /// `range` is (start_line, start_char, end_line, end_char) in 0-based LSP coordinates.
    /// `new_text` is the replacement text.
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

        // Calculate byte offsets from rope
        let start_byte = self.position_to_byte(start_line, start_char);
        let old_end_byte = self.position_to_byte(end_line, end_char);

        let start_point = Point::new(start_line, start_char);
        let old_end_point = Point::new(end_line, end_char);

        // Apply the edit to the rope
        let start_char_idx = self.rope.byte_to_char(start_byte);
        let end_char_idx = self.rope.byte_to_char(old_end_byte);
        self.rope.remove(start_char_idx..end_char_idx);
        self.rope.insert(start_char_idx, new_text);

        // Calculate new end position
        let new_end_byte = start_byte + new_text.len();
        let new_end_line_char = self.byte_to_position(new_end_byte);
        let new_end_point = Point::new(new_end_line_char.0, new_end_line_char.1);

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

    /// Convert (line, char) to byte offset in the rope.
    fn position_to_byte(&self, line: usize, character: usize) -> usize {
        if line >= self.rope.len_lines() {
            return self.rope.len_bytes();
        }
        let line_start = self.rope.line_to_byte(line);
        let line_len = if line + 1 < self.rope.len_lines() {
            self.rope.line_to_byte(line + 1) - line_start
        } else {
            self.rope.len_bytes() - line_start
        };
        line_start + character.min(line_len)
    }

    /// Convert byte offset to (line, char).
    fn byte_to_position(&self, byte: usize) -> (usize, usize) {
        let byte = byte.min(self.rope.len_bytes());
        let line = self.rope.byte_to_line(byte);
        let line_start = self.rope.line_to_byte(line);
        let character = byte - line_start;
        (line, character)
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
}
