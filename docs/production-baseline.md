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
