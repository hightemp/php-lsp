#!/usr/bin/env bash
# Check the packaged Linux x64 ABI and execute it in the supported Ubuntu floor.
#
# Usage:
#   scripts/smoke-linux-x64-compat.sh BINARY MAX_GLIBC_VERSION [UBUNTU_IMAGE]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BINARY_PATH="${1:-}"
MAX_GLIBC_VERSION="${2:-}"
UBUNTU_IMAGE="${3:-ubuntu:20.04}"

if [[ -z "$BINARY_PATH" || -z "$MAX_GLIBC_VERSION" ]]; then
    echo "Usage: $0 BINARY MAX_GLIBC_VERSION [UBUNTU_IMAGE]" >&2
    exit 2
fi

if ! command -v docker >/dev/null 2>&1; then
    echo "ERROR: required tool not found: docker" >&2
    exit 1
fi

"$SCRIPT_DIR/check-linux-abi.sh" "$BINARY_PATH" "$MAX_GLIBC_VERSION"

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT
install -m 0755 "$BINARY_PATH" "$TMP_DIR/php-lsp"

docker run --rm \
    --network none \
    --platform linux/amd64 \
    --read-only \
    --volume "$TMP_DIR:/opt/php-lsp:ro" \
    "$UBUNTU_IMAGE" \
    /opt/php-lsp/php-lsp --version

echo "Linux x64 compatibility smoke passed in $UBUNTU_IMAGE"
