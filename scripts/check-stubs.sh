#!/usr/bin/env bash
# Validate that a phpstorm-stubs tree is usable for development, CI, and VSIX packaging.
#
# Usage:
#   scripts/check-stubs.sh [--kind source|bundled] PATH

set -euo pipefail

KIND="stubs"
MIN_PHP_FILES="${PHP_LSP_STUBS_MIN_PHP_FILES:-80}"
STUBS_PATH=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --kind)
            if [[ $# -lt 2 ]]; then
                echo "ERROR: --kind requires a value" >&2
                exit 2
            fi
            KIND="$2"
            shift 2
            ;;
        --min-php-files)
            if [[ $# -lt 2 ]]; then
                echo "ERROR: --min-php-files requires a value" >&2
                exit 2
            fi
            MIN_PHP_FILES="$2"
            shift 2
            ;;
        -h|--help)
            echo "Usage: $0 [--kind source|bundled] [--min-php-files N] PATH"
            exit 0
            ;;
        -*)
            echo "ERROR: unknown option: $1" >&2
            exit 2
            ;;
        *)
            if [[ -n "$STUBS_PATH" ]]; then
                echo "ERROR: unexpected extra argument: $1" >&2
                exit 2
            fi
            STUBS_PATH="$1"
            shift
            ;;
    esac
done

if [[ -z "$STUBS_PATH" ]]; then
    echo "Usage: $0 [--kind source|bundled] [--min-php-files N] PATH" >&2
    exit 2
fi

if ! [[ "$MIN_PHP_FILES" =~ ^[0-9]+$ ]]; then
    echo "ERROR: --min-php-files must be a non-negative integer: $MIN_PHP_FILES" >&2
    exit 2
fi

case "$KIND" in
    source|bundled|stubs) ;;
    *)
        echo "ERROR: --kind must be source, bundled, or stubs: $KIND" >&2
        exit 2
        ;;
esac

if [[ ! -d "$STUBS_PATH" ]]; then
    echo "ERROR: $KIND phpstorm-stubs directory not found: $STUBS_PATH" >&2
    if [[ "$KIND" == "source" ]]; then
        echo "Run: git submodule update --init --recursive" >&2
    elif [[ "$KIND" == "bundled" ]]; then
        echo "Run: scripts/bundle-stubs.sh" >&2
    fi
    exit 1
fi

REQUIRED_FILES=(
    "PhpStormStubsMap.php"
    "Core/Core.php"
    "SPL/SPL.php"
    "standard/basic.php"
    "standard/standard_0.php"
    "date/date.php"
    "json/json.php"
    "pcre/pcre.php"
    "Reflection/Reflection.php"
    "SimpleXML/SimpleXML.php"
    "soap/soap.php"
)

MISSING=0
for relative in "${REQUIRED_FILES[@]}"; do
    if [[ ! -f "$STUBS_PATH/$relative" ]]; then
        echo "ERROR: $KIND phpstorm-stubs is missing required file: $relative" >&2
        MISSING=1
    fi
done

PHP_FILES=$(find "$STUBS_PATH" -type f -name '*.php' | wc -l | tr -d '[:space:]')
if (( PHP_FILES < MIN_PHP_FILES )); then
    echo "ERROR: $KIND phpstorm-stubs has too few PHP files: $PHP_FILES < $MIN_PHP_FILES" >&2
    MISSING=1
fi

if (( MISSING != 0 )); then
    if [[ "$KIND" == "source" ]]; then
        echo "The phpstorm-stubs submodule is missing or not initialized correctly." >&2
        echo "Run: git submodule update --init --recursive" >&2
    elif [[ "$KIND" == "bundled" ]]; then
        echo "The bundled VS Code stubs are incomplete." >&2
        echo "Run: scripts/bundle-stubs.sh" >&2
    fi
    exit 1
fi

echo "OK: $KIND phpstorm-stubs at $STUBS_PATH ($PHP_FILES PHP files)"
