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
  lastUpdatedAt?: number;
}

export function mergeIndexingStatus(
  current: IndexingStatus,
  incoming: IndexingStatus,
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
  const traversalReset = incoming.phase === "starting" || generationChanged
    ? {
      truncated: false,
      truncationReason: undefined,
      truncationLimit: undefined,
      visitedEntries: undefined,
      runtimeGeneration: undefined,
    }
    : {};
  const merged = {
    ...current,
    ...traversalReset,
    ...incoming,
    lastUpdatedAt: now,
  };
  if (
    !generationChanged
    && incoming.phase !== "starting"
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
