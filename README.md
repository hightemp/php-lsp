# php-lsp

![Experimental](https://img.shields.io/badge/status-experimental-orange)
![Vibe Coded](https://img.shields.io/badge/vibe-coded-blueviolet)

PHP Language Server (LSP 3.17) written in Rust for Visual Studio Code.

Provides IDE-level features for PHP 7.4+ projects: diagnostics, hover, go-to-definition, completion, references, rename, and more.

## Status

**In development** — MVP phase.

## Features (planned for MVP)

- [x] Syntax error diagnostics (incremental, tree-sitter based)
- [x] Hover: type/signature/PHPDoc
- [x] Go to Definition (classes/functions/methods/properties/consts/variables)
- [x] Completion (members, statics, variables, namespaces, keywords)
- [x] Find All References (classes/functions/methods/properties/class const/global const)
- [x] Rename symbol (classes/functions/methods/properties/class const/global const)
- [x] Document/workspace symbols
- [x] Composer PSR-4 autoload support
- [x] Built-in PHP stubs (phpstorm-stubs)

Current gaps:
- [ ] Variable references/rename (not supported yet)

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

### Packaging VSIX (with bundled server)

Build the server, bundle stubs, and package into a universal `.vsix` containing all platform binaries:

```bash
# 1. Build server binary for current platform → copies to client/bin/<platform>/
./scripts/build-server.sh

# 2. Bundle phpstorm-stubs → copies to client/stubs/
./scripts/bundle-stubs.sh

# 3. Package universal VSIX
cd client
npm install
npx @vscode/vsce package
```

The VSIX contains binaries for all platforms in `bin/<platform>/php-lsp`:
```
bin/
├── linux-x64/php-lsp
├── linux-arm64/php-lsp
├── darwin-x64/php-lsp
├── darwin-arm64/php-lsp
├── win32-x64/php-lsp.exe
└── win32-arm64/php-lsp.exe
```

The extension auto-detects the current OS/arch and uses the correct binary.

For local development, build only your host target:
```bash
./scripts/build-server.sh                          # auto-detect host
./scripts/build-server.sh x86_64-unknown-linux-gnu # specific target
./scripts/build-server.sh --all                    # all 6 targets (CI)
```

CI builds all targets and produces a single universal VSIX on git tag push (see `.github/workflows/release.yml`).

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
