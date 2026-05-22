# Production Risk Register

Дата: 2026-05-22  
Scope: production-readiness milestone, weeks 1-6.

Этот документ фиксирует известные production gaps после baseline/profiling setup. Формат намеренно операционный: каждый риск привязан к owner task из `TASKS.md`, чтобы его можно было закрыть измеримым изменением.

## Summary

| ID | Area | Severity | Owner task | Status |
|----|------|----------|------------|--------|
| R-001 | Disk cache maturity | High | `PR-010`, `PR-011` | Partially mitigated |
| R-002 | `references`/`rename`/`codeLens` делают workspace scan | High | `PR-022`, `PR-021` | Open |
| R-003 | Индексация фактически последовательная | High | `PR-013`, `PR-023` | Open |
| R-004 | Sync file IO в async/hot paths | High | `PR-023` | Open |
| R-005 | Нет request cancellation для тяжелых операций | High | `PR-021`, `PR-050` | Open |
| R-006 | `didChange` без debounce/version ordering | High | `PR-020`, `PR-050` | Open |
| R-007 | Stubs грузятся на старте без separate cache/version filtering | Medium | `PR-030`, `PR-011` | Open |
| R-008 | Lazy vendor indexing без metadata/LRU cache | Medium | `PR-012` | Open |
| R-009 | PHPDoc/type model shallow для production PHP | Medium | `PR-031`, `PR-032`, `PR-040`, `PR-041` | Open |
| R-010 | LSP polish/capability mismatch risk | Medium | `PR-043`, `PR-051`, `PR-052` | Open |

## Risks

### R-001: Disk cache maturity

Current evidence:

- `PR-010` добавил schema-versioned workspace disk cache for file symbols/top-level snapshots.
- Cache path: `~/.cache/php-lsp/{workspace-hash}/index.bin`.
- Cache invalidates by file mtime/size, php-lsp version, PHP version, include/exclude paths, stub extension set and stubs hash.
- Fixture smoke run shows cached workspace file symbols loading on second start.

Impact:

- Повторный запуск на 5k-10k PHP файлов still needs acceptance validation against the production target `< 5s до готового индекса из disk cache`.
- Stubs/vendor are not split into separate cache namespaces yet, so startup still includes stub loading.

Mitigation:

- `PR-010`: implemented workspace index disk cache with mtime/size/config/stubs hash invalidation.
- `PR-011`: отдельные cache namespaces для workspace/stubs/vendor.

Exit signal:

- `scripts/profile-workspace.sh --scenario large=/path/to/project` показывает cold start after first run `< 5s` до `phase=ready`.
- Cache invalidates changed files without full rebuild.

### R-002: `references`/`rename`/`codeLens` делают workspace scan

Current evidence:

- `references` и `rename` проходят по `self.index.file_symbols.iter()`.
- Для неоткрытых файлов handlers делают `std::fs::read_to_string`, `FileParser::new()`, `parse_full()` и `find_references_in_file()`.
- `codeLens` вызывает `reference_locations_for_symbol()` для каждого class/function/member symbol, что масштабируется плохо на больших файлах/workspace.

Impact:

- Latency растет O(indexed files * parse cost) для каждого запроса.
- `rename` может подвисать на больших workspace, особенно при открытом файле с большим числом символов и включенном code lens.

Mitigation:

- `PR-022`: построить reference/occurrence index при indexing and incremental updates.
- `PR-021`: добавить cancellation для long-running references/rename/codeLens.
- `PR-003`: держать latency benchmark как regression gate.

Exit signal:

- Warm p95 `references`/`renameDryRun` на large fixture перестает зависеть от полного reparse workspace.
- `codeLens` не делает full references scan на каждый visible document refresh.

### R-003: Индексация фактически последовательная

Current evidence:

- `index_workspace()` создает `Semaphore::new(4)`, но затем идет обычный `for` loop и держит один permit внутри каждой итерации.
- File read/parse/update выполняются inline в этой async task.

Impact:

- CPU cores простаивают на первом full index.
- На больших проектах startup и reindex будут линейно расти по числу PHP files.

Mitigation:

- `PR-013`: заменить sequential loop на bounded task queue или `JoinSet`.
- `PR-023`: вынести blocking file IO/parse work из hot async path.

Exit signal:

- `scripts/profile-workspace.sh` показывает устойчивый рост files/sec на многоядерных машинах без роста false errors/races.
- Progress reporting остается корректным при параллельном indexing.

### R-004: Sync file IO в async/hot paths

Current evidence:

- `std::fs::read_to_string` используется в lazy vendor indexing, workspace indexing, references, rename, codeLens, folding range and formatter result readback.
- Часть этих чтений происходит в LSP request handlers.

Impact:

- Медленная FS, network mounts or huge files can block the async executor and delay unrelated hover/completion/diagnostics.

Mitigation:

- `PR-023`: использовать `tokio::task::spawn_blocking` или dedicated file IO worker for bulk reads.
- Добавить slow IO telemetry в profiling JSON/logs.

Exit signal:

- Parallel hover/completion remain responsive while indexing/references are reading files.
- Slow file reads are observable and timeout-safe.

### R-005: Нет request cancellation для тяжелых операций

Current evidence:

- There is no task registry/cancellation token around indexing, references, rename, codeLens or external analyzer runs.
- Existing external analyzers have timeouts, but LSP `$/cancelRequest` is not a general control path for heavy server work.

Impact:

- Editor can keep waiting for obsolete references/rename/analyzer requests.
- Rapid navigation/editing can leave expensive work running after the user no longer needs it.

Mitigation:

- `PR-021`: introduce request-scoped cancellation tokens and task registry.
- `PR-050`: stress test cancel references/rename on large workspace.

Exit signal:

- Cancelled long-running requests return LSP cancellation errors where appropriate.
- New requests are not delayed by obsolete cancelled work.

### R-006: `didChange` без debounce/version ordering

Current evidence:

- `did_change()` applies edits, updates index immediately, then calls `publish_fast_diagnostics()` directly.
- It does not track LSP document version in a queue and does not cancel outdated diagnostic work.

Impact:

- Rapid typing can produce unnecessary parser/index/diagnostic churn.
- Older diagnostics could race with newer results once slower diagnostic paths are added.

Mitigation:

- `PR-020`: debounce diagnostics 150-250 ms, store document versions, cancel stale tasks.
- `PR-050`: 100 `didChange` events/sec stress case with non-ASCII text.

Exit signal:

- Only latest document version publishes diagnostics after a burst.
- No stale diagnostics overwrite newer diagnostics.

### R-007: Stubs грузятся на старте без separate cache/version filtering

Current evidence:

- `load_configured_stubs()` reads bundled phpstorm-stubs and loads configured extensions into the main index.
- `stubs::load_stubs()` parses stub files and marks symbols builtin, but stubs are not yet stored in a dedicated cache namespace.
- `phpLsp.phpVersion` affects diagnostics/refactors in server logic, but built-in symbol availability is not yet filtered from version-gated stub metadata.

Impact:

- Startup includes repeated stub parse/load cost.
- Completion/definition can expose built-ins that are not available for the configured PHP version.

Mitigation:

- `PR-030`: parse version-gated stub attributes and filter symbols/signatures by `phpLsp.phpVersion`.
- `PR-011`: separate stubs cache keyed by php-lsp version, PHP version, extension list and stubs hash.

Exit signal:

- Changing PHP version updates built-in completion/definition/diagnostics without restart.
- Stub load time is near-zero from cache after first run.

### R-008: Lazy vendor indexing без metadata/LRU cache

Current evidence:

- Lazy class resolution checks composer namespace maps and can parse `vendor/composer/installed.json`.
- Vendor metadata and file symbols are not cached as a dedicated layer.

Impact:

- Repeated vendor lookups can re-read metadata/files.
- Large vendor directories may cause unpredictable first-hit latency.

Mitigation:

- `PR-012`: cache composer installed/autoload metadata and add LRU for vendor file symbols.
- Defer popular package preloading until after workspace ready.

Exit signal:

- First vendor hit is measured; subsequent hits are stable and cheap.
- Vendor cache invalidates on composer metadata changes.

### R-009: PHPDoc/type model shallow для production PHP

Current evidence:

- PHPDoc and type inference support many editor cases, but complex generics/callables/array shapes are still milestone work.
- Current tasks `PR-031`-`PR-041` exist because framework-heavy code needs richer type propagation.

Impact:

- Completion/definition/diagnostics can be incomplete or noisy for Laravel/Symfony/PHPUnit patterns that rely on PHPDoc generics, templates and fluent APIs.

Mitigation:

- `PR-031`: robust PHPDoc type parser.
- `PR-032`/`PR-033`: virtual members and property access modes.
- `PR-040`/`PR-041`: extend `TypeInfo` and inference.

Exit signal:

- Fixture-driven PHPDoc e2e tests cover hover/completion/definition/diagnostics behavior.
- Framework-heavy regression corpus shows reduced false positives without project-specific hardcode.

### R-010: LSP polish/capability mismatch risk

Current evidence:

- Capabilities advertise broad workspace and file operation support.
- Some behavior is still intentionally limited, such as `semanticTokens/range` absence and file operation `will*` hooks that do not yet perform namespace/class refactors.

Impact:

- Users may expect IDE-level behavior for every advertised capability.
- Release/Marketplace docs can overpromise if limitations are not explicit.

Mitigation:

- `PR-043`: close or document LSP polish gaps.
- `PR-051`: smoke test packaged VSIX and release workflow.
- `PR-052`: publish architecture, feature matrix and troubleshooting docs.

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
