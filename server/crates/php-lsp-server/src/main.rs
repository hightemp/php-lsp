//! PHP Language Server entry point.
//!
//! Starts the LSP server on stdio using tower-lsp-server.

use php_lsp_server::config::{
    write_default_project_config, InitConfigResult, PROJECT_CONFIG_FILE_NAME,
};
use php_lsp_server::PhpLspBackend;
use std::path::PathBuf;
use tower_lsp::{LspService, Server};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    if handle_cli_command().await {
        return;
    }

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

    let (service, socket) = LspService::new(PhpLspBackend::new);

    Server::new(stdin, stdout, socket).serve(service).await;
}

async fn handle_cli_command() -> bool {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        return false;
    };

    match command.as_str() {
        "analyze" => {
            let result = php_lsp_server::analyze::run_analyze_cli(args.collect());
            if !result.stdout.is_empty() {
                print!("{}", result.stdout);
            }
            if !result.stderr.is_empty() {
                eprint!("{}", result.stderr);
            }
            std::process::exit(result.exit_code);
        }
        "init-config" => {
            let path = parse_init_config_path(args.collect());
            match write_default_project_config(&path) {
                Ok(InitConfigResult::Created(path)) => {
                    println!("Created {}", path.display());
                    true
                }
                Ok(InitConfigResult::AlreadyExists(path)) => {
                    println!("Config already exists: {}", path.display());
                    true
                }
                Err(err) => {
                    eprintln!("Failed to create {}: {}", path.display(), err);
                    std::process::exit(1);
                }
            }
        }
        "--help" | "-h" | "help" => {
            print_help();
            true
        }
        "--version" | "-V" | "version" => {
            println!("php-lsp {}", env!("CARGO_PKG_VERSION"));
            true
        }
        _ => false,
    }
}

fn parse_init_config_path(args: Vec<String>) -> PathBuf {
    let mut path = None;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--path" | "-p" => {
                if let Some(value) = iter.next() {
                    path = Some(PathBuf::from(value));
                }
            }
            value if !value.starts_with('-') && path.is_none() => {
                path = Some(PathBuf::from(value));
            }
            _ => {}
        }
    }

    path.unwrap_or_else(|| PathBuf::from(PROJECT_CONFIG_FILE_NAME))
}

fn print_help() {
    println!(
        "php-lsp {}\n\nUsage:\n  php-lsp                 Start the LSP server on stdio\n  php-lsp analyze [PATH]  Analyze PHP files and print diagnostics\n  php-lsp analyze [PATH] --project-root <DIR> --severity <all|hint|info|warning|error> --format <table|json|github>\n  php-lsp init-config     Create .php-lsp.toml in the current directory\n  php-lsp init-config --path <path>\n  php-lsp --version",
        env!("CARGO_PKG_VERSION")
    );
}
