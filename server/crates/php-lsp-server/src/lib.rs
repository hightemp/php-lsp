//! PHP Language Server library.
//!
//! Re-exports the server module for integration testing.

pub mod analyze;
pub mod config;
pub mod fix;
mod framework;
mod server;

pub use server::PhpLspBackend;
