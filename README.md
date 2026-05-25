# php-lsp

[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange?logo=rust)](server/Cargo.toml)
[![PHP](https://img.shields.io/badge/php-7.4--8.4-777BB4?logo=php&logoColor=white)](README.md)
[![CI](https://github.com/hightemp/php-lsp/actions/workflows/ci.yml/badge.svg)](https://github.com/hightemp/php-lsp/actions/workflows/ci.yml)
[![Release](https://github.com/hightemp/php-lsp/actions/workflows/release.yml/badge.svg)](https://github.com/hightemp/php-lsp/actions/workflows/release.yml)
[![GitHub Release](https://img.shields.io/github/v/release/hightemp/php-lsp?label=github%20release)](https://github.com/hightemp/php-lsp/releases)
[![Release Downloads](https://img.shields.io/github/downloads/hightemp/php-lsp/total.svg?label=release%20downloads&logo=github)](https://github.com/hightemp/php-lsp/releases)
[![VS Marketplace Version](https://badgen.net/vs-marketplace/v/hightemp.ht-php-lsp?label=marketplace)](https://marketplace.visualstudio.com/items?itemName=hightemp.ht-php-lsp)
[![VS Marketplace Downloads](https://img.shields.io/badge/marketplace%20downloads-5-007ACC?logo=visualstudiocode&logoColor=white)](https://marketplace.visualstudio.com/items?itemName=hightemp.ht-php-lsp)
[![VS Marketplace Installs](https://img.shields.io/badge/installs-1-007ACC?logo=visualstudiocode&logoColor=white)](https://marketplace.visualstudio.com/items?itemName=hightemp.ht-php-lsp)
[![VS Marketplace Rating](https://img.shields.io/badge/rating-no%20ratings-lightgrey?logo=visualstudiocode&logoColor=white)](https://marketplace.visualstudio.com/items?itemName=hightemp.ht-php-lsp)
[![License](https://img.shields.io/github/license/hightemp/php-lsp)](LICENSE)
![](https://asdertasd.site/counter/php-lsp)

Rust PHP Language Server (LSP 3.17) with a VS Code extension.

php-lsp targets PHP 7.4-8.4 projects and provides indexed PHP language
intelligence: diagnostics, hover, completion, navigation, references, rename,
formatting integration, semantic tokens, hierarchy views, and built-in
phpstorm-stubs support.

## Features

### Language Intelligence

- Syntax diagnostics with incremental tree-sitter parsing.
- Semantic diagnostics for unknown classes, functions, imports, members, and
  duplicate workspace symbols.
- Member diagnostics for visibility, static/instance misuse, missing methods,
  missing properties, and missing class constants.
- Basic type compatibility checks for assignments, returns, arguments,
  properties, and member calls.
- Override signature and PHP-version compatibility diagnostics.
- Optional PHPStan and Psalm diagnostics through configured external commands.
- Test-friendly diagnostics for common PHPUnit patterns, including assertion
  helpers, test doubles, trait-based test helpers, anonymous classes, and
  closure/destructuring variable scopes.
- Hover for symbols, signatures, types, and PHPDoc.
- Completion for classes, interfaces, traits, enums, functions, constants,
  methods, properties, variables, namespaces, keywords, and snippets.
- Signature help for functions, methods, and constructors.
- Document symbols and workspace symbols.

### Navigation

- Go to definition, declaration, type definition, and implementation.
- Find all references.
- Document highlight.
- Selection ranges based on the parsed AST.
- Linked editing for namespace/use alias edits.
- Call hierarchy and type hierarchy.

### Refactoring And Editing

- Rename for classes, functions, methods, properties, constants, and local
  variables.
- Quick fixes to import unresolved classes and functions.
- Source action to organize imports.
- Refactor action to add return types from PHPDoc when supported by the target
  PHP version.
- Document formatting, range formatting, and on-type formatting through
  external formatters (`php-cs-fixer`, `phpcbf`, or a custom command).

### Editor UI

- Status bar popup with indexing status, file/percentage progress, symbol count,
  stubs information, active diagnostics/analyzers, formatter, include paths, and
  server binary details.
- Semantic tokens with full and delta updates.
- Inlay hints for call arguments and PHPDoc-inferred parameter/return types.
- Code lenses with reference counts.
- Folding ranges for PHP structures, comments, arrays, and blocks.

### Workspace Support

- Composer autoload support for PSR-4, PSR-0, classmap, and files entries.
- Additional include and exclude paths from extension configuration.
- Built-in phpstorm-stubs bundle with configurable extension stubs.
- Lazy `vendor/` indexing.
- Multi-root workspace support.
- Watched PHP file changes and LSP file-operation notifications.
- Runtime configuration updates through `workspace/didChangeConfiguration`.

## Known Limitations

- Production hardening is still in progress. The current risk register and
  baseline measurements live in `docs/production-risk-register.md` and
  `docs/production-baseline.md`.
- Workspace, stub, and lazy vendor file symbols are cached in separate disk
  namespaces; Composer vendor metadata is cached in memory with an LRU for
  lazy vendor symbols. Large-project acceptance thresholds are still production
  hardening work.
- `references`, `rename`, and reference-count code lenses can still scan and
  reparse indexed workspace files. They are functional, but not yet optimized
  for very large repositories.
- Workspace indexing parses files through a bounded CPU-aware task queue;
  large-project acceptance thresholds are still being measured.
- Heavy operations do not yet have general LSP request cancellation support.
- Rapid `didChange` bursts are handled synchronously without a debounce/version
  queue for diagnostics.
- Built-in stubs are configurable, but version-gated symbols from phpstorm-stubs
  are not yet filtered for every `phpLsp.phpVersion` case.
- Cross-file local variable analysis is intentionally limited; variable
  references and rename are local-scope oriented.
- Type inference is useful but still shallow compared with mature PHP static
  analyzers.
- Diagnostics are optimized for editor feedback: file changes publish fast
  in-process diagnostics, while full diagnostics and optional external analyzer
  runs are used on open/save and reconfiguration.
- External PHPStan/Psalm diagnostics require those tools to be installed and
  configured by the workspace.
- Formatting is delegated to external tools; php-lsp does not implement a native
  PHP formatter.

## Configuration

The VS Code extension contributes these settings under `phpLsp.*`:

| Setting | Default | Description |
|---|---:|---|
| `phpLsp.enable` | `true` | Enable the language server. |
| `phpLsp.phpVersion` | `8.2` | Target PHP version for diagnostics and version-aware refactors (`7.4`-`8.4`). |
| `phpLsp.serverPath` | `""` | Custom server binary path. Empty uses the bundled binary. |
| `phpLsp.includePaths` | `[]` | Additional relative or absolute directories/files to include in workspace indexing. |
| `phpLsp.excludePaths` | `[]` | Relative or absolute directories/files to exclude from workspace indexing. |
| `phpLsp.stubs.extensions` | Common extensions | PHP stub extension set to index from the bundled stubs. |
| `phpLsp.composer.enabled` | `true` | Enable `composer.json` autoload indexing. |
| `phpLsp.indexVendor` | `true` | Index `vendor/` lazily. |
| `phpLsp.diagnostics.mode` | `basic-semantic` | `off`, `syntax-only`, or `basic-semantic`. |
| `phpLsp.formatting.provider` | `none` | `none`, `php-cs-fixer`, `phpcbf`, or `custom`. |
| `phpLsp.formatting.command` | `""` | Custom formatter command; use `{file}` for the temporary PHP file. |
| `phpLsp.phpstan.enabled` | `false` | Enable PHPStan diagnostics. |
| `phpLsp.phpstan.command` | `vendor/bin/phpstan ... {file}` | PHPStan command that prints JSON output. |
| `phpLsp.phpstan.timeoutMs` | `30000` | PHPStan timeout per file. |
| `phpLsp.psalm.enabled` | `false` | Enable Psalm diagnostics. |
| `phpLsp.psalm.command` | `vendor/bin/psalm ... {file}` | Psalm command that prints JSON output. |
| `phpLsp.psalm.timeoutMs` | `30000` | Psalm timeout per file. |
| `phpLsp.trace.server` | `off` | LSP transport trace: `off`, `messages`, or `verbose`. |
| `phpLsp.logLevel` | `info` | Server log level: `error`, `warn`, `info`, `debug`, or `trace`. |

Example external diagnostics setup:

```json
{
  "phpLsp.phpstan.enabled": true,
  "phpLsp.phpstan.command": "vendor/bin/phpstan analyse --error-format=json --no-progress --no-interaction {file}",
  "phpLsp.psalm.enabled": true,
  "phpLsp.psalm.command": "vendor/bin/psalm --output-format=json --no-progress {file}"
}
```

Example external formatting setup:

```json
{
  "phpLsp.formatting.provider": "php-cs-fixer"
}
```

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
make check      # run Rust/TypeScript checks
```

`make` uses the host Rust target detected from `rustc -vV`, builds a release
server binary into `client/bin/<platform>/`, bundles phpstorm-stubs into
`client/stubs/`, builds the TypeScript extension, and packages a `.vsix`.

Available targets:

| Command | Description |
|---|---|
| `make` / `make all` / `make package` | Full build: server + client + stubs → `.vsix` |
| `make install` | Build and install `.vsix` into VS Code |
| `make server` | Build a release Rust binary for the detected host platform and copy it to `client/bin/<platform>/` |
| `make server-all` | Cross-compile server binaries for all configured targets |
| `make package-all` | Universal `.vsix` with all configured platform binaries |
| `make client` | `npm ci` + build extension JS |
| `make stubs` | Init submodule + bundle phpstorm-stubs |
| `make check` | Lint + test (Rust & TypeScript) |
| `make test` | Run Rust tests |
| `make lint` | `cargo fmt --check`, `clippy`, `tsc --noEmit` |
| `make fmt` | Auto-format Rust code |
| `make release` | Read `VERSION`, patch package/Cargo versions, commit, force-update the release tag, and push |
| `make clean` | Remove all build artefacts |

Stubs submodule (`server/data/stubs`) is pulled automatically on first build if not initialized.

`make server-all` and `make package-all` use `scripts/build-server.sh --all`
for these VS Code platform directories:

- `linux-x64`
- `linux-arm64`
- `darwin-x64`
- `darwin-arm64`
- `win32-x64`
- `win32-arm64`

Published Linux binaries are built from the GNU targets
(`*-unknown-linux-gnu`). Alpine/musl is not part of the universal VSIX release
target set.

`make release` requires a clean working tree, reads the semver value from
`VERSION`, updates `client/package.json`, `client/package-lock.json`,
`server/Cargo.toml`, and `server/Cargo.lock`, commits those version changes
when needed, creates or updates tag `v<VERSION>`, then pushes `main` and the
tag to GitHub. Build the universal package with `make package-all` before
publishing release artefacts. The GitHub release workflow also publishes the
packaged extension to VS Marketplace using the `VSCE_PAT` repository secret.

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
npx @vscode/vsce package --no-dependencies
```

#### Cross-compilation

```bash
./scripts/build-server.sh x86_64-unknown-linux-gnu # specific target
./scripts/build-server.sh --all                    # configured targets
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
├── images/          # README and marketplace media
├── scripts/         # Build helpers (build-server.sh, bundle-stubs.sh)
└── test-fixtures/   # Test PHP projects
```

## License

MIT
