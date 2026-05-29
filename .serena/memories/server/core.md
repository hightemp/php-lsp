# Server Core

- Rust workspace lives in `server/`; primary crates are under `server/crates/`.
- `php-lsp-server`: binary/LSP orchestration; `src/server.rs` owns `PhpLspBackend`, common state, constructor, and `LanguageServer` trait wiring. Feature request bodies should live under `src/lsp/`; indexing helpers under `src/indexing/`; shared server utilities under `src/util/`.
- `php-lsp-parser`: tree-sitter PHP wrapper, incremental parsing, symbol extraction, PHPDoc, references, semantic tokens, parser diagnostics, UTF-16 helpers.
- `php-lsp-index`: workspace index, Composer autoload, stub loading, vendor lazy indexing, cache.
- `php-lsp-completion`: completion context detection and item generation.
- `php-lsp-types`: shared data models (`SymbolInfo`, `FileSymbols`, `TypeInfo`, `SymbolReference`, `PhpSymbolKind`) and should remain dependency-light.