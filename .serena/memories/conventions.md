# Conventions

- Before code work, record the planned task in `TASKS.md`; after completion, mark it done there.
- Behavior-preserving refactors must not change URI semantics, range semantics, diagnostics/completion behavior, indexing behavior, or LSP response shapes.
- Ranges: tree-sitter uses byte columns; LSP uses UTF-16. `SymbolInfo.range`, `selection_range`, and `UseStatement.range` are parser byte-column ranges unless documented otherwise. `SymbolReference.range` is already UTF-16. Convert byte-backed ranges before LSP responses with `php_lsp_parser::utf16::range_byte_to_utf16` or `Utf16LineIndex`.
- URI handling: do not add raw `format!("file://{}", path.display())`; use shared helpers. Cache migration must tolerate malformed/legacy URI entries by invalidating/rebuilding, not crashing.
- Async/IO: no unbounded blocking filesystem/process work directly in async LSP handlers; use existing blocking helpers or `tokio::task::spawn_blocking`; drop locks/DashMap guards before `.await`.
- Security: project `.php-lsp.toml` is untrusted for command execution by default. Respect explicit trust gates for formatter/phpstan/psalm command execution.
- Rename/indexing: use `WorkspaceIndex::update_file_with_references()` when symbols and references are available; use `WorkspaceIndex::remove_file()` on delete/exclusion/root removal; do not destructively rename unresolved member references.
- Rust style: rustfmt defaults, `snake_case` functions/modules, `CamelCase` types. TypeScript: strict mode, explicit imports, current 2-space style in `client/src`.
- Keep crate/package names in the `php-lsp-*` pattern. Avoid project-specific hardcode and duplicate docs.