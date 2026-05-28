//! PHP Language Server library.
//!
//! Re-exports the server module for integration testing.

pub mod analyze;
pub mod config;
pub mod fix;
mod framework;
mod server;
mod template;
pub(crate) mod util;

pub use server::PhpLspBackend;
