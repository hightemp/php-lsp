#!/usr/bin/env bash
# Smoke-test a packaged VS Code extension archive before publishing.
#
# Usage:
#   scripts/smoke-vsix.sh path/to/ht-php-lsp.vsix
#
# By default this checks the universal release package platforms. Override with:
#   PHP_LSP_VSIX_PLATFORMS="linux-x64 darwin-arm64" scripts/smoke-vsix.sh ...

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
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
require_entry "extension/package-lock.json"
require_entry "extension/out/extension.js"
require_entry "extension/language-configuration.json"
require_entry "extension/twig-language-configuration.json"
require_one_of "README" "extension/README.md" "extension/readme.md"
require_one_of "LICENSE" "extension/LICENSE" "extension/LICENSE.txt" "extension/license.txt"
require_entry "extension/stubs/PhpStormStubsMap.php"
require_entry "extension/stubs/Core/Core.php"
require_entry "extension/stubs/SPL/SPL.php"
require_entry "extension/stubs/standard/basic.php"
require_entry "extension/stubs/standard/standard_0.php"
require_entry "extension/stubs/Reflection/Reflection.php"
require_entry "extension/stubs/SimpleXML/SimpleXML.php"
require_entry "extension/stubs/soap/soap.php"

MIN_STUB_PHP_FILES="${PHP_LSP_VSIX_MIN_STUB_PHP_FILES:-80}"
STUB_PHP_FILES=$(grep -E '^extension/stubs/.+\.php$' "$CONTENTS" | wc -l | tr -d '[:space:]')
if (( STUB_PHP_FILES < MIN_STUB_PHP_FILES )); then
    echo "ERROR: VSIX contains too few bundled PHP stub files: $STUB_PHP_FILES < $MIN_STUB_PHP_FILES" >&2
    exit 1
fi

read -r -a PLATFORMS <<< "${PHP_LSP_VSIX_PLATFORMS:-linux-x64 linux-arm64 darwin-x64 darwin-arm64 win32-x64 win32-arm64}"
for platform in "${PLATFORMS[@]}"; do
    binary_name="php-lsp"
    if [[ "$platform" == win32-* ]]; then
        binary_name="php-lsp.exe"
    fi
    require_entry "extension/bin/$platform/$binary_name"
done

unzip -q "$VSIX" \
    extension/package.json \
    extension/package-lock.json \
    extension/out/extension.js \
    extension/language-configuration.json \
    extension/twig-language-configuration.json \
    -d "$TMP_DIR"
mkdir -p "$TMP_DIR/extension/node_modules/vscode"
cat > "$TMP_DIR/extension/node_modules/vscode/index.js" <<'JS'
const any = new Proxy(function () {}, {
  get(_target, property) {
    if (property === "then") {
      return undefined;
    }
    if (property === Symbol.iterator) {
      return function* emptyIterator() {};
    }
    if (property === Symbol.toPrimitive) {
      return () => "";
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

const smokeState = {
  errorMessages: [],
  fileWatchersCreated: 0,
  fileWatchersDisposed: 0,
  outputLines: [],
};
const disposable = () => new Disposable();
const configuration = {
  get(key, fallback) {
    if (key === "enable") {
      return true;
    }
    if (key === "serverPath") {
      return process.execPath;
    }
    return fallback;
  },
  inspect() {
    return undefined;
  },
};

const withFallback = (target) => new Proxy(target, {
  get(value, property) {
    if (property in value) {
      return value[property];
    }
    return any;
  },
});

module.exports = new Proxy({
  __phpLspSmoke: smokeState,
  commands: withFallback({
    executeCommand: async () => undefined,
    registerCommand: disposable,
  }),
  Disposable,
  MarkdownString,
  StatusBarAlignment: { Left: 1, Right: 2 },
  ThemeColor,
  version: process.env.PHP_LSP_SMOKE_VSCODE_VERSION ?? "0.0.0",
  Uri: {
    file(fsPath) {
      return { fsPath, scheme: "file", toString: () => `file://${fsPath}` };
    },
  },
  window: withFallback({
    activeTextEditor: undefined,
    createOutputChannel: () => ({
      append(value) { smokeState.outputLines.push(String(value)); },
      appendLine(value) { smokeState.outputLines.push(String(value)); },
      show() {},
      dispose() {},
    }),
    createStatusBarItem: () => ({
      show() {},
      hide() {},
      dispose() {},
    }),
    showErrorMessage: async (message) => {
      smokeState.errorMessages.push(message);
      return undefined;
    },
    showInformationMessage: async () => undefined,
    showQuickPick: async () => undefined,
    showWarningMessage: async () => undefined,
  }),
  workspace: withFallback({
    createFileSystemWatcher: () => {
      smokeState.fileWatchersCreated += 1;
      return {
        dispose() {
          smokeState.fileWatchersDisposed += 1;
        },
        onDidChange: disposable,
        onDidCreate: disposable,
        onDidDelete: disposable,
      };
    },
    getConfiguration: () => configuration,
    onDidChangeConfiguration: disposable,
    notebookDocuments: [],
    textDocuments: [],
    workspaceFolders: [],
  }),
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
const childProcess = require("child_process");
const { EventEmitter } = require("events");
const path = require("path");
const { PassThrough } = require("stream");

const extensionRoot = process.argv[2];
const packageJson = require(path.join(extensionRoot, "package.json"));
const packageLock = require(path.join(extensionRoot, "package-lock.json"));

assert.strictEqual(packageJson.main, "./out/extension.js", "package.json main must point at bundled extension.js");
assert.strictEqual(packageLock.packages?.[""]?.engines?.vscode, packageJson.engines?.vscode, "package-lock.json must preserve package.json engines.vscode");
assert.strictEqual(packageJson.engines?.vscode, packageLock.packages?.["node_modules/vscode-languageclient"]?.engines?.vscode, "extension and vscode-languageclient VS Code engine ranges must agree");
assert(Array.isArray(packageJson.activationEvents), "package.json activationEvents must be an array");
assert(packageJson.activationEvents.includes("onLanguage:php"), "extension must activate for PHP files");
assert(packageJson.contributes?.commands?.some((command) => command.command === "phpLsp.restartServer"), "restart command must be contributed");
assert(packageJson.contributes?.commands?.some((command) => command.command === "phpLsp.clearCacheAndRestart"), "clear cache command must be contributed");

const minimumVersionMatch = packageJson.engines.vscode.match(/\d+\.\d+\.\d+/);
assert(minimumVersionMatch, "engines.vscode must contain a minimum version");
process.env.PHP_LSP_SMOKE_VSCODE_VERSION = minimumVersionMatch[0];

for (const language of packageJson.contributes?.languages ?? []) {
  if (!language.configuration) {
    continue;
  }
  const relativeConfiguration = language.configuration.replace(/^\.\//, "");
  const configurationPath = path.join(extensionRoot, relativeConfiguration);
  assert.doesNotThrow(
    () => JSON.parse(require("fs").readFileSync(configurationPath, "utf8")),
    `language configuration for ${language.id} must be packaged valid JSON`,
  );
}

const extensionModule = require(path.join(extensionRoot, "out", "extension.js"));
assert.strictEqual(typeof extensionModule.activate, "function", "extension.js must export activate()");
assert.strictEqual(typeof extensionModule.deactivate, "function", "extension.js must export deactivate()");

const context = {
  extension: {
    packageJSON: packageJson,
  },
  extensionPath: extensionRoot,
  asAbsolutePath(relativePath) {
    return path.join(extensionRoot, relativePath);
  },
  subscriptions: [],
};

const protocolState = {
  exitNotifications: 0,
  initializeRequests: 0,
  initializedNotifications: 0,
  shutdownRequests: 0,
  spawnCalls: 0,
};

function waitFor(predicate, description, timeoutMs = 5000) {
  const deadline = Date.now() + timeoutMs;
  return new Promise((resolve, reject) => {
    const poll = () => {
      if (predicate()) {
        resolve();
      } else if (Date.now() >= deadline) {
        reject(new Error(`Timed out waiting for ${description}`));
      } else {
        setTimeout(poll, 10);
      }
    };
    poll();
  });
}

function writeProtocolMessage(stream, message) {
  const payload = Buffer.from(JSON.stringify(message), "utf8");
  stream.write(Buffer.concat([
    Buffer.from(`Content-Length: ${payload.length}\r\n\r\n`, "ascii"),
    payload,
  ]));
}

function createMockLanguageServerProcess(command) {
  assert.strictEqual(command, process.execPath, "LanguageClient must launch the configured smoke server executable");
  protocolState.spawnCalls += 1;

  const serverProcess = new EventEmitter();
  const stdin = new PassThrough();
  const stdout = new PassThrough();
  const stderr = new PassThrough();
  let input = Buffer.alloc(0);

  Object.assign(serverProcess, {
    exitCode: null,
    killed: false,
    pid: Number.MAX_SAFE_INTEGER,
    signalCode: null,
    stderr,
    stdin,
    stdio: [stdin, stdout, stderr],
    stdout,
    kill() {
      this.killed = true;
      this.signalCode = "SIGTERM";
      this.emit("exit", null, this.signalCode);
      return true;
    },
  });

  const handleMessage = (message) => {
    if (message.method === "initialize" && message.id !== undefined) {
      protocolState.initializeRequests += 1;
      writeProtocolMessage(stdout, {
        jsonrpc: "2.0",
        id: message.id,
        result: {
          capabilities: {},
          serverInfo: { name: "php-lsp-vsix-smoke", version: "0.0.0" },
        },
      });
    } else if (message.method === "initialized") {
      protocolState.initializedNotifications += 1;
    } else if (message.method === "shutdown" && message.id !== undefined) {
      protocolState.shutdownRequests += 1;
      writeProtocolMessage(stdout, { jsonrpc: "2.0", id: message.id, result: null });
    } else if (message.method === "exit") {
      protocolState.exitNotifications += 1;
      serverProcess.exitCode = 0;
      serverProcess.killed = true;
    }
  };

  stdin.on("data", (chunk) => {
    input = Buffer.concat([input, chunk]);
    while (true) {
      const headerEnd = input.indexOf("\r\n\r\n");
      if (headerEnd < 0) {
        return;
      }
      const header = input.subarray(0, headerEnd).toString("ascii");
      const contentLengthMatch = /^Content-Length:\s*(\d+)$/im.exec(header);
      assert(contentLengthMatch, `LanguageClient emitted a message without Content-Length: ${header}`);
      const contentLength = Number(contentLengthMatch[1]);
      const messageEnd = headerEnd + 4 + contentLength;
      if (input.length < messageEnd) {
        return;
      }
      const payload = input.subarray(headerEnd + 4, messageEnd).toString("utf8");
      input = input.subarray(messageEnd);
      handleMessage(JSON.parse(payload));
    }
  });

  return serverProcess;
}

async function runActivationSmoke() {
  const originalSpawn = childProcess.spawn;
  const vscode = require(path.join(extensionRoot, "node_modules", "vscode"));
  childProcess.spawn = createMockLanguageServerProcess;
  try {
    extensionModule.activate(context);
    await waitFor(
      () => protocolState.initializedNotifications === 1
        && vscode.__phpLspSmoke.outputLines.some((line) => line.includes("Started language server:")),
      "LanguageClient.start() completion at the declared VS Code floor",
    );

    assert.strictEqual(protocolState.spawnCalls, 1, "activation must call LanguageClient.start() exactly once");
    assert.strictEqual(protocolState.initializeRequests, 1, "LanguageClient.start() must complete the initialize request");

    assert(vscode.__phpLspSmoke.fileWatchersCreated > 0, "LanguageClient construction must create file watchers");
    assert.deepStrictEqual(vscode.__phpLspSmoke.errorMessages, [], "activation must not report compatibility errors");

    await extensionModule.deactivate();
    assert.strictEqual(protocolState.shutdownRequests, 1, "deactivation must send shutdown to the started client");
    assert.strictEqual(protocolState.exitNotifications, 1, "deactivation must send exit to the started client");
    assert.strictEqual(
      vscode.__phpLspSmoke.fileWatchersDisposed,
      vscode.__phpLspSmoke.fileWatchersCreated,
      "deactivation must dispose every watcher created for the LanguageClient",
    );
  } finally {
    childProcess.spawn = originalSpawn;
  }
}

runActivationSmoke().then(() => {
  console.log(`VSIX smoke test passed at declared VS Code minimum ${process.env.PHP_LSP_SMOKE_VSCODE_VERSION}`);
}).catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
NODE

if [[ "$(uname -s)" == "Linux" ]] && grep -Fxq "extension/bin/linux-x64/php-lsp" "$CONTENTS"; then
    unzip -q "$VSIX" extension/bin/linux-x64/php-lsp -d "$TMP_DIR"
    chmod +x "$TMP_DIR/extension/bin/linux-x64/php-lsp"
    "$REPO_ROOT/scripts/smoke-cli.sh" \
        "$TMP_DIR/extension/bin/linux-x64/php-lsp" \
        "$REPO_ROOT/test-fixtures/basic"
else
    echo "Skipping packaged binary CLI smoke on $(uname -s)"
fi
