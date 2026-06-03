# Production Risk Register

Date: 2026-05-22
Last updated: 2026-06-01
Scope: production-readiness milestone, weeks 1-6.

This document tracks known production gaps after the baseline/profiling setup.
The format is intentionally operational: every risk is tied to an owner task in
`TASKS.md` so it can be closed by a measurable change.

## Summary

| ID | Area | Severity | Owner task | Status |
|----|------|----------|------------|--------|
| R-001 | Disk cache maturity | High | `PR-010`, `PR-011`, `PV-002`, `IE-045`, `PHB-010`, `PHB-015` | Mitigated |
| R-002 | `references`/`rename`/`codeLens` scale | High | `PR-022`, `PR-021`, `PV-003`, `PV-004`, `PV-011`, `IE-045` | Accepted limitation |
| R-003 | Parallel indexing acceptance | High | `PR-013`, `PR-023`, `PV-002`, `IE-045` | Mitigated |
| R-004 | Sync file IO in async/hot paths | High | `PR-023`, `PV-003`, `PV-004`, `IE-045` | Mitigated |
| R-005 | Request cancellation coverage for heavy operations | High | `PR-021`, `PR-050`, `PV-004`, `IE-045` | Mitigated |
| R-006 | `didChange` debounce/version ordering | High | `PR-020`, `PR-050` | Mitigated |
| R-007 | Version-aware stubs and package integrity | Medium | `PR-030`, `PR-011`, `PHA-005` | Mitigated |
| R-008 | Lazy vendor indexing scale validation | Medium | `PR-012`, `PR-011`, `PV-014`, `PHB-017` | Partially mitigated |
| R-009 | PHPDoc/type model depth for production PHP | Medium | `PR-031`, `PR-032`, `PR-040`, `PR-041`, `IE-030`, `IE-031`, `PV-012`, `IE-045`, `PHB-003`, `PHB-016` | Accepted limitation |
| R-010 | LSP polish/capability mismatch risk | Medium | `PR-043`, `PR-051`, `PR-052`, `IE-045`, `PHB-001`, `PHB-002`, `PHB-004`, `PHB-005`, `PHB-012`, `PHB-014` | Mitigated |

## Risks

### R-001: Disk cache maturity

Current evidence:

- `PR-010` added a schema-versioned workspace disk cache for file symbols/top-level snapshots.
- `PR-011` split cache files into `workspace`, `stubs`, and `vendor` namespace directories.
- Cache path: `~/.cache/php-lsp/{workspace-hash}/{namespace}/index.bin`.
- Cache invalidates by file mtime, size, content hash, php-lsp version, PHP version, include/exclude paths, stub extension set and stubs hash.
- `PHB-010` records unavailable or pre-epoch filesystem mtimes explicitly and
  keeps content hash as the correctness backstop for file freshness checks.
- `PHB-015` added a serialized cache-shape fixture guard so schema-layout
  changes fail tests until `CACHE_SCHEMA_VERSION` and fixture metadata are
  updated together.
- Fixture smoke run shows cached workspace file symbols loading on second start.
- `PV-002` large workspace run on `large-symfony` loaded 10575 workspace files
  from disk cache on warm start; ready time improved from 7349.48 ms cold to
  3423.19 ms warm, meeting the `<5s` large-workspace warm-start target.
- `PV-002` also showed stubs cache load dropping from 313.73 ms cold to 33.79
  ms warm.
- `IE-045` repeated the same primary large workspace profile after the
  intelligence milestone: 10575 workspace files loaded from disk cache, ready
  time 3436.05 ms warm, and stubs cache load 25.72 ms warm.

Impact:

- Repeated startup on the primary 5k-10k PHP-file workspace meets the
  production target of `<5s` to a ready index from disk cache.
- Changed-file invalidation remains covered by tests and normal dogfood watch,
  but is no longer considered a blocking production risk.
- Vendor composer metadata cache/LRU is tracked separately by `R-008`.

Mitigation:

- `PR-010`: implemented workspace index disk cache with file fingerprint/config/stubs hash invalidation.
- `PR-011`: implemented separate cache namespaces for workspace/stubs/vendor and preserved stub/vendor symbols across workspace reindex.
- `PHA-023`: hardened cache replacement over existing snapshots and added content-hash validation.
- `PHB-010`: hardened file metadata fallback semantics for filesystems with
  unavailable or non-Unix mtimes.
- `PHB-015`: added cache schema fixture validation for serialized layout
  changes.

Exit signal:

- `IE-045` warm `large-symfony` run reaches `phase=ready` in `3436.05 ms`.
- Cache invalidates changed files without full rebuild; keep this covered by
  cache tests and reindex dogfood.
- Cache schema and timestamp fallback behavior stay covered by focused
  `php-lsp-index` tests.

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
- `IE-045` repeated the latency baseline after the intelligence milestone:
  warm open-file `references` p95/p99 76.147 ms / 76.658 ms, rename dry-run
  p95/p99 74.853 ms / 98.153 ms, hover p95 3.727 ms, completion p95 6.720 ms,
  and definition p95 3.302 ms.
- `PV-004` heavy-responsiveness benchmark on `large-symfony` shows
  hover/completion p95 staying under 6.4 ms while `references` or rename dry-run
  is outstanding.
- `PV-004` normal heavy request p95: `references` 76.329 ms, rename dry-run
  82.806 ms.
- `IE-045` heavy-responsiveness run kept hover/completion below 9.3 ms p95
  while heavy requests were outstanding. Heavy request p95 was 85.743 ms for
  `references` and 86.113 ms for rename dry-run.

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
- `IE-045` refreshed those measurements after type/framework/template
  intelligence work.
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
- `IE-045` cold run indexed the same 10575 files / 72683 symbols with
  `filesPerSec=1492.38`, `symbolsPerSec=10257.27`, and peak RSS 751,562,752
  bytes. The warm run stayed under the `<5s` target at 3436.05 ms.

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
- `IE-045` repeated the acceptance run after the intelligence milestone and
  stayed within the same performance envelope.
- Progress reporting remains correct enough for the acceptance/profile harness.

### R-004: Sync file IO in async/hot paths

Current evidence:

- `PR-023` added `run_file_io_blocking()` with `spawn_blocking`, a 15s timeout, and warning telemetry for file reads slower than 100ms.
- Watched-file reindex, lazy PHP/vendor indexing, vendor cache load/save, vendor autoload metadata parsing, call hierarchy disk reads, `codeLens` source reads, and `foldingRange` source reads use blocking/background paths.
- Remaining synchronous reads are limited to synchronous helper code called from blocking contexts, formatter temporary-file IO around timeout/cancellable subprocesses, and startup Composer discovery.
- `PV-003` did not show common request latency symptoms from disk IO on the
  primary large workspace: warm open-file hover/completion/definition p95 stayed
  under 7 ms.
- `IE-045` repeated the common warm open-file request check with hover p95
  3.727 ms, completion p95 6.720 ms, and definition p95 3.302 ms.

Impact:

- Slow filesystems can still affect background work and startup discovery.
- Hot LSP request paths are materially less likely to block unrelated hover/completion/diagnostics.

Mitigation:

- `PR-023`: implemented blocking/background wrappers and slow IO telemetry.
- Keep profiling large workspaces on slow disks/network mounts.

Exit signal:

- `PV-003` and `PV-004` show hover/completion staying under the common
  interactive target during large-workspace warm and heavy-request scenarios.
- `IE-045` refreshed this evidence after Blade/Twig and framework-provider
  work.
- Slow file reads are observable and timeout-safe.

### R-005: Request cancellation coverage for heavy operations

Current evidence:

- `PR-021` introduced `OperationCancellationToken` for background indexing and external analyzer runs.
- New indexing/reindex work cancels the previous indexing run.
- PHPStan/Psalm runs are per URI and cancelled by newer document events, close, delete, or rename.
- `references` and `rename` have cooperative yield points and e2e coverage for `$/cancelRequest`.
- `PV-004` large-workspace cancellation check cancelled `references` 20/20 and
  rename dry-run 20/20; cancel p95 stayed near 2 ms for both request types.
- `IE-045` repeated cancellation after the intelligence milestone:
  `references` cancelled 20/20 with p95 2.441 ms and rename dry-run cancelled
  20/20 with p95 2.256 ms.

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
- `IE-045` refreshed the cancellation result after the intelligence milestone.
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

### R-007: Version-aware stubs and package integrity

Current evidence:

- `load_configured_stubs()` reads bundled phpstorm-stubs and loads configured extensions into the main index.
- `PR-011` stores stubs in a dedicated `stubs` cache namespace and reloads changed/missing stub files by file fingerprint/config hash.
- `PR-030` parses phpstorm-stubs version-gating attributes and filters symbols/signatures by `phpLsp.phpVersion`.
- Changing PHP version reloads stubs and republishes diagnostics without restart.
- `PHA-005` added source/bundled stubs integrity guards for development, CI,
  release packaging, and packaged VSIX smoke tests.
- Server startup now logs intentional stubs disablement separately from missing
  or uninitialized stubs paths.

Impact:

- First startup may still parse configured stubs; repeated startup/reload can load unchanged stub files from cache.
- Remaining risk is incomplete coverage if phpstorm-stubs adds new version-gating metadata forms not yet parsed.
- Publishing a VSIX without usable core stubs is guarded by CI/release checks,
  but package smoke should remain a required release gate.

Mitigation:

- `PR-030`: implemented version-aware symbol and parameter filtering.
- `PR-011`: implemented separate stubs cache keyed by php-lsp version, PHP version, extension list and stubs hash.
- `PHA-005`: added `scripts/check-stubs.sh`, `make check-stubs`,
  `bundle-stubs.sh` hard failures, workflow guards, VSIX smoke checks for core
  stub files/minimum count, and bundled-stubs symbol availability coverage.

Exit signal:

- Changing PHP version updates built-in completion/definition/diagnostics without restart; covered by e2e.
- Source and bundled stubs integrity checks pass in CI and release workflow;
  packaged VSIX smoke fails when required core stubs are absent.
- Stub load time is near-zero from cache after first run.

### R-008: Lazy vendor indexing scale validation

Current evidence:

- Lazy class resolution checks composer namespace maps and can parse `vendor/composer/installed.json`.
- `PR-011` caches lazy-indexed vendor file symbols in a dedicated `vendor` cache namespace.
- `PR-012` caches parsed Composer installed/autoload metadata in memory until the Composer metadata fingerprint changes.
- `PR-012` bounds lazy vendor symbols with a 512-file LRU and restores evicted file symbols from the `vendor` disk cache when needed.
- `PR-012` preloads up to 16 Composer `autoload.files` entrypoints after workspace ready.
- `PHA-023` persists successfully verified lazy vendor class files immediately and rejects PSR-4 candidate files that do not actually define the requested class.
- `PHB-017` fixed PSR-0 path derivation so underscores become path separators
  only in the unqualified class-name segment, preserving underscores in
  namespace segments.
- `PV-012` diagnostics samples on `large-symfony`, `large-laravel-crm`, and
  `large-monica` produced no LSP request/stderr failures, but the top unknown
  symbol diagnostics are dominated by missing external vendor metadata in these
  local checkouts.
- `IE-045` did not change the installed-vendor evidence; the primary large
  Symfony checkout used for acceptance still lacks installed Composer vendor
  metadata.

Impact:

- Large vendor directories may still have unpredictable first-hit latency until acceptance is measured on real projects with installed vendor metadata.
- The LRU cap is conservative and may need tuning after Laravel/Symfony-size profiling.

Mitigation:

- `PR-012`: implemented Composer installed/autoload metadata cache, vendor file symbol LRU and nonblocking `autoload.files` preload.
- `PR-012`: keep vendor file symbols in the dedicated `vendor` disk cache so LRU evictions do not force reparsing unchanged files.
- `PHA-023`: lazy vendor class hits save the dedicated vendor cache and survive restart cache loads when file fingerprints still match.
- `PHB-017`: corrected PSR-0 candidate generation for namespace segments that
  contain underscores.

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
- `PHB-003` invalidates Twig disk context cache entries when PHP render-context
  sources change, keeping open-template refresh and cached disk reads aligned.
- `H-TWIG-LSP-SURFACE-2026-06-01` maps Twig inlay hints back to original
  template ranges, adds conservative Twig delimiter/block syntax diagnostics,
  and infers Twig context variables from typed controller parameters passed
  through render arrays, falling back to `mixed` for render keys whose value
  type cannot be inferred. The same task raised the default Tokio worker stack
  to 8 MiB, with `PHP_LSP_WORKER_THREAD_STACK_SIZE` override, after real
  `bdpn-ui` Twig diagnostics plus lazy vendor indexing exposed a worker stack
  overflow on the runtime default.
- `H-TWIG-PAGINATION-CONTEXT-2026-06-02` extends Twig render-context inference
  for Knp-style paginator variables backed by Doctrine repository/query-builder
  sources, restoring hover/completion/definition/inlay hints for foreach item
  variables in paginated Twig tables.
- `H-TWIG-DATA-REQUEST-SURFACE-2026-06-02` maps simple Twig member chains inside
  otherwise unsupported filters/tests/functions as standalone no-op virtual PHP
  fragments and adds Twig-only getter-derived property completion aliases,
  restoring `dr.id`-style completion plus hover/definition inside `path(...)`,
  `is`, `slice`, and `date` expressions in the `bdpn-ui` data request table.
- `H-TWIG-DEBT-SUSPENSION-MESSAGE-LOG-2026-06-02` extends the same unsupported
  Twig expression fallback to root variables such as `messageLogs is defined`
  and `messageLogs|length`, and resolves iterable repository method PHPDoc
  return types against the method's declaring file before seeding Twig foreach
  variables. This restored class-linked hover/inlay hints and property
  hover/definition/completion for `messageLog.*` in the `bdpn-ui` debt
  suspension message-log table.
- `H-TWIG-BDPN-ARRAY-SHAPE-CONTEXT-2026-06-02` adds Twig array-shape context
  support for repository rows, literal nested arrays, append-built arrays, and
  `compact(...)` render variables. Shape-key hover, source-backed definition,
  completion, and inlay hints now work for records such as `row.npId`,
  `row.messageLog`, `item.nr`, `config_params.encryption.temp_dir_path`,
  `f.type`, and `result.success`.
- `H-TWIG-BDPN-DTO-SERVICE-CONTEXT-2026-06-02` keeps service-returned DTOs and
  repository result collections available through Twig render context and
  one-level `{% include ... with {...} %}` component context. Hover,
  completion, definition, and inlay hints now cover SFTP CSV DTO/service result
  fields and included autocomplete items such as `item.code`.
- `H-TWIG-SYMFONY-GLOBALS-FORMS-2026-06-02` adds static Symfony Twig globals,
  login/form-theme error context, and FormType-backed `FormView` field shapes.
  Hover, completion, definition, and inlay hints now cover `app.current_route`,
  `app.user.*`, `error.messageKey`, form errors, `form.field`, and included
  component values such as `form_field.vars.id` without booting Symfony.
- `H-TWIG-TEMPLATE-PATH-AND-ROUTE-DEFINITION-2026-06-02` extends Twig
  definition for template-path literals and Symfony route keys. Existing
  template files under `templates/` are resolved from tag strings and static
  HTML attribute values, while `path()` / `url()` route names jump to PHP 8
  `#[Route(name: ...)]` attributes through the framework string-key cache.
- `H-TWIG-BDPN-EMAIL-DEBUG-CONTEXT-2026-06-02` extends static Twig context
  inference to literal-template notifier/service helpers and preserves list
  element types through common PHP array pipelines. Email/debug Twig templates
  can now recover hover/definition context from service `notify(..., [...])`
  calls and branchy controller `$result = [...]` arrays without booting
  Symfony.
- `H-TWIG-INLAY-HINT-COVERAGE-2026-06-02` preserves Twig foreach item types
  through type-preserving `filter` expressions and adds focused inlay coverage
  for append-built context arrays and Doctrine repository `findAll()` context
  variables. Mixed foreach values remain suppressed as inlay hints.
- `H-HOVER-PHPSTORM-LIKE-MARKDOWN-2026-06-03` makes PHP hover signatures more
  PHP-like and multi-line for non-trivial callables, while parameter sections
  now include scalar, array, mixed, untyped, nullable, defaulted, by-reference,
  variadic, and PHPDoc-refined parameters with descriptions and class links
  where resolvable.
- `H-HOVER-LOCAL-DECLARATION-SOURCE-LINKS-2026-06-03` keeps indexed symbol hover
  declarations source-like by moving FQNs out of the PHP code block and adding
  linked `Symbol` and `Source` metadata for PHP, vendor-path, and mapped Twig
  getter hovers.
- `H-HOVER-CLASS-RELATIONS-AND-TEMPLATES-2026-06-03` surfaces indexed
  `extends`, `implements`, trait uses, PHPDoc mixins, template params, and
  generic bindings in hover without reparsing source in the request path.
- `H-HOVER-FRAMEWORK-ROLES-AND-ATTRIBUTES-2026-06-03` stores PHP 8 attribute
  groups on indexed symbols and uses them for Symfony/Doctrine hover metadata,
  including controller/action/entity/repository/association roles, rendered
  attributes, and linked Doctrine `repositoryClass` targets without
  request-time source reads.
- `H-HOVER-METHOD-IMPLEMENTS-OVERRIDES-2026-06-03` adds hover links from method
  declarations/calls to exact indexed interface methods and inherited parent
  method overrides, including vendor-interface targets, without full workspace
  scans in the hover hot path.
- `H-HOVER-CALLSITE-GENERIC-SPECIALIZATION-2026-06-03` reuses the existing
  request-time call expression type resolver in hover, so generic
  `class-string<T>` factories, conditional returns, Doctrine
  `getRepository<T>()`, and repository `find`/`findOneBy`/`findBy` calls can
  display concrete `Resolved returns` sections instead of only declared
  `T`/`object`/`EntityRepository<T>` types.
- `PHB-016` tightened PHPDoc literal parsing for scalar numeric forms while
  leaving unsupported or malformed forms as non-literal types.
- `IE-045` fixture audit over `test-fixtures/lsp-cases` passed with no request
  errors, no missing diagnostic payloads, and no expected non-null definition
  misses across the current type/framework/template corpus.

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
- `IE-044` / `IE-044A` / `PHA-030`: template diagnostics are mapped only when
  source ranges are exact and the diagnostic belongs to a conservative
  expression allowlist; generated virtual PHP, template functions,
  incomplete/magic properties, and uncertain ranges are suppressed. Twig
  delimiter/block syntax diagnostics are computed from original template text.
- `PHA-031`: open Twig documents refresh inferred render-context types after
  relevant PHP controller/render edits and workspace reindex completion, with a
  bounded open-template refresh limit.
- `PHA-032`: unsupported complex Twig expressions are explicitly classified and
  skipped with unmapped placeholders instead of being partially converted into
  misleading virtual PHP.
- `PHB-003`: Twig context caches are invalidated on relevant PHP changes.
- `PHB-016`: PHPDoc literal parsing now follows the supported scalar literal
  subset for decimal, binary, octal, hexadecimal, separator, and scientific
  numeric forms.

Exit signal:

- Fixture-driven PHPDoc e2e tests cover hover/completion/definition/diagnostics behavior.
- Framework-heavy regression corpus shows reduced false positives without project-specific hardcode.
- `IE-045` broad fixture audit covers the current milestone corpus and records
  any non-blocking completion-label misses for review.
- Future work should be driven by real-project misses rather than broad parser rewrites.

### R-010: LSP polish/capability mismatch risk

Current evidence:

- `PR-043` added `textDocument/semanticTokens/range`, improved `workspace/symbol`, and stopped advertising `willRenameFiles` until meaningful path-refactor edits exist.
- `PR-051` aligned release packaging with documented platforms and added VSIX smoke checks.
- `PHA-005` tightened VSIX smoke so it verifies required bundled stubs and a
  minimum PHP stub-file count before publishing.
- `PR-052` added `docs/lsp-features.md` with supported/partial/unsupported behavior.
- `IE-045` refreshed README, feature, architecture, performance, baseline, and
  risk documentation after the intelligence milestone acceptance run.
- `PHB-001` made completion context detection byte-column based after LSP
  UTF-16 conversion and added non-ASCII/CRLF coverage.
- `PHB-002` locked down array-key completion byte slicing around ASCII tokens
  and added Chinese/Tibetan key regressions.
- `PHB-004` closed the static-call chain gap for `self`, `static`, and
  `parent` completion inference.
- `PHB-005` narrowed nullsafe-member parsing so suffix handling does not
  consume unrelated question marks.
- `PHB-012` ranked namespace completions by prefix quality before truncation.
- `PHB-014` clarified module ownership and generated-path boundaries in
  architecture documentation.

Impact:

- Users can still expect IDE-level behavior beyond the current implementation, but the public docs now call out partial behavior and non-goals.

Mitigation:

- `PR-043`: closed capability mismatches in semantic tokens, workspace symbols, and file rename advertising.
- `PR-051`: smoke test packaged VSIX and release workflow.
- `PHA-005`: stubs integrity gates for source tree, bundled client stubs, and
  packaged VSIX.
- `PR-052`: published architecture, feature matrix and troubleshooting docs.
- `PHB-001`/`PHB-002`/`PHB-004`/`PHB-005`/`PHB-012`: tightened completion
  context and ranking behavior without changing advertised LSP capabilities.
- `PHB-014`: documented duplicated module names and generated-path boundaries
  so future fixes land in the intended crate/module.

Exit signal:

- README and `docs/lsp-features.md` clearly mark supported/partial/unsupported behavior.
- Capabilities align with behavior or known limitations explain the gap.

## LLM Audit Follow-Up Closure (2026-06-01)

The PHB milestone closed the high-signal correctness findings from the LLM
audit. Validation is recorded here so the production claims remain tied to
observable checks rather than implied coverage.

| Area | Closed findings | Validation | Residual limitation |
|---|---|---|---|
| Completion and context detection | `PHB-001`, `PHB-002`, `PHB-004`, `PHB-005`, `PHB-012` | Focused completion/context tests, non-ASCII UTF-8 boundary coverage, and full Rust test gates for the changed crates. | Completion quality still depends on indexed symbols and best-effort type inference; it is not PHPStan/Psalm parity. |
| References, rename, and semantic classification | `PHB-006`, `PHB-007`, `PHB-011` | Parser reference tests and LSP reference/rename regression coverage. | Unsafe unresolved member references remain non-destructive only; broad workspace reference scans remain an accepted scale limitation in `R-002`. |
| Cache, template context, reliability, and hot-path cost | `PHB-003`, `PHB-008`, `PHB-009`, `PHB-010`, `PHB-015` | Twig context/cache tests, resolver panic-depth guard tests, workspace hierarchy tests, and cache schema/timestamp tests. | First-hit vendor latency is still tracked by `R-008`; no new large-workspace cache timing was collected in this docs-only task. |
| Packaging metadata, docs, Composer, and PHPDoc parsing | `PHB-013`, `PHB-014`, `PHB-016`, `PHB-017` | Metadata/docs checks plus focused Composer and PHPDoc parser tests. | The feature matrix still documents partial template behavior and conservative PHPDoc/type inference instead of claiming full framework/runtime coverage. |

`PHB-018` did not add new smoke measurements. Production timing claims remain
the `IE-045` numbers in `docs/production-baseline.md`.

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
