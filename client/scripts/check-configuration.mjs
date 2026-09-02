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

const {
  buildClientConfigurationSnapshot,
  buildExplicitClientSettings,
  selectStatusConfiguration,
} = module.exports;

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
  "indexing.maxFiles": 100000,
  "indexing.maxEntries": 1000000,
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
config.set("indexing.maxFiles", 250000, "workspaceFolderValue");
config.set("indexing.maxEntries", 0, "workspaceFolderValue");
let snapshot = JSON.parse(JSON.stringify(buildExplicitClientSettings(config, "/bundled/stubs")));
assert.deepEqual(snapshot, {
  phpVersion: "7.4",
  diagnosticsMode: "off",
  indexVendor: false,
  indexingMaxFiles: 250000,
  indexingMaxEntries: 0,
  stubExtensions: [],
  bundledStubsPath: "/bundled/stubs",
});

config.reset("phpVersion");
config.reset("diagnostics.mode");
snapshot = JSON.parse(JSON.stringify(buildExplicitClientSettings(config, "/bundled/stubs")));
assert.equal("phpVersion" in snapshot, false, "reset override must disappear from snapshot");
assert.equal("diagnosticsMode" in snapshot, false, "language override reset must disappear");
assert.equal(snapshot.indexVendor, false, "unrelated explicit override must remain");
assert.equal(snapshot.indexingMaxFiles, 250000);
assert.equal(snapshot.indexingMaxEntries, 0);
assert.deepEqual(snapshot.stubExtensions, [], "explicit empty list must remain distinguishable");
assert.equal(snapshot.bundledStubsPath, "/bundled/stubs");

const rootA = new FakeConfiguration({ phpVersion: "8.2", indexVendor: true });
rootA.set("phpVersion", "7.4", "workspaceFolderValue");
rootA.set("indexVendor", false, "workspaceFolderValue");
const rootB = new FakeConfiguration({ phpVersion: "8.2", indexVendor: true });
rootB.set("diagnostics.mode", "off", "workspaceFolderValue");

const multiRoot = JSON.parse(JSON.stringify(buildClientConfigurationSnapshot(
  config,
  [
    { uri: "file:///workspace/a", configuration: rootA },
    { uri: "file:///workspace/b", configuration: rootB },
  ],
  "/bundled/stubs",
)));
assert.deepEqual(multiRoot, {
  configurationVersion: 2,
  global: {
    indexVendor: false,
    indexingMaxFiles: 250000,
    indexingMaxEntries: 0,
    stubExtensions: [],
  },
  workspaceFolders: [
    {
      uri: "file:///workspace/a",
      settings: { phpVersion: "7.4", indexVendor: false },
    },
    {
      uri: "file:///workspace/b",
      settings: { diagnosticsMode: "off" },
    },
  ],
  bundledStubsPath: "/bundled/stubs",
});

const fallbackStatusConfiguration = { name: "fallback" };
const rootAStatusConfiguration = { name: "root-a" };
assert.deepEqual(
  JSON.parse(JSON.stringify(selectStatusConfiguration(fallbackStatusConfiguration))),
  {
    configuration: fallbackStatusConfiguration,
    scopeLabel: "workspace defaults",
  },
);
assert.deepEqual(
  JSON.parse(JSON.stringify(selectStatusConfiguration(fallbackStatusConfiguration, {
    configuration: rootAStatusConfiguration,
    resourceUri: "file:///workspace/a/Subject.php",
    workspaceFolderLabel: "root-a",
  }))),
  {
    configuration: rootAStatusConfiguration,
    scopeLabel: "root-a",
    resourceUri: "file:///workspace/a/Subject.php",
  },
);
assert.deepEqual(
  JSON.parse(JSON.stringify(selectStatusConfiguration(fallbackStatusConfiguration, {
    configuration: fallbackStatusConfiguration,
    resourceUri: "file:///outside/Subject.php",
  }))),
  {
    configuration: fallbackStatusConfiguration,
    scopeLabel: "outside workspace fallback",
    resourceUri: "file:///outside/Subject.php",
  },
);
