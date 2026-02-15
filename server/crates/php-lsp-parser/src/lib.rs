//! PHP parser for php-lsp.
//!
//! Wraps tree-sitter-php for incremental parsing and provides
//! symbol extraction, diagnostic generation, and symbol resolution from CST.

pub mod diagnostics;
pub mod parser;
pub mod phpdoc;
pub mod references;
pub mod resolve;
pub mod semantic;
pub mod symbols;
