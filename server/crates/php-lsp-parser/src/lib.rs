//! PHP parser for php-lsp.
//!
//! Wraps tree-sitter-php for incremental parsing and provides
//! symbol extraction, diagnostic generation, and symbol resolution from CST.

pub mod control;
mod cst;
pub mod diagnostics;
pub mod function_names;
pub mod line_index;
pub mod parsed;
pub mod parser;
pub mod phpdoc;
pub mod references;
pub mod resolve;
pub mod return_type;
pub mod semantic;
pub mod semantic_tokens;
pub mod signature_help;
pub mod symbols;
pub mod type_contract;
pub mod utf16;
pub mod variable_scope;

#[cfg(any(test, feature = "test-instrumentation"))]
pub mod analysis_metrics;
