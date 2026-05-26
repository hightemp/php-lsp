# CLI And CI Usage

php-lsp can run the same parser, index, and built-in diagnostics pipeline from
the command line. This is useful for optional CI reporting, release smoke
checks, and local fixture or large-workspace validation.

## GitHub Actions Example

Use `--format github` when you want GitHub workflow annotations. Keep the job
report-only until the project has accepted diagnostic thresholds, because
`php-lsp analyze` exits with code `2` when diagnostics are found.

```yaml
name: PHP LSP Analyze

on:
  pull_request:

jobs:
  php-lsp-analyze:
    runs-on: ubuntu-latest
    continue-on-error: true

    steps:
      - uses: actions/checkout@v4
        with:
          submodules: recursive

      - uses: dtolnay/rust-toolchain@stable

      - name: Build php-lsp
        run: cargo build --release --manifest-path server/Cargo.toml

      - name: Analyze project
        run: |
          ./server/target/release/php-lsp analyze . \
            --project-root . \
            --severity warning \
            --format github
```

Remove `continue-on-error: true` only after diagnostics are stable enough for
the repository to treat them as a required gate.

## Exit Codes

| Command | Code | Meaning |
|---|---:|---|
| `analyze` | `0` | No diagnostics at the requested severity. |
| `analyze` | `1` | Execution or configuration error. |
| `analyze` | `2` | Diagnostics were found. |
| `fix --dry-run` | `0` | No edits would be produced. |
| `fix --dry-run` | `1` | Execution or configuration error. |
| `fix --dry-run` | `2` | Edits would be produced. |

## Local Examples

Run the checked-in fixture scenario:

```bash
scripts/examples/run-cli-analyze-fixture.sh
```

Run a local large workspace and write an anonymized JSON report:

```bash
scripts/examples/run-cli-analyze-large-workspace.sh /path/to/project
```

Both scripts build `server/target/release/php-lsp` when the release binary is
missing. Set `PHP_LSP_BIN` to test a different binary.
