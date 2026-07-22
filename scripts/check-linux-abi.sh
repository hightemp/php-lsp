#!/usr/bin/env bash
# Verify that a Linux ELF binary does not require a newer glibc than allowed.
#
# Usage:
#   scripts/check-linux-abi.sh path/to/php-lsp MAX_GLIBC_VERSION

set -euo pipefail

BINARY_PATH="${1:-}"
MAX_GLIBC_VERSION="${2:-}"

if [[ -z "$BINARY_PATH" || -z "$MAX_GLIBC_VERSION" ]]; then
    echo "Usage: $0 path/to/php-lsp MAX_GLIBC_VERSION" >&2
    exit 2
fi

if [[ ! -f "$BINARY_PATH" ]]; then
    echo "ERROR: Linux binary not found: $BINARY_PATH" >&2
    exit 1
fi

if [[ ! "$MAX_GLIBC_VERSION" =~ ^[0-9]+\.[0-9]+([.][0-9]+)?$ ]]; then
    echo "ERROR: invalid maximum glibc version: $MAX_GLIBC_VERSION" >&2
    exit 2
fi

for tool in readelf sort; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "ERROR: required tool not found: $tool" >&2
        exit 1
    fi
done

if ! LANG=C readelf -h "$BINARY_PATH" | grep -Fq "ELF"; then
    echo "ERROR: expected a Linux ELF binary: $BINARY_PATH" >&2
    exit 1
fi

GLIBC_VERSIONS="$({
    LANG=C readelf -W --version-info --dyn-syms "$BINARY_PATH" \
        | grep -oE 'GLIBC_[0-9]+([.][0-9]+)+' \
        | sed 's/^GLIBC_//' \
        | sort -Vu
} || true)"

if [[ -z "$GLIBC_VERSIONS" ]]; then
    echo "ERROR: no versioned glibc symbols found in $BINARY_PATH" >&2
    exit 1
fi

REQUIRED_GLIBC_VERSION="$(printf '%s\n' "$GLIBC_VERSIONS" | tail -n 1)"
HIGHEST_VERSION="$(printf '%s\n%s\n' "$MAX_GLIBC_VERSION" "$REQUIRED_GLIBC_VERSION" | sort -Vu | tail -n 1)"

if [[ "$HIGHEST_VERSION" != "$MAX_GLIBC_VERSION" ]]; then
    echo "ERROR: $BINARY_PATH requires glibc $REQUIRED_GLIBC_VERSION, newer than allowed $MAX_GLIBC_VERSION" >&2
    exit 1
fi

echo "Linux ABI OK: $BINARY_PATH requires glibc <= $REQUIRED_GLIBC_VERSION (allowed <= $MAX_GLIBC_VERSION)"
