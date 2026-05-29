# Repository Guidelines

## Project Structure & Module Organization
- `server/` contains the Rust workspace for the language server.
  Key crates live in `server/crates/`: `php-lsp-server` (binary), `php-lsp-parser`, `php-lsp-index`, `php-lsp-completion`, and `php-lsp-types`.
- `client/` contains the VS Code extension (`src/extension.ts`, build output in `out/`).
- `test-fixtures/` contains PHP sample projects used by tests and parser/index scenarios.
- `scripts/` contains release helpers such as `build-server.sh` and `bundle-stubs.sh`.
- `server/data/stubs/` is a git submodule (phpstorm-stubs) used for bundled PHP symbols.

## Crate Map
- `server/crates/php-lsp-types`
  - Shared data structures used across parser, index, completion, and server.
  - Important models: `SymbolInfo`, `FileSymbols`, `TypeInfo`, `SymbolReference`, `PhpSymbolKind`.
  - Keep this crate dependency-light and avoid server-specific behavior here.
- `server/crates/php-lsp-parser`
  - Tree-sitter PHP wrapper, incremental parsing, symbol extraction, PHPDoc parsing, references, semantic tokens, and parser-side diagnostics.
  - Common entry files: `src/parser.rs`, `src/symbols.rs`, `src/phpdoc.rs`, `src/resolve.rs`, `src/references.rs`, `src/semantic.rs`, `src/utf16.rs`.
- `server/crates/php-lsp-index`
  - Workspace index, Composer autoload metadata, stub loading, vendor lazy indexing, and disk cache.
  - Common entry files: `src/workspace.rs`, `src/composer.rs`, `src/stubs.rs`, `src/cache.rs`.
- `server/crates/php-lsp-completion`
  - Completion context detection and completion item generation.
  - Common entry files: `src/context.rs`, `src/provider.rs`.
- `server/crates/php-lsp-server`
  - LSP server orchestration, CLI analyze/fix, configuration, framework heuristics, and template support.
  - Common entry files: `src/server.rs`, `src/config.rs`, `src/analyze.rs`, `src/fix.rs`, `src/framework.rs`, `src/template.rs`.
  - LSP request bodies and feature helpers live in `src/lsp/`; `src/server.rs` owns `PhpLspBackend`, shared state/config orchestration, and `LanguageServer` trait wiring.
  - Workspace/file-operation handlers and server-side cache/stub/vendor helpers live in `src/indexing/`.
  - Shared server utilities live in `src/util/`.

## `php-lsp-server` Layout

- `src/server.rs`
  - Defines `PhpLspBackend`, shared request caches, runtime configuration state, small cross-feature orchestration helpers, and `LanguageServer` delegation.
  - Keep new LSP feature request bodies and large helper blocks out of this file unless they are only trait wiring or shared backend state.
- `src/lsp/lifecycle.rs`
  - `initialize` and `shutdown` behavior.
- `src/lsp/diagnostics.rs`
  - `didOpen`, `didChange`, `didSave`, `didClose` notification handlers, diagnostic computation, analyzer publishing, and lazy diagnostic filtering.
- `src/lsp/completion.rs`
  - `textDocument/completion`, `completionItem/resolve`, `textDocument/signatureHelp`, and backend completion type/member inference methods.
- `src/lsp/completion_helpers.rs`
  - PHPDoc/framework virtual members, framework string-key helpers, shape completion/definition helpers, local-variable completion helpers, signature-help assembly, and auto-import edit helpers.
- `src/lsp/hover.rs`
  - `textDocument/hover` request assembly; local-variable type display helpers live in `inlay_hints.rs`, and PHPDoc/framework virtual-member helpers live in `completion_helpers.rs`.
- `src/lsp/definition.rs`
  - Definition, declaration, type definition, implementation requests, and import/source-location lookup helpers.
- `src/lsp/references.rs`
  - Document highlight, references, and reference-count code lens requests.
- `src/lsp/rename.rs`
  - Prepare rename and rename request handling.
- `src/lsp/code_action.rs`
  - Code actions, lazy code-action resolve, edit builders, generate-members/refactor/PHPDoc/import fix helpers.
- `src/lsp/formatting.rs`
  - Document/range/on-type formatting request handling.
- `src/lsp/inlay_hints.rs`
  - `textDocument/inlayHint` request handling, local-variable type inference/display, local-variable hover data, call-site type resolution, and related type markdown.
- `src/lsp/semantic_tokens.rs`
  - Full, delta, and range semantic token request handling.
- `src/lsp/hierarchy.rs`
  - Call hierarchy and type hierarchy requests.
- `src/lsp/document_symbols.rs`
  - Document symbols, workspace symbols, selection range, linked editing range, and AST selection helper logic.
- `src/lsp/folding.rs`
  - Folding range requests.
- `src/lsp/document_links.rs`
  - Static include/require document links.
- `src/lsp/templates.rs`
  - Template document lookup, virtual PHP position mapping helpers, Twig/Blade context helpers, and template-aware definition mapping.
- `src/lsp/external_command.rs`
  - Shared external analyzer/formatter command execution helpers, timeouts, cancellation, and JSON output parsing support.
- `src/lsp/conversions.rs`
  - Shared conversions between server `lsp_types` values and `tower_lsp::ls_types` values.
- `src/indexing/workspace.rs`
  - `initialized`, workspace folders, watched files, configuration changes, file-operation handlers, workspace config/exclusion helpers, PHP file collection, workspace parse/index execution, and indexed-root cleanup.
- `src/indexing/cache.rs`
  - Server-side index cache config/hash helpers.
- `src/indexing/stubs.rs`
  - Server-side stub loading and stub cache-source helpers.
- `src/indexing/vendor.rs`
  - Vendor autoload metadata cache, lazy vendor file LRU helpers, vendor path resolution, and lazy FQN/class/member indexing helpers.
- `src/util/uri.rs`
  - Re-exports shared path/URI helpers from `php-lsp-types`; use these instead of raw `file://` string formatting.
- `src/util/lsp_text.rs`
  - UTF-16 LSP position/range to source byte-offset helpers.

## Build, Test, and Development Commands
- Rust server (from `server/`):
  - `cargo build --release` builds the server binary.
  - `cargo test --all` runs unit/integration/e2e tests.
  - `cargo fmt --all --check` enforces formatting.
  - `cargo clippy --all-targets -- -D warnings` treats warnings as errors.
- VS Code client (from `client/`):
  - `npm ci` installs exact dependencies from lockfile.
  - `npm run lint` runs TypeScript type checks (`tsc --noEmit`).
  - `npm run build` bundles extension code with esbuild.
- Packaging helpers (repo root):
  - `./scripts/build-server.sh` builds and copies server binaries to `client/bin/<platform>/`.
  - `./scripts/bundle-stubs.sh` copies default phpstorm stubs into `client/stubs/`.
- Existing root Makefile shortcuts:
  - `make check` runs server fmt/clippy, client lint, and Rust tests.
  - `make test-server` runs `php-lsp-server` crate tests.
  - `make test-e2e` runs split LSP e2e protocol tests.
  - `make check-server` runs Rust fmt, clippy, and tests.
  - `make check-client` runs VS Code client lint/build.

## Coding Style & Naming Conventions
- Rust: follow `rustfmt` defaults (4-space indentation), `snake_case` for functions/modules, `CamelCase` for types.
- TypeScript: strict mode is enabled; keep imports explicit and prefer existing 2-space formatting style in `client/src`.
- Keep crate and package names consistent with current `php-lsp-*` naming.

## Architecture Rules

### Ranges And Positions
- Tree-sitter positions use byte columns.
- LSP positions use UTF-16 columns.
- `SymbolInfo.range`, `SymbolInfo.selection_range`, and `UseStatement.range` are parser byte-column ranges unless a field comment explicitly says otherwise.
- `SymbolReference.range` is already an LSP UTF-16 range.
- Before returning byte-backed ranges to LSP, convert with `php_lsp_parser::utf16::range_byte_to_utf16` or `Utf16LineIndex`.
- Do not return `SymbolInfo.range` or `selection_range` directly through LSP responses.

### URI Handling
- Do not add new raw URI formatting such as `format!("file://{}", path.display())`.
- Use or add a shared path/URI helper that percent-encodes file paths and can decode LSP file URIs back to paths.
- Keep cache migration tolerant: malformed or legacy URI entries should invalidate/rebuild cache data rather than crash the server.

### Async And IO
- Do not add unbounded blocking filesystem or process work directly in async LSP request handlers.
- Use existing blocking helpers or `tokio::task::spawn_blocking` for expensive filesystem/parser/diagnostic work.
- Drop `DashMap` guards and other locks before any `.await`.

### Security
- Project `.php-lsp.toml` is not trusted to execute commands by default.
- Do not auto-run project-provided shell commands without the explicit command trust gate.
- Be careful around `formatting.command`, executable formatter providers, `phpstan.command`, and `psalm.command`.

### Index And Rename
- Use `WorkspaceIndex::update_file_with_references()` when both symbols and references are available.
- Use `WorkspaceIndex::remove_file()` on delete, exclusion, or root removal.
- Do not apply destructive rename edits based only on unresolved member references like `::method` or `::$prop`.
- Exact/type-resolved references are required for safe cross-file member rename.

## Testing Guidelines
- Add Rust tests close to changed behavior and run `cargo test --all` before opening a PR.
- End-to-end protocol tests are split across `server/crates/php-lsp-server/tests/e2e_*.rs`.
- Shared e2e JSON-RPC harness helpers are in `server/crates/php-lsp-server/tests/support/mod.rs`.
- Use descriptive test names (for example, `test_open_file_and_hover`).
- For client-side changes, always run both `npm run lint` and `npm run build`.
- Prefer focused unit tests for parser/index/completion changes and e2e tests only when behavior crosses crate or LSP protocol boundaries.
- If a full test run is too expensive for the current change, run the narrowest relevant test first and state the subset.

## Test Selection
- PHPDoc parser behavior: `cd server && cargo test -p php-lsp-parser phpdoc`.
- Symbol extraction/resolution: `cd server && cargo test -p php-lsp-parser symbols` or a focused resolver test.
- Completion context/provider behavior: `cd server && cargo test -p php-lsp-completion`.
- Workspace index/cache/stubs behavior: `cd server && cargo test -p php-lsp-index`.
- All split LSP e2e tests: `cd server && cargo test -p php-lsp-server --tests`.
- Focused LSP e2e tests: `cd server && cargo test -p php-lsp-server --test e2e_completion <test_name>` or the relevant `e2e_*` target.
- Server helper/config behavior: `cd server && cargo test -p php-lsp-server <test_name>`; extracted `server.rs` unit tests live in `server/crates/php-lsp-server/src/server_tests.rs`.
- Client behavior: `cd client && npm run lint && npm run build`.

## Where To Look
- Completion bugs:
  - `server/crates/php-lsp-completion/src/context.rs`
  - `server/crates/php-lsp-completion/src/provider.rs`
  - `server/crates/php-lsp-server/src/lsp/completion.rs`
  - `server/crates/php-lsp-server/src/lsp/completion_helpers.rs`
  - `server/crates/php-lsp-server/tests/e2e_completion.rs`
- Hover, definition, declaration, and type definition bugs:
  - `server/crates/php-lsp-parser/src/resolve.rs`
  - `server/crates/php-lsp-server/src/lsp/hover.rs`
  - `server/crates/php-lsp-server/src/lsp/definition.rs`
  - `server/crates/php-lsp-server/src/lsp/completion_helpers.rs`
  - `server/crates/php-lsp-server/src/lsp/inlay_hints.rs` for local-variable hover/type display.
  - `server/crates/php-lsp-server/tests/e2e_hover.rs`
  - `server/crates/php-lsp-server/tests/e2e_definition.rs`
- Rename and references bugs:
  - `server/crates/php-lsp-parser/src/references.rs`
  - `server/crates/php-lsp-index/src/workspace.rs`
  - `server/crates/php-lsp-server/src/lsp/rename.rs`
  - `server/crates/php-lsp-server/src/lsp/references.rs`
  - `server/crates/php-lsp-server/tests/e2e_references.rs`
- Diagnostics bugs:
  - `server/crates/php-lsp-parser/src/diagnostics.rs`
  - `server/crates/php-lsp-parser/src/semantic.rs`
  - `server/crates/php-lsp-server/src/lsp/diagnostics.rs`
  - `server/crates/php-lsp-server/tests/e2e_diagnostics.rs`
- Code action/refactor bugs:
  - `server/crates/php-lsp-server/src/lsp/code_action.rs`
  - `server/crates/php-lsp-server/tests/e2e_code_actions.rs`
- Inlay hint bugs:
  - `server/crates/php-lsp-server/src/lsp/inlay_hints.rs`
  - `server/crates/php-lsp-server/tests/e2e_hover.rs`
- Formatting bugs:
  - `server/crates/php-lsp-server/src/lsp/formatting.rs`
  - `server/crates/php-lsp-server/src/lsp/external_command.rs`
  - `server/crates/php-lsp-server/tests/e2e_formatting.rs`
- Semantic token / symbol / folding / document link bugs:
  - `server/crates/php-lsp-server/src/lsp/semantic_tokens.rs`
  - `server/crates/php-lsp-server/src/lsp/document_symbols.rs`
  - `server/crates/php-lsp-server/src/lsp/conversions.rs`
  - `server/crates/php-lsp-server/src/lsp/folding.rs`
  - `server/crates/php-lsp-server/src/lsp/document_links.rs`
  - `server/crates/php-lsp-server/tests/e2e_symbols.rs`
- Hierarchy bugs:
  - `server/crates/php-lsp-server/src/lsp/hierarchy.rs`
  - `server/crates/php-lsp-server/tests/e2e_hierarchy.rs`
- PHPDoc/type bugs:
  - `server/crates/php-lsp-parser/src/phpdoc.rs`
  - `server/crates/php-lsp-parser/src/symbols.rs`
  - `server/crates/php-lsp-types/src/lib.rs`
  - `server/crates/php-lsp-index/src/workspace.rs`.
- Composer, vendor, stubs, and cache bugs:
  - `server/crates/php-lsp-index/src/composer.rs`
  - `server/crates/php-lsp-index/src/stubs.rs`
  - `server/crates/php-lsp-index/src/cache.rs`
  - `server/crates/php-lsp-server/src/indexing/workspace.rs`
  - `server/crates/php-lsp-server/src/indexing/vendor.rs`
  - `server/crates/php-lsp-server/src/indexing/stubs.rs`
  - `server/crates/php-lsp-server/src/indexing/cache.rs`
  - `server/crates/php-lsp-server/tests/e2e_indexing.rs`
- Blade/Twig bugs:
  - `server/crates/php-lsp-server/src/template.rs`
  - `server/crates/php-lsp-server/src/lsp/templates.rs`
  - `server/crates/php-lsp-server/src/lsp/hover.rs`, `completion.rs`, `definition.rs`, and `semantic_tokens.rs` for template-aware LSP behavior.
  - `server/crates/php-lsp-server/tests/e2e_templates.rs`

## Known Pitfalls
- Do not confuse byte columns with UTF-16 columns.
- Do not add new raw `file://` URI construction.
- Do not assume `$this` belongs to the first class in `FileSymbols`; current-class logic must use cursor position.
- Do not rely on phpstorm-stubs being initialized in every checkout; packaging and tests must handle missing stubs deliberately.
- Do not mix broad refactors with behavior fixes unless the task explicitly calls for it.
- Do not create duplicate docs with different casing or overlapping purpose; extend the existing file instead.

## Commit & Pull Request Guidelines
- Use Conventional Commits with scopes when relevant (examples from history: `feat(parser): ...`, `feat(server): ...`, `feat(release): ...`).
- Keep commits focused by concern (parser, index, server, client, release tooling).
- PRs should include: concise summary, affected paths/crates, validation steps run locally, and linked issues.
- If behavior changes in diagnostics/completion/hover, include a minimal fixture or reproduction snippet in the PR description.

## Configuration Tips
- CI uses Node.js 20 for `client/` and Rust stable with `clippy` + `rustfmt`.
- Initialize submodules before packaging work:
  - `git submodule update --init --recursive`

## Важно

1. Перед тем как сделать задачу помечай что будешь делать в TASKS.md.
2. После выполнения задачи отмечай в TASKS.md.
3. При выполнении задач не должно быть хардкода
4. не используй mgrep
