import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import vm from "node:vm";
import * as esbuild from "esbuild";

const result = await esbuild.build({
  entryPoints: ["src/serverProcess.ts"],
  bundle: true,
  format: "cjs",
  platform: "node",
  write: false,
  logLevel: "silent",
});

const module = { exports: {} };
vm.runInNewContext(result.outputFiles[0].text, {
  clearTimeout,
  console,
  module,
  exports: module.exports,
  setTimeout,
}, {
  filename: "server-process.bundle.cjs",
});

const {
  childProcessIsRunning,
  terminateManagedServerProcess,
  waitForChildProcessExit,
} = module.exports;

class FakeChildProcess extends EventEmitter {
  constructor(pid, onKill = () => true) {
    super();
    this.pid = pid;
    this.killed = false;
    this.exitCode = null;
    this.signalCode = null;
    this.signals = [];
    this.onKill = onKill;
    this.exitWhenListenerAdded = false;
  }

  kill(signal = "SIGTERM") {
    this.killed = true;
    this.signals.push(signal);
    return this.onKill(signal, this);
  }

  exitWithSignal(signal) {
    if (this.exitCode !== null || this.signalCode !== null) {
      return;
    }
    this.signalCode = signal;
    this.emit("exit", null, signal);
  }

  exitNormally(code = 0) {
    if (this.exitCode !== null || this.signalCode !== null) {
      return;
    }
    this.exitCode = code;
    this.emit("exit", code, null);
  }

  once(event, listener) {
    const result = super.once(event, listener);
    if (event === "exit" && this.exitWhenListenerAdded) {
      this.exitWhenListenerAdded = false;
      this.exitNormally();
    }
    return result;
  }
}

const shortWaits = { graceMs: 5, forceKillWaitMs: 10 };

{
  const logs = [];
  assert.equal(
    await terminateManagedServerProcess(undefined, "missing", (message) => logs.push(message), shortWaits),
    "not-running",
  );
  assert.match(logs.join("\n"), /termination skipped/);

  const exited = new FakeChildProcess(100);
  exited.exitNormally();
  assert.equal(childProcessIsRunning(exited), false);
  assert.equal(
    await terminateManagedServerProcess(exited, "already exited", () => {}, shortWaits),
    "not-running",
  );
}

{
  const child = new FakeChildProcess(101, (signal, process) => {
    if (signal === "SIGTERM") {
      process.exitWithSignal("SIGTERM");
    }
    return true;
  });
  const logs = [];
  assert.equal(
    await terminateManagedServerProcess(child, "sync graceful", (message) => logs.push(message), shortWaits),
    "terminated-gracefully",
  );
  assert.deepEqual(child.signals, ["SIGTERM"]);
  assert.equal(child.listenerCount("exit"), 0);
  assert.match(logs.join("\n"), /exited after SIGTERM/);
}

{
  const child = new FakeChildProcess(102, (signal, process) => {
    if (signal === "SIGTERM") {
      setTimeout(() => process.exitWithSignal("SIGTERM"), 1);
    }
    return true;
  });
  assert.equal(
    await terminateManagedServerProcess(child, "delayed graceful", () => {}, {
      graceMs: 30,
      forceKillWaitMs: 30,
    }),
    "terminated-gracefully",
  );
  assert.deepEqual(child.signals, ["SIGTERM"]);
  assert.equal(child.listenerCount("exit"), 0);
}

{
  const child = new FakeChildProcess(103, (signal, process) => {
    if (signal === "SIGKILL") {
      setTimeout(() => process.exitWithSignal("SIGKILL"), 1);
    }
    return true;
  });
  const logs = [];
  const result = await terminateManagedServerProcess(
    child,
    "forced",
    (message) => logs.push(message),
    shortWaits,
  );
  assert.equal(result, "terminated-forcibly");
  assert.equal(child.killed, true, "SIGTERM delivery must set killed before actual exit");
  assert.deepEqual(child.signals, ["SIGTERM", "SIGKILL"]);
  assert.equal(child.listenerCount("exit"), 0);
  assert.match(logs.join("\n"), /escalating to SIGKILL/);
  assert.match(logs.join("\n"), /pid=103/);
}

for (const termFailure of ["false", "throw"]) {
  const child = new FakeChildProcess(104, (signal, process) => {
    if (signal === "SIGTERM") {
      if (termFailure === "throw") {
        throw new Error("synthetic SIGTERM failure");
      }
      return false;
    }
    process.exitWithSignal("SIGKILL");
    return true;
  });
  const logs = [];
  assert.equal(
    await terminateManagedServerProcess(child, termFailure, (message) => logs.push(message), shortWaits),
    "terminated-forcibly",
  );
  assert.deepEqual(child.signals, ["SIGTERM", "SIGKILL"]);
  assert.equal(child.listenerCount("exit"), 0);
  assert.match(logs.join("\n"), termFailure === "throw" ? /SIGTERM failed/ : /sent=false/);
}

for (const killFailure of ["false", "throw"]) {
  const child = new FakeChildProcess(108, (signal) => {
    if (signal === "SIGKILL") {
      if (killFailure === "throw") {
        throw new Error("synthetic SIGKILL failure");
      }
      return false;
    }
    return true;
  });
  const logs = [];
  assert.equal(
    await terminateManagedServerProcess(child, killFailure, (message) => logs.push(message), {
      graceMs: 1,
      forceKillWaitMs: 1,
    }),
    "still-running",
  );
  assert.deepEqual(child.signals, ["SIGTERM", "SIGKILL"]);
  assert.equal(child.listenerCount("exit"), 0);
  assert.match(logs.join("\n"), killFailure === "throw" ? /SIGKILL failed/ : /sent=false/);
  assert.match(logs.join("\n"), /still running after SIGKILL/);
}

{
  const child = new FakeChildProcess(105, () => true);
  const logs = [];
  assert.equal(
    await terminateManagedServerProcess(child, "ignores both", (message) => logs.push(message), {
      graceMs: 1,
      forceKillWaitMs: 1,
    }),
    "still-running",
  );
  assert.deepEqual(child.signals, ["SIGTERM", "SIGKILL"]);
  assert.equal(child.listenerCount("exit"), 0);
  assert.match(logs.join("\n"), /still running after SIGKILL/);
  assert.match(logs.join("\n"), /pid=105/);
}

{
  const child = new FakeChildProcess(106, (signal, process) => {
    if (signal === "SIGTERM") {
      process.pid = 999;
    }
    return true;
  });
  const logs = [];
  assert.equal(
    await terminateManagedServerProcess(child, "pid changed", (message) => logs.push(message), {
      graceMs: 1,
      forceKillWaitMs: 1,
    }),
    "still-running",
  );
  assert.deepEqual(child.signals, ["SIGTERM"]);
  assert.match(logs.join("\n"), /SIGKILL skipped after PID changed/);
}

{
  const child = new FakeChildProcess(107);
  child.exitWhenListenerAdded = true;
  assert.equal(await waitForChildProcessExit(child, 20), true);
  assert.equal(child.listenerCount("exit"), 0);
}
