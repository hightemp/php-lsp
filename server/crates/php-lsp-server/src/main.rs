//! PHP Language Server entry point.
//!
//! Starts the LSP server on stdio using tower-lsp-server.

mod server;

use tower_lsp::{LspService, Server};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    // Initialize tracing (logs go to stderr so they don't interfere with stdio LSP transport)
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    tracing::info!("Starting php-lsp server");

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(server::PhpLspBackend::new);

    Server::new(stdin, stdout, socket).serve(service).await;
}
