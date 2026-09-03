export type IndexingPhase =
  | "starting"
  | "discovering"
  | "loadingStubs"
  | "stubsLoaded"
  | "indexing"
  | "ready"
  | "error";

export interface IndexingStatus {
  phase: IndexingPhase | string;
  root?: string;
  message?: string;
  indexedFiles?: number;
  totalFiles?: number;
  indexedSymbols?: number;
  percentage?: number;
  elapsedMs?: number;
  stubFiles?: number;
  truncated?: boolean;
  truncationReason?: "maxFiles" | "maxEntries";
  truncationLimit?: number;
  visitedEntries?: number;
  runtimeGeneration?: number;
  indexingRunId?: number;
  workspaceFolder?: string;
  lastUpdatedAt?: number;
}

export interface IndexingStatusUpdate extends IndexingStatus {
  resetTraversal?: boolean;
}

export interface LatestIndexingRun {
  runtimeGeneration?: number;
  indexingRunId: number;
}

export function indexingStatusUpdateIsCurrent(
  latestRuns: Map<string, LatestIndexingRun>,
  incoming: IndexingStatusUpdate,
): boolean {
  if (incoming.phase === "starting" || incoming.resetTraversal === true) {
    latestRuns.clear();
  }
  const workspace = incoming.workspaceFolder ?? incoming.root;
  if (!workspace || incoming.indexingRunId === undefined) {
    return true;
  }
  const current = latestRuns.get(workspace);
  if (current) {
    const incomingGeneration = incoming.runtimeGeneration ?? 0;
    const currentGeneration = current.runtimeGeneration ?? 0;
    if (incomingGeneration < currentGeneration) {
      return false;
    }
    if (
      incomingGeneration === currentGeneration
      && incoming.indexingRunId < current.indexingRunId
    ) {
      return false;
    }
  }
  latestRuns.set(workspace, {
    runtimeGeneration: incoming.runtimeGeneration,
    indexingRunId: incoming.indexingRunId,
  });
  return true;
}

export function mergeIndexingStatus(
  current: IndexingStatus,
  incoming: IndexingStatusUpdate,
  now = Date.now(),
): IndexingStatus {
  if (
    incoming.runtimeGeneration !== undefined
    && current.runtimeGeneration !== undefined
    && incoming.runtimeGeneration < current.runtimeGeneration
  ) {
    return current;
  }
  const generationChanged = incoming.runtimeGeneration !== undefined
    && (
      current.runtimeGeneration === undefined
      || incoming.runtimeGeneration > current.runtimeGeneration
    );
  const traversalReset = incoming.phase === "starting"
    || incoming.resetTraversal === true
    || generationChanged
    ? {
      truncated: false,
      truncationReason: undefined,
      truncationLimit: undefined,
      visitedEntries: undefined,
      runtimeGeneration: undefined,
    }
    : {};
  const { resetTraversal: _resetTraversal, ...incomingStatus } = incoming;
  const merged = {
    ...current,
    ...traversalReset,
    ...incomingStatus,
    lastUpdatedAt: now,
  };
  if (
    !generationChanged
    && incoming.phase !== "starting"
    && incoming.resetTraversal !== true
    && incoming.truncated !== true
    && current.truncated
  ) {
    merged.truncated = true;
    merged.truncationReason = current.truncationReason;
    merged.truncationLimit = current.truncationLimit;
    merged.visitedEntries = current.visitedEntries;
  }
  return merged;
}

export function statusText(status: IndexingStatus): string {
  if (status.phase === "indexing") {
    const percent = typeof status.percentage === "number" ? ` ${Math.round(status.percentage)}%` : "";
    return `$(sync~spin) PHP LSP${percent}`;
  }
  if (status.phase === "discovering" || status.phase === "loadingStubs") {
    return "$(sync~spin) PHP LSP";
  }
  if (status.phase === "error") {
    return "$(error) PHP LSP";
  }
  if (status.truncated) {
    return "$(warning) PHP LSP";
  }
  return "$(check) PHP LSP";
}

export function phaseIcon(phase: string, truncated = false): string {
  if (phase === "indexing" || phase === "discovering" || phase === "loadingStubs") {
    return "$(sync~spin)";
  }
  if (phase === "error") {
    return "$(error)";
  }
  if (truncated) {
    return "$(warning)";
  }
  return "$(check)";
}

export function phaseTitle(phase: string, truncated = false): string {
  if (truncated && phase === "ready") {
    return "Ready (partial index)";
  }
  switch (phase) {
    case "starting":
      return "Starting";
    case "discovering":
      return "Discovering files";
    case "loadingStubs":
      return "Loading stubs";
    case "stubsLoaded":
      return "Stubs loaded";
    case "indexing":
      return "Indexing";
    case "ready":
      return "Ready";
    case "error":
      return "Error";
    default:
      return phase;
  }
}
