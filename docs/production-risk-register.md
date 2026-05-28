# Production Risk Register

Date: 2026-05-22
Last updated: 2026-05-25
Scope: production-readiness milestone, weeks 1-6.

This document tracks known production gaps after the baseline/profiling setup.
The format is intentionally operational: every risk is tied to an owner task in
`TASKS.md` so it can be closed by a measurable change.

## Summary

| ID | Area | Severity | Owner task | Status |
|----|------|----------|------------|--------|
| R-001 | Disk cache maturity | High | `PR-010`, `PR-011`, `PV-002` | Mitigated |
| R-002 | `references`/`rename`/`codeLens` scale | High | `PR-022`, `PR-021`, `PV-003`, `PV-004`, `PV-011` | Accepted limitation |
| R-003 | Parallel indexing acceptance | High | `PR-013`, `PR-023`, `PV-002` | Mitigated |
| R-004 | Sync file IO in async/hot paths | High | `PR-023`, `PV-003`, `PV-004` | Mitigated |
| R-005 | Request cancellation coverage for heavy operations | High | `PR-021`, `PR-050`, `PV-004` | Mitigated |
| R-006 | `didChange` debounce/version ordering | High | `PR-020`, `PR-050` | Mitigated |
| R-007 | Version-aware stubs | Medium | `PR-030`, `PR-011` | Mitigated |
| R-008 | Lazy vendor indexing scale validation | Medium | `PR-012`, `PR-011`, `PV-014` | Partially mitigated |
| R-009 | PHPDoc/type model depth for production PHP | Medium | `PR-031`, `PR-032`, `PR-040`, `PR-041`, `IE-030`, `IE-031`, `PV-012` | Accepted limitation |
| R-010 | LSP polish/capability mismatch risk | Medium | `PR-043`, `PR-051`, `PR-052` | Mitigated |

## Risks

### R-001: Disk cache maturity

Current evidence:

- `PR-010` added a schema-versioned workspace disk cache for file symbols/top-level snapshots.
- `PR-011` split cache files into `workspace`, `stubs`, and `vendor` namespace directories.
- Cache path: `~/.cache/php-lsp/{workspace-hash}/{namespace}/index.bin`.
- Cache invalidates by file mtime/size, php-lsp version, PHP version, include/exclude paths, stub extension set and stubs hash.
- Fixture smoke run shows cached workspace file symbols loading on second start.
- `PV-002` large workspace run on `large-symfony` loaded 10575 workspace files
  from disk cache on warm start; ready time improved from 7349.48 ms cold to
  3423.19 ms warm, meeting the `<5s` large-workspace warm-start target.
- `PV-002` also showed stubs cache load dropping from 313.73 ms cold to 33.79
  ms warm.

Impact:

- Repeated startup on the primary 5k-10k PHP-file workspace meets the
  production target of `<5s` to a ready index from disk cache.
- Changed-file invalidation remains covered by tests and normal dogfood watch,
  but is no longer considered a blocking production risk.
- Vendor composer metadata cache/LRU is tracked separately by `R-008`.

Mitigation:

- `PR-010`: implemented workspace index disk cache with mtime/size/config/stubs hash invalidation.
- `PR-011`: implemented separate cache namespaces for workspace/stubs/vendor and preserved stub/vendor symbols across workspace reindex.

Exit signal:

- `PV-002` warm `large-symfony` run reaches `phase=ready` in `3423.19 ms`.
- Cache invalidates changed files without full rebuild; keep this covered by
  cache tests and reindex dogfood.

### R-002: `references`/`rename`/`codeLens` scale

Current evidence:

- `PR-022` added `WorkspaceIndex::file_references` and `SymbolReference`.
- Workspace indexing, lazy vendor/stub indexing, `didOpen`, `didChange`, and watched-file reindex now collect per-file references.
- `references`, `rename`, and `codeLens` use indexed references; closed files are not reparsed for the common path.
- These requests still iterate indexed file reference sets and can remain O(indexed files) on very large workspaces.
- `PV-003` latency benchmark on `large-symfony` shows warm open-file
  `references` p95/p99 at 72.527 ms / 74.123 ms and rename dry-run p95/p99 at
  73.529 ms / 73.619 ms.
- In the same run, common warm open-file requests stayed below target:
  hover p95 3.562 ms, completion p95 6.556 ms, definition p95 2.855 ms.
- `PV-004` heavy-responsiveness benchmark on `large-symfony` shows
  hover/completion p95 staying under 6.4 ms while `references` or rename dry-run
  is outstanding.
- `PV-004` normal heavy request p95: `references` 76.329 ms, rename dry-run
  82.806 ms.

Impact:

- Latency no longer includes full workspace reparse, but can still grow with indexed workspace size.
- `references` and rename dry-run meet the current acceptance bar on the primary
  large workspace, including responsiveness while heavy requests are outstanding.
- `codeLens` reference counts may still be expensive on very large files/workspaces.
- `references` and rename dry-run no longer look like a responsiveness blocker
  on the primary large workspace; `codeLens` still needs separate dogfood watch.

Mitigation:

- `PR-022`: implemented reference/occurrence index and cache roundtrip for references.
- `PR-021`: added cooperative yield points so `$/cancelRequest` can cancel long references/rename requests.
- `PR-003`: keep the latency benchmark as a regression gate.

Exit signal:

- `PV-003`/`PV-004` measured warm `references`/rename dry-run and concurrent
  hover/completion responsiveness on `large-symfony`.
- `PV-011`/`PV-014` dogfood should keep `codeLens` on the watch list; this is
  an accepted limitation rather than a GA blocker unless dogfood finds visible UI stalls.

### R-003: Parallel indexing acceptance

Current evidence:

- `PR-013` replaced the sequential indexing loop with a bounded `JoinSet::spawn_blocking` task queue.
- Parse concurrency uses a CPU-aware default capped at 8 workers.
- `WorkspaceIndex::update_file()` is still centralized after each parse task completes, and a regression test covers concurrent index updates.
- `PV-002` primary large workspace cold run indexed 10575 files / 72683 symbols
  with `filesPerSec=1503.84`, `symbolsPerSec=10336.04`, and peak RSS
  730,419,200 bytes.
- `PV-002` warm cache run loaded the same workspace with peak RSS 625,729,536
  bytes and ready time 3423.19 ms.

Impact:

- Large-project indexing and memory profile have an acceptance measurement on
  `large-symfony` and supporting measurements on Laravel-like projects.
- Other sync file IO hot paths are still tracked by `R-004`/`PR-023`.

Mitigation:

- `PR-013`: implemented bounded parallel parse queue with stable progress/error aggregation.
- `PR-023`: moved remaining known blocking file IO/parse work out of hot async
  request paths or into explicit blocking contexts.

Exit signal:

- `PV-002` measured `1503.84` files/sec and `10336.04` symbols/sec on
  `large-symfony` cold indexing with bounded parallel parsing.
- Progress reporting remains correct enough for the acceptance/profile harness.

### R-004: Sync file IO in async/hot paths

Current evidence:

- `PR-023` added `run_file_io_blocking()` with `spawn_blocking`, a 15s timeout, and warning telemetry for file reads slower than 100ms.
- Watched-file reindex, lazy PHP/vendor indexing, vendor cache load/save, vendor autoload metadata parsing, call hierarchy disk reads, `codeLens` source reads, and `foldingRange` source reads use blocking/background paths.
- Remaining synchronous reads are limited to synchronous helper code called from blocking contexts, formatter temporary-file IO around timeout/cancellable subprocesses, and startup Composer discovery.
- `PV-003` did not show common request latency symptoms from disk IO on the
  primary large workspace: warm open-file hover/completion/definition p95 stayed
  under 7 ms.

Impact:

- Slow filesystems can still affect background work and startup discovery.
- Hot LSP request paths are materially less likely to block unrelated hover/completion/diagnostics.

Mitigation:

- `PR-023`: implemented blocking/background wrappers and slow IO telemetry.
- Keep profiling large workspaces on slow disks/network mounts.

Exit signal:

- `PV-003` and `PV-004` show hover/completion staying under the common
  interactive target during large-workspace warm and heavy-request scenarios.
- Slow file reads are observable and timeout-safe.

### R-005: Request cancellation coverage for heavy operations

Current evidence:

- `PR-021` introduced `OperationCancellationToken` for background indexing and external analyzer runs.
- New indexing/reindex work cancels the previous indexing run.
- PHPStan/Psalm runs are per URI and cancelled by newer document events, close, delete, or rename.
- `references` and `rename` have cooperative yield points and e2e coverage for `$/cancelRequest`.
- `PV-004` large-workspace cancellation check cancelled `references` 20/20 and
  rename dry-run 20/20; cancel p95 stayed near 2 ms for both request types.

Impact:

- Cancellation coverage exists for the riskiest paths. Not every implemented
  LSP request has a request-scoped cancellation token, but this is no longer a
  high production blocker based on `PV-004`.
- Heavy hierarchy/codeLens requests should continue to be watched in latency benchmarks.

Mitigation:

- `PR-021`: implemented cancellation for indexing, analyzers, references, and rename.
- `PR-050`: added stress tests for cancel references/rename and analyzer timeout/malformed JSON.

Exit signal:

- `PV-004` cancelled `references` and rename dry-run 20/20 with cancel p95 near
  2 ms.
- New hover/completion requests remain responsive while obsolete work is cancelled or yields.

### R-006: `didChange` debounce/version ordering

Current evidence:

- `PR-020` added `document_versions` for open documents and a per-URI debounce task registry.
- `didChange` ignores stale/duplicate document versions.
- Fast diagnostics publish after a 180ms debounce and include the LSP document version.
- Pending debounce tasks are cancelled on new edits, save, close, delete, and rename.

Impact:

- The stale diagnostics overwrite risk is covered by version checks.
- Parser/index refresh still happens on each accepted edit; monitor burst CPU cost on large files.

Mitigation:

- `PR-020`: implemented debounce and version ordering.
- `PR-050`: added 100 `didChange` events/sec stress case with non-ASCII text.

Exit signal:

- Latest-version diagnostics only after a burst; covered by e2e tests.
- No stale diagnostics overwrite newer diagnostics.

### R-007: Version-aware stubs

Current evidence:

- `load_configured_stubs()` reads bundled phpstorm-stubs and loads configured extensions into the main index.
- `PR-011` stores stubs in a dedicated `stubs` cache namespace and reloads changed/missing stub files by mtime/size/config hash.
- `PR-030` parses phpstorm-stubs version-gating attributes and filters symbols/signatures by `phpLsp.phpVersion`.
- Changing PHP version reloads stubs and republishes diagnostics without restart.

Impact:

- First startup may still parse configured stubs; repeated startup/reload can load unchanged stub files from cache.
- Remaining risk is incomplete coverage if phpstorm-stubs adds new version-gating metadata forms not yet parsed.

Mitigation:

- `PR-030`: implemented version-aware symbol and parameter filtering.
- `PR-011`: implemented separate stubs cache keyed by php-lsp version, PHP version, extension list and stubs hash.

Exit signal:

- Changing PHP version updates built-in completion/definition/diagnostics without restart; covered by e2e.
- Stub load time is near-zero from cache after first run.

### R-008: Lazy vendor indexing scale validation

Current evidence:

- Lazy class resolution checks composer namespace maps and can parse `vendor/composer/installed.json`.
- `PR-011` caches lazy-indexed vendor file symbols in a dedicated `vendor` cache namespace.
- `PR-012` caches parsed Composer installed/autoload metadata in memory until the Composer metadata fingerprint changes.
- `PR-012` bounds lazy vendor symbols with a 512-file LRU and restores evicted file symbols from the `vendor` disk cache when needed.
- `PR-012` preloads up to 16 Composer `autoload.files` entrypoints after workspace ready.
- `PV-012` diagnostics samples on `large-symfony`, `large-laravel-crm`, and
  `large-monica` produced no LSP request/stderr failures, but the top unknown
  symbol diagnostics are dominated by missing external vendor metadata in these
  local checkouts.

Impact:

- Large vendor directories may still have unpredictable first-hit latency until acceptance is measured on real projects with installed vendor metadata.
- The LRU cap is conservative and may need tuning after Laravel/Symfony-size profiling.

Mitigation:

- `PR-012`: implemented Composer installed/autoload metadata cache, vendor file symbol LRU and nonblocking `autoload.files` preload.
- `PR-012`: keep vendor file symbols in the dedicated `vendor` disk cache so LRU evictions do not force reparsing unchanged files.

Exit signal:

- First vendor hit is measured; subsequent hits are stable and cheap.
- Vendor cache invalidates on composer metadata changes.

### R-009: PHPDoc/type model depth for production PHP

Current evidence:

- `PR-031` rewrote the PHPDoc type parser for nested generics, callables, array shapes, list/literal/intersection/union types, and common PHPDoc syntax.
- `PR-032`/`PR-033` added `@property`, `@property-read`, `@property-write`, and `@method` virtual members in LSP UI.
- `PR-040`/`PR-041` extended `TypeInfo` and inference for common production PHPDoc/PHP expression patterns.
- `PR-042` reduced framework-heavy diagnostics false positives for common Symfony/Laravel/Doctrine/PHPUnit patterns.
- `IE-030` added PHPDoc template metadata and generic inheritance substitution
  for common repository and collection member types.
- `IE-031` added PHPStan/Psalm `@phpstan-type`/`@psalm-type` and imported
  type alias expansion for indexed signatures, with cycle guards.
- `IE-032` added PHPStan/Psalm conditional return parsing, `class-string<T>`
  call-site template binding, fallback branch unions, and coverage for hover,
  local variable inlay hints, and completion chains after factory calls.
- `IE-033` added shape-aware completion and definition for PHPDoc
  `array{...}` / `object{...}` shapes and literal array shapes, including
  nested shape keys and optional keys.
- `IE-034` added closure/arrow callback parameter inference from
  `callable(...)` signatures, generic collection callback signatures,
  `array_map`-style helper signatures, and `Generator<TKey,TValue>` foreach
  key/value inference.
- `PV-012` fixed a real Symfony false positive for promoted constructor
  properties accessed through a `self`-typed parameter
  (`withDefaults(self $defaults)` then `$defaults->objectManager`).
- `PV-012` release audit confirms the new fixture and real Symfony
  `MapEntity.php` both publish 0 diagnostics.
- `IE-044` / `IE-044A` added conservative Blade-like and Symfony/Twig template
  document support through virtual PHP and source maps, including mapped
  hover/completion/definition/diagnostics/semantic tokens for supported
  template expressions and static Twig template path lookup.

Impact:

- Completion/definition/diagnostics are materially better for PHPDoc-heavy projects, including shape-heavy code, but still not a full static analyzer type system.
- Complex framework magic, fluent generics, project-specific dynamic behavior,
  unsupported template filters/functions/tests, runtime template inheritance,
  and missing Composer/vendor metadata can still need PHPStan/Psalm, framework
  plugins, or diagnostic category tuning.

Mitigation:

- `PR-031`-`PR-034`: PHPDoc parser, virtual members, and e2e coverage.
- `PR-040`-`PR-042`, `IE-030`/`IE-031`: richer type model, inference, PHPDoc
  template/type-alias metadata, and framework false-positive reductions.
- `IE-033`: shape-aware key/property completion and go-to-definition coverage
  for PHPDoc and literal array shapes.
- `PV-012`: added regression coverage for `self`/`static` parameter type
  resolution before member diagnostics.
- `IE-044` / `IE-044A`: template diagnostics are mapped only when source ranges
  are precise; generated or uncertain virtual-PHP ranges are suppressed.

Exit signal:

- Fixture-driven PHPDoc e2e tests cover hover/completion/definition/diagnostics behavior.
- Framework-heavy regression corpus shows reduced false positives without project-specific hardcode.
- Future work should be driven by real-project misses rather than broad parser rewrites.

### R-010: LSP polish/capability mismatch risk

Current evidence:

- `PR-043` added `textDocument/semanticTokens/range`, improved `workspace/symbol`, and stopped advertising `willRenameFiles` until meaningful path-refactor edits exist.
- `PR-051` aligned release packaging with documented platforms and added VSIX smoke checks.
- `PR-052` added `docs/lsp-features.md` with supported/partial/unsupported behavior.

Impact:

- Users can still expect IDE-level behavior beyond the current implementation, but the public docs now call out partial behavior and non-goals.

Mitigation:

- `PR-043`: closed capability mismatches in semantic tokens, workspace symbols, and file rename advertising.
- `PR-051`: smoke test packaged VSIX and release workflow.
- `PR-052`: published architecture, feature matrix and troubleshooting docs.

Exit signal:

- README and `docs/lsp-features.md` clearly mark supported/partial/unsupported behavior.
- Capabilities align with behavior or known limitations explain the gap.

## Current Measurements To Watch

Baseline docs:

- `docs/production-baseline.md`
- `target/php-lsp-profile/*.json`
- `target/php-lsp-profile/*-latency.json`

High-signal metrics:

- cold start to `phpLsp/indexingStatus phase=ready`
- stubs load time
- files/sec and symbols/sec
- peak RSS
- warm p95 hover/completion/definition
- warm p95 references/rename/codeLens
- didChange burst diagnostics latency and stale-result count

## Update Policy

- Update this register when a mitigation lands or a new production blocker is found.
- Keep owner task IDs in sync with `TASKS.md`.
- Do not mark a risk closed until there is a repeatable command or test that proves the exit signal.
