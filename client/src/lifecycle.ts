export interface DisposableResource {
  dispose(): unknown;
}

export interface LanguageClientFailurePolicy {
  pipe: "continue" | "shutdown";
  close: "restart" | "doNotRestart";
}

export function languageClientFailurePolicy(
  activeClient: object | undefined,
  failedClient: object | undefined,
  isStopping: boolean,
): LanguageClientFailurePolicy {
  const staleOrStopping = failedClient === undefined
    || activeClient !== failedClient
    || isStopping;
  return staleOrStopping
    ? { pipe: "shutdown", close: "doNotRestart" }
    : { pipe: "continue", close: "restart" };
}

export class BoundedRestartTracker {
  private readonly restarts: number[] = [];

  constructor(
    private readonly maxRestartCount = 4,
    private readonly restartWindowMs = 3 * 60 * 1000,
    private readonly now: () => number = Date.now,
  ) {}

  shouldRestart(): boolean {
    const current = this.now();
    while (
      this.restarts.length > 0
      && current - this.restarts[0] > this.restartWindowMs
    ) {
      this.restarts.shift();
    }

    if (this.restarts.length >= this.maxRestartCount) {
      return false;
    }

    this.restarts.push(current);
    return true;
  }
}

export type LifecycleEvent =
  | { phase: "begin" | "complete"; reason: string }
  | { phase: "failed"; reason: string; error: unknown };

export class LifecycleCoordinator {
  private queue: Promise<void> = Promise.resolve();
  private operationDepth = 0;

  get active(): boolean {
    return this.operationDepth > 0;
  }

  enqueue(
    reason: string,
    operation: () => Promise<void>,
    report: (event: LifecycleEvent) => void,
  ): Promise<void> {
    const run = this.queue
      .catch(() => undefined)
      .then(async () => {
        report({ phase: "begin", reason });
        this.operationDepth += 1;
        try {
          await operation();
          report({ phase: "complete", reason });
        } catch (error: unknown) {
          report({ phase: "failed", reason, error });
          throw error;
        } finally {
          this.operationDepth = Math.max(0, this.operationDepth - 1);
        }
      });

    this.queue = run.catch(() => undefined);
    return run;
  }
}

export class DisposableResourceRegistry<Owner extends object> {
  private readonly resources = new WeakMap<Owner, readonly DisposableResource[]>();

  register(owner: Owner, resources: readonly DisposableResource[]): void {
    this.resources.set(owner, resources);
  }

  dispose(owner: Owner, reportError?: (error: unknown) => void): number {
    const resources = this.resources.get(owner);
    this.resources.delete(owner);
    if (!resources) {
      return 0;
    }

    for (const resource of resources) {
      try {
        resource.dispose();
      } catch (error: unknown) {
        reportError?.(error);
      }
    }
    return resources.length;
  }
}

export interface LanguageClientReconciliationTarget {
  isEnabled(): boolean;
  hasClient(): boolean;
  start(): Promise<boolean>;
  stop(): Promise<void>;
  onDisabled?(): void | Promise<void>;
  onRunning?(): void | Promise<void>;
  onStarting?(): void | Promise<void>;
}

/**
 * Reconcile the running client with the latest configuration value.
 *
 * State is deliberately read again after every asynchronous start/stop. This
 * lets one queued operation converge on a setting that changed while a slow
 * lifecycle transition was still in progress.
 */
export async function reconcileLanguageClientState(
  target: LanguageClientReconciliationTarget,
): Promise<void> {
  while (true) {
    const enabled = target.isEnabled();
    const clientExists = target.hasClient();

    if (!enabled) {
      if (clientExists) {
        await target.stop();
        continue;
      }
      await target.onDisabled?.();
      return;
    }

    if (!clientExists) {
      await target.onStarting?.();
      if (!(await target.start())) {
        return;
      }
      continue;
    }

    await target.onRunning?.();
    return;
  }
}
