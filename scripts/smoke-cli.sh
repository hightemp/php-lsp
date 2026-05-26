#!/usr/bin/env bash
# Smoke-test php-lsp CLI subcommands on an already built binary.
#
# Usage:
#   scripts/smoke-cli.sh path/to/php-lsp [path/to/fixture]

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PHP_LSP_BIN="${1:-${PHP_LSP_BIN:-}}"
FIXTURE="${2:-$REPO_ROOT/test-fixtures/basic}"

usage() {
    cat <<'USAGE'
Usage:
  scripts/smoke-cli.sh path/to/php-lsp [path/to/fixture]

Checks:
  php-lsp --version
  php-lsp --help
  php-lsp analyze --help
  php-lsp fix --help
  php-lsp init-config --path <temp>
  php-lsp analyze <fixture> --severity error --format json
  php-lsp fix <fixture> --dry-run --format json
USAGE
}

if [[ -z "$PHP_LSP_BIN" || "$PHP_LSP_BIN" == "-h" || "$PHP_LSP_BIN" == "--help" ]]; then
    usage
    exit 2
fi

if [[ "$PHP_LSP_BIN" != /* ]]; then
    PHP_LSP_BIN="$REPO_ROOT/$PHP_LSP_BIN"
fi
if [[ "$FIXTURE" != /* ]]; then
    FIXTURE="$REPO_ROOT/$FIXTURE"
fi

for tool in grep mktemp; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "ERROR: required tool not found: $tool" >&2
        exit 1
    fi
done

if [[ ! -x "$PHP_LSP_BIN" ]]; then
    echo "ERROR: php-lsp binary not found or not executable: $PHP_LSP_BIN" >&2
    exit 1
fi

if [[ ! -d "$FIXTURE" ]]; then
    echo "ERROR: fixture directory not found: $FIXTURE" >&2
    exit 1
fi

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT
mkdir -p "$TMP_DIR/home" "$TMP_DIR/xdg"

run_clean() {
    HOME="$TMP_DIR/home" \
    XDG_CONFIG_HOME="$TMP_DIR/xdg" \
    "$@"
}

expect_output_contains() {
    local label="$1"
    local needle="$2"
    shift 2

    local output
    output="$(run_clean "$@")"
    if ! grep -Fq "$needle" <<< "$output"; then
        echo "ERROR: $label did not contain expected text: $needle" >&2
        echo "$output" >&2
        exit 1
    fi
}

expect_json_command() {
    local label="$1"
    shift

    local output
    local status
    set +e
    output="$(run_clean "$@" 2>&1)"
    status=$?
    set -e

    if [[ "$status" != "0" && "$status" != "2" ]]; then
        echo "ERROR: $label exited with $status" >&2
        echo "$output" >&2
        exit 1
    fi
    if ! grep -Fq '"schemaVersion"' <<< "$output"; then
        echo "ERROR: $label did not produce JSON report output" >&2
        echo "$output" >&2
        exit 1
    fi
}

echo "CLI smoke: $PHP_LSP_BIN"

expect_output_contains "version" "php-lsp " "$PHP_LSP_BIN" --version
expect_output_contains "top-level help" "Start the LSP server" "$PHP_LSP_BIN" --help
expect_output_contains "analyze help" "php-lsp analyze" "$PHP_LSP_BIN" analyze --help
expect_output_contains "fix help" "php-lsp fix" "$PHP_LSP_BIN" fix --help

run_clean "$PHP_LSP_BIN" init-config --path "$TMP_DIR/.php-lsp.toml" >/dev/null
if [[ ! -s "$TMP_DIR/.php-lsp.toml" ]]; then
    echo "ERROR: init-config did not create .php-lsp.toml" >&2
    exit 1
fi

expect_json_command \
    "analyze fixture" \
    "$PHP_LSP_BIN" analyze "$FIXTURE" --project-root "$FIXTURE" --severity error --format json

expect_json_command \
    "fix fixture" \
    "$PHP_LSP_BIN" fix "$FIXTURE" --dry-run --project-root "$FIXTURE" --format json

echo "CLI smoke test passed"
