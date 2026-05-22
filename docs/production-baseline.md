# Production Baseline

Дата замера: 2026-05-21 15:57 MSK  
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
rm -rf target/php-lsp-profile/cache-smoke
XDG_CACHE_HOME="$PWD/target/php-lsp-profile/cache-smoke" scripts/profile-workspace.sh --scenario small-cache-smoke=test-fixtures/basic --timeout 60
XDG_CACHE_HOME="$PWD/target/php-lsp-profile/cache-smoke" scripts/profile-workspace.sh --scenario small-cache-smoke=test-fixtures/basic --timeout 60
```

Second run result:

| Scenario | Cache path | Cache files loaded | Indexed files | Symbols | Ready time |
|----------|------------|--------------------|---------------|---------|------------|
| `small-cache-smoke` | `target/php-lsp-profile/cache-smoke/php-lsp/0da0d009104fa203/index.bin` | 4 | 4 | 14 | 285.43 ms |

This validates the `PR-010` workspace cache path and mtime/size-valid file-symbol loading on a small fixture. Large-project acceptance is still tracked by the milestone exit criteria and should be measured with the same command against 5k-10k PHP files.
