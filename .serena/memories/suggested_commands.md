# Suggested Commands

- Server workspace: `cd server && cargo build --release`.
- Full Rust tests: `cd server && cargo test --all`.
- Server format check: `cd server && cargo fmt --all --check`.
- Server clippy: `cd server && cargo clippy --all-targets -- -D warnings` or narrowed with `-p php-lsp-server` for server-only work.
- Client install: `cd client && npm ci`.
- Client checks: `cd client && npm run lint && npm run build`.
- Root shortcuts: `make check`, `make check-server`, `make test-server`, `make test-e2e`, `make check-client`.
- Packaging: `./scripts/build-server.sh`; bundle stubs with `./scripts/bundle-stubs.sh`.
- Initialize stubs/submodules when packaging requires them: `git submodule update --init --recursive`.
- Search with `rg`; do not use `mgrep`.