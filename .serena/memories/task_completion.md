# Task Completion

- For Rust server changes: run `cd server && cargo fmt --all --check`, `cd server && cargo check -p php-lsp-server --tests` for server work, relevant focused tests, and `cd server && cargo clippy -p php-lsp-server --all-targets -- -D warnings` when touching `php-lsp-server`.
- For full server acceptance: `cd server && cargo test -p php-lsp-server`; broader cross-crate behavior may require `cd server && cargo test --all`.
- For client changes: `cd client && npm run lint && npm run build`.
- Always run `git diff --check` before handoff.
- For server.rs refactor work, also check `wc -l server/crates/php-lsp-server/src/server.rs` and update `TASKS.md` with completed extraction steps and final/current line count.