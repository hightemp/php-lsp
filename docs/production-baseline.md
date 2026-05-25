# Production Baseline

Latest acceptance refresh: 2026-05-25
Current project version: `0.5.5`
Latest checked revision: `a9692c0` + working tree validation updates

## Initial Baseline

Measurement date: 2026-05-21 15:57 MSK
Git revision: `ca1d6d6`  
Host: `Linux apanov-Legion-S7-16IAH7 6.8.0-110-generic x86_64`

## Toolchain

| Tool | Version |
|------|---------|
| Rust | `rustc 1.93.1 (01f6ddf75 2026-02-11)` |
| Cargo | `cargo 1.93.1 (083ac5135 2025-12-15)` |
| Node.js | `v24.13.0` |
| npm | `11.6.2` |
| VSIX packager | `npx --yes @vscode/vsce package` |

## Validation Baseline

| Command | Result | Wall time |
|---------|--------|-----------|
| `cargo fmt --all --check` | pass | 0.55s |
| `cargo test --all` | pass, 226 tests | 11.20s |
| `cargo clippy --all-targets -- -D warnings` | pass | 3.60s |
| `npm run lint` | pass | 0.77s |
| `npm run build` | pass | 0.19s |

Rust test count: 226 non-doc tests, 0 failures.

Breakdown from `cargo test --all`:

| Target | Tests |
|--------|-------|
| `php-lsp-completion` | 12 |
| `php-lsp-index` | 22 |
| `php-lsp-parser` | 121 |
| `php-lsp-server` unit tests | 26 |
| `php-lsp-server` e2e tests | 43 |
| `php-lsp-types` | 2 |

## Acceptance Validation Refresh

Date: 2026-05-25
Scope: final production-readiness acceptance after PR-052 documentation updates.

| Command | Result |
|---------|--------|
| `cargo fmt --all --check` | pass |
| `cargo test --all` | pass, 311 tests |
| `cargo clippy --all-targets -- -D warnings` | pass |
| `npm run lint` | pass |
| `npm run build` | pass |
| `go run github.com/rhysd/actionlint/cmd/actionlint@latest .github/workflows/ci.yml .github/workflows/release.yml` | pass |
| `bash -n scripts/build-server.sh scripts/bundle-stubs.sh scripts/profile-workspace.sh scripts/benchmark-lsp-latency.sh scripts/smoke-vsix.sh` | pass |
| `git diff --check` | pass |

Rust acceptance test breakdown:

| Target | Tests |
|--------|-------|
| `php-lsp-completion` | 25 |
| `php-lsp-index` | 28 |
| `php-lsp-parser` | 154 |
| `php-lsp-server` unit tests | 45 |
| `php-lsp-server` e2e tests | 57 |
| `php-lsp-types` | 2 |

## Build Artifacts

| Artifact | Command | Size |
|----------|---------|------|
| Host server binary, stripped | `./scripts/build-server.sh` | 6.8M / 7,087,536 bytes |
| Baseline VSIX | `npx --yes @vscode/vsce package --out ../target/php-lsp-profile/php-lsp-0.4.1-baseline.vsix` | 3.7M / 3,854,637 bytes |

`./scripts/build-server.sh` built host target `x86_64-unknown-linux-gnu` and copied the stripped binary to `client/bin/linux-x64/php-lsp`.

VSIX contents reported by `vsce`:

| Path | Count / Size |
|------|--------------|
| `extension/bin/` | 1 file, 6.76 MB |
| `extension/images/` | 1 file, 664.8 KB |
| `extension/out/` | 1 file, 354.36 KB |
| `extension/stubs/` | 90 files, 3.05 MB |
| Total package | 98 files, 3.68 MB |

## Notes

- Packaging output is intentionally written under `target/php-lsp-profile/` so release artifacts in `client/` are not overwritten during baseline collection.
- The current baseline validates correctness and package size only. Production performance measurements for cold start, indexing throughput, memory RSS, stubs load time and request latency are tracked separately in `PR-002` and `PR-003`.

## Perf Harness Smoke Run

Command:

```bash
scripts/profile-workspace.sh --timeout 60
```

Outputs:

| Scenario | JSON | Indexed files | Symbols | Stub files | Ready time | Peak RSS |
|----------|------|---------------|---------|------------|------------|----------|
| `small-fixture` | `target/php-lsp-profile/small-fixture.json` | 4 | 14 | 85 | 286.41 ms | 20,041,728 bytes |
| `composer-psr4` | `target/php-lsp-profile/composer-psr4.json` | 1 | 5 | 85 | 272.44 ms | 19,959,808 bytes |
| `vendor-heavy` | `target/php-lsp-profile/vendor-heavy.json` | 4 | 18 | 85 | 266.95 ms | 20,025,344 bytes |

The harness supports additional real-project runs via:

```bash
scripts/profile-workspace.sh --scenario laravel=/path/to/laravel --scenario symfony=/path/to/symfony
```

## LSP Latency Benchmark Smoke Run

Command:

```bash
scripts/benchmark-lsp-latency.sh --iterations 3 --timeout 60
```

The benchmark starts separate LSP sessions for `unopened` and `open` document states. Each session measures the same request batch before explicit index readiness (`cold`) and after `phpLsp/indexingStatus` reports `ready` (`warm`). Full per-request measurements are written to `target/php-lsp-profile/*-latency.json`.

Warm-index p95 summary:

| Scenario | State | Hover | Completion | Definition | References | Rename dry-run |
|----------|-------|-------|------------|------------|------------|----------------|
| `lsp-cases` | open | 0.341 ms | 0.335 ms | 0.222 ms | 4.957 ms | 4.259 ms |
| `lsp-cases` | unopened | 0.207 ms | 0.190 ms | 0.262 ms | 0.243 ms | 0.191 ms |
| `vendor-heavy` | open | 0.314 ms | 0.624 ms | 0.442 ms | 0.566 ms | 0.260 ms |
| `vendor-heavy` | unopened | 0.100 ms | 0.172 ms | 0.156 ms | 0.074 ms | 0.053 ms |
| `small-fixture` | open | 0.688 ms | 0.655 ms | 0.373 ms | 4.312 ms | 3.667 ms |
| `small-fixture` | unopened | 0.338 ms | 0.269 ms | 0.245 ms | 0.238 ms | 0.251 ms |

For real-project latency runs:

```bash
scripts/benchmark-lsp-latency.sh --iterations 10 --scenario laravel=/path/to/laravel --scenario symfony=/path/to/symfony
```

## Disk Cache Smoke Run

Command:

```bash
rm -rf target/php-lsp-profile/cache-smoke-pr011
XDG_CACHE_HOME="$PWD/target/php-lsp-profile/cache-smoke-pr011" scripts/profile-workspace.sh --scenario small-cache-smoke-pr011=test-fixtures/basic --timeout 60
XDG_CACHE_HOME="$PWD/target/php-lsp-profile/cache-smoke-pr011" scripts/profile-workspace.sh --scenario small-cache-smoke-pr011=test-fixtures/basic --timeout 60
```

Second run result:

| Scenario | Cache path | Cache files loaded | Indexed files | Symbols | Ready time |
|----------|------------|--------------------|---------------|---------|------------|
| `small-cache-smoke-pr011` | `target/php-lsp-profile/cache-smoke-pr011/php-lsp/0da0d009104fa203/workspace/index.bin` | 4 | 4 | 14 | 26.71 ms |

This validates the `PR-010` workspace cache path and mtime/size-valid file-symbol loading on a small fixture. After `PR-011`, workspace/stubs/vendor snapshots live under separate namespace directories below the same workspace hash; this smoke run created `workspace/index.bin` and `stubs/index.bin`, with second-run stubs load at 23.23 ms. Large-project acceptance is still tracked by the milestone exit criteria and should be measured with the same command against 5k-10k PHP files.

## Production Validation Large Workspace Run

Date: 2026-05-25
Milestone task: `PV-001` / `PV-002`
Primary scenario: `large-symfony`

Workspace inventory:

| Scenario | PHP files | Indexed files | Symbols | Workspace size | Composer metadata | Notes |
|----------|-----------|---------------|---------|----------------|-------------------|-------|
| `large-laravel-crm` | 904 | 866 | 2401 | 99M | `composer.json`, `composer.lock` | Additional Laravel-like scenario; below the 5k-file target. |
| `large-symfony` | 10631 | 10575 | 72683 | 482M | `composer.json` | Primary large acceptance scenario; no `composer.lock` or `vendor/composer/installed.json`. |
| `large-monica` | 1656 | 1330 | 7163 | 169M | `composer.json`, `composer.lock` | Additional Laravel-like scenario; below the 5k-file target. |

Profile smoke outputs:

| Scenario | JSON | Ready time | Peak RSS |
|----------|------|------------|----------|
| `large-laravel-crm` | `target/php-lsp-profile/large-laravel-crm.json` | 467.61 ms | 76,668,928 bytes |
| `large-symfony` | `target/php-lsp-profile/large-symfony.json` | 7460.54 ms | 728,272,896 bytes |
| `large-monica` | `target/php-lsp-profile/large-monica.json` | 620.68 ms | 107,020,288 bytes |

Isolated cache command:

```bash
rm -rf target/php-lsp-profile/large-cache
XDG_CACHE_HOME="$PWD/target/php-lsp-profile/large-cache" \
  scripts/profile-workspace.sh --scenario large-symfony-cold=<large-symfony-workspace> --timeout 300
XDG_CACHE_HOME="$PWD/target/php-lsp-profile/large-cache" \
  scripts/profile-workspace.sh --scenario large-symfony-warm=<large-symfony-workspace> --timeout 300
```

Cold/warm cache results:

| Scenario | Cache loaded | Cache missing | Indexed files | Symbols | Stub files | Stubs load | Ready time | Peak RSS |
|----------|--------------|---------------|---------------|---------|------------|------------|------------|----------|
| `large-symfony-cold` | 0 | 10575 | 10575 | 72683 | 86 | 313.73 ms | 7349.48 ms | 730,419,200 bytes |
| `large-symfony-warm` | 10575 | 0 | 10575 | 72683 | 86 | 33.79 ms | 3423.19 ms | 625,729,536 bytes |

Cache artifacts:

| Namespace | Path | Size |
|-----------|------|------|
| `workspace` | `target/php-lsp-profile/large-cache/php-lsp/b416e4a456d014e6/workspace/index.bin` | 141,725,060 bytes |
| `stubs` | `target/php-lsp-profile/large-cache/php-lsp/b416e4a456d014e6/stubs/index.bin` | 4,351,427 bytes |

Result: the primary large workspace meets the warm-start target `< 5s`
(`3423.19 ms` to `phase=ready`) from disk cache. Latency and heavy-request
acceptance are tracked separately by `PV-003` and `PV-004`.

### Large Workspace Latency

Command:

```bash
scripts/benchmark-lsp-latency.sh \
  --iterations 20 \
  --timeout 300 \
  --scenario large-symfony=<large-symfony-workspace>
```

Output: `target/php-lsp-profile/large-symfony-latency.json`

Warm-index p95/p99 summary:

| State | Hover p95 / p99 | Completion p95 / p99 | Definition p95 / p99 | References p95 / p99 | Rename dry-run p95 / p99 |
|-------|-----------------|----------------------|----------------------|----------------------|--------------------------|
| `open` | 3.562 ms / 4.164 ms | 6.556 ms / 7.462 ms | 2.855 ms / 3.050 ms | 72.527 ms / 74.123 ms | 73.529 ms / 73.619 ms |
| `unopened` | 0.206 ms / 0.449 ms | 0.248 ms / 0.289 ms | 0.338 ms / 0.398 ms | 0.218 ms / 0.285 ms | 0.144 ms / 0.230 ms |

Result: warm p95 for common interactive requests (`hover`, `completion`,
`definition`) is below the `<50ms` production target on the primary large
workspace. Warm open-file `references` and rename dry-run are materially more
expensive at roughly 73ms p95 and remain covered by `PV-004` heavy-request
responsiveness checks.

### Large Workspace Heavy-Request Responsiveness

Command:

```bash
scripts/benchmark-lsp-latency.sh \
  --heavy-responsiveness \
  --iterations 20 \
  --timeout 300 \
  --scenario large-symfony=<large-symfony-workspace>
```

Output: `target/php-lsp-profile/large-symfony-heavy-responsiveness.json`

Fast requests while a heavy request is outstanding:

| Heavy request | Hover p95 / p99 | Completion p95 / p99 |
|---------------|-----------------|----------------------|
| `references` | 6.067 ms / 6.199 ms | 5.851 ms / 6.381 ms |
| `renameDryRun` | 5.755 ms / 5.875 ms | 6.390 ms / 6.425 ms |

Heavy request duration and cancellation:

| Request | Normal p95 / p99 | Cancelled | Cancel p95 / p99 |
|---------|------------------|-----------|------------------|
| `references` | 76.329 ms / 77.006 ms | 20/20 | 2.101 ms / 2.827 ms |
| `renameDryRun` | 82.806 ms / 85.172 ms | 20/20 | 2.216 ms / 2.524 ms |

Result: `references` and rename dry-run do not block unrelated
hover/completion on the primary large workspace. Large-workspace
`$/cancelRequest` also cancels both heavy request types consistently in this
benchmark.

### Large Workspace Diagnostics Audit

Date: 2026-05-25
Milestone task: `PV-012`

Sample command shape:

```bash
python3 scripts/audit-lsp-workspace.py \
  --scenario <scenario-name> \
  --workspace <workspace-root> \
  --server server/target/release/php-lsp \
  --stubs client/stubs \
  --out target/php-lsp-profile \
  --max-files 500 \
  --max-definition-probes 0 \
  --no-document-symbol \
  --no-include-vendor
```

Diagnostics sample results:

| Scenario | Output | Files opened | Files with diagnostics | Diagnostics | Missing diagnostics | Request/stderr errors | Classification |
|----------|--------|--------------|------------------------|-------------|---------------------|-----------------------|----------------|
| `large-symfony-diagnostics-sample` | `target/php-lsp-profile/large-symfony-diagnostics-sample.json` | 500 | 366 | 2800 | 0 | 0 / 0 | Mostly unknown external PHPUnit/Twig/Doctrine/Monolog symbols because this checkout has no `composer.lock` or `vendor/composer/installed.json`. |
| `large-laravel-crm-diagnostics-sample` | `target/php-lsp-profile/large-laravel-crm-diagnostics-sample.json` | 500 | 183 | 1006 | 0 | 0 / 0 | Mostly missing Laravel/Illuminate/Carbon/Konekt vendor metadata. |
| `large-monica-diagnostics-sample` | `target/php-lsp-profile/large-monica-diagnostics-sample.json` | 500 | 453 | 2124 | 0 | 0 / 0 | Mostly missing Laravel/Sabre/Carbon/Inertia vendor metadata plus accepted dynamic Eloquent relation member limits. |

Fixed false positive:

| Scenario | Output | Result |
|----------|--------|--------|
| `fixture-promoted-self-diagnostics-release` | `target/php-lsp-profile/fixture-promoted-self-diagnostics-release.json` | 1 file, 0 diagnostics |
| `large-symfony-mapentity-diagnostics-release` | `target/php-lsp-profile/large-symfony-mapentity-diagnostics-release.json` | Real Symfony `MapEntity.php`, 0 diagnostics |

The fixed issue was a member diagnostic false positive for promoted constructor
properties accessed through a `self`-typed parameter, as in
`withDefaults(self $defaults)` followed by `$defaults->objectManager`. The
resolver now maps `self`/`static` parameter types to the enclosing class before
member lookup. Regression coverage exists in the parser resolver, server
diagnostics, and `test-fixtures/lsp-cases/src/Diagnostics/PromotedSelfDefaults.php`.
The copied host VS Code binary `client/bin/linux-x64/php-lsp` was also checked
with the same fixture and real Symfony `MapEntity.php`; both runs published 0
diagnostics.

## Packaged VSIX Dogfood Smoke

Date: 2026-05-25
Milestone task: `PV-011`

Commands:

```bash
./scripts/build-server.sh
./scripts/bundle-stubs.sh
cd client
npm ci
npm run build
npx @vscode/vsce package --no-dependencies -o /tmp/ht-php-lsp-pv011.vsix
cd ..
PHP_LSP_VSIX_PLATFORMS=linux-x64 scripts/smoke-vsix.sh /tmp/ht-php-lsp-pv011.vsix
code --install-extension /tmp/ht-php-lsp-pv011.vsix --force
code --list-extensions | rg '^hightemp\.ht-php-lsp$'
```

Result:

| Check | Result |
|-------|--------|
| Host server build | pass, `client/bin/linux-x64/php-lsp` size 7.6M |
| Stubs bundle | pass, 31 extensions, 3.5M |
| Client build | pass |
| Host VSIX package | pass, `/tmp/ht-php-lsp-pv011.vsix`, 3.99M |
| Host VSIX smoke | pass with `PHP_LSP_VSIX_PLATFORMS=linux-x64` |
| VS Code CLI install | pass, extension ID `hightemp.ht-php-lsp` installed |

Notes:

- The default `scripts/smoke-vsix.sh` checks the universal six-platform release
  package. The local PV-011 package is host-only, so the smoke test was run with
  `PHP_LSP_VSIX_PLATFORMS=linux-x64`.
- `npm ci` reported audit findings in dependencies: 2 moderate and 1 high. The
  audit warning did not fail build/package, but should be reviewed before GA if
  dependency policy requires a clean audit.
- Interactive VS Code UI dogfood checks for status popup and command palette
  behavior still require a GUI session.
