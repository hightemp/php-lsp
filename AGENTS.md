# Repository Guidelines

## Project Structure & Module Organization
- `server/` contains the Rust workspace for the language server.
  Key crates live in `server/crates/`: `php-lsp-server` (binary), `php-lsp-parser`, `php-lsp-index`, `php-lsp-completion`, and `php-lsp-types`.
- `client/` contains the VS Code extension (`src/extension.ts`, build output in `out/`).
- `test-fixtures/` contains PHP sample projects used by tests and parser/index scenarios.
- `scripts/` contains release helpers such as `build-server.sh` and `bundle-stubs.sh`.
- `server/data/stubs/` is a git submodule (phpstorm-stubs) used for bundled PHP symbols.

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

## Coding Style & Naming Conventions
- Rust: follow `rustfmt` defaults (4-space indentation), `snake_case` for functions/modules, `CamelCase` for types.
- TypeScript: strict mode is enabled; keep imports explicit and prefer existing 2-space formatting style in `client/src`.
- Keep crate and package names consistent with current `php-lsp-*` naming.

## Testing Guidelines
- Add Rust tests close to changed behavior and run `cargo test --all` before opening a PR.
- End-to-end protocol tests are in `server/crates/php-lsp-server/tests/e2e.rs`.
- Use descriptive test names (for example, `test_open_file_and_hover`).
- For client-side changes, always run both `npm run lint` and `npm run build`.

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