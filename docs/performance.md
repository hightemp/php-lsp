# Performance Guide

This document defines how php-lsp performance is measured and which numbers are
used as production-readiness signals.

## Current Baseline

The baseline snapshot lives in `docs/production-baseline.md`.

Current tracked artifacts:

- Validation command wall times.
- Rust test count.
- Host server binary size.
- VSIX package size and contents.
- Small fixture indexing/profile runs.
- LSP latency smoke runs.
- Disk cache smoke runs.
- Large workspace cold/warm profile, latency, and heavy-request responsiveness
  runs.
- Broad fixture audits for type, framework, and template intelligence.
- Packaged VSIX smoke checks.

The risk register in `docs/production-risk-register.md` explains which numbers
are still production blockers.

Latest large-workspace validation numbers are recorded in
`docs/production-baseline.md` under "Production Validation Large Workspace Run".
The latest intelligence milestone refresh is recorded there under
"IE-045 Intelligence Milestone Acceptance Refresh".

## Latest Acceptance Snapshot

The latest acceptance refresh was captured on 2026-05-28 against the primary
10k-file Symfony workspace.

| Area | Artifact | Result |
|---|---|---|
| Cold profile | `target/php-lsp-profile/ie045-large-symfony-cold.json` | 10575 indexed files, 72683 symbols, ready in 7419.09 ms, peak RSS 751,562,752 bytes. |
| Warm profile | `target/php-lsp-profile/ie045-large-symfony-warm.json` | 10575 files loaded from cache, ready in 3436.05 ms, peak RSS 643,149,824 bytes. |
| Latency | `target/php-lsp-profile/ie045-large-symfony-latency.json` | Warm open-file p95: hover 3.727 ms, completion 6.720 ms, definition 3.302 ms. |
| Heavy responsiveness | `target/php-lsp-profile/ie045-large-symfony-heavy-responsiveness.json` | Hover/completion stayed under 10 ms p95 while references or rename dry-run was outstanding; both heavy requests cancelled 20/20. |
| Fixture audit | `target/php-lsp-profile/ie045-lsp-cases-audit.json` | Passed with 20 PHP files, 35 expected diagnostics, no request errors, and no expected non-null definition misses. |
| VSIX smoke | `target/php-lsp-profile/ht-php-lsp-ie045.vsix` | Passed package smoke and host CLI smoke for `linux-x64`. |

## Key Metrics

| Metric | Why it matters |
|---|---|
| Cold start to `phpLsp/indexingStatus phase=ready` | Determines how long a new workspace takes to become fully indexed. |
| Warm start from disk cache | Validates cache effectiveness and invalidation. |
| Stubs load time | Affects every startup. |
| Indexed files/sec and symbols/sec | Measures workspace indexing throughput. |
| Peak RSS | Prevents large projects from exhausting memory. |
| Warm p95 hover/completion/definition | Measures common editor responsiveness. |
| Warm p95 references/rename/codeLens | Tracks heavy workspace request cost. |
| `didChange` burst latency and stale diagnostic count | Guards typing responsiveness and version-ordering correctness. |
| External analyzer timeout/cancellation behavior | Prevents PHPStan/Psalm from hanging the editor. |

## Profiling Workspace Indexing

Use the wrapper script from the repository root:

```bash
scripts/profile-workspace.sh --timeout 60
```

This runs built-in fixture scenarios and writes JSON reports under
`target/php-lsp-profile/`.

For real projects:

```bash
scripts/profile-workspace.sh \
  --timeout 180 \
  --scenario laravel=/path/to/laravel \
  --scenario symfony=/path/to/symfony
```

For cache validation, run the same scenario twice with an isolated cache:

```bash
rm -rf target/php-lsp-profile/cache-smoke
XDG_CACHE_HOME="$PWD/target/php-lsp-profile/cache-smoke" \
  scripts/profile-workspace.sh --scenario app=/path/to/app --timeout 180
XDG_CACHE_HOME="$PWD/target/php-lsp-profile/cache-smoke" \
  scripts/profile-workspace.sh --scenario app=/path/to/app --timeout 180
```

The second run should report cache-loaded files and a materially shorter ready
time.

## Benchmarking LSP Latency

Use:

```bash
scripts/benchmark-lsp-latency.sh --iterations 10 --timeout 60
```

For real projects:

```bash
scripts/benchmark-lsp-latency.sh \
  --iterations 10 \
  --timeout 180 \
  --scenario app=/path/to/app
```

The benchmark measures requests in both unopened and open document states, then
records cold and warm timings. JSON output is written to
`target/php-lsp-profile/*-latency.json`.

Watch these requests closely:

- `textDocument/hover`
- `textDocument/completion`
- `textDocument/definition`
- `textDocument/references`
- Rename dry-run

Hover, completion, and definition are the primary interactive latency budget.
References and rename use indexed per-file references, but remain active
measurement targets because workspace-wide result collection can still scale
with indexed project size.

## Package And Release Size

Host package smoke:

```bash
./scripts/build-server.sh
./scripts/bundle-stubs.sh
cd client
npm ci
npm run build
npx @vscode/vsce package --no-dependencies
```

Universal release package smoke is covered by the release workflow and
`scripts/smoke-vsix.sh`. The smoke test checks:

- `extension/package.json`
- Bundled `extension/out/extension.js`
- README and license files.
- Bundled stubs, including required core files and a minimum PHP stub-file
  count.
- Platform binaries.
- Extension module exports and an activation/deactivation load check.

## Cache Interpretation

Cache paths are reported in indexing status as `cachePath`. The default layout
is:

```text
~/.cache/php-lsp/<workspace-hash>/workspace/index.bin
~/.cache/php-lsp/<workspace-hash>/stubs/index.bin
~/.cache/php-lsp/<workspace-hash>/vendor/index.bin
```

If a warm run reparses too much, inspect the status fields:

- `cacheFilesLoaded`
- `cacheFilesStale`
- `cacheFilesMissing`
- `cachePath`

Common causes of cache misses:

- Different php-lsp version.
- Changed PHP version setting.
- Changed include/exclude paths.
- Changed stub extension list or stub files.
- Composer metadata changes.
- File mtime/size changes.
- Different resolved workspace or Composer root.

## Large Project Acceptance Targets

The production milestone tracks these practical acceptance signals:

| Area | Target signal |
|---|---|
| Warm startup | Ready from disk cache in under 5 seconds on a 5k-10k PHP file workspace. |
| Common requests | Warm p95 hover/completion/definition remains interactive while indexing is running. |
| Heavy requests | References/rename/codeLens do not block unrelated hover/completion. |
| Typing | Burst `didChange` publishes only the latest-version diagnostics. |
| External analyzers | Timeout and malformed JSON never hang the server. |
| Cache | Changed files invalidate without rebuilding unchanged workspace/stub/vendor symbols. |

These are not all hard CI gates yet; they are the numbers to capture before a
production release.

## Local Validation Commands

Before changing performance-sensitive server code, run:

```bash
cd server
cargo fmt --all --check
cargo test --all
cargo clippy --all-targets -- -D warnings
```

For client/package changes:

```bash
cd client
npm ci
npm run lint
npm run build
```

For workflow/release changes:

```bash
go run github.com/rhysd/actionlint/cmd/actionlint@latest .github/workflows/ci.yml .github/workflows/release.yml
bash -n scripts/build-server.sh scripts/bundle-stubs.sh scripts/check-stubs.sh scripts/profile-workspace.sh scripts/benchmark-lsp-latency.sh scripts/smoke-vsix.sh
scripts/check-stubs.sh --kind source server/data/stubs
scripts/check-stubs.sh --kind bundled client/stubs
git diff --check
```
