//! PHP Language Server library.
//!
//! Re-exports the server module for integration testing.

pub mod config;
mod server;

pub use server::PhpLspBackend;
