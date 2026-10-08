import assert from "node:assert/strict";
import { createRequire } from "node:module";
import path from "node:path";
import vm from "node:vm";
import * as esbuild from "esbuild";

const require = createRequire(import.meta.url);
const result = await esbuild.build({
  entryPoints: ["src/extension.ts"], bundle: true, format: "cjs", platform: "node",
  write: false, logLevel: "silent", external: ["vscode", "vscode-languageclient/node"],
});

async function checkActivation(startupEnvironment) {
  let changed;
  let started;
  const activation = new Promise((resolve) => { started = resolve; });
  const explicit = new Map([["logLevel", "debug"]]);
  const configuration = {
    get(key, fallback) {
      if (key === "serverPath") return process.execPath;
      return explicit.get(key) ?? fallback;
    },
    inspect(key) {
      return explicit.has(key) ? { workspaceValue: explicit.get(key) } : undefined;
    },
  };
  const state = { starts: 0, stops: 0, notifications: [], clients: [], commands: new Map(), popup: [] };
  const disposable = () => ({ dispose() {} });
  class MarkdownString {
    appendMarkdown() { return this; }
  }
  class LanguageClient {
    constructor(_id, _name, serverOptions, clientOptions) {
      this.serverOptions = serverOptions;
      this.clientOptions = clientOptions;
      this.running = false;
      state.clients.push(this);
    }
    onNotification() { return disposable(); }
    async start() { state.starts += 1; this.running = true; }
    isRunning() { return this.running; }
    async sendNotification(method, params) { state.notifications.push({ method, params }); }
    async stop() { state.stops += 1; this.running = false; }
    async dispose() {}
  }
  const vscode = {
    workspace: {
      workspaceFolders: [],
      getConfiguration: () => configuration,
      createFileSystemWatcher: disposable,
      onDidChangeConfiguration(handler) { changed = handler; return disposable(); },
      onDidChangeWorkspaceFolders: disposable,
    },
    commands: { registerCommand(name, handler) { state.commands.set(name, handler); return disposable(); } },
    env: {},
    window: {
      createStatusBarItem: () => ({ show() {}, dispose() {} }),
      createOutputChannel: () => ({
        appendLine(line) { if (line.includes("Lifecycle complete: extension activation")) started(); },
        dispose() {},
      }),
      onDidChangeActiveTextEditor: disposable,
      async showQuickPick(items) { state.popup = items; return undefined; },
      showErrorMessage(message) { throw new Error(message); },
    },
    StatusBarAlignment: { Left: 1 }, MarkdownString, ThemeColor: class {},
  };
  const module = { exports: {} };
  const environment = { ...startupEnvironment };
  vm.runInNewContext(result.outputFiles[0].text, {
    console, module, exports: module.exports,
    process: { env: environment, pid: process.pid },
    require(name) {
      if (name === "vscode") return vscode;
      if (name === "vscode-languageclient/node") return { LanguageClient, TransportKind: { stdio: 0 } };
      return require(name);
    },
  }, { filename: "extension.logging.bundle.cjs" });
  const context = {
    extension: { packageJSON: { version: "test" } }, subscriptions: [],
    asAbsolutePath: (relative) => path.join(process.cwd(), "out", "logging-fixture", relative),
  };
  module.exports.activate(context);
  let timer;
  try {
    await Promise.race([activation, new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error("activation deadline")), 5000);
    })]);
    assert.equal(state.clients.length, 1);
    const active = state.clients[0];
    assert.equal(active.serverOptions.run.options.env.RUST_LOG, startupEnvironment.RUST_LOG,
      "startup logLevel must not replace the inherited RUST_LOG");
    assert.equal(active.serverOptions.debug.options.env.RUST_LOG, startupEnvironment.RUST_LOG);
    assert.deepEqual(environment, startupEnvironment, "activation mutated parent environment");
    assert.equal(active.clientOptions.initializationOptions.global.logLevel, "debug");
    await state.commands.get("phpLsp.showStatus")();
    assert.equal(state.popup.find((item) => item.label.endsWith("Log level")).description, "debug");

    explicit.set("logLevel", "error");
    await changed({ affectsConfiguration: () => true });
    assert.equal(state.notifications.at(-1).method, "workspace/didChangeConfiguration");
    assert.equal(state.notifications.at(-1).params.settings.global.logLevel, "error");
    await state.commands.get("phpLsp.showStatus")();
    assert.equal(state.popup.find((item) => item.label.endsWith("Log level")).description, "error");
    explicit.delete("logLevel");
    await changed({ affectsConfiguration: () => true });
    assert.equal("logLevel" in state.notifications.at(-1).params.settings.global, false,
      "removal must omit the override instead of materializing the info default");
    await state.commands.get("phpLsp.showStatus")();
    assert.equal(state.popup.find((item) => item.label.endsWith("Log level")).description,
      "Inherited startup filter", "reset popup must not report info as the active level");
    assert.equal(state.starts, 1, "log-level changes restarted the client");
    assert.equal(state.stops, 0);
    assert.equal(state.clients[0], active);
  } finally {
    clearTimeout(timer);
    await module.exports.deactivate();
  }
  assert.equal(state.stops, 1, "deactivation must stop the original client once");
}

await checkActivation({ RUST_LOG: "error,custom=trace" });
await checkActivation({});
console.log("logging startup environment, live changes, reset and client identity OK");
