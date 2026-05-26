#!/usr/bin/env bash
# Run php-lsp analyze against a local large workspace and write an anonymized report.
#
# Usage:
#   scripts/examples/run-cli-analyze-large-workspace.sh /path/to/project
#   PHP_LSP_LARGE_WORKSPACE=/path/to/project scripts/examples/run-cli-analyze-large-workspace.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PHP_LSP_BIN="${PHP_LSP_BIN:-$REPO_ROOT/server/target/release/php-lsp}"
WORKSPACE="${1:-${PHP_LSP_LARGE_WORKSPACE:-}}"
OUT_DIR="${PHP_LSP_EXAMPLE_OUT:-$REPO_ROOT/target/php-lsp-cli-examples}"

if [[ -z "$WORKSPACE" || "$WORKSPACE" == "-h" || "$WORKSPACE" == "--help" ]]; then
    cat <<'USAGE'
Usage:
  scripts/examples/run-cli-analyze-large-workspace.sh /path/to/project

Environment:
  PHP_LSP_BIN              php-lsp binary path
  PHP_LSP_LARGE_WORKSPACE  workspace path when no positional arg is passed
  PHP_LSP_EXAMPLE_OUT      output directory
USAGE
    exit 2
fi

if [[ "$WORKSPACE" != /* ]]; then
    WORKSPACE="$(cd "$WORKSPACE" && pwd)"
fi

if [[ ! -d "$WORKSPACE" ]]; then
    echo "ERROR: workspace does not exist: $WORKSPACE" >&2
    exit 1
fi

if ! command -v python3 >/dev/null 2>&1; then
    echo "ERROR: python3 is required to write the anonymized report" >&2
    exit 1
fi

if [[ ! -x "$PHP_LSP_BIN" ]]; then
    echo "Building php-lsp release binary..."
    cargo build --release --manifest-path "$REPO_ROOT/server/Cargo.toml"
fi

mkdir -p "$OUT_DIR"
RAW_REPORT="$OUT_DIR/large-workspace-analyze.raw.json"
ANON_REPORT="$OUT_DIR/large-workspace-analyze.anonymized.json"

echo "Using binary: $PHP_LSP_BIN"
echo "Using workspace: $WORKSPACE"

set +e
"$PHP_LSP_BIN" analyze "$WORKSPACE" \
    --project-root "$WORKSPACE" \
    --severity warning \
    --format json \
    > "$RAW_REPORT"
ANALYZE_STATUS=$?
set -e

if [[ "$ANALYZE_STATUS" != "0" && "$ANALYZE_STATUS" != "2" ]]; then
    echo "ERROR: analyze failed with exit code $ANALYZE_STATUS" >&2
    exit "$ANALYZE_STATUS"
fi

python3 - "$WORKSPACE" "$RAW_REPORT" "$ANON_REPORT" <<'PY'
import pathlib
import sys

workspace = sys.argv[1]
raw_path = pathlib.Path(sys.argv[2])
anon_path = pathlib.Path(sys.argv[3])

text = raw_path.read_text(encoding="utf-8")
text = text.replace(workspace, "<workspace>")
text = text.replace("file://" + workspace, "file://<workspace>")
anon_path.write_text(text, encoding="utf-8")
raw_path.unlink()
PY

echo "Anonymized analyze report: $ANON_REPORT"
