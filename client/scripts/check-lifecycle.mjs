import assert from "node:assert/strict";
import vm from "node:vm";
import * as esbuild from "esbuild";

const result = await esbuild.build({
  entryPoints: ["src/lifecycle.ts"],
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
  filename: "lifecycle.bundle.cjs",
});

const {
  BoundedRestartTracker,
  DisposableResourceRegistry,
  languageClientFailurePolicy,
  LifecycleCoordinator,
  reconcileLanguageClientState,
} = module.exports;

{
  let now = 0;
  const restartTracker = new BoundedRestartTracker(4, 3 * 60 * 1000, () => now);
  assert.equal(restartTracker.shouldRestart(), true);
  assert.equal(restartTracker.shouldRestart(), true);
  assert.equal(restartTracker.shouldRestart(), true);
  assert.equal(restartTracker.shouldRestart(), true);
  assert.equal(
    restartTracker.shouldRestart(),
    false,
    "the fifth crash inside the restart window must stop the restart loop",
  );

  now = 3 * 60 * 1000 + 1;
  assert.equal(
    restartTracker.shouldRestart(),
    true,
    "a crash after a quiet restart window may be restarted again",
  );
  assert.equal(restartTracker.shouldRestart(), true);
  assert.equal(restartTracker.shouldRestart(), true);
  assert.equal(restartTracker.shouldRestart(), true);
  assert.equal(
    restartTracker.shouldRestart(),
    false,
    "a new quiet-window burst must still allow exactly four restarts",
  );
}

function deferred() {
  let resolve;
  const promise = new Promise((resolvePromise) => {
    resolve = resolvePromise;
  });
  return { promise, resolve };
}

{
  const coordinator = new LifecycleCoordinator();
  const events = [];
  const blockerGate = deferred();
  const blockerStarted = deferred();
  const state = {
    client: { id: "initial" },
    enabled: true,
    starts: 0,
    stops: 0,
  };
  const report = (event) => events.push(`${event.phase}:${event.reason}`);
  const target = {
    isEnabled: () => state.enabled,
    hasClient: () => state.client !== undefined,
    start: async () => {
      state.starts += 1;
      state.client = { id: "started" };
      return true;
    },
    stop: async () => {
      state.stops += 1;
      state.client = undefined;
    },
  };

  const blocker = coordinator.enqueue("blocker", async () => {
    blockerStarted.resolve();
    await blockerGate.promise;
  }, report);
  await blockerStarted.promise;

  const currentFailure = languageClientFailurePolicy(state.client, state.client, false);
  assert.equal(currentFailure.pipe, "continue");
  assert.equal(
    currentFailure.close,
    "restart",
    "an unrelated active lifecycle operation must not suppress current-client recovery",
  );
  const detachedFailure = languageClientFailurePolicy(state.client, {}, false);
  assert.equal(detachedFailure.pipe, "shutdown");
  assert.equal(
    detachedFailure.close,
    "doNotRestart",
    "a detached client's late failure must stay suppressed",
  );
  const stoppingFailure = languageClientFailurePolicy(state.client, state.client, true);
  assert.equal(stoppingFailure.pipe, "shutdown");
  assert.equal(
    stoppingFailure.close,
    "doNotRestart",
    "an explicitly stopping client must not restart",
  );

  state.enabled = false;
  const queuedDisable = coordinator.enqueue(
    "disable configuration",
    async () => reconcileLanguageClientState(target),
    report,
  );
  state.enabled = true;
  const queuedEnable = coordinator.enqueue(
    "enable configuration",
    async () => reconcileLanguageClientState(target),
    report,
  );

  blockerGate.resolve();
  await Promise.all([blocker, queuedDisable, queuedEnable]);
  assert.equal(state.client.id, "initial", "latest enabled state must preserve the running client");
  assert.equal(state.stops, 0, "stale queued disable must re-read configuration before stopping");
  assert.equal(state.starts, 0, "reconciliation must not duplicate an existing client");
  assert.equal(coordinator.active, false, "completed lifecycle queue should not stay active");
  assert.deepEqual(events, [
    "begin:blocker",
    "complete:blocker",
    "begin:disable configuration",
    "complete:disable configuration",
    "begin:enable configuration",
    "complete:enable configuration",
  ]);
}

{
  const coordinator = new LifecycleCoordinator();
  const stopGate = deferred();
  const stopStarted = deferred();
  const state = {
    client: { id: "initial" },
    enabled: false,
    starts: 0,
    stops: 0,
  };
  const target = {
    isEnabled: () => state.enabled,
    hasClient: () => state.client !== undefined,
    start: async () => {
      state.starts += 1;
      state.client = { id: "restarted" };
      return true;
    },
    stop: async () => {
      state.stops += 1;
      state.client = undefined;
      stopStarted.resolve();
      await stopGate.promise;
    },
  };
  const report = () => {};

  const queuedDisable = coordinator.enqueue(
    "disable configuration",
    async () => reconcileLanguageClientState(target),
    report,
  );
  await stopStarted.promise;
  assert.equal(coordinator.active, true, "slow stop should keep reconciliation active");
  assert.equal(state.client, undefined, "stop should detach the old client before awaiting shutdown");

  state.enabled = true;
  const queuedEnable = coordinator.enqueue(
    "enable configuration",
    async () => reconcileLanguageClientState(target),
    report,
  );
  stopGate.resolve();
  await Promise.all([queuedDisable, queuedEnable]);

  assert.equal(state.client.id, "restarted", "latest enable must restore the client after slow stop");
  assert.equal(state.stops, 1, "slow stop must run once");
  assert.equal(state.starts, 1, "queued reconciliation must start exactly one replacement client");
  assert.equal(coordinator.active, false, "slow-stop reconciliation queue must drain");
}

const registry = new DisposableResourceRegistry();
const owner = {};
const disposeCounts = Array.from({ length: 8 }, () => 0);
const disposalErrors = [];
registry.register(
  owner,
  disposeCounts.map((_, index) => ({
    dispose() {
      disposeCounts[index] += 1;
      if (index === 3) {
        throw new Error("synthetic watcher disposal failure");
      }
    },
  })),
);
assert.equal(
  registry.dispose(owner, (error) => disposalErrors.push(error)),
  8,
  "all client file watchers must be disposed",
);
assert.equal(registry.dispose(owner), 0, "watchers must only be disposed once");
assert.ok(disposeCounts.every((count) => count === 1));
assert.equal(disposalErrors.length, 1, "one watcher failure should be reported");

console.log("lifecycle reconciliation queue and client resource ownership OK");
