#!/usr/bin/env bash
# Smoke-test a packaged VS Code extension archive before publishing.
#
# Usage:
#   scripts/smoke-vsix.sh path/to/ht-php-lsp.vsix
#
# By default this checks the universal release package platforms. Override with:
#   PHP_LSP_VSIX_PLATFORMS="linux-x64 darwin-arm64" scripts/smoke-vsix.sh ...

set -euo pipefail

VSIX="${1:-}"

if [[ -z "$VSIX" ]]; then
    echo "Usage: $0 path/to/extension.vsix" >&2
    exit 2
fi

if [[ ! -f "$VSIX" ]]; then
    echo "ERROR: VSIX not found: $VSIX" >&2
    exit 1
fi

for tool in unzip node grep mktemp; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "ERROR: required tool not found: $tool" >&2
        exit 1
    fi
done

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

CONTENTS="$TMP_DIR/contents.txt"
unzip -Z1 "$VSIX" > "$CONTENTS"

require_entry() {
    local entry="$1"
    if ! grep -Fxq "$entry" "$CONTENTS"; then
        echo "ERROR: VSIX is missing required entry: $entry" >&2
        exit 1
    fi
}

require_one_of() {
    local label="$1"
    shift
    local entry
    for entry in "$@"; do
        if grep -Fxq "$entry" "$CONTENTS"; then
            return 0
        fi
    done
    echo "ERROR: VSIX is missing required entry group: $label" >&2
    printf '  expected one of:\n' >&2
    printf '    %s\n' "$@" >&2
    exit 1
}

require_entry "extension/package.json"
require_entry "extension/out/extension.js"
require_one_of "README" "extension/README.md" "extension/readme.md"
require_one_of "LICENSE" "extension/LICENSE" "extension/LICENSE.txt" "extension/license.txt"
require_entry "extension/stubs/PhpStormStubsMap.php"
require_entry "extension/stubs/Core/Core.php"

read -r -a PLATFORMS <<< "${PHP_LSP_VSIX_PLATFORMS:-linux-x64 linux-arm64 darwin-x64 darwin-arm64 win32-x64 win32-arm64}"
for platform in "${PLATFORMS[@]}"; do
    binary_name="php-lsp"
    if [[ "$platform" == win32-* ]]; then
        binary_name="php-lsp.exe"
    fi
    require_entry "extension/bin/$platform/$binary_name"
done

unzip -q "$VSIX" extension/package.json extension/out/extension.js -d "$TMP_DIR"
mkdir -p "$TMP_DIR/extension/node_modules/vscode"
cat > "$TMP_DIR/extension/node_modules/vscode/index.js" <<'JS'
const any = new Proxy(function () {}, {
  get(_target, property) {
    if (property === "then") {
      return undefined;
    }
    return any;
  },
  apply() {
    return any;
  },
  construct() {
    return any;
  },
});

class Disposable {
  constructor(callOnDispose) {
    this.callOnDispose = callOnDispose;
  }

  dispose() {
    if (typeof this.callOnDispose === "function") {
      this.callOnDispose();
    }
  }

  static from(...items) {
    return new Disposable(() => {
      for (const item of items) {
        if (item && typeof item.dispose === "function") {
          item.dispose();
        }
      }
    });
  }

  static create(callOnDispose) {
    return new Disposable(callOnDispose);
  }
}

class MarkdownString {
  constructor(value = "") {
    this.value = value;
  }

  appendMarkdown(value) {
    this.value += value;
    return this;
  }

  appendText(value) {
    this.value += value;
    return this;
  }
}

class ThemeColor {
  constructor(id) {
    this.id = id;
  }
}

const disposable = () => new Disposable();
const configuration = {
  get(key, fallback) {
    if (key === "enable") {
      return false;
    }
    return fallback;
  },
};

module.exports = new Proxy({
  commands: {
    executeCommand: async () => undefined,
    registerCommand: disposable,
  },
  Disposable,
  MarkdownString,
  StatusBarAlignment: { Left: 1, Right: 2 },
  ThemeColor,
  Uri: {
    file(fsPath) {
      return { fsPath, scheme: "file", toString: () => `file://${fsPath}` };
    },
  },
  window: {
    createOutputChannel: () => ({ appendLine() {}, show() {}, dispose() {} }),
    createStatusBarItem: () => ({
      show() {},
      hide() {},
      dispose() {},
    }),
    showErrorMessage: async () => undefined,
    showInformationMessage: async () => undefined,
    showQuickPick: async () => undefined,
    showWarningMessage: async () => undefined,
  },
  workspace: {
    createFileSystemWatcher: disposable,
    getConfiguration: () => configuration,
    onDidChangeConfiguration: disposable,
    workspaceFolders: [],
  },
}, {
  get(target, property) {
    if (property in target) {
      return target[property];
    }
    return any;
  },
});
JS

node - "$TMP_DIR/extension" <<'NODE'
const assert = require("assert");
const path = require("path");

const extensionRoot = process.argv[2];
const packageJson = require(path.join(extensionRoot, "package.json"));

assert.strictEqual(packageJson.main, "./out/extension.js", "package.json main must point at bundled extension.js");
assert(Array.isArray(packageJson.activationEvents), "package.json activationEvents must be an array");
assert(packageJson.activationEvents.includes("onLanguage:php"), "extension must activate for PHP files");
assert(packageJson.contributes?.commands?.some((command) => command.command === "phpLsp.restartServer"), "restart command must be contributed");
assert(packageJson.contributes?.commands?.some((command) => command.command === "phpLsp.clearCacheAndRestart"), "clear cache command must be contributed");

const extensionModule = require(path.join(extensionRoot, "out", "extension.js"));
assert.strictEqual(typeof extensionModule.activate, "function", "extension.js must export activate()");
assert.strictEqual(typeof extensionModule.deactivate, "function", "extension.js must export deactivate()");

const context = {
  asAbsolutePath(relativePath) {
    return path.join(extensionRoot, relativePath);
  },
  subscriptions: [],
};

extensionModule.activate(context);
Promise.resolve(extensionModule.deactivate()).then(() => {
  console.log("VSIX smoke test passed");
});
NODE
