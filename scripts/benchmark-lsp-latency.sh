#!/usr/bin/env bash
# Run php-lsp request latency benchmarks and write JSON metrics.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SERVER_BIN="${PHP_LSP_BENCH_SERVER:-$REPO_ROOT/client/bin/linux-x64/php-lsp}"
STUBS_PATH="${PHP_LSP_BENCH_STUBS:-$REPO_ROOT/client/stubs}"
OUT_DIR="${PHP_LSP_BENCH_OUT:-$REPO_ROOT/target/php-lsp-profile}"
TIMEOUT="${PHP_LSP_BENCH_TIMEOUT:-120}"
ITERATIONS="${PHP_LSP_BENCH_ITERATIONS:-5}"

SCENARIOS=()

usage() {
    cat <<'USAGE'
Usage:
  scripts/benchmark-lsp-latency.sh [options]
  scripts/benchmark-lsp-latency.sh --scenario name=/path/to/project
  scripts/benchmark-lsp-latency.sh /path/to/project

Options:
  --server PATH          php-lsp server binary (default: client/bin/linux-x64/php-lsp)
  --stubs PATH           bundled stubs directory (default: client/stubs)
  --out DIR              output directory (default: target/php-lsp-profile)
  --timeout SECONDS      server ready timeout per session (default: 120)
  --iterations N         iterations per request/phase/open-state (default: 5)
  --scenario NAME=PATH   add a named scenario; may be passed multiple times
  -h, --help             show this help

Default scenarios:
  lsp-cases              test-fixtures/lsp-cases
  vendor-heavy           test-fixtures/vendor-resolve
  small-fixture          test-fixtures/basic

Output:
  One JSON file per scenario: target/php-lsp-profile/*-latency.json.
USAGE
}

add_scenario() {
    local SPEC="$1"
    local NAME
    local PATH_VALUE

    if [[ "$SPEC" == *=* ]]; then
        NAME="${SPEC%%=*}"
        PATH_VALUE="${SPEC#*=}"
    else
        PATH_VALUE="$SPEC"
        NAME="$(basename "$PATH_VALUE")"
    fi

    if [[ "$PATH_VALUE" != /* ]]; then
        PATH_VALUE="$REPO_ROOT/$PATH_VALUE"
    fi

    SCENARIOS+=("$NAME=$PATH_VALUE")
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --server)
            SERVER_BIN="$2"
            shift 2
            ;;
        --stubs)
            STUBS_PATH="$2"
            shift 2
            ;;
        --out)
            OUT_DIR="$2"
            shift 2
            ;;
        --timeout)
            TIMEOUT="$2"
            shift 2
            ;;
        --iterations)
            ITERATIONS="$2"
            shift 2
            ;;
        --scenario)
            add_scenario "$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        -*)
            echo "ERROR: unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
        *)
            add_scenario "$1"
            shift
            ;;
    esac
done

if [[ ${#SCENARIOS[@]} -eq 0 ]]; then
    [[ -d "$REPO_ROOT/test-fixtures/lsp-cases" ]] && add_scenario "lsp-cases=$REPO_ROOT/test-fixtures/lsp-cases"
    [[ -d "$REPO_ROOT/test-fixtures/vendor-resolve" ]] && add_scenario "vendor-heavy=$REPO_ROOT/test-fixtures/vendor-resolve"
    [[ -d "$REPO_ROOT/test-fixtures/basic" ]] && add_scenario "small-fixture=$REPO_ROOT/test-fixtures/basic"
fi

if [[ ${#SCENARIOS[@]} -eq 0 ]]; then
    echo "ERROR: no latency benchmark scenarios configured" >&2
    exit 2
fi

if [[ "$SERVER_BIN" != /* ]]; then
    SERVER_BIN="$REPO_ROOT/$SERVER_BIN"
fi
if [[ "$STUBS_PATH" != /* ]]; then
    STUBS_PATH="$REPO_ROOT/$STUBS_PATH"
fi
if [[ "$OUT_DIR" != /* ]]; then
    OUT_DIR="$REPO_ROOT/$OUT_DIR"
fi

if [[ ! -x "$SERVER_BIN" ]]; then
    echo "Server binary not found or not executable at $SERVER_BIN"
    echo "Building host server binary..."
    "$REPO_ROOT/scripts/build-server.sh"
fi

mkdir -p "$OUT_DIR"

echo "Benchmarking php-lsp request latency"
echo "  server:     $SERVER_BIN"
echo "  stubs:      $STUBS_PATH"
echo "  out:        $OUT_DIR"
echo "  timeout:    ${TIMEOUT}s"
echo "  iterations: $ITERATIONS"

for SCENARIO in "${SCENARIOS[@]}"; do
    NAME="${SCENARIO%%=*}"
    WORKSPACE="${SCENARIO#*=}"

    if [[ ! -d "$WORKSPACE" ]]; then
        echo "ERROR: workspace does not exist for scenario '$NAME': $WORKSPACE" >&2
        exit 2
    fi

    echo
    echo "=== Scenario: $NAME ==="
    "$REPO_ROOT/scripts/benchmark-lsp-latency.py" \
        --scenario "$NAME" \
        --workspace "$WORKSPACE" \
        --server "$SERVER_BIN" \
        --stubs "$STUBS_PATH" \
        --out "$OUT_DIR" \
        --timeout "$TIMEOUT" \
        --iterations "$ITERATIONS"
done
