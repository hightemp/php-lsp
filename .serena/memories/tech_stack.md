# Tech Stack

- Rust stable for the LSP server workspace; `cargo fmt`, `cargo clippy`, and `cargo test` are authoritative validation tools.
- Tree-sitter PHP powers parser behavior.
- VS Code client uses TypeScript strict mode and Node.js 20 in CI.
- PHP sample projects in `test-fixtures/` drive parser/index/e2e scenarios.
- Packaging relies on shell helpers under `scripts/` and bundled phpstorm stubs from `server/data/stubs/` / `client/stubs/`.