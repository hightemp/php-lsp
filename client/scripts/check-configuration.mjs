import assert from "node:assert/strict";
import vm from "node:vm";
import * as esbuild from "esbuild";

const result = await esbuild.build({
  entryPoints: ["src/configuration.ts"],
  bundle: true,
  format: "cjs",
  platform: "node",
  write: false,
  logLevel: "silent",
});

const module = { exports: {} };
vm.runInNewContext(result.outputFiles[0].text, {
  console,
  module,
  exports: module.exports,
}, {
  filename: "configuration.bundle.cjs",
});

const { buildExplicitClientSettings } = module.exports;

class FakeConfiguration {
  constructor(defaults = {}) {
    this.defaults = new Map(Object.entries(defaults));
    this.explicit = new Map();
  }

  set(key, value, scope = "workspaceValue") {
    this.explicit.set(key, { scope, value });
  }

  reset(key) {
    this.explicit.delete(key);
  }

  inspect(key) {
    const explicit = this.explicit.get(key);
    if (!explicit && !this.defaults.has(key)) {
      return undefined;
    }
    return {
      defaultValue: this.defaults.get(key),
      ...(explicit ? { [explicit.scope]: explicit.value } : {}),
    };
  }

  get(key, fallback) {
    return this.explicit.get(key)?.value ?? this.defaults.get(key) ?? fallback;
  }
}

const config = new FakeConfiguration({
  phpVersion: "8.2",
  "diagnostics.mode": "basic-semantic",
  indexVendor: true,
  "phpstan.enabled": false,
});

assert.deepEqual(
  JSON.parse(JSON.stringify(buildExplicitClientSettings(config, undefined))),
  {},
  "schema defaults must not be materialized as client overrides",
);

config.set("phpVersion", "7.4", "globalValue");
config.set("diagnostics.mode", "off", "workspaceLanguageValue");
config.set("indexVendor", false, "workspaceFolderValue");
config.set("stubs.extensions", [], "workspaceValue");
let snapshot = JSON.parse(JSON.stringify(buildExplicitClientSettings(config, "/bundled/stubs")));
assert.deepEqual(snapshot, {
  phpVersion: "7.4",
  diagnosticsMode: "off",
  indexVendor: false,
  stubExtensions: [],
  bundledStubsPath: "/bundled/stubs",
});

config.reset("phpVersion");
config.reset("diagnostics.mode");
snapshot = JSON.parse(JSON.stringify(buildExplicitClientSettings(config, "/bundled/stubs")));
assert.equal("phpVersion" in snapshot, false, "reset override must disappear from snapshot");
assert.equal("diagnosticsMode" in snapshot, false, "language override reset must disappear");
assert.equal(snapshot.indexVendor, false, "unrelated explicit override must remain");
assert.deepEqual(snapshot.stubExtensions, [], "explicit empty list must remain distinguishable");
assert.equal(snapshot.bundledStubsPath, "/bundled/stubs");
