//! PHP Language Server library.
//!
//! Re-exports the server module for integration testing.

mod server;

pub use server::PhpLspBackend;
