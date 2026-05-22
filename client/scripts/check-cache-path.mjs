import assert from "node:assert/strict";
import { createRequire } from "node:module";
import path from "node:path";
import vm from "node:vm";
import * as esbuild from "esbuild";

const require = createRequire(import.meta.url);

const result = await esbuild.build({
  entryPoints: ["src/cachePath.ts"],
  bundle: true,
  format: "cjs",
  platform: "node",
  write: false,
  logLevel: "silent",
});

const module = { exports: {} };
vm.runInNewContext(result.outputFiles[0].text, {
  Buffer,
  console,
  module,
  exports: module.exports,
  process,
  require,
}, {
  filename: "cachePath.bundle.cjs",
});

const {
  stableHashStrings,
  phpLspCacheDirForRootWithBase,
} = module.exports;

const root = "/tmp/project";
const expectedHash = "610b7c4200a8dcaa";
const expectedCacheDir = path.join("/tmp/php-lsp-cache-base", "php-lsp", expectedHash);

assert.equal(stableHashStrings([root]), expectedHash);
assert.equal(
  phpLspCacheDirForRootWithBase("/tmp/php-lsp-cache-base", root),
  expectedCacheDir,
);

console.log(`cache path helper OK: ${expectedCacheDir}`);
