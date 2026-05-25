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
5. The server stores the settings and advertises capabilities.
6. After `initialized`, the server:
   - Discovers effective workspace roots, including Composer roots.
   - Loads configured phpstorm-stubs.
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

Writes are atomic: the server writes a unique temporary file and renames it to
`index.bin`.

## Indexing Pipeline

Workspace indexing runs in the background after initialization and after relevant
configuration changes. A new indexing run cancels the previous one.

Flow:

1. Send `discovering` indexing status.
2. Resolve source directories from Composer maps, include paths, and workspace
   root.
3. Collect PHP files while honoring exclude paths.
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
- The server loads configured extension directories from phpstorm-stubs.
- Stub symbols are marked as built-in.
- Stub cache is keyed by PHP version, extension list, php-lsp version, and stub
  metadata.
- Changing PHP version or stub extension configuration reloads stubs and
  republishes open-file diagnostics.

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
| `syntax-only` | Tree-sitter syntax diagnostics only. |
| `basic-semantic` | Syntax plus built-in semantic diagnostics. |

Built-in semantic diagnostics include unknown symbols, unused imports/variables,
duplicate symbols, member access problems, type compatibility, override
signatures, and PHP-version checks. Per-category severity is controlled by
`phpLsp.diagnostics.severity`.

There are two publishing paths:

- Fast diagnostics after `didChange`: debounced, in-process, version-checked,
  and intended for editor feedback.
- Full diagnostics after open/save/reconfiguration: in-process diagnostics plus
  enabled PHPStan/Psalm external analyzer output.

External analyzer runs are per document. A newer document event cancels the
previous analyzer run for that URI. Analyzer commands are timeout-bound and
expected to print JSON.

## Request Paths

Low-latency requests such as hover, completion, signature help, definition, and
semantic tokens operate primarily on the open file parser and the global index.

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

## Configuration Updates

`workspace/didChangeConfiguration` and watched `.php-lsp.toml` changes are
applied at runtime:

| Changed setting group | Server action |
|---|---|
| PHP version | Reload stubs and republish diagnostics. |
| Diagnostics mode/severity | Republish open diagnostics. |
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
