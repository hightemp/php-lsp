# Architecture

This document describes the current php-lsp runtime architecture. It is meant
for production support work: where data is stored, when it is refreshed, and
which paths affect latency.

## Components

| Component | Path | Responsibility |
|---|---|---|
| VS Code client | `client/src/extension.ts` | Starts the bundled or configured server binary, forwards `phpLsp.*` settings, shows status UI, clears disk cache, and registers VS Code commands. |
| Server binary | `server/crates/php-lsp-server` | Implements LSP 3.17 over stdio with `tower-lsp-server`, owns request handlers and orchestration. |
| Parser | `server/crates/php-lsp-parser` | Wraps tree-sitter PHP, incremental edits, symbol extraction, diagnostics helpers, references, semantic tokens, PHPDoc parsing, and type helpers. |
| Index | `server/crates/php-lsp-index` | Stores global workspace symbols, Composer namespace maps, phpstorm-stubs, vendor metadata, and disk cache snapshots. |
| Completion | `server/crates/php-lsp-completion` | Builds completion items from parser context and the global index. |
| Types | `server/crates/php-lsp-types` | Shared symbol, range, signature, type, and reference data structures. |

The VS Code extension launches `php-lsp` over stdio. Server logs go to stderr
through `tracing`, so they do not corrupt JSON-RPC messages.

The server binary builds its Tokio runtime explicitly instead of relying on the
attribute macro default. Worker threads use an 8 MiB stack by default because
large framework projects can combine deeply nested PHPDoc/type expansion,
template diagnostics, and lazy vendor indexing on worker tasks. The stack size
can be overridden with `PHP_LSP_WORKER_THREAD_STACK_SIZE` in bytes; values below
1 MiB are ignored and the default is used.

## Server Crate Layout

The server crate is intentionally split by operational area. New code should
prefer the focused module for its feature instead of growing `server.rs`.

```text
server/crates/php-lsp-server/src/
  server.rs                  # PhpLspBackend state, shared helpers, LanguageServer delegation
  lsp/
    lifecycle.rs             # initialize/shutdown
    diagnostics.rs           # didOpen/didChange/didSave/didClose
    completion.rs            # completion, completion resolve, signature help
    hover.rs                 # hover response assembly
    definition.rs            # definition/declaration/typeDefinition/implementation
    references.rs            # documentHighlight/references/codeLens
    rename.rs                # prepareRename/rename
    code_action.rs           # code actions, lazy resolve, edit/refactor helpers
    formatting.rs            # document/range/on-type formatting
    inlay_hints.rs           # inlayHint request handling
    semantic_tokens.rs       # full/delta/range semantic tokens
    hierarchy.rs             # call hierarchy and type hierarchy
    document_symbols.rs      # document symbols, workspace symbols, selection/linked editing
    folding.rs               # folding ranges
    document_links.rs        # include/require document links
  indexing/
    workspace.rs             # initialized, workspace sync, watched files, file operations
    cache.rs                 # runtime cache config/hash inputs for php-lsp-index
    stubs.rs                 # stub path discovery/validation and reload orchestration
    vendor.rs                # vendor autoload cache and lazy vendor LRU helpers
  util/
    uri.rs                   # shared URI/path helpers
    lsp_text.rs              # LSP UTF-16 range/position to byte-offset helpers
```

`server.rs` still contains shared inference, diagnostic, framework, and
cross-feature helpers that are used by multiple LSP modules. When adding new
request handling code, keep the `LanguageServer` trait method in `server.rs`
as delegation and put the request body in `src/lsp/<feature>.rs`.

## Indexing Module Boundaries

Some server modules intentionally share names with `php-lsp-index` modules. The
server crate owns runtime orchestration; the index crate owns storage and symbol
loading primitives.

| Path | Responsibility | Not Responsible For |
|---|---|---|
| `php-lsp-server/src/indexing/cache.rs` | Builds `IndexCacheConfig` values for workspace, stubs, and vendor caches from current server settings; hashes configured stub and vendor source metadata. | Cache file schema, `bincode` serialization, atomic save/load, or per-file freshness validation. |
| `php-lsp-index/src/cache.rs` | Defines the cache schema version and serialized snapshot model; validates metadata; loads and saves namespace-scoped `index.bin` files. | Reading live LSP configuration, discovering stub paths, or deciding when the server should reindex. |
| `php-lsp-server/src/indexing/stubs.rs` | Finds candidate phpstorm-stubs directories, rejects unusable paths, clears/reloads configured stub symbols, and collects stub source files for cache hashes. | Parsing stub PHP files or extracting built-in symbols. |
| `php-lsp-index/src/stubs.rs` | Reads verified phpstorm-stubs files, extracts built-in symbols, and inserts them into `WorkspaceIndex`. | Choosing configured extension lists or fallback stub locations. |

Use the server-side modules when the code needs `PhpLspBackend` state,
configuration, logging, or LSP lifecycle behavior. Use the index-crate modules
when the code needs pure index data structures, cache persistence, Composer
metadata, or parser-backed symbol loading.

## Generated And Auxiliary Paths

Generated client and build outputs are local artifacts and should not be
tracked:

- `client/node_modules/`
- `client/out/`
- `client/bin/`
- `client/stubs/`
- `client/*.vsix`
- `target/` and `server/target/`

These paths are covered by the root `.gitignore`; `npm ci`,
`npm run build`, `scripts/build-server.sh`, and `scripts/bundle-stubs.sh`
recreate them as needed. `server/data/stubs/` is different: it is the
intentional phpstorm-stubs git submodule used as the source for bundled PHP
symbols.

## E2E Test Layout

The protocol tests are split by feature area. Shared JSON-RPC request builders
and response helpers live in `tests/support/mod.rs`.

| Test target | Covers |
|---|---|
| `tests/e2e_initialize.rs` | initialize/shutdown, runtime configuration, project config trust. |
| `tests/e2e_completion.rs` | completion, completion resolve, signature help, shape completion. |
| `tests/e2e_hover.rs` | hover, inlay hints, local variable type inference, callback inference. |
| `tests/e2e_definition.rs` | definition, declaration, type definition, implementation. |
| `tests/e2e_references.rs` | document highlight, references, rename, code lens, cancellation. |
| `tests/e2e_code_actions.rs` | quick fixes, organize imports, generate members, refactors, PHPDoc sync. |
| `tests/e2e_diagnostics.rs` | diagnostics debounce/staleness, PHP version gates, vendor metadata refresh. |
| `tests/e2e_formatting.rs` | document/range/on-type formatting. |
| `tests/e2e_symbols.rs` | semantic tokens, document/workspace symbols, selection range, folding, document links. |
| `tests/e2e_hierarchy.rs` | call hierarchy and type hierarchy. |
| `tests/e2e_indexing.rs` | watched files, file operations, workspace folders, index-related inference. |
| `tests/e2e_templates.rs` | Blade/Twig virtual PHP behavior. |

Run all split protocol tests with:

```bash
cd server && cargo test -p php-lsp-server --tests
```

Run a focused target with:

```bash
cd server && cargo test -p php-lsp-server --test e2e_completion
```

## Shared Invariants

These invariants are intentionally called out because many LSP features cross
crate boundaries.

### Position Model

Tree-sitter and parser data use byte columns. LSP uses UTF-16 columns.

| Data | Position unit |
|---|---|
| `SymbolInfo.range` | Tree-sitter byte columns. |
| `SymbolInfo.selection_range` | Tree-sitter byte columns. |
| `UseStatement.range` | Tree-sitter byte columns. |
| Parser semantic diagnostic ranges | Tree-sitter byte columns unless converted at the server boundary. |
| `SymbolReference.range` | LSP UTF-16 columns. |
| LSP request and response ranges | UTF-16 columns. |

Outbound LSP handlers must convert byte-backed ranges with
`php_lsp_parser::utf16::range_byte_to_utf16` or `Utf16LineIndex`. Do not return
`SymbolInfo.range` or `selection_range` directly as an LSP `Range`.
Completion context detection receives byte columns after the server converts
LSP UTF-16 positions and clamps them to valid UTF-8 boundaries before slicing,
so non-ASCII text and CRLF line endings do not change the internal unit model.

### URI Model

File URIs are an LSP/client boundary format, not an internal path format. New
code should not build URIs with raw string formatting such as
`format!("file://{}", path.display())`. URI conversion should go through a
shared helper (`php_lsp_types::uri`) that percent-encodes paths, decodes client
URIs, and handles platform-specific path forms.

### Symbol Model

Top-level classes, interfaces, traits, enums, functions, and constants are
indexed in dedicated `WorkspaceIndex` maps. Members remain part of
`FileSymbols.symbols` and are resolved through the owning type. Property
`SymbolInfo.name` is stored without `$`, while property FQNs include `$` as in
`Class::$prop`.

`SymbolReference` entries are precomputed occurrences used by references,
rename, and code lenses. Unresolved member references such as `::method` and
`::$prop` may be useful for non-destructive discovery, but they are not precise
enough for workspace rename edits without a resolved receiver type.

## Data Flow

```text
VS Code
  |
  | LSP over stdio
  v
php-lsp server
  |
  | open/change/save
  v
FileParser (tree-sitter PHP) -----> diagnostics / semantic tokens / local queries
  |
  | symbols + references
  v
WorkspaceIndex
  |
  | lookup / lazy resolution
  v
Composer maps + stubs + vendor cache
  |
  | persisted snapshots
  v
Disk cache: workspace / stubs / vendor
```

Disk cache snapshots are schema- and namespace-scoped. Each cached file is
validated by encoded URI, mtime, size, and content hash before its symbols and
references are restored. Cache saves write a unique temporary file and replace
the existing snapshot so repeated saves to the same path also work on Windows.
Lazy vendor indexing persists a parsed vendor file only after the requested
class is actually present in the index, and later sessions can reload that file
from the `vendor` cache namespace.

## Feature Ownership Map

| Feature area | LSP/server entry point | Parser/completion layer | Index/cache layer | Primary tests |
|---|---|---|---|---|
| Hover | `src/lsp/hover.rs` | `resolve.rs`, PHPDoc helpers | `workspace.rs` symbol lookup | `tests/e2e_hover.rs` |
| Definition/declaration/type definition | `src/lsp/definition.rs` | `resolve.rs` | `workspace.rs`, lazy vendor lookup | `tests/e2e_definition.rs` |
| Completion | `src/lsp/completion.rs` | `php-lsp-completion/src/context.rs`, `provider.rs` | `workspace.rs` members/symbols/stubs | completion unit tests + `tests/e2e_completion.rs` |
| Signature help | `src/lsp/completion.rs` | call/member resolution helpers | `workspace.rs` signature lookup | `tests/e2e_completion.rs` |
| References/code lens | `src/lsp/references.rs` | `references.rs` | `file_references` in `WorkspaceIndex` | `tests/e2e_references.rs` |
| Rename | `src/lsp/rename.rs` | `references.rs`, local variable search | `file_references`, symbol lookup | `tests/e2e_references.rs` |
| Diagnostics | `src/lsp/diagnostics.rs` plus shared helpers in `server.rs` | `diagnostics.rs`, `semantic.rs` | `workspace.rs` symbol resolution | parser unit tests + `tests/e2e_diagnostics.rs` |
| Code actions/refactors | `src/lsp/code_action.rs` | parser helpers, return type helpers | symbol/member lookup | server unit tests + `tests/e2e_code_actions.rs` |
| Inlay hints | `src/lsp/inlay_hints.rs` plus shared inference helpers in `server.rs` | type inference and local variable scans | indexed signatures/types | `tests/e2e_hover.rs` |
| Templates | `template.rs` plus template-aware LSP handlers | virtual PHP/source maps | Twig context scans | `tests/e2e_templates.rs` |
| Stubs/vendor/cache | `src/indexing/*` runtime orchestration and lazy index paths | symbol extraction | `php-lsp-index::{stubs,composer,cache}` storage/loading primitives | index unit tests + `tests/e2e_indexing.rs` |

## Completion Context

The LSP completion path calls `provide_completions_at_range(...)` with the
cursor byte-column range. The completion provider uses that range to find the
class-like symbol containing the cursor before filtering member visibility for
`$this`, `self`, `static`, and `parent`. This keeps private and protected
members tied to the actual class, trait, enum, or anonymous class at the cursor
instead of the first class-like symbol in file order.

Member-access completion contexts also carry a read/write mode inferred from
the text after the cursor. PHPDoc virtual properties use that mode to honor
`@property-read` and `@property-write`: read contexts hide write-only virtual
properties, and assignment contexts hide read-only virtual properties.

When ranges are nested, the innermost containing class-like symbol wins. This is
important for anonymous classes declared inside methods or other class bodies:
completion inside the anonymous class must not leak private members from the
outer class. The older `provide_completions(...)` helper remains available for
non-position-aware callers and tests, but server-side LSP requests should use
the positional API.

Array-shape key completion can trigger either after `[` or inside quoted string
keys. Completion after `[` inserts a quoted key, while completion inside an
existing quote inserts only the key text. Parser PHPDoc shapes, completion
items, and shape-key definition lookups all normalize quoted/optional shape keys
through `php_lsp_types::normalize_shape_key_text(...)` so lookup and display do
not diverge.

PHPDoc tag parsing treats tag names as exact tokens: analyzer-specific tags such
as `@param-out`, malformed `@returnFoo`, and vendor-specific `@var-*` tags do
not fall through to the base `@param`, `@return`, or `@var` parser. PHPStan and
Psalm type aliases may span multiple PHPDoc lines for common `array{...}` /
`object{...}` shapes; file-level aliases are expanded for local `@var` shape
hover and completion best-effort, while indexed signatures continue to expand
aliases through `WorkspaceIndex`.
PHPDoc literal parsing accepts the supported scalar subset deliberately:
quoted strings, booleans, null, decimal/binary/octal/hex integers with numeric
separators, and decimal or scientific floats. Unsupported or malformed numeric
forms remain plain type names rather than guessed literals.

## Startup Flow

1. VS Code activates on PHP files or a workspace containing `composer.json`.
2. The client resolves the server binary:
   - `phpLsp.serverPath` if configured.
   - Otherwise `client/bin/<platform>/php-lsp` or `php-lsp.exe`.
   - If the bundled binary is missing and no custom path is configured,
     `php-lsp` from `PATH`.
3. The client sends `initialize` with explicit `phpLsp.*` settings plus the
   bundled stubs fallback path. VS Code default values are not sent as
   overrides, so `.php-lsp.toml` can define shared project defaults.
4. The server loads effective configuration in this order: built-in defaults,
   global config, project `.php-lsp.toml`, then explicit client settings.
   Executable analyzer and formatter settings from project config are ignored
   unless command trust is enabled from VS Code or global config.
5. The server stores the settings and advertises capabilities.
6. After `initialized`, the server:
   - Discovers effective workspace roots, including Composer roots.
   - Loads configured phpstorm-stubs, after rejecting missing or incomplete
     candidate stubs paths.
   - Starts background workspace indexing.
   - Preloads Composer `autoload.files` entrypoints when lazy vendor indexing is
     enabled.
   - Republishes diagnostics for open files after indexing finishes.

The server sends `phpLsp/indexingStatus` notifications during this flow. The
client uses those notifications for the status bar popup and progress display.

## Workspace Roots

The server accepts multi-root workspaces. Each VS Code workspace folder is mapped
to an effective root:

- If Composer support is enabled, `composer.json` discovery can narrow indexing
  to the Composer project root.
- Composer `autoload` and `autoload-dev` entries are parsed for PSR-4, PSR-0,
  classmap, and files entries.
- PSR-0 candidate paths map namespace separators to directories and treat
  underscores as path separators only in the unqualified class-name segment;
  underscores inside namespace segments are preserved.
- `phpLsp.includePaths` adds explicit directories or files.
- `phpLsp.excludePaths` removes relative or absolute paths from indexing and
  lazy vendor work.

Workspace folder changes update the root list and remove symbols for removed
roots. Configuration changes that affect indexing trigger a workspace reindex.

## Open File Model

Open documents are stored in `open_files` as `FileParser` instances. The server
also tracks the latest LSP document version per URI.

On `textDocument/didOpen`:

- The file is parsed from the editor text.
- Symbols and non-local references are extracted.
- The global index is updated immediately unless the path is excluded.
- Full diagnostics are published.

On `textDocument/didChange`:

- Incremental LSP edits are applied to the existing parser.
- Document versions are checked so older changes are ignored.
- Symbols and references are refreshed in the index.
- Fast diagnostics are debounced and published only for the latest known version.
- Any running external analyzer for that document is cancelled.

On `textDocument/didSave`:

- Pending fast diagnostics are cancelled.
- Full diagnostics, including enabled external analyzers, are published.

On `textDocument/didClose`:

- Parser state, version state, semantic-token cache, pending diagnostics, and
  analyzer runs for the URI are cleared.
- Diagnostics are cleared in the client.

## Template Documents

Blade-like and Twig documents are kept out of the normal PHP workspace index.
When an open document is recognized as a template, the server stores a
`TemplateDocument` next to the virtual `FileParser`:

- The original template source remains the LSP document source of truth.
- A conservative preprocessor emits virtual PHP only for supported expression
  and control-block ranges.
- A source map converts template positions to virtual PHP positions for hover,
  completion, definition, inlay hints, diagnostics, and semantic tokens, then
  maps returned ranges back to the template.
- Template diagnostics run through a conservative allowlist after virtual PHP
  analysis. Only exact source-mapped expression diagnostics such as unknown
  methods/class constants, unknown classes, and type compatibility errors are
  published. Twig undefined-variable diagnostics are suppressed because valid
  templates frequently receive variables through includes, components, email
  contexts, and extensions the server cannot statically see. Twig
  delimiter/block syntax diagnostics are produced from the original template
  source. Generated virtual PHP ranges, template functions, incomplete/magic
  properties, and any partially mapped ranges are suppressed instead of
  reporting whole-template errors.

Blade support covers escaped/raw echo blocks and common `@if`, `@foreach`,
`@isset`, and `@empty` directives. Twig support is a separate language path for
`.twig` / `.html.twig` files and covers `{{ expr }}`, `{% if %}`,
`{% for item in items %}`, `{% set name = expr %}`, common structural tags, and
static include/extends/embed path lookup.

Twig expression conversion is intentionally conservative. Simple variable,
literal, operator, dot-member, and object method-call expressions can be mapped
to virtual PHP. Filters, tests, `in`, functions, macros imported with
`import`/`from` or `_self`, ternaries, null coalescing, ranges, and
dynamic/bracket attribute access are classified as unsupported expression
backlog. Those expressions emit valid unmapped placeholders (`null`, `true`, or
an empty array iterable) so parser state remains usable while diagnostics,
hover, completion, and inlay requests avoid pretending the Twig expression is
ordinary PHP. For editor navigation inside those unsupported envelopes, simple
member chains such as `item.owner.id` and root variables such as
`messageLogs` in `messageLogs is defined` / `messageLogs|length` are
additionally emitted as standalone no-op virtual PHP fragments. Only those
variable/member tokens are source-mapped; the surrounding Twig function, filter,
test, or operator tokens stay unmapped. Unfinished chains such as `item.` are
mapped as well so member completion can run while the expression is still being
typed. Type-preserving fallbacks such as `item.items|slice(...)` map the base
expression once and avoid duplicate no-op fragments for that same chain.
Twig member completion post-processes getter-like methods into property-style
aliases (`getId()` -> `id`, `isActive()` -> `active`) only for Twig documents.
If a Twig property-style alias has no backing PHP property, hover and
definition can fall back to the zero-required-argument getter method that
created the alias. Twig `foreach` over Doctrine entity collections exposed
through property-style access can infer item hover, completion, definition, and
inlay types from indexed ORM `targetEntity` property metadata and collection
mutator signatures such as `addItem(Item $item)` / `removeItem(Item $item)`.
The mutator fallback also covers getter names that do not exactly match the
backing collection property, for example `getStatusHistory()` returning
`$statusHistories`.
Twig attribute access also treats PHPDoc and inferred `array{...}` shapes as
property-style records inside Twig documents. This lets `row.npId`, nested
chains such as `config_params.encryption.temp_dir_path`, and `{% set
message_log = row.messageLog %}` feed hover, completion, definition, and inlay
hints from shape keys without changing normal PHP `->` semantics. The inferred
Twig context stores source-backed shape-key definition metadata when keys come
from indexed PHPDoc, literal render arrays, append-built arrays, or `compact`
locals, so `textDocument/definition` can jump to the PHP source key instead of
falling back to the current Twig member token.
When a mapped `foreach` iterates a known but non-parameterized `array` or
`iterable`, the value variable can still expose `mixed` in hover; inlay hints
continue to suppress `mixed` labels to avoid noise.

Twig context variables are inferred statically from simple PHP
`render('template.html.twig', ['name' => expr])` call sites. Supported context
expressions include `new Class()`, simple arrays of new objects, typed
controller parameter variables passed through to the render context, nullable
locals assigned conditionally before render, and indexed
`$this->service->method()` return types. Literal associative arrays are rendered
as nested array shapes, `$items[] = [...]` append patterns become
`array<int, array{...}>`, and `compact('name')` render contexts look up the
latest local assignment or parameter type for each compacted variable.
Repository method results with iterable PHPDoc/native return types can seed
collection context variables; Doctrine magic `find*` and `findOneBy*`
repository results can seed entity or nullable entity context variables. Short
PHPDoc class names are resolved against the file where the indexed method is
declared before they become Twig foreach item types.
Knp-style pagination variables can also expose Doctrine repository/query-builder
item types, so `{% for item in pagination %}` can inherit the entity type
without booting Symfony. Custom Doctrine repositories are resolved from indexed
`@extends ServiceEntityRepository<Entity>` PHPDoc and indexed ORM
`repositoryClass` attributes; request handlers avoid synchronous source reads
for this lookup. A bounded Twig include scan also handles one-level
`{% include 'partial.html.twig' with {'items': items} %}` calls by evaluating
the caller template's static render context and copying simple `with` object
values into the included template. Render keys whose value type cannot be inferred still
seed `mixed` variables in the virtual prelude so valid templates do not publish
false undefined-variable diagnostics just because the server cannot infer a
richer type. The context scanner combines open PHP/Twig files from memory with a
bounded, disk-backed cache for closed PHP files. Cache misses run through
Tokio's blocking pool and file watcher/save events clear the cache. Open PHP
buffers are authoritative over cached disk scan results; opening or editing a
PHP source evicts disk-cache
entries that were derived from that source URI, so a later close falls back to a
refreshed disk snapshot instead of stale render context. Open Twig documents are
bounded-refresh candidates after PHP controller/render edits, open Twig caller
edits, and workspace reindex completion: their context
prelude, virtual PHP parser, diagnostics, and request-time hover/completion/inlay
state are rebuilt from current open buffers plus the disk cache. The scanner
does not boot Symfony, evaluate Twig extensions, run user code, or read the
service container.

## Symbol Index

`WorkspaceIndex` is a concurrent in-memory index:

| Map | Key | Contents |
|---|---|---|
| `types` | FQN | Classes, interfaces, traits, enums. |
| `functions` | FQN | Global functions. |
| `constants` | FQN | Global constants. |
| `file_symbols` | URI | Full symbol list for a file, including members and namespaces. |
| `file_references` | URI | Precomputed non-local references found during parsing. |

Top-level symbols are stored in dedicated maps for direct lookup. Members are
stored in `file_symbols` and resolved through parent type lookup. Reference
queries, rename, and reference-count code lenses use `file_references` for
non-local symbols. They avoid reparsing closed files in the common path, but can
still iterate many indexed reference sets for workspace-wide operations.

## Disk Cache Model

Index snapshots are serialized with `bincode` under:

```text
<cache-base>/php-lsp/<workspace-hash>/<namespace>/index.bin
```

Cache base is:

- `$XDG_CACHE_HOME` when set.
- `$HOME/.cache` when available.
- The OS temp directory as fallback.

Namespaces are separate:

| Namespace | Contents |
|---|---|
| `workspace` | Workspace PHP file symbols and references. |
| `stubs` | Configured phpstorm-stubs symbols. |
| `vendor` | Lazy-indexed vendor file symbols. |

Each cache file stores schema version, namespace, php-lsp version, workspace
root, config hash, stubs/vendor hash, file metadata, file symbols, references,
and top-level symbol snapshots.

Because the cache uses `bincode`, the snapshot format is not self-describing.
Any change to `IndexCache`, nested cached structs, or serialized
`php-lsp-types` fields must bump `CACHE_SCHEMA_VERSION` in
`php-lsp-index/src/cache.rs`. The index crate keeps a representative serialized
fixture hash/length test so CI catches schema-shape changes before old
`index.bin` files are accidentally treated as current.

Cache invalidation checks:

- Cache schema version.
- Cache namespace.
- php-lsp package version.
- PHP version.
- Include and exclude path settings.
- Stub extension set.
- Stub metadata hash or vendor metadata hash.
- Per-file size and mtime.
- Missing or extra files relative to the current source set.

If a filesystem cannot provide a usable modification time, the cache metadata
records that state explicitly and relies on the content hash and file size to
avoid accepting stale file snapshots.

Writes are atomic: the server writes a unique temporary file and renames it to
`index.bin`.

## Indexing Pipeline

Workspace indexing runs in the background after initialization and after relevant
configuration changes. A new indexing run cancels the previous one.

Flow:

1. Send `discovering` indexing status.
2. Resolve source directories from Composer maps, include paths, and workspace
   root.
3. Collect PHP files on Tokio's blocking pool while honoring exclude paths.
4. Load valid cached files into `WorkspaceIndex`.
5. Parse changed or missing files through a bounded `spawn_blocking` queue.
6. Update the global index as parse tasks finish.
7. Save a new workspace cache.
8. Send `ready` status with counts, elapsed time, cache stats, parse
   concurrency, and cache path.

Parse concurrency is CPU-aware and capped to avoid unbounded memory growth.

## Stubs And Vendor

Stubs:

- The client passes `stubsPath` for the bundled stubs directory.
- The server loads configured extension directories from phpstorm-stubs. If the
  extension list is explicitly empty, stubs are treated as disabled by config.
- Missing, non-directory, or uninitialized stubs paths are skipped and logged
  separately from intentional stubs disablement.
- Stub symbols are marked as built-in.
- Stub cache is keyed by PHP version, extension list, php-lsp version, and stub
  metadata.
- Changing PHP version or stub extension configuration reloads stubs and
  republishes open-file diagnostics.
- Development, CI, and release packaging validate source and bundled stubs with
  `scripts/check-stubs.sh`; VSIX smoke also checks required core stubs and a
  minimum PHP stub-file count.

Vendor:

- Composer metadata is parsed from `composer.json`, `composer.lock`, and
  Composer generated files where available.
- Vendor classes are lazy-indexed when resolution needs them.
- Lazy vendor symbols are bounded by an in-memory LRU.
- Evicted vendor symbols can be restored from the `vendor` disk cache.
- Composer `autoload.files` entrypoints are preloaded after workspace indexing
  when `phpLsp.indexVendor` is enabled.

## Diagnostics Pipeline

Diagnostics are controlled by `phpLsp.diagnostics.mode`:

| Mode | Behavior |
|---|---|
| `off` | No php-lsp diagnostics. |
| `syntax-only` | Tree-sitter syntax diagnostics, plus conservative Twig syntax diagnostics for Twig documents. |
| `basic-semantic` | Syntax plus built-in semantic diagnostics. |

Built-in semantic diagnostics include unknown symbols, unused imports/variables,
duplicate symbols, member access problems, type compatibility, override
signatures, and PHP-version checks. Per-category severity is controlled by
`phpLsp.diagnostics.severity`.

Member access and type-compatibility diagnostics share a latency budget because
they can resolve many call sites and inferred types in large files. The default
`phpLsp.diagnostics.memberTypeNodeBudget` is `512` relevant syntax nodes. When a
file exceeds that budget, php-lsp skips those two expensive categories, logs the
partial analysis, and by default publishes an informational `partial-analysis`
diagnostic at the start of the file. Set the budget higher for large files or to
`0` to disable the cap; set `phpLsp.diagnostics.partialAnalysisDiagnostic` to
`false` to keep the log but hide the informational diagnostic.

Duplicate-symbol diagnostics are split by scope: parser semantic diagnostics
report duplicate declarations inside the current file, while workspace
diagnostics report only cross-file duplicates from the index. This avoids
publishing the same declaration pair twice when the current file is already in
the workspace index.

Unknown function diagnostics use PHP's namespace fallback order. Explicitly
qualified calls and `use function` aliases must resolve to the indexed symbol
they name. Unqualified calls are checked against the current namespace first and
then the global function table, which is where bundled PHP stubs expose built-in
functions. A diagnostic is emitted only when those fallbacks do not resolve.

Type compatibility diagnostics are intentionally approximate. They compare
known literal/scalar values, class names, arrays, array/object shapes,
class-string/callable types, and unions where enough local information is
available; intersections and relative `self`/`static`/`parent` types are treated
optimistically to avoid false positives without whole-program type flow.
Override diagnostics apply incremental PHP variance rules: parameters may widen
class types, returns may narrow class types, and PHPDoc/native refinements are
handled conservatively rather than as a full PHPStan/Psalm type lattice.

There are two publishing paths:

- Fast diagnostics after `didChange`: debounced, in-process, version-checked,
  computed on Tokio's blocking pool, and intended for editor feedback.
- Full diagnostics after open/save/reconfiguration: in-process diagnostics plus
  enabled PHPStan/Psalm external analyzer output.

In-process diagnostic parsing and semantic checks are queued through
`spawn_blocking` before publication, so expensive diagnostics do not occupy an
async executor worker. The pipeline records tracing spans for queue wait,
compute, and publish phases, then checks the document version before sending the
result to the client.

External analyzer runs are per document. A newer document event cancels the
previous analyzer run for that URI. Analyzer commands are timeout-bound and
expected to print JSON.

## Request Paths

Low-latency requests such as hover, completion, signature help, definition, and
semantic tokens operate primarily on the open file parser and the global index.
Request-time filesystem work is kept bounded and off the async executor:

- Composer/config discovery used by LSP initialization/reindex runs on the
  blocking pool.
- Framework string-key completion/definition uses a bounded per-workspace cache;
  cache misses run static project scans on the blocking pool.
- Twig render-context inference uses a bounded disk-scan cache, always overlays
  open PHP files from memory, and refreshes a bounded set of open Twig documents
  after relevant PHP/reindex events. PHP open/change events evict only Twig
  disk-cache entries that include the changed source URI; save, watcher, and
  configuration events still clear the whole request FS cache.
- Formatter auto-detection and formatter temporary-file reads/writes run through
  blocking helpers around the external async command.
- Inlay hint inference, including Doctrine source-inspection fallbacks, runs on
  the blocking pool.

Heavier requests include:

- References.
- Rename.
- Reference-count code lenses.
- Incoming call hierarchy.
- Some file-operation refreshes.

These paths can iterate indexed files and some hierarchy/lens paths may read
unopened files from disk through blocking/background IO. The current production
target is to keep common open-file requests responsive while heavier operations
are measured on large projects.

The latest acceptance refresh was captured on 2026-05-28 after the IDE
intelligence milestone. On the primary 10k-file Symfony workspace, warm
open-file p95 for hover/completion/definition stayed under 7 ms, while heavy
`references` and rename dry-run requests kept unrelated hover/completion below
10 ms p95.

## Public Entry Points

`PhpLspBackend` implements `LanguageServer` in
`server/crates/php-lsp-server/src/server.rs`, but those trait methods delegate
to focused modules under `src/lsp/` and `src/indexing/`. The most important
entry points are:

| Area | Entry points |
|---|---|
| Lifecycle | `src/lsp/lifecycle.rs`, `src/indexing/workspace.rs`. |
| Document sync | `src/lsp/diagnostics.rs`. |
| Workspace sync | `src/indexing/workspace.rs`. |
| Navigation | `src/lsp/hover.rs`, `src/lsp/definition.rs`. |
| Symbols and hierarchy | `src/lsp/document_symbols.rs`, `src/lsp/hierarchy.rs`. |
| Editing | `src/lsp/rename.rs`, `src/lsp/code_action.rs`, `src/lsp/formatting.rs`. |
| Intelligence | `src/lsp/completion.rs`, `src/lsp/inlay_hints.rs`, `src/lsp/semantic_tokens.rs`, `src/lsp/folding.rs`. |

Non-LSP command-line entry points are `analyze::run_analyze_cli` and
`fix::run_fix_cli`.

## Configuration Updates

`workspace/didChangeConfiguration` and watched `.php-lsp.toml` changes are
applied at runtime:

| Changed setting group | Server action |
|---|---|
| PHP version | Reload stubs and republish diagnostics. |
| Diagnostics mode/severity/budget | Republish open diagnostics. |
| Composer enabled | Recompute workspace roots and reindex. |
| Include/exclude paths | Reindex. |
| Stub extensions/path | Reload stubs and republish diagnostics. |
| `indexVendor` disabled | Clear vendor metadata, LRU entries, and indexed vendor symbols. |
| Formatter/analyzer/log settings | Update runtime config for future requests. |

## Cache Clearing

The VS Code command `PHP: Clear PHP LSP Cache and Restart` deletes the disk cache
directories for current workspace roots and discovered Composer roots, then
restarts the language server. The older restart/reindex path refreshes symbols
but does not delete disk cache files by itself.

Client lifecycle operations are serialized so restart, cache clearing, enable,
disable, and activation paths cannot start overlapping server processes. Stop
uses the language-client timeout path, which terminates the managed child
process when the server does not exit cleanly. The LSP output channel records
the lifecycle reason, selected binary source, binary path, platform target, stop
reason, and cache directories removed by the cache-clearing command.
