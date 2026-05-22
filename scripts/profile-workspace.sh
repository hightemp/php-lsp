#!/usr/bin/env bash
# Run php-lsp production profiling scenarios and write JSON metrics.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SERVER_BIN="${PHP_LSP_PROFILE_SERVER:-$REPO_ROOT/client/bin/linux-x64/php-lsp}"
STUBS_PATH="${PHP_LSP_PROFILE_STUBS:-$REPO_ROOT/client/stubs}"
OUT_DIR="${PHP_LSP_PROFILE_OUT:-$REPO_ROOT/target/php-lsp-profile}"
TIMEOUT="${PHP_LSP_PROFILE_TIMEOUT:-120}"

SCENARIOS=()

usage() {
    cat <<'USAGE'
Usage:
  scripts/profile-workspace.sh [options]
  scripts/profile-workspace.sh --scenario name=/path/to/project
  scripts/profile-workspace.sh /path/to/project

Options:
  --server PATH          php-lsp server binary (default: client/bin/linux-x64/php-lsp)
  --stubs PATH           bundled stubs directory (default: client/stubs)
  --out DIR              output directory (default: target/php-lsp-profile)
  --timeout SECONDS      per-scenario timeout (default: 120)
  --scenario NAME=PATH   add a named scenario; may be passed multiple times
  -h, --help             show this help

Default scenarios:
  small-fixture          test-fixtures/basic
  composer-psr4          test-fixtures/composer-psr4
  vendor-heavy           test-fixtures/vendor-resolve

Output:
  One JSON file per scenario in target/php-lsp-profile/*.json.
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
    [[ -d "$REPO_ROOT/test-fixtures/basic" ]] && add_scenario "small-fixture=$REPO_ROOT/test-fixtures/basic"
    [[ -d "$REPO_ROOT/test-fixtures/composer-psr4" ]] && add_scenario "composer-psr4=$REPO_ROOT/test-fixtures/composer-psr4"
    [[ -d "$REPO_ROOT/test-fixtures/vendor-resolve" ]] && add_scenario "vendor-heavy=$REPO_ROOT/test-fixtures/vendor-resolve"
fi

if [[ ${#SCENARIOS[@]} -eq 0 ]]; then
    echo "ERROR: no profiling scenarios configured" >&2
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

echo "Profiling php-lsp"
echo "  server:  $SERVER_BIN"
echo "  stubs:   $STUBS_PATH"
echo "  out:     $OUT_DIR"
echo "  timeout: ${TIMEOUT}s"

for SCENARIO in "${SCENARIOS[@]}"; do
    NAME="${SCENARIO%%=*}"
    WORKSPACE="${SCENARIO#*=}"

    if [[ ! -d "$WORKSPACE" ]]; then
        echo "ERROR: workspace does not exist for scenario '$NAME': $WORKSPACE" >&2
        exit 2
    fi

    echo
    echo "=== Scenario: $NAME ==="
    "$REPO_ROOT/scripts/profile-workspace.py" \
        --scenario "$NAME" \
        --workspace "$WORKSPACE" \
        --server "$SERVER_BIN" \
        --stubs "$STUBS_PATH" \
        --out "$OUT_DIR" \
        --timeout "$TIMEOUT"
done
