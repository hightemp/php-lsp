# php-lsp

![Experimental](https://img.shields.io/badge/status-experimental-orange)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange?logo=rust)](server/Cargo.toml)
[![CI](https://github.com/hightemp/php-lsp/actions/workflows/ci.yml/badge.svg)](https://github.com/hightemp/php-lsp/actions/workflows/ci.yml)
[![Release](https://github.com/hightemp/php-lsp/actions/workflows/release.yml/badge.svg)](https://github.com/hightemp/php-lsp/actions/workflows/release.yml)
[![GitHub Release](https://img.shields.io/github/v/release/hightemp/php-lsp?label=github%20release)](https://github.com/hightemp/php-lsp/releases)
[![GitHub Downloads](https://img.shields.io/github/downloads/hightemp/php-lsp/total?label=github%20downloads)](https://github.com/hightemp/php-lsp/releases)
[![VS Marketplace Version](https://img.shields.io/visual-studio-marketplace/v/php-lsp.php-lsp?label=marketplace)](https://marketplace.visualstudio.com/items?itemName=php-lsp.php-lsp)
[![VS Marketplace Downloads](https://img.shields.io/visual-studio-marketplace/d/php-lsp.php-lsp?label=marketplace%20downloads)](https://marketplace.visualstudio.com/items?itemName=php-lsp.php-lsp)
[![VS Marketplace Installs](https://img.shields.io/visual-studio-marketplace/i/php-lsp.php-lsp?label=installs)](https://marketplace.visualstudio.com/items?itemName=php-lsp.php-lsp)
[![VS Marketplace Rating](https://img.shields.io/visual-studio-marketplace/r/php-lsp.php-lsp?label=rating)](https://marketplace.visualstudio.com/items?itemName=php-lsp.php-lsp)
[![License](https://img.shields.io/github/license/hightemp/php-lsp)](LICENSE)
![](https://asdertasd.site/counter/php-lsp)

PHP Language Server (LSP 3.17) written in Rust for Visual Studio Code.

Provides IDE-level features for PHP 7.4+ projects: diagnostics, hover, go-to-definition, completion, references, rename, and more.

## Status

**In development** — MVP phase.

## Features (planned for MVP)

- [x] Syntax error diagnostics (incremental, tree-sitter based)
- [x] Hover: type/signature/PHPDoc
- [x] Go to Definition (classes/functions/methods/properties/consts/variables)
- [x] Completion (members, statics, variables, namespaces, keywords)
- [x] Find All References (classes/functions/methods/properties/class const/global const/variables)
- [x] Rename symbol (classes/functions/methods/properties/class const/global const/variables)
- [x] Document/workspace symbols
- [x] Composer PSR-4 autoload support
- [x] Built-in PHP stubs (phpstorm-stubs)

Current gaps:
- [ ] Cross-file variable analysis (variable references/rename are local-scope only by design)

## Architecture

- **Server**: Rust (tokio + tower-lsp-server + tree-sitter-php)
- **Client**: VS Code extension (TypeScript + vscode-languageclient)
- **Transport**: stdio (JSON-RPC 2.0)

## Building

### Prerequisites

- **Rust** 1.85+ (`rustup update stable`)
- **Node.js** 20+ and npm
- **Git** (for submodules)

### Quick start (Makefile)

```bash
make            # build server + client + stubs → .vsix
make install    # build + install extension into VS Code
```

All available targets:

| Command | Description |
|---|---|
| `make` / `make package` | Full build: server + client + stubs → `.vsix` |
| `make install` | Build and install `.vsix` into VS Code |
| `make server` | Build Rust binary for host platform |
| `make server-all` | Cross-compile server for all 6 platforms |
| `make package-all` | Universal `.vsix` with all platform binaries |
| `make client` | `npm ci` + build extension JS |
| `make stubs` | Init submodule + bundle phpstorm-stubs |
| `make check` | Lint + test (Rust & TypeScript) |
| `make test` | Run Rust tests |
| `make lint` | `cargo fmt --check`, `clippy`, `tsc --noEmit` |
| `make fmt` | Auto-format Rust code |
| `make clean` | Remove all build artefacts |

Stubs submodule (`server/data/stubs`) is pulled automatically on first build if not initialized.

### Manual steps

#### Server

```bash
cd server
cargo build --release
```

#### Client (VS Code extension)

```bash
cd client
npm ci
npm run build
```

#### Packaging VSIX

```bash
# 1. Build server binary for current platform → client/bin/<platform>/
./scripts/build-server.sh

# 2. Bundle phpstorm-stubs → client/stubs/
./scripts/bundle-stubs.sh

# 3. Package VSIX
cd client
npx @vscode/vsce package
```

#### Cross-compilation

```bash
./scripts/build-server.sh x86_64-unknown-linux-gnu # specific target
./scripts/build-server.sh --all                    # all 6 targets (CI)
```

## Project Structure

```
php-lsp/
├── Makefile         # Build automation
├── server/          # Rust LSP server (Cargo workspace)
│   ├── data/stubs/  # phpstorm-stubs (git submodule)
│   └── crates/
│       ├── php-lsp-server/      # Main binary
│       ├── php-lsp-parser/      # tree-sitter PHP wrapper
│       ├── php-lsp-index/       # Symbol index
│       ├── php-lsp-completion/  # Completion engine
│       └── php-lsp-types/       # Shared types
├── client/          # VS Code extension (TypeScript)
├── scripts/         # Build helpers (build-server.sh, bundle-stubs.sh)
├── test-fixtures/   # Test PHP projects
└── docs/            # Documentation
```

## License

MIT
