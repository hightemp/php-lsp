import assert from "node:assert/strict";
import vm from "node:vm";
import * as esbuild from "esbuild";

const result = await esbuild.build({
  entryPoints: ["src/indexingStatus.ts"],
  bundle: true,
  format: "cjs",
  platform: "node",
  write: false,
  logLevel: "silent",
});

const module = { exports: {} };
vm.runInNewContext(result.outputFiles[0].text, {
  module,
  exports: module.exports,
}, {
  filename: "indexing-status.bundle.cjs",
});

const {
  indexingStatusUpdateIsCurrent,
  mergeIndexingStatus,
  phaseIcon,
  phaseTitle,
  statusText,
} = module.exports;

let status = {
  phase: "ready",
  message: "Indexed a deterministic partial workspace",
  truncated: true,
  truncationReason: "maxEntries",
  truncationLimit: 100,
  visitedEntries: 100,
  runtimeGeneration: 7,
};
assert.equal(statusText(status), "$(warning) PHP LSP");
assert.equal(phaseIcon(status.phase, status.truncated), "$(warning)");
assert.equal(phaseTitle(status.phase, status.truncated), "Ready (partial index)");

status = mergeIndexingStatus(status, {
  phase: "starting",
  message: "Restarting language server",
}, 1234);
assert.equal(status.truncated, false);
assert.equal(status.truncationReason, undefined);
assert.equal(status.truncationLimit, undefined);
assert.equal(status.visitedEntries, undefined);
assert.equal(status.lastUpdatedAt, 1234);
assert.equal(statusText(status), "$(check) PHP LSP");

status = mergeIndexingStatus({
  ...status,
  phase: "ready",
  truncated: true,
  truncationReason: "maxEntries",
  truncationLimit: 100,
  visitedEntries: 100,
  runtimeGeneration: 7,
}, {
  phase: "discovering",
  runtimeGeneration: 7,
});
status = mergeIndexingStatus(status, {
  phase: "indexing",
  truncated: false,
  runtimeGeneration: 7,
});
assert.equal(status.truncated, true, "another root in the same generation must keep partial state");
assert.equal(status.truncationReason, "maxEntries");

status = mergeIndexingStatus(status, {
  phase: "discovering",
  runtimeGeneration: 8,
});
assert.equal(status.truncated, false, "a new runtime generation starts a fresh traversal state");

const currentGenerationStatus = status;
status = mergeIndexingStatus(status, {
  phase: "ready",
  truncated: true,
  truncationReason: "maxFiles",
  runtimeGeneration: 7,
});
assert.equal(status, currentGenerationStatus, "a stale runtime generation must be ignored");
assert.equal(status.runtimeGeneration, 8);
assert.equal(status.truncated, false);

status = mergeIndexingStatus({
  ...status,
  phase: "ready",
  truncated: true,
  truncationReason: "maxFiles",
  truncationLimit: 10,
  visitedEntries: 10,
}, {
  phase: "ready",
  message: "Language server is disabled",
  resetTraversal: true,
});
assert.equal(status.truncated, false, "disabling the server must clear partial-index state");
assert.equal(status.runtimeGeneration, undefined);

status = mergeIndexingStatus(status, {
  phase: "ready",
  message: "Index ready",
  runtimeGeneration: 8,
});
assert.equal(phaseTitle(status.phase, status.truncated), "Ready");

const latestRuns = new Map();
assert.equal(indexingStatusUpdateIsCurrent(latestRuns, {
  phase: "discovering",
  workspaceFolder: "/workspace/a",
  runtimeGeneration: 9,
  indexingRunId: 12,
}), true);
assert.equal(indexingStatusUpdateIsCurrent(latestRuns, {
  phase: "ready",
  workspaceFolder: "/workspace/a",
  runtimeGeneration: 9,
  indexingRunId: 11,
}), false, "an older run for the same workspace must be ignored");
assert.equal(indexingStatusUpdateIsCurrent(latestRuns, {
  phase: "ready",
  workspaceFolder: "/workspace/b",
  runtimeGeneration: 9,
  indexingRunId: 10,
}), true, "run ordering must remain isolated across workspace folders");
assert.equal(indexingStatusUpdateIsCurrent(latestRuns, {
  phase: "starting",
  resetTraversal: true,
}), true);
assert.equal(latestRuns.size, 0, "server restart must reset remembered run identities");
