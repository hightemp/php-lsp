//! Symbol index for php-lsp.
//!
//! Provides the global workspace symbol index, name resolution,
//! composer.json autoload support, and phpstorm-stubs loading.

pub mod composer;
pub mod stubs;
pub mod workspace;
