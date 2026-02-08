//! PHP parser for php-lsp.
//!
//! Wraps tree-sitter-php for incremental parsing and provides
//! symbol extraction and diagnostic generation from CST.

pub mod diagnostics;
pub mod parser;
