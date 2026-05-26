# LSP Feature Matrix

This matrix documents the behavior advertised by the current server. "Partial"
means the server implements the LSP method, but the behavior is intentionally
limited, performance-sensitive on large workspaces, or delegated to external
tools.

## Status Legend

| Status | Meaning |
|---|---|
| Supported | Implemented and expected to work for normal PHP projects. |
| Partial | Implemented with known limits documented in the notes. |
| Unsupported | Not advertised or returns no edit/result by design. |

## Synchronization And Workspace

| LSP feature | Status | Notes |
|---|---|---|
| `initialize` / `initialized` | Supported | Applies initialization options, loads stubs, starts background indexing, publishes status notifications. |
| `textDocument/didOpen` | Supported | Parses editor text, updates index, publishes diagnostics. |
| `textDocument/didChange` | Supported | Incremental parser edits, index refresh, version checks, debounced fast diagnostics. |
| `textDocument/didSave` | Supported | Publishes full diagnostics, including enabled external analyzers. |
| `textDocument/didClose` | Supported | Clears parser state, diagnostics, semantic-token state, and pending analyzer work. |
| `workspace/didChangeWatchedFiles` | Supported | Reindexes changed/created PHP files and removes deleted files. |
| `workspace/didChangeConfiguration` | Supported | Runtime updates for diagnostics, stubs, indexing, vendor, formatter, analyzers, and logging. |
| `workspace/didChangeWorkspaceFolders` | Supported | Adds/removes roots and indexes new roots. |
| `workspace/willCreateFiles` | Partial | Advertised for PHP files but currently returns no edit. |
| `workspace/didCreateFiles` | Supported | Reindexes created PHP files. |
| `workspace/willRenameFiles` | Unsupported | Not advertised. |
| `workspace/didRenameFiles` | Supported | Moves indexed file state from old URI to new URI. Does not rewrite namespaces/classes. |
| `workspace/willDeleteFiles` | Partial | Advertised for PHP files but currently returns no edit. |
| `workspace/didDeleteFiles` | Supported | Removes indexed symbols for deleted PHP files. |

## Navigation

| LSP feature | Status | Notes |
|---|---|---|
| `textDocument/definition` | Supported | Handles indexed symbols, local variables, `$this`, constructors, PHPDoc virtual members, and lazy vendor fallback. |
| `textDocument/declaration` | Supported | Goes to import declarations when applicable, otherwise falls back to definition. |
| `textDocument/typeDefinition` | Supported | Resolves variable/member/function return types where inferred or indexed. |
| `textDocument/implementation` | Supported | Interface/trait/base type to implementations, and method implementation lookup. |
| `textDocument/references` | Partial | Uses indexed per-file references for symbols and same-scope references for local variables. Workspace-wide references can still be expensive on large workspaces. |
| `textDocument/documentHighlight` | Supported | Local variables and non-local symbols in the current document. |
| `textDocument/selectionRange` | Supported | AST-based selection expansion. |
| `textDocument/linkedEditingRange` | Partial | Namespace/use alias ranges only. |
| `textDocument/documentLink` | Supported | Static `include`, `include_once`, `require`, and `require_once` paths resolve to existing local files. |

## Symbols And Hierarchies

| LSP feature | Status | Notes |
|---|---|---|
| `textDocument/documentSymbol` | Supported | Nested namespace/type/member symbols with signatures and deprecation tags. |
| `workspace/symbol` | Supported | Ranked search over indexed workspace symbols, limited to 200 results. |
| `textDocument/prepareCallHierarchy` | Supported | Functions, methods, constructors, and containing callable fallback. |
| `callHierarchy/incomingCalls` | Partial | Scans indexed files and can read unopened files. Can be expensive on large workspaces. |
| `callHierarchy/outgoingCalls` | Supported | Reads the target callable file and resolves outgoing calls through the index. |
| `textDocument/prepareTypeHierarchy` | Supported | Classes, interfaces, traits, and enums. |
| `typeHierarchy/supertypes` | Supported | Uses extends/implements/use relationships and lazy class indexing. |
| `typeHierarchy/subtypes` | Supported | Uses indexed direct subtype relationships. |

## Editing And Refactoring

| LSP feature | Status | Notes |
|---|---|---|
| `textDocument/rename` | Partial | Supports classes, functions, methods, properties, constants, and same-scope local variables. Built-ins and PHPDoc virtual members are not renamed. Workspace rename can still be expensive on large workspaces. |
| `textDocument/prepareRename` | Supported | Rejects unsupported or built-in targets before rename. |
| `textDocument/codeAction` quick fix | Supported | Adds imports for unresolved classes/functions when candidates exist. |
| `textDocument/codeAction` implement missing methods | Supported | Generates concrete stubs for missing interface, abstract parent, and abstract trait methods. Preserves method PHPDoc, analyzer-specific contract tags, safe method attributes, visibility, static, params, defaults, and native-safe return types. Edits are resolved lazily and stale document versions resolve to a no-op edit. |
| `textDocument/codeAction` generate members | Supported | Generates constructors and property getters/setters from indexed property symbols. Handles readonly/static properties, bool getter naming, nullable/default values, refined property PHPDoc types, analyzer-specific `@phpstan-var`/`@psalm-var` tags, and native-safe signatures. |
| `textDocument/codeAction` visibility and promotion refactors | Supported | Changes visibility for methods, properties, constants, and promoted properties with interface, abstract, and override contract guards. Promotes simple constructor assignment patterns to constructor property promotion, moving safe property PHPDoc and attributes onto the promoted parameter and refusing complex assignment patterns. |
| `textDocument/codeAction` PHPDoc signature sync | Supported | Updates `@param` order/types/tokens and native-return-driven `@return` tags from function/method signatures. Preserves descriptions, analyzer-specific richer generic PHPDoc types, summaries, and unrelated tags such as templates, throws, deprecation, virtual properties, and virtual methods. |
| `source.organizeImports` | Supported | Sorts/removes import statements based on parser/index data. |
| `codeAction/resolve` | Supported | Used for heavier refactor actions so `textDocument/codeAction` can return lightweight actions first. |
| `refactor.rewrite` add return type | Partial | Adds return types from PHPDoc where supported by the configured PHP version. Edits are resolved lazily and stale document versions resolve to a no-op edit. |
| Native PHP formatter | Unsupported | Formatting is delegated to external commands. |
| `textDocument/formatting` | Partial | Requires `phpLsp.formatting.provider` or `phpLsp.formatting.command`. |
| `textDocument/rangeFormatting` | Partial | Requires external formatter; formats selected PHP fragments via temporary files. |
| `textDocument/onTypeFormatting` | Supported | Local indentation edits for newline, semicolon, and closing brace. |

## Intelligence

| LSP feature | Status | Notes |
|---|---|---|
| Diagnostics: syntax | Supported | Tree-sitter syntax errors. |
| Diagnostics: built-in semantic | Supported | Unknown symbols, unused code, duplicate symbols, member access, type compatibility, override signatures, PHP-version checks. Without Composer/vendor metadata, external framework symbols can be reported as unknown; highly dynamic framework members such as some Eloquent relation APIs remain best-effort. |
| Diagnostics: PHPStan | Partial | Optional external command, timeout-bound, JSON output required. |
| Diagnostics: Psalm | Partial | Optional external command, timeout-bound, JSON output required. |
| `textDocument/hover` | Supported | Symbols, signatures, types, PHPDoc, variables, deprecation, PHPDoc virtual members. |
| `textDocument/completion` | Supported | Classes, interfaces, traits, enums, functions, constants, members, variables, namespaces, keywords, snippets, and auto-import edits. |
| `completionItem/resolve` | Supported | Enriches PHPDoc virtual member completions. |
| `textDocument/signatureHelp` | Supported | Functions, methods, constructors, and active parameter tracking. |
| `textDocument/inlayHint` | Supported | Argument labels and inferred PHPDoc parameter/return hints. |
| `textDocument/codeLens` | Partial | Reference-count lenses for symbols. Counts use indexed references but can still be expensive across very large workspaces. |
| `textDocument/foldingRange` | Supported | PHP structures, comments, arrays, namespaces, and blocks. |
| `textDocument/semanticTokens/full` | Supported | Full semantic token snapshots with result IDs. |
| `textDocument/semanticTokens/full/delta` | Supported | Delta edits from previous full snapshots. |
| `textDocument/semanticTokens/range` | Supported | Range semantic token requests for open files. |

## Explicit Non-Goals For Current Milestone

- Namespace/class rewrites during file rename.
- Native formatter implementation.
- Full PHP static analyzer replacement.
- Complete generic/template/array-shape type system parity with PHPStan/Psalm.
- Guaranteed sublinear references/rename/codeLens performance on very large
  workspaces without additional reference-index sharding or aggregation.
