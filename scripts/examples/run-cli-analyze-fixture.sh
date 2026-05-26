#!/usr/bin/env bash
# Run php-lsp CLI commands against the checked-in fixture project.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PHP_LSP_BIN="${PHP_LSP_BIN:-$REPO_ROOT/server/target/release/php-lsp}"
FIXTURE="${PHP_LSP_FIXTURE:-$REPO_ROOT/test-fixtures/lsp-cases}"
OUT_DIR="${PHP_LSP_EXAMPLE_OUT:-$REPO_ROOT/target/php-lsp-cli-examples}"

if [[ ! -x "$PHP_LSP_BIN" ]]; then
    echo "Building php-lsp release binary..."
    cargo build --release --manifest-path "$REPO_ROOT/server/Cargo.toml"
fi

mkdir -p "$OUT_DIR"

echo "Using binary: $PHP_LSP_BIN"
echo "Using fixture: $FIXTURE"

set +e
"$PHP_LSP_BIN" analyze "$FIXTURE" \
    --project-root "$FIXTURE" \
    --severity warning \
    --format json \
    > "$OUT_DIR/fixture-analyze.json"
ANALYZE_STATUS=$?
set -e

if [[ "$ANALYZE_STATUS" != "0" && "$ANALYZE_STATUS" != "2" ]]; then
    echo "ERROR: analyze failed with exit code $ANALYZE_STATUS" >&2
    exit "$ANALYZE_STATUS"
fi

set +e
"$PHP_LSP_BIN" fix "$FIXTURE" \
    --dry-run \
    --project-root "$FIXTURE" \
    --format json \
    > "$OUT_DIR/fixture-fix-dry-run.json"
FIX_STATUS=$?
set -e

if [[ "$FIX_STATUS" != "0" && "$FIX_STATUS" != "2" ]]; then
    echo "ERROR: fix dry-run failed with exit code $FIX_STATUS" >&2
    exit "$FIX_STATUS"
fi

echo "Analyze report: $OUT_DIR/fixture-analyze.json"
echo "Fix dry-run report: $OUT_DIR/fixture-fix-dry-run.json"
