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
- Best-effort PHPDoc template metadata, PHPStan/Psalm type aliases and imported
  aliases, and inherited generic member type substitution for common repository
  and collection patterns.
- Call-site inference for PHPStan/Psalm conditional return types and
  `class-string<T>` factory/service-locator patterns in hover, completion
  chains, and local variable type inlay hints.
- Shape-aware inference for PHPDoc `array{...}` / `object{...}` and literal
  array shapes in completion and key navigation.
- Closure and arrow-function parameter inference from `callable(...)`
  signatures, including generic map/filter-style collection callbacks and
  `array_map`-style helpers.
- Framework-aware static providers for common Laravel string keys and Symfony
  Twig template names without booting the application.
- Blade-like and Symfony/Twig template documents use virtual PHP plus source
  maps for conservative hover, completion, definition, diagnostics, and semantic
  tokens in supported template expressions and control blocks.
- Override signature and PHP-version compatibility diagnostics.
- Optional PHPStan and Psalm diagnostics through configured external commands.
- Per-category diagnostic severity controls for unknown symbols, unused code,
  duplicate symbols, members, type compatibility, override signatures, and
  PHP-version checks.
- Test-friendly diagnostics for common PHPUnit patterns, including assertion
  helpers, test doubles, trait-based test helpers, anonymous classes, and
  closure/destructuring variable scopes.
- Hover for symbols, signatures, types, variables, PHPDoc, deprecation, and
  PHPDoc virtual members.
- Completion for classes, interfaces, traits, enums, functions, constants,
  methods, properties, variables, namespaces, keywords, snippets, PHPDoc virtual
  members, shape keys/properties, framework string keys, template paths, and
  auto-import edits.
- Completion resolve enriches PHPDoc virtual member completions.
- Signature help for functions, methods, constructors, and active parameter
  tracking.
- Inlay hints for argument labels, inferred PHPDoc parameter/return types, and
  useful inferred local variable types.
- Semantic tokens with full, delta, and range requests.

### Navigation

- Go to definition for indexed symbols, local variables, `$this`, constructors,
  PHPDoc virtual members, PHPDoc/literal shape keys, static framework string
  keys, template paths, and lazy vendor fallback.
- Go to declaration for imports, with definition fallback.
- Go to type definition for inferred variables, members, function returns, and
  indexed symbol types.
- Go to implementation for interface/trait/base types and methods.
- Find references through indexed per-file references and same-scope local
  variable references.
- Document highlight for local variables and non-local symbols.
- Selection ranges based on the parsed AST.
- Linked editing for namespace/use alias edits.
- Document links for statically resolvable `include`/`require` paths.

### Symbols And Hierarchies

- Nested document symbols for namespaces, types, and members, including
  signatures and deprecation tags.
- Ranked workspace symbol search over the indexed workspace.
- Call hierarchy for functions, methods, constructors, incoming calls, and
  outgoing calls.
- Type hierarchy for classes, interfaces, traits, enums, supertypes, and
  subtypes.

### Refactoring And Editing

- Rename for classes, functions, methods, properties, constants, and local
  variables.
- Prepare rename rejects unsupported or built-in targets before editing.
- Quick fixes to import unresolved classes/functions, remove unused imports,
  apply diagnostic replacement metadata, and optionally map PHPStan/Psalm
  findings to local fixes.
- Source action to organize imports.
- Quick fix to implement missing interface, abstract parent, and abstract trait
  methods while preserving PHPDoc, analyzer tags, attributes, visibility,
  static, params, defaults, and native-safe return types.
- Refactor actions to generate constructors and property getters/setters from
  indexed properties.
- Refactor actions to change member visibility and promote simple constructor
  assignments to constructor property promotion.
- Refactor action to synchronize PHPDoc `@param` and `@return` tags from
  function/method signatures while preserving richer analyzer-specific tags.
- Refactor action to add return types from PHPDoc when supported by the target
  PHP version.
- Refactor actions to extract selected expressions to local variables, extract
  class-scope literals to constants, and inline simple same-block local
  variables.
- Heavy refactor edits use `codeAction/resolve` so initial code-action requests
  stay lightweight.
- Document formatting, range formatting, and on-type formatting through
  auto-detected or configured external formatters (`pint`, `php-cs-fixer`,
  `phpcbf`, or a custom command).

### Editor UI

- Status bar popup with indexing status, file/percentage progress, symbol count,
  stubs information, active diagnostics/analyzers, formatter, include paths, and
  server binary details.
- Code lenses with reference counts.
- Folding ranges for PHP structures, comments, arrays, and blocks.
- Document formatting and range formatting through auto-detected or configured
  external tools.
- On-type indentation edits for newline, semicolon, and closing brace.

### CLI And Tooling

- Running `php-lsp` without a subcommand starts the LSP server on stdio.
- `php-lsp init-config` creates a starter `.php-lsp.toml` file.
- `php-lsp analyze [PATH]` runs the same parser, workspace index, and built-in
  diagnostics pipeline from the command line.
- `analyze` supports `--project-root <DIR>`,
  `--severity <all|hint|info|warning|error>`, and
  `--format <table|json|github>`.
- Analyze output is available as a local table, stable JSON for scripts, or
  GitHub workflow annotations.
- `php-lsp fix [PATH] --dry-run` previews safe native fixes without writing
  files.
- `fix` supports repeated `--rule` values for `unused-imports`,
  `organize-imports`, and `add-return-type`, plus `--format <table|json>`.

### Workspace Support

- Initialization options and runtime configuration updates through
  `workspace/didChangeConfiguration`.
- Composer autoload support for PSR-4, PSR-0, classmap, and files entries.
- Additional include and exclude paths from extension configuration.
- Built-in phpstorm-stubs bundle with configurable extension stubs.
- Lazy `vendor/` indexing.
- Multi-root workspace support.
- Watched PHP file changes and LSP file-operation notifications.
- Create/change/delete PHP file events reindex or remove symbols from the
  workspace index.
- Rename file notifications move indexed file state from old URI to new URI.

## Known Limitations

- Production validation has measured a primary 10k-file Symfony workspace and
  two additional Laravel-like workspaces. Remaining GA work is tracked in
  `docs/production-risk-register.md` and `docs/production-baseline.md`.
- Workspace, stub, and lazy vendor file symbols are cached in separate disk
  namespaces; Composer vendor metadata is cached in memory with an LRU for
  lazy vendor symbols. The primary large-workspace warm cache target is met;
  installed-vendor first-hit behavior remains a watch item.
- `references`, `rename`, and reference-count code lenses use indexed
  per-file references, but still iterate workspace reference sets and can be
  expensive on very large repositories.
- Workspace indexing parses files through a bounded CPU-aware task queue; the
  primary large-workspace indexing baseline is measured in
  `docs/production-baseline.md`.
- Heavy references/rename requests, background indexing, and external analyzers
  have cancellation coverage; some other heavy requests remain benchmark watch
  items.
- Rapid `didChange` bursts still refresh parser/index state on each accepted
  edit, while diagnostics are debounced and version-checked.
- Built-in stubs are configurable and version-filtered for supported
  phpstorm-stubs version-gating metadata. New metadata forms may require parser
  updates.
- Cross-file local variable analysis is intentionally limited; variable
  references and rename are local-scope oriented.
- Type inference includes common PHPDoc generic inheritance bindings,
  `class-string<T>` call-site bindings, conditional return fallbacks, and
  class/file-scoped PHPStan/Psalm type aliases, callback parameter inference,
  `Generator<TKey,TValue>` foreach key/value inference, and best-effort
  PHPDoc/literal shapes, but it is still shallow compared with mature PHP
  static analyzers.
- Built-in semantic diagnostics depend on indexed project and vendor symbols.
  If Composer/vendor metadata is absent, external framework classes can be
  reported as unknown; dynamic framework APIs such as some Eloquent relation
  members are best-effort.
- Template support is conservative. Blade-like and Twig documents are not full
  template-engine implementations; diagnostics are syntax-only on mapped virtual
  PHP, Twig filters/functions/tests are treated as mixed unless statically
  modeled, and Twig context variables are inferred only from static
  `render(..., [...])` call sites and simple context expressions.
- Diagnostics are optimized for editor feedback: file changes publish fast
  in-process diagnostics, while full diagnostics and optional external analyzer
  runs are used on open/save and reconfiguration.
- External PHPStan/Psalm diagnostics require those tools to be installed and
  configured by the workspace.
- Formatting is delegated to external tools; php-lsp auto-detects common
  Composer dev tools but does not implement or advertise a native PHP
  formatter provider.

## Configuration

The VS Code extension contributes these settings under `phpLsp.*`:

| Setting | Default | Description |
|---|---:|---|
| `phpLsp.enable` | `true` | Enable the language server. |
| `phpLsp.phpVersion` | `8.2` | Target PHP version for diagnostics and version-aware refactors (`7.4`-`8.4`). |
| `phpLsp.serverPath` | `""` | Custom server binary path. Empty uses the bundled binary, then falls back to `php-lsp` from `PATH` if the bundled binary is missing. |
| `phpLsp.includePaths` | `[]` | Additional relative or absolute directories/files to include in workspace indexing. |
| `phpLsp.excludePaths` | `[]` | Relative or absolute directories/files to exclude from workspace indexing. |
| `phpLsp.stubs.extensions` | Common extensions | PHP stub extension set to index from the bundled stubs. |
| `phpLsp.composer.enabled` | `true` | Enable `composer.json` autoload indexing. |
| `phpLsp.indexVendor` | `true` | Index `vendor/` lazily. |
| `phpLsp.diagnostics.mode` | `basic-semantic` | `off`, `syntax-only`, or `basic-semantic`. |
| `phpLsp.diagnostics.severity` | Category warnings | Per-category severity for `unknownSymbols`, `unused`, `duplicateSymbols`, `members`, `typeCompatibility`, `overrideSignatures`, and `phpVersion`; values are `off`, `error`, `warning`, `information`, or `hint`. |
| `phpLsp.formatting.provider` | `auto` | `auto`, `none`, `pint`, `php-cs-fixer`, `phpcbf`, or `custom`. |
| `phpLsp.formatting.command` | `""` | Custom formatter command; use `{file}` for the temporary PHP file. |
| `phpLsp.formatting.timeoutMs` | `30000` | External formatter timeout per request. |
| `phpLsp.phpstan.enabled` | `false` | Enable PHPStan diagnostics. |
| `phpLsp.phpstan.command` | `vendor/bin/phpstan ... {file}` | PHPStan command that prints JSON output. |
| `phpLsp.phpstan.timeoutMs` | `30000` | PHPStan timeout per file. |
| `phpLsp.psalm.enabled` | `false` | Enable Psalm diagnostics. |
| `phpLsp.psalm.command` | `vendor/bin/psalm ... {file}` | Psalm command that prints JSON output. |
| `phpLsp.psalm.timeoutMs` | `30000` | Psalm timeout per file. |
| `phpLsp.analyzerCodeActions.enabled` | `false` | Enable opt-in quick fixes for PHPStan and Psalm diagnostics when diagnostic metadata is available. |
| `phpLsp.trace.server` | `off` | LSP transport trace: `off`, `messages`, or `verbose`. |
| `phpLsp.logLevel` | `info` | Server log level: `error`, `warn`, `info`, `debug`, or `trace`. |

Shared project defaults can also be stored in `.php-lsp.toml`. Use
`php-lsp init-config` to create a default file without overwriting an existing
one. Config precedence is built-in defaults, global config, project config, then
explicit VS Code settings. See [Configuration](docs/configuration.md).

Example external diagnostics setup:

```json
{
  "phpLsp.phpstan.enabled": true,
  "phpLsp.phpstan.command": "vendor/bin/phpstan analyse --error-format=json --no-progress --no-interaction {file}",
  "phpLsp.psalm.enabled": true,
  "phpLsp.psalm.command": "vendor/bin/psalm --output-format=json --no-progress {file}",
  "phpLsp.analyzerCodeActions.enabled": true
}
```

Example external formatting setup:

```json
{
  "phpLsp.formatting.provider": "php-cs-fixer"
}
```

Formatter resolution order:

1. Explicit `phpLsp.formatting.*` settings or `[formatting]` values in
   `.php-lsp.toml`.
2. Composer metadata auto-detection from `require-dev`/`require`: `laravel/pint`,
   `friendsofphp/php-cs-fixer`, then `squizlabs/php_codesniffer`.
3. No formatting provider when no explicit provider or supported Composer tool is
   available.

External formatter commands are timeout-bound and cancelled when the document
changes, closes, or a newer formatting request supersedes the old one. Range
formatting stays conservative: php-lsp formats only the selected fragment via a
temporary file and never silently formats the whole document for a range request.

## CLI

```bash
php-lsp analyze [PATH] --project-root <DIR> --severity warning --format table
php-lsp fix [PATH] --dry-run --project-root <DIR> --rule unused-imports --format json
```

`PATH` can be a PHP file or directory. When it is omitted, php-lsp analyzes the
effective project root. CLI commands load the same global/project
`.php-lsp.toml` configuration used by the language server for PHP version,
diagnostic mode/severity, Composer discovery, and include/exclude paths.

Analyze exit codes:

| Code | Meaning |
|---:|---|
| `0` | No diagnostics at the requested severity. |
| `1` | Execution or configuration error. |
| `2` | Diagnostics were found. |

Output formats:

| Format | Use |
|---|---|
| `table` | Human-readable local output. |
| `json` | Stable machine-readable report with `schemaVersion`, `summary`, and `diagnostics`. |
| `github` | GitHub Actions workflow annotations. |

Fix dry-run mode:

- `php-lsp fix` currently requires `--dry-run` and refuses to write files.
- Without `--rule`, it runs the preferred safe native fixers: unused imports and
  PHPDoc-derived return types that can be represented as native PHP return
  types for the configured PHP version.
- `--rule` can be repeated. Supported values are `unused-imports`,
  `organize-imports`, and `add-return-type`.
- Exit code `0` means no edits would be produced, `1` means execution or
  configuration error, and `2` means edits would be produced.
- The fix command does not run project formatters.
- CI and local example scripts are documented in [CLI And CI Usage](docs/cli-ci.md).

## Commands

The extension contributes these VS Code commands:

| Command palette title | Command ID | Behavior |
|---|---|---|
| `PHP: Show Language Server Status` | `phpLsp.showStatus` | Opens the status quick pick with indexing, cache, stubs, diagnostics, formatter, analyzer, and server-binary details. |
| `PHP: Show Language Server Version` | `phpLsp.showServerVersion` | Shows the initialized server name/version plus resolved binary, platform, stubs, cache roots, and last startup errors. |
| `PHP: Restart Language Server` | `phpLsp.restartServer` | Restarts the client/server process and reuses the existing disk cache. |
| `PHP: Clear PHP LSP Cache and Restart` | `phpLsp.clearCacheAndRestart` | Deletes cache directories for current workspace roots and discovered Composer roots, then restarts the server. |

## Documentation

- [Architecture](docs/architecture.md): server/client data flow, indexing,
  cache model, diagnostics pipeline, and runtime configuration behavior.
- [Configuration](docs/configuration.md): `.php-lsp.toml` discovery,
  precedence, schema, and examples.
- [CLI and CI usage](docs/cli-ci.md): GitHub Actions reporting and local CLI
  examples.
- [LSP feature matrix](docs/lsp-features.md): supported, partial, and
  unsupported LSP behavior.
- [Performance guide](docs/performance.md): baseline methodology, profiling
  commands, cache interpretation, and production acceptance metrics.
- [Production baseline](docs/production-baseline.md): current measured
  validation and performance numbers.
- [Production risk register](docs/production-risk-register.md): tracked
  production gaps and exit signals.

## Troubleshooting

### Server Does Not Start

- Check `PHP: Show Language Server Status` or
  `PHP: Show Language Server Version` for the resolved server binary path,
  source, platform target, and last startup error.
- If `phpLsp.serverPath` is set, verify that it points to an executable
  `php-lsp` binary.
- If using the bundled binary, verify that your platform is one of
  `linux-x64`, `linux-arm64`, `darwin-x64`, `darwin-arm64`, `win32-x64`, or
  `win32-arm64`.
- If the bundled binary is absent and `phpLsp.serverPath` is empty, the client
  tries `php-lsp` from `PATH` and logs the selected source in the LSP output
  channel.
- Set `"phpLsp.logLevel": "debug"` and `"phpLsp.trace.server": "messages"` for
  more output.

### Indexing Is Slow Or Stale

- Use `PHP: Show Language Server Status` to inspect indexed file count, cache
  path, stubs path, include/exclude paths, and analyzer settings.
- Add generated directories to `phpLsp.excludePaths`.
- Keep `phpLsp.indexVendor` enabled for lazy vendor lookup, but exclude very
  large generated vendor subtrees if they are not useful.
- Use `PHP: Clear PHP LSP Cache and Restart` when changing branches, Composer
  metadata, stubs, or project layout and the disk cache looks stale.

### Diagnostics Are Too Noisy

- Ensure Composer metadata and `vendor/composer/installed.json` are available
  when you want built-in diagnostics to resolve external framework symbols.
- Set `"phpLsp.diagnostics.mode": "syntax-only"` to keep only parser syntax
  diagnostics.
- Set `"phpLsp.diagnostics.mode": "off"` to disable built-in diagnostics.
- Prefer per-category severity controls when only one category is noisy:

```json
{
  "phpLsp.diagnostics.severity": {
    "members": "off",
    "unused": "hint"
  }
}
```

### PHPStan Or Psalm Diagnostics Do Not Appear

- Enable the analyzer explicitly with `phpLsp.phpstan.enabled` or
  `phpLsp.psalm.enabled`.
- Make sure the configured command works from the workspace root and prints JSON.
- Keep `{file}` in the command template unless the tool should receive the file
  path appended at the end.
- Increase `phpLsp.phpstan.timeoutMs` or `phpLsp.psalm.timeoutMs` for slow
  projects.

### Formatting Does Nothing

- With the default `auto` provider, make sure Composer `require-dev` includes
  `laravel/pint`, `friendsofphp/php-cs-fixer`, or `squizlabs/php_codesniffer`.
- Set `phpLsp.formatting.provider` explicitly to `pint`, `php-cs-fixer`,
  `phpcbf`, or `custom` to bypass auto-detection; set it to `none` to disable
  formatting.
- For `custom`, configure `phpLsp.formatting.command` and include `{file}` where
  the temporary PHP file path should be inserted.
- Ensure the formatter executable is available from the workspace root.
- Increase `phpLsp.formatting.timeoutMs` if the external formatter times out on
  large files.

## Architecture Summary

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
