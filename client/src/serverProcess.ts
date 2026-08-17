import type { ChildProcess } from "child_process";

export const DEFAULT_TERMINATION_GRACE_MS = 1000;
export const DEFAULT_FORCE_KILL_WAIT_MS = 1000;

export type ManagedServerTerminationResult =
  | "not-running"
  | "terminated-gracefully"
  | "terminated-forcibly"
  | "still-running";

export interface ManagedServerTerminationOptions {
  graceMs?: number;
  forceKillWaitMs?: number;
}

export type ManagedServerProcess = Pick<
  ChildProcess,
  "pid" | "killed" | "exitCode" | "signalCode" | "kill" | "once" | "removeListener"
>;

type ProcessLogger = (message: string) => void;

export function childProcessIsRunning(
  childProcess: ManagedServerProcess | undefined,
): childProcess is ManagedServerProcess & { pid: number } {
  return !!childProcess
    && childProcess.pid !== undefined
    && childProcess.exitCode === null
    && childProcess.signalCode === null;
}

function processState(childProcess: ManagedServerProcess): string {
  return `pid=${childProcess.pid ?? "unknown"}; exitCode=${childProcess.exitCode ?? "null"}; signalCode=${childProcess.signalCode ?? "null"}; killed=${childProcess.killed}`;
}

export function waitForChildProcessExit(
  childProcess: ManagedServerProcess,
  timeoutMs: number,
): Promise<boolean> {
  if (!childProcessIsRunning(childProcess)) {
    return Promise.resolve(true);
  }

  return new Promise<boolean>((resolve) => {
    let settled = false;
    let timeout: NodeJS.Timeout | undefined;

    const finish = (exited: boolean): void => {
      if (settled) {
        return;
      }
      settled = true;
      if (timeout !== undefined) {
        clearTimeout(timeout);
      }
      childProcess.removeListener("exit", onExit);
      resolve(exited);
    };
    const onExit = (): void => finish(true);

    childProcess.once("exit", onExit);
    if (settled) {
      return;
    }
    timeout = setTimeout(
      () => finish(!childProcessIsRunning(childProcess)),
      Math.max(0, timeoutMs),
    );

    // Close the race where the process exited after the initial check but
    // before the exit listener was installed.
    if (!childProcessIsRunning(childProcess)) {
      finish(true);
    }
  });
}

function sendSignal(
  childProcess: ManagedServerProcess,
  signal: NodeJS.Signals,
  reason: string,
  pid: number,
  log: ProcessLogger,
): boolean {
  try {
    const sent = childProcess.kill(signal);
    lifecycleSignalLog(log, signal, reason, pid, sent);
    return sent;
  } catch (error: unknown) {
    const message = error instanceof Error ? error.message : String(error);
    log(`Managed server ${signal} failed: reason=${reason}; pid=${pid}; error=${message}`);
    return false;
  }
}

function lifecycleSignalLog(
  log: ProcessLogger,
  signal: NodeJS.Signals,
  reason: string,
  pid: number,
  sent: boolean,
): void {
  log(`Managed server ${signal} requested: reason=${reason}; pid=${pid}; sent=${sent}`);
}

export async function terminateManagedServerProcess(
  processToTerminate: ManagedServerProcess | undefined,
  reason: string,
  log: ProcessLogger,
  options: ManagedServerTerminationOptions = {},
): Promise<ManagedServerTerminationResult> {
  if (!childProcessIsRunning(processToTerminate)) {
    log(`Managed server termination skipped: reason=${reason}; process handle unavailable or exited`);
    return "not-running";
  }

  const pid = processToTerminate.pid;
  const graceMs = options.graceMs ?? DEFAULT_TERMINATION_GRACE_MS;
  const forceKillWaitMs = options.forceKillWaitMs ?? DEFAULT_FORCE_KILL_WAIT_MS;
  const termSent = sendSignal(processToTerminate, "SIGTERM", reason, pid, log);

  if (termSent && await waitForChildProcessExit(processToTerminate, graceMs)) {
    log(`Managed server exited after SIGTERM: reason=${reason}; ${processState(processToTerminate)}`);
    return "terminated-gracefully";
  }
  if (!childProcessIsRunning(processToTerminate)) {
    log(`Managed server exited before SIGKILL escalation: reason=${reason}; ${processState(processToTerminate)}`);
    return "terminated-gracefully";
  }
  if (processToTerminate.pid !== pid) {
    log(`Managed server SIGKILL skipped after PID changed: reason=${reason}; expectedPid=${pid}; actualPid=${processToTerminate.pid}`);
    return "still-running";
  }

  log(`Managed server termination escalating to SIGKILL: reason=${reason}; ${processState(processToTerminate)}`);
  sendSignal(processToTerminate, "SIGKILL", reason, pid, log);

  if (await waitForChildProcessExit(processToTerminate, forceKillWaitMs)) {
    log(`Managed server exited after SIGKILL: reason=${reason}; ${processState(processToTerminate)}`);
    return "terminated-forcibly";
  }

  log(`Managed server still running after SIGKILL: reason=${reason}; ${processState(processToTerminate)}`);
  return "still-running";
}
