#!/usr/bin/env bash
# Build the php-lsp server binary for a given Rust target.
#
# Usage:
#   ./scripts/build-server.sh                          # build for host
#   ./scripts/build-server.sh x86_64-unknown-linux-gnu # cross-compile for one target
#   ./scripts/build-server.sh --all                    # build for all supported targets
#
# Output: client/bin/<platform>/php-lsp (or php-lsp.exe for Windows)
#
# Platform directory mapping:
#   x86_64-unknown-linux-gnu   → linux-x64
#   aarch64-unknown-linux-gnu  → linux-arm64
#   x86_64-apple-darwin        → darwin-x64
#   aarch64-apple-darwin       → darwin-arm64
#   x86_64-pc-windows-msvc     → win32-x64
#   aarch64-pc-windows-msvc    → win32-arm64

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# Map Rust target triple → VS Code platform directory
target_to_platform() {
    case "$1" in
        x86_64-unknown-linux-gnu)   echo "linux-x64" ;;
        x86_64-unknown-linux-musl)  echo "linux-x64" ;;
        aarch64-unknown-linux-gnu)  echo "linux-arm64" ;;
        x86_64-apple-darwin)        echo "darwin-x64" ;;
        aarch64-apple-darwin)       echo "darwin-arm64" ;;
        x86_64-pc-windows-msvc)     echo "win32-x64" ;;
        aarch64-pc-windows-msvc)    echo "win32-arm64" ;;
        *)
            echo "ERROR: Unknown target '$1'" >&2
            exit 1
            ;;
    esac
}

host_target() {
    rustc -vV | awk '/^host:/ {print $2}'
}

build_one() {
    local TARGET="$1"
    local PLATFORM
    PLATFORM="$(target_to_platform "$TARGET")"

    echo "=== Building php-lsp: $TARGET → bin/$PLATFORM ==="

    cargo build --release --manifest-path "$REPO_ROOT/server/Cargo.toml" --target "$TARGET"

    local BINARY_DIR="$REPO_ROOT/server/target/$TARGET/release"
    local BINARY_NAME="php-lsp"
    if [[ "$TARGET" == *windows* ]]; then
        BINARY_NAME="php-lsp.exe"
    fi

    local SRC_BINARY="$BINARY_DIR/$BINARY_NAME"
    if [[ ! -f "$SRC_BINARY" ]]; then
        echo "ERROR: Binary not found at $SRC_BINARY"
        exit 1
    fi

    local DEST_DIR="$REPO_ROOT/client/bin/$PLATFORM"
    mkdir -p "$DEST_DIR"
    cp "$SRC_BINARY" "$DEST_DIR/$BINARY_NAME"

    # Strip on non-Windows for smaller size
    if [[ "$TARGET" != *windows* ]] && command -v strip &>/dev/null; then
        strip "$DEST_DIR/$BINARY_NAME" 2>/dev/null || true
    fi

    local SIZE
    SIZE=$(du -h "$DEST_DIR/$BINARY_NAME" | cut -f1)
    echo "    → $DEST_DIR/$BINARY_NAME ($SIZE)"
}

if [[ "${1:-}" == "--all" ]]; then
    ALL_TARGETS=(
        x86_64-unknown-linux-gnu
        aarch64-unknown-linux-gnu
        x86_64-apple-darwin
        aarch64-apple-darwin
        x86_64-pc-windows-msvc
        aarch64-pc-windows-msvc
    )
    echo "=== Building for ALL targets ==="
    for t in "${ALL_TARGETS[@]}"; do
        build_one "$t"
    done
    echo "=== Done: all binaries in client/bin/ ==="
elif [[ -n "${1:-}" ]]; then
    build_one "$1"
else
    # Build for host
    HOST="$(host_target)"
    build_one "$HOST"
fi
