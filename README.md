# php-lsp

PHP Language Server (LSP 3.17) written in Rust for Visual Studio Code.

Provides IDE-level features for PHP 7.4+ projects: diagnostics, hover, go-to-definition, completion, references, rename, and more.

## Status

**In development** — MVP phase.

## Features (planned for MVP)

- Syntax error diagnostics (incremental, tree-sitter based)
- Hover: type/signature/PHPDoc
- Go to Definition
- Completion (members, statics, variables, namespaces, keywords)
- Find All References
- Rename symbol
- Document/workspace symbols
- Composer PSR-4 autoload support
- Built-in PHP stubs (phpstorm-stubs)

## Architecture

- **Server**: Rust (tokio + tower-lsp-server + tree-sitter-php)
- **Client**: VS Code extension (TypeScript + vscode-languageclient)
- **Transport**: stdio (JSON-RPC 2.0)

## Building

### Server

```bash
cd server
cargo build --release
```

### Client (VS Code extension)

```bash
cd client
npm install
npm run build
```

## Project Structure

```
php-lsp/
├── server/          # Rust LSP server (Cargo workspace)
│   └── crates/
│       ├── php-lsp-server/      # Main binary
│       ├── php-lsp-parser/      # tree-sitter PHP wrapper
│       ├── php-lsp-index/       # Symbol index
│       ├── php-lsp-completion/  # Completion engine
│       └── php-lsp-types/       # Shared types
├── client/          # VS Code extension (TypeScript)
├── test-fixtures/   # Test PHP projects
└── docs/            # Documentation
```

## License

MIT
