#!/usr/bin/env bash
# Copy phpstorm-stubs into the extension bundle.
# Only copies the stub extensions that are enabled by default.
#
# Usage:
#   ./scripts/bundle-stubs.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
STUBS_SRC="$REPO_ROOT/server/data/stubs"
STUBS_DEST="$REPO_ROOT/client/stubs"

if [[ ! -d "$STUBS_SRC" ]]; then
    echo "WARNING: phpstorm-stubs not found at $STUBS_SRC"
    echo "Run: git submodule update --init --recursive"
    exit 0
fi

# Default extensions to bundle (matches package.json defaults)
DEFAULT_EXTENSIONS=(
    Core SPL standard pcre date json
    mbstring ctype tokenizer dom SimpleXML
    PDO curl filter hash session
    Reflection intl fileinfo openssl phar
    xml xmlreader xmlwriter zip zlib
    bcmath gd iconv mysqli sodium
)

echo "=== Bundling phpstorm-stubs ==="

rm -rf "$STUBS_DEST"
mkdir -p "$STUBS_DEST"

COUNT=0
for ext in "${DEFAULT_EXTENSIONS[@]}"; do
    SRC_DIR="$STUBS_SRC/$ext"
    if [[ -d "$SRC_DIR" ]]; then
        cp -r "$SRC_DIR" "$STUBS_DEST/$ext"
        COUNT=$((COUNT + 1))
    else
        echo "  skip: $ext (not found)"
    fi
done

# Copy meta files needed by the loader
for f in PhpStormStubsMap.php; do
    if [[ -f "$STUBS_SRC/$f" ]]; then
        cp "$STUBS_SRC/$f" "$STUBS_DEST/"
    fi
done

STUBS_SIZE=$(du -sh "$STUBS_DEST" | cut -f1)
echo "=== Bundled $COUNT extensions ($STUBS_SIZE) ==="
