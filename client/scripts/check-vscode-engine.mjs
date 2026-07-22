import assert from "node:assert/strict";
import fs from "node:fs";
import semver from "semver";

const packageJson = JSON.parse(fs.readFileSync("package.json", "utf8"));
const packageLock = JSON.parse(fs.readFileSync("package-lock.json", "utf8"));

const extensionRange = packageJson.engines?.vscode;
const lockedExtensionRange = packageLock.packages?.[""]?.engines?.vscode;
const languageClientRange =
  packageLock.packages?.["node_modules/vscode-languageclient"]?.engines?.vscode;

assert.equal(
  typeof extensionRange,
  "string",
  "package.json must declare engines.vscode",
);
assert.equal(
  lockedExtensionRange,
  extensionRange,
  "package-lock.json must preserve package.json engines.vscode",
);
assert.equal(
  typeof languageClientRange,
  "string",
  "vscode-languageclient must declare its VS Code engine range in package-lock.json",
);
assert.ok(
  semver.subset(extensionRange, languageClientRange),
  `Extension engine ${extensionRange} includes VS Code versions unsupported by vscode-languageclient ${languageClientRange}`,
);

const minimumVersion = semver.minVersion(extensionRange);
assert.ok(minimumVersion, `Unable to determine minimum VS Code version from ${extensionRange}`);
assert.ok(
  semver.satisfies(minimumVersion, languageClientRange),
  `Minimum declared VS Code ${minimumVersion.version} is unsupported by vscode-languageclient ${languageClientRange}`,
);

console.log(
  `VS Code engine OK: ${extensionRange} (minimum ${minimumVersion.version}), vscode-languageclient ${languageClientRange}`,
);
