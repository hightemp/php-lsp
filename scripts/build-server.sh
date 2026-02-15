#!/usr/bin/env bash
# Build the php-lsp server binary for a given Rust target.
#
# Usage:
#   ./scripts/build-server.sh                          # build for host
#   ./scripts/build-server.sh x86_64-unknown-linux-gnu # cross-compile
#
# Output: client/bin/php-lsp (or php-lsp.exe on Windows targets)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="${1:-}"

echo "=== Building php-lsp server ==="

BUILD_ARGS=(--release --manifest-path "$REPO_ROOT/server/Cargo.toml")
if [[ -n "$TARGET" ]]; then
    BUILD_ARGS+=(--target "$TARGET")
    echo "Target: $TARGET"
else
    echo "Target: host (native)"
fi

cargo build "${BUILD_ARGS[@]}"

# Determine binary location
if [[ -n "$TARGET" ]]; then
    BINARY_DIR="$REPO_ROOT/server/target/$TARGET/release"
else
    BINARY_DIR="$REPO_ROOT/server/target/release"
fi

# Determine binary name
case "${TARGET:-$(rustc -vV | awk '/^host:/ {print $2}')}" in
    *windows*)
        BINARY_NAME="php-lsp.exe"
        ;;
    *)
        BINARY_NAME="php-lsp"
        ;;
esac

SRC_BINARY="$BINARY_DIR/$BINARY_NAME"

if [[ ! -f "$SRC_BINARY" ]]; then
    echo "ERROR: Binary not found at $SRC_BINARY"
    exit 1
fi

# Copy to client/bin/
DEST_DIR="$REPO_ROOT/client/bin"
mkdir -p "$DEST_DIR"
cp "$SRC_BINARY" "$DEST_DIR/$BINARY_NAME"

# Strip binary on non-Windows targets for smaller size
case "${TARGET:-$(rustc -vV | awk '/^host:/ {print $2}')}" in
    *windows*) ;;
    *)
        if command -v strip &>/dev/null; then
            strip "$DEST_DIR/$BINARY_NAME" 2>/dev/null || true
        fi
        ;;
esac

FINAL_SIZE=$(du -h "$DEST_DIR/$BINARY_NAME" | cut -f1)
echo "=== Binary: $DEST_DIR/$BINARY_NAME ($FINAL_SIZE) ==="
