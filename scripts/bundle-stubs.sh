#!/usr/bin/env bash
# Copy phpstorm-stubs into the extension bundle.
# Copies every real stub extension directory used by the default server config.
#
# Usage:
#   ./scripts/bundle-stubs.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
STUBS_SRC="$REPO_ROOT/server/data/stubs"
STUBS_DEST="$REPO_ROOT/client/stubs"

if [[ ! -d "$STUBS_SRC" ]]; then
    echo "ERROR: phpstorm-stubs not found at $STUBS_SRC" >&2
    echo "Run: git submodule update --init --recursive" >&2
    exit 1
fi

is_stub_extension_dir() {
    local dir="$1"
    local name
    name="$(basename "$dir")"

    if [[ "$name" == .* || "$name" == "meta" || "$name" == "tests" || "$name" == "vendor" ]]; then
        return 1
    fi

    [[ -n "$(find "$dir" -type f -name '*.php' -print -quit)" ]]
}

echo "=== Bundling phpstorm-stubs ==="

"$REPO_ROOT/scripts/check-stubs.sh" --kind source "$STUBS_SRC"

rm -rf "$STUBS_DEST"
mkdir -p "$STUBS_DEST"

COUNT=0
while IFS= read -r -d '' SRC_DIR; do
    if is_stub_extension_dir "$SRC_DIR"; then
        ext="$(basename "$SRC_DIR")"
        cp -r "$SRC_DIR" "$STUBS_DEST/$ext"
        COUNT=$((COUNT + 1))
    fi
done < <(find "$STUBS_SRC" -mindepth 1 -maxdepth 1 -type d -print0 | sort -z)

# Copy meta files needed by the loader
for f in PhpStormStubsMap.php; do
    if [[ -f "$STUBS_SRC/$f" ]]; then
        cp "$STUBS_SRC/$f" "$STUBS_DEST/"
    fi
done

"$REPO_ROOT/scripts/check-stubs.sh" --kind bundled "$STUBS_DEST"

STUBS_SIZE=$(du -sh "$STUBS_DEST" | cut -f1)
echo "=== Bundled $COUNT extensions ($STUBS_SIZE) ==="
