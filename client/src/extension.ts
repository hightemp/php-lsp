import * as path from "path";
import * as os from "os";
import * as fs from "fs";
import type { ChildProcess } from "child_process";
import { phpLspCacheDirForRoot } from "./cachePath";
import {
  workspace,
  commands,
  env,
  window,
  ExtensionContext,
  StatusBarAlignment,
  StatusBarItem,
  QuickPickItem,
  Disposable,
  MarkdownString,
  OutputChannel,
  ThemeColor,
} from "vscode";
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  TransportKind,
} from "vscode-languageclient/node";

let client: LanguageClient | undefined;
let statusController: PhpLspStatusController | undefined;
let indexingStatusSubscription: Disposable | undefined;
let outputChannel: OutputChannel | undefined;
let lastBinaryResolutionError: string | undefined;
let lastStartError: string | undefined;
let lifecycleQueue: Promise<void> = Promise.resolve();

const STOP_TIMEOUT_MS = 5000;

type IndexingPhase =
  | "starting"
  | "discovering"
  | "loadingStubs"
  | "stubsLoaded"
  | "indexing"
  | "ready"
  | "error";

interface IndexingStatus {
  phase: IndexingPhase | string;
  root?: string;
  message?: string;
  indexedFiles?: number;
  totalFiles?: number;
  indexedSymbols?: number;
  percentage?: number;
  elapsedMs?: number;
  stubFiles?: number;
  lastUpdatedAt?: number;
}

interface ExtensionSnapshot {
  extensionVersion: string;
  serverName: string;
  serverVersion?: string;
  serverPath: string;
  serverBinarySource: ServerBinarySource;
  serverBinaryExists: boolean;
  stubsPath?: string;
  platformDir?: string;
  cacheDirs: string[];
  lastBinaryResolutionError?: string;
  lastStartError?: string;
  workspaceFolders: string[];
  phpVersion: string;
  diagnosticsMode: string;
  composerEnabled: boolean;
  indexVendor: boolean;
  phpstanEnabled: boolean;
  psalmEnabled: boolean;
  formattingProvider: string;
  logLevel: string;
  includePaths: string[];
  excludePaths: string[];
}

interface StatusQuickPickItem extends QuickPickItem {
  action?: "version" | "restart" | "clearCache" | "output" | "settings";
}

interface ServerBinaryResolution {
  serverPath: string;
  source: ServerBinarySource;
  exists: boolean;
  platformDir?: string;
  error?: string;
}

type ServerBinarySource = "custom" | "bundled" | "path" | "unsupported";

class PhpLspStatusController implements Disposable {
  private readonly statusBar: StatusBarItem;
  private status: IndexingStatus = {
    phase: "starting",
    message: "Starting language server",
    lastUpdatedAt: Date.now(),
  };

  constructor(private readonly snapshotProvider: () => ExtensionSnapshot) {
    this.statusBar = window.createStatusBarItem(StatusBarAlignment.Left, 100);
    this.statusBar.name = "PHP Language Server";
    this.statusBar.command = "phpLsp.showStatus";
    this.statusBar.accessibilityInformation = {
      label: "PHP Language Server status",
      role: "button",
    };
    this.render();
    this.statusBar.show();
  }

  update(status: IndexingStatus): void {
    this.status = {
      ...this.status,
      ...status,
      lastUpdatedAt: Date.now(),
    };
    this.render();
  }

  async showPopup(): Promise<void> {
    const snapshot = this.snapshotProvider();
    const status = this.status;
    const items: StatusQuickPickItem[] = [
      {
        label: `${phaseIcon(status.phase)} ${phaseTitle(status.phase)}`,
        description: percentDescription(status),
        detail: status.message ?? "PHP Language Server is running",
      },
      {
        label: "$(folder) Workspace",
        description: compactPath(status.root) ?? folderCountLabel(snapshot.workspaceFolders.length),
        detail: status.root ?? (snapshot.workspaceFolders.join("; ") || "No workspace folder"),
      },
      {
        label: "$(files) Indexed files",
        description: fileProgressLabel(status),
        detail: `Symbols: ${formatCount(status.indexedSymbols)}${elapsedLabel(status.elapsedMs)}`,
      },
      {
        label: "$(database) Stubs",
        description: formatCount(status.stubFiles),
        detail: snapshot.stubsPath ?? "Bundled stubs directory was not found",
      },
      {
        label: "$(settings-gear) Diagnostics",
        description: snapshot.diagnosticsMode,
        detail: `PHP ${snapshot.phpVersion}; Composer: ${onOff(snapshot.composerEnabled)}; Vendor lazy index: ${onOff(snapshot.indexVendor)}`,
      },
      {
        label: "$(beaker) External analyzers",
        description: analyzerSummary(snapshot),
        detail: `PHPStan: ${onOff(snapshot.phpstanEnabled)}; Psalm: ${onOff(snapshot.psalmEnabled)}`,
      },
      {
        label: "$(tools) Formatter",
        description: snapshot.formattingProvider,
        detail: formatterDetail(snapshot.formattingProvider),
      },
      {
        label: "$(output) Log level",
        description: snapshot.logLevel,
        detail: "Applied when the language server process starts",
      },
      {
        label: "$(list-tree) Include paths",
        description: `${snapshot.includePaths.length}`,
        detail: snapshot.includePaths.length > 0 ? snapshot.includePaths.join("; ") : "No additional include paths",
      },
      {
        label: "$(exclude) Exclude paths",
        description: `${snapshot.excludePaths.length}`,
        detail: snapshot.excludePaths.length > 0 ? snapshot.excludePaths.join("; ") : "No excluded paths",
      },
      {
        label: "$(server-process) Server binary",
        description: binaryDescription(snapshot),
        detail: snapshot.serverPath,
      },
      {
        label: "$(versions) Server version",
        description: snapshot.serverVersion ?? "not initialized",
        detail: serverDiagnosticsDetail(snapshot),
        action: "version",
      },
      {
        label: "$(debug-restart) Restart language server",
        action: "restart",
      },
      {
        label: "$(trash) Clear cache and restart",
        detail: "Deletes PHP LSP disk cache for the current workspace roots",
        action: "clearCache",
      },
      {
        label: "$(output) Open LSP output",
        action: "output",
      },
      {
        label: "$(settings) Open PHP LSP settings",
        action: "settings",
      },
    ];

    const selected = await window.showQuickPick(items, {
      title: "PHP Language Server",
      placeHolder: "Indexing status and extension details",
      matchOnDescription: true,
      matchOnDetail: true,
    });

    if (selected?.action === "version") {
      await showServerVersion(this.snapshotProvider());
    } else if (selected?.action === "restart") {
      await commands.executeCommand("phpLsp.restartServer");
    } else if (selected?.action === "clearCache") {
      await commands.executeCommand("phpLsp.clearCacheAndRestart");
    } else if (selected?.action === "output") {
      getOutputChannel().show(true);
    } else if (selected?.action === "settings") {
      await commands.executeCommand("workbench.action.openSettings", "phpLsp");
    }
  }

  dispose(): void {
    this.statusBar.dispose();
  }

  private render(): void {
    const status = this.status;
    this.statusBar.text = statusText(status);
    this.statusBar.tooltip = statusTooltip(status, this.snapshotProvider());
    this.statusBar.backgroundColor = status.phase === "error"
      ? new ThemeColor("statusBarItem.errorBackground")
      : undefined;
  }
}

function statusText(status: IndexingStatus): string {
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
  return "$(check) PHP LSP";
}

function statusTooltip(status: IndexingStatus, snapshot: ExtensionSnapshot): MarkdownString {
  const tooltip = new MarkdownString();
  tooltip.appendMarkdown("**PHP Language Server**\n\n");
  tooltip.appendMarkdown(phaseTitle(status.phase));
  if (status.message) {
    tooltip.appendMarkdown(`: ${status.message}`);
  }
  tooltip.appendMarkdown("\n\n");
  tooltip.appendMarkdown(`Server: ${serverVersionLabel(snapshot)}\n\n`);
  if (typeof status.indexedFiles === "number" || typeof status.totalFiles === "number") {
    tooltip.appendMarkdown(`Files: ${fileProgressLabel(status)}\n\n`);
  }
  if (typeof status.indexedSymbols === "number") {
    tooltip.appendMarkdown(`Symbols: ${formatCount(status.indexedSymbols)}\n\n`);
  }
  tooltip.appendMarkdown(`Diagnostics: ${snapshot.diagnosticsMode}\n\n`);
  tooltip.appendMarkdown("Click to show details.");
  return tooltip;
}

function phaseIcon(phase: string): string {
  if (phase === "indexing" || phase === "discovering" || phase === "loadingStubs") {
    return "$(sync~spin)";
  }
  if (phase === "error") {
    return "$(error)";
  }
  return "$(check)";
}

function phaseTitle(phase: string): string {
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

function percentDescription(status: IndexingStatus): string | undefined {
  return typeof status.percentage === "number" ? `${Math.round(status.percentage)}%` : undefined;
}

function fileProgressLabel(status: IndexingStatus): string {
  const indexed = formatCount(status.indexedFiles);
  const total = formatCount(status.totalFiles);
  if (indexed === "n/a" && total === "n/a") {
    return "n/a";
  }
  return `${indexed}/${total}`;
}

function elapsedLabel(elapsedMs: number | undefined): string {
  if (typeof elapsedMs !== "number") {
    return "";
  }
  const seconds = Math.max(0, Math.round(elapsedMs / 1000));
  if (seconds < 60) {
    return `; elapsed: ${seconds}s`;
  }
  return `; elapsed: ${Math.floor(seconds / 60)}m ${seconds % 60}s`;
}

function formatCount(value: number | undefined): string {
  return typeof value === "number" ? value.toLocaleString() : "n/a";
}

function compactPath(value: string | undefined): string | undefined {
  if (!value) {
    return undefined;
  }
  return path.basename(value) || value;
}

function folderCountLabel(count: number): string {
  if (count === 0) {
    return "no folders";
  }
  if (count === 1) {
    return "1 folder";
  }
  return `${count} folders`;
}

function onOff(enabled: boolean): string {
  return enabled ? "on" : "off";
}

function analyzerSummary(snapshot: ExtensionSnapshot): string {
  const enabled = [
    snapshot.phpstanEnabled ? "PHPStan" : undefined,
    snapshot.psalmEnabled ? "Psalm" : undefined,
  ].filter(Boolean);
  return enabled.length > 0 ? enabled.join(", ") : "off";
}

function formatterDetail(provider: string): string {
  if (provider === "none") {
    return "Document formatting is disabled";
  }
  if (provider === "auto") {
    return "Auto-detects Laravel Pint, php-cs-fixer, or phpcbf from Composer metadata";
  }
  return "External formatter is configured";
}

function getExtensionSnapshot(context: ExtensionContext): ExtensionSnapshot {
  const config = workspace.getConfiguration("phpLsp");
  const binary = resolveServerBinary(context);
  if (binary.error) {
    lastBinaryResolutionError = binary.error;
  }
  const serverInfo = client?.initializeResult?.serverInfo;
  return {
    extensionVersion: String(context.extension.packageJSON.version ?? "unknown"),
    serverName: serverInfo?.name ?? "php-lsp",
    serverVersion: serverInfo?.version,
    serverPath: binary.serverPath,
    serverBinarySource: binary.source,
    serverBinaryExists: binary.exists,
    stubsPath: getStubsPath(context),
    platformDir: binary.platformDir,
    cacheDirs: currentWorkspaceCacheDirs(),
    lastBinaryResolutionError,
    lastStartError,
    workspaceFolders: workspace.workspaceFolders?.map((folder) => folder.uri.fsPath) ?? [],
    phpVersion: config.get<string>("phpVersion", "8.2"),
    diagnosticsMode: config.get<string>("diagnostics.mode", "basic-semantic"),
    composerEnabled: config.get<boolean>("composer.enabled", true),
    indexVendor: config.get<boolean>("indexVendor", true),
    phpstanEnabled: config.get<boolean>("phpstan.enabled", false),
    psalmEnabled: config.get<boolean>("psalm.enabled", false),
    formattingProvider: config.get<string>("formatting.provider", "auto"),
    logLevel: config.get<string>("logLevel", "info"),
    includePaths: config.get<string[]>("includePaths", []),
    excludePaths: config.get<string[]>("excludePaths", []),
  };
}

function binaryDescription(snapshot: ExtensionSnapshot): string {
  if (snapshot.serverBinarySource === "unsupported") {
    return "unsupported platform";
  }
  const source = binarySourceLabel(snapshot.serverBinarySource, snapshot.platformDir);
  return snapshot.serverBinaryExists ? source : `${source}; missing`;
}

function binarySourceLabel(source: ServerBinarySource, platformDir: string | undefined): string {
  switch (source) {
    case "custom":
      return "custom";
    case "bundled":
      return platformDir ?? "bundled";
    case "path":
      return "PATH";
    case "unsupported":
      return "unsupported platform";
  }
}

function serverVersionLabel(snapshot: ExtensionSnapshot): string {
  return `${snapshot.serverName} ${snapshot.serverVersion ?? "not initialized"}`;
}

function formatPathList(paths: string[], empty: string): string {
  if (paths.length === 0) {
    return empty;
  }
  return paths.join("\n");
}

function serverDiagnosticsDetail(snapshot: ExtensionSnapshot): string {
  return [
    `Server: ${serverVersionLabel(snapshot)}`,
    `Extension: ht-php-lsp ${snapshot.extensionVersion}`,
    `Binary source: ${binaryDescription(snapshot)}`,
    `Binary path: ${snapshot.serverPath || "unresolved"}`,
    `Platform target: ${snapshot.platformDir ?? `${os.platform()}-${os.arch()}`}`,
    `Stubs path: ${snapshot.stubsPath ?? "not found"}`,
    `Cache roots:\n${formatPathList(snapshot.cacheDirs, "No workspace cache roots")}`,
    `Last binary resolution error: ${snapshot.lastBinaryResolutionError ?? "none"}`,
    `Last start error: ${snapshot.lastStartError ?? "none"}`,
  ].join("\n");
}

async function showServerVersion(snapshot: ExtensionSnapshot): Promise<void> {
  const detail = serverDiagnosticsDetail(snapshot);
  const selected = await window.showInformationMessage(
    `PHP Language Server: ${serverVersionLabel(snapshot)}`,
    { modal: true, detail },
    "Copy Details",
    "Open Output",
  );

  if (selected === "Copy Details") {
    await env.clipboard.writeText(detail);
  } else if (selected === "Open Output") {
    getOutputChannel().show(true);
  }
}

function getOutputChannel(): OutputChannel {
  outputChannel ??= window.createOutputChannel("PHP Language Server");
  return outputChannel;
}

function lifecycleLog(message: string): void {
  getOutputChannel().appendLine(`[${new Date().toISOString()}] ${message}`);
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function enqueueLifecycleOperation(reason: string, operation: () => Promise<void>): Promise<void> {
  const run = lifecycleQueue
    .catch(() => undefined)
    .then(async () => {
      lifecycleLog(`Lifecycle begin: ${reason}`);
      try {
        await operation();
        lifecycleLog(`Lifecycle complete: ${reason}`);
      } catch (error: unknown) {
        lifecycleLog(`Lifecycle failed: ${reason}: ${errorMessage(error)}`);
        throw error;
      }
    });

  lifecycleQueue = run.catch(() => undefined);
  return run;
}

// Best-effort fallback for a timed-out stop; vscode-languageclient owns this child process.
function managedServerProcess(languageClient: LanguageClient): ChildProcess | undefined {
  return (languageClient as unknown as { _serverProcess?: ChildProcess })._serverProcess;
}

async function terminateManagedServerProcess(processToTerminate: ChildProcess | undefined, reason: string): Promise<void> {
  if (!processToTerminate || processToTerminate.pid === undefined || processToTerminate.killed) {
    lifecycleLog(`Fallback process termination skipped: reason=${reason}; process handle unavailable`);
    return;
  }

  lifecycleLog(`Fallback process termination requested: reason=${reason}; pid=${processToTerminate.pid}`);
  try {
    processToTerminate.kill();
  } catch (error: unknown) {
    lifecycleLog(`Fallback process termination failed: reason=${reason}; error=${errorMessage(error)}`);
    return;
  }

  await new Promise<void>((resolve) => {
    const timeout = setTimeout(resolve, 1000);
    processToTerminate.once("exit", () => {
      clearTimeout(timeout);
      resolve();
    });
  });
}

function getServerEnvironment(logLevel: string): NodeJS.ProcessEnv {
  return {
    ...process.env,
    RUST_LOG: logLevel.trim() || "info",
  };
}

function discoverComposerRoot(root: string): string | undefined {
  if (fs.existsSync(path.join(root, "composer.json"))) {
    return root;
  }

  const skipDirs = new Set([
    "node_modules",
    "vendor",
    ".git",
    ".github",
    "docker",
    "cache",
    "logs",
    "tmp",
  ]);

  let candidates: string[] = [];
  try {
    candidates = fs.readdirSync(root, { withFileTypes: true })
      .filter((entry) => entry.isDirectory())
      .filter((entry) => !entry.name.startsWith(".") && !skipDirs.has(entry.name))
      .map((entry) => path.join(root, entry.name))
      .filter((candidate) => fs.existsSync(path.join(candidate, "composer.json")));
  } catch {
    return undefined;
  }

  if (candidates.length === 0) {
    return undefined;
  }
  if (candidates.length === 1) {
    return candidates[0];
  }

  return candidates.find((candidate) => {
    try {
      const content = fs.readFileSync(path.join(candidate, "composer.json"), "utf8");
      return content.includes("\"autoload\"") || content.includes("\"psr-4\"");
    } catch {
      return false;
    }
  }) ?? candidates[0];
}

function currentWorkspaceCacheDirs(): string[] {
  const roots = new Set<string>();
  for (const folder of workspace.workspaceFolders ?? []) {
    roots.add(folder.uri.fsPath);
    const composerRoot = discoverComposerRoot(folder.uri.fsPath);
    if (composerRoot) {
      roots.add(composerRoot);
    }
  }

  return Array.from(new Set(Array.from(roots, phpLspCacheDirForRoot)));
}

function cacheDirectoryCountLabel(count: number): string {
  return `${count} director${count === 1 ? "y" : "ies"}`;
}

/**
 * Map Node.js os.platform()+os.arch() to the binary subdirectory name.
 */
function getPlatformDir(): string | undefined {
  const platform = os.platform();
  const arch = os.arch();

  const map: Record<string, Record<string, string>> = {
    linux:  { x64: "linux-x64",   arm64: "linux-arm64" },
    darwin: { x64: "darwin-x64",  arm64: "darwin-arm64" },
    win32:  { x64: "win32-x64",   arm64: "win32-arm64" },
  };

  return map[platform]?.[arch];
}

function isExecutableFile(candidate: string): boolean {
  try {
    const stat = fs.statSync(candidate);
    if (!stat.isFile()) {
      return false;
    }
    return os.platform() === "win32" || (stat.mode & 0o111) !== 0;
  } catch {
    return false;
  }
}

function findExecutableOnPath(binaryName: string): string | undefined {
  const pathEnv = process.env.PATH;
  if (!pathEnv) {
    return undefined;
  }

  const extensions = os.platform() === "win32" && path.extname(binaryName) === ""
    ? (process.env.PATHEXT ?? ".EXE;.CMD;.BAT;.COM")
      .split(";")
      .filter((extension) => extension.length > 0)
    : [""];

  for (const directory of pathEnv.split(path.delimiter)) {
    if (directory.length === 0) {
      continue;
    }
    for (const extension of extensions) {
      const candidate = path.join(directory, `${binaryName}${extension}`);
      if (isExecutableFile(candidate)) {
        return candidate;
      }
    }
  }

  return undefined;
}

function resolveServerBinary(context: ExtensionContext): ServerBinaryResolution {
  const config = workspace.getConfiguration("phpLsp");
  const customPath = config.get<string>("serverPath", "").trim();
  if (customPath.length > 0) {
    const exists = fs.existsSync(customPath);
    const executable = exists && isExecutableFile(customPath);
    return {
      serverPath: customPath,
      source: "custom",
      exists,
      error: !exists
        ? `Configured phpLsp.serverPath does not exist: ${customPath}`
        : executable ? undefined : `Configured phpLsp.serverPath is not executable: ${customPath}`,
    };
  }

  const platformDir = getPlatformDir();
  const binaryName = os.platform() === "win32" ? "php-lsp.exe" : "php-lsp";

  if (!platformDir) {
    const pathBinary = findExecutableOnPath("php-lsp");
    if (pathBinary) {
      return {
        serverPath: pathBinary,
        source: "path",
        exists: true,
        error: undefined,
      };
    }

    return {
      serverPath: "",
      source: "unsupported",
      exists: false,
      error: `Unsupported platform: ${os.platform()}-${os.arch()}; php-lsp was not found in PATH`,
    };
  }

  const serverPath = context.asAbsolutePath(path.join("bin", platformDir, binaryName));
  const exists = fs.existsSync(serverPath);
  const executable = exists && isExecutableFile(serverPath);
  if (executable) {
    return {
      serverPath,
      source: "bundled",
      exists: true,
      platformDir,
      error: undefined,
    };
  }

  const pathBinary = findExecutableOnPath("php-lsp");
  if (pathBinary) {
    return {
      serverPath: pathBinary,
      source: "path",
      exists: true,
      platformDir,
      error: undefined,
    };
  }

  return {
    serverPath,
    source: "bundled",
    exists,
    platformDir,
    error: exists
      ? `Bundled php-lsp binary is not executable for ${platformDir}: ${serverPath}; php-lsp was not found in PATH`
      : `Bundled php-lsp binary was not found for ${platformDir}: ${serverPath}; php-lsp was not found in PATH`,
  };
}

/**
 * Determine the path to the php-lsp server binary.
 */
function getServerBinaryForStart(context: ExtensionContext): ServerBinaryResolution {
  const binary = resolveServerBinary(context);
  if (binary.error) {
    lastBinaryResolutionError = binary.error;
    lifecycleLog(`Binary resolution failed: ${binary.error}`);
    window.showErrorMessage(`PHP Language Server: ${binary.error}`);
    throw new Error(binary.error);
  }

  lastBinaryResolutionError = undefined;
  return binary;
}

/**
 * Determine the path to bundled phpstorm-stubs.
 */
function getStubsPath(context: ExtensionContext): string | undefined {
  const stubsPath = context.asAbsolutePath("stubs");
  if (fs.existsSync(stubsPath)) {
    return stubsPath;
  }
  return undefined;
}

function hasConfiguredValue(config: ReturnType<typeof workspace.getConfiguration>, key: string): boolean {
  const inspected = config.inspect<unknown>(key);
  return inspected !== undefined && (
    inspected.globalValue !== undefined ||
    inspected.workspaceValue !== undefined ||
    inspected.workspaceFolderValue !== undefined ||
    inspected.globalLanguageValue !== undefined ||
    inspected.workspaceLanguageValue !== undefined ||
    inspected.workspaceFolderLanguageValue !== undefined
  );
}

function setIfConfigured<T>(
  target: Record<string, unknown>,
  config: ReturnType<typeof workspace.getConfiguration>,
  key: string,
  optionKey: string,
  defaultValue: T,
): void {
  if (hasConfiguredValue(config, key)) {
    target[optionKey] = config.get<T>(key, defaultValue);
  }
}

function buildInitializationOptions(config: ReturnType<typeof workspace.getConfiguration>, stubsPath: string | undefined): Record<string, unknown> {
  const options: Record<string, unknown> = {};

  setIfConfigured(options, config, "phpVersion", "phpVersion", "8.2");
  setIfConfigured(options, config, "diagnostics.mode", "diagnosticsMode", "basic-semantic");
  setIfConfigured(options, config, "diagnostics.severity", "diagnosticsSeverity", {});
  setIfConfigured(options, config, "composer.enabled", "composerEnabled", true);
  setIfConfigured(options, config, "indexVendor", "indexVendor", true);
  setIfConfigured(options, config, "includePaths", "includePaths", []);
  setIfConfigured(options, config, "excludePaths", "excludePaths", []);
  setIfConfigured(options, config, "stubs.extensions", "stubExtensions", []);
  setIfConfigured(options, config, "logLevel", "logLevel", "info");
  setIfConfigured(options, config, "formatting.provider", "formattingProvider", "auto");
  setIfConfigured(options, config, "formatting.command", "formattingCommand", "");
  setIfConfigured(options, config, "formatting.timeoutMs", "formattingTimeoutMs", 30000);
  setIfConfigured(options, config, "phpstan.enabled", "phpstanEnabled", false);
  setIfConfigured(
    options,
    config,
    "phpstan.command",
    "phpstanCommand",
    "vendor/bin/phpstan analyse --error-format=json --no-progress --no-interaction {file}",
  );
  setIfConfigured(options, config, "phpstan.timeoutMs", "phpstanTimeoutMs", 30000);
  setIfConfigured(options, config, "psalm.enabled", "psalmEnabled", false);
  setIfConfigured(
    options,
    config,
    "psalm.command",
    "psalmCommand",
    "vendor/bin/psalm --output-format=json --no-progress {file}",
  );
  setIfConfigured(options, config, "psalm.timeoutMs", "psalmTimeoutMs", 30000);
  setIfConfigured(
    options,
    config,
    "analyzerCodeActions.enabled",
    "analyzerCodeActionsEnabled",
    false,
  );

  if (stubsPath) {
    options.bundledStubsPath = stubsPath;
  }

  return options;
}

function createLanguageClient(context: ExtensionContext, binary: ServerBinaryResolution): LanguageClient {
  const config = workspace.getConfiguration("phpLsp");
  const stubsPath = getStubsPath(context);
  const logLevel = config.get<string>("logLevel", "info");
  const serverEnvironment = getServerEnvironment(logLevel);

  const serverOptions: ServerOptions = {
    run: {
      command: binary.serverPath,
      transport: TransportKind.stdio,
      options: {
        env: serverEnvironment,
      },
    },
    debug: {
      command: binary.serverPath,
      transport: TransportKind.stdio,
      args: ["--debug"],
      options: {
        env: serverEnvironment,
      },
    },
  };

  const clientOptions: LanguageClientOptions = {
    outputChannel: getOutputChannel(),
    documentSelector: [
      { scheme: "file", language: "php" },
      { scheme: "untitled", language: "php" },
      { scheme: "file", language: "blade" },
      { scheme: "untitled", language: "blade" },
    ],
    synchronize: {
      fileEvents: [
        workspace.createFileSystemWatcher("**/*.php"),
        workspace.createFileSystemWatcher("**/.php-lsp.toml"),
      ],
    },
    initializationOptions: buildInitializationOptions(config, stubsPath),
  };

  return new LanguageClient(
    "phpLsp",
    "PHP Language Server",
    serverOptions,
    clientOptions,
  );
}

async function notifyServerConfigurationChanged(context: ExtensionContext): Promise<void> {
  if (!client?.isRunning()) {
    return;
  }

  const config = workspace.getConfiguration("phpLsp");
  await client.sendNotification("workspace/didChangeConfiguration", {
    settings: buildInitializationOptions(config, getStubsPath(context)),
  });
}

async function stopLanguageClient(reason: string): Promise<void> {
  indexingStatusSubscription?.dispose();
  indexingStatusSubscription = undefined;

  const currentClient = client;
  client = undefined;

  if (!currentClient) {
    lifecycleLog(`Stop skipped: reason=${reason}; no active client`);
    return;
  }

  const processToTerminate = managedServerProcess(currentClient);
  lifecycleLog(`Stopping language server: reason=${reason}; timeoutMs=${STOP_TIMEOUT_MS}`);
  try {
    await currentClient.stop(STOP_TIMEOUT_MS);
    lifecycleLog(`Stopped language server: reason=${reason}`);
  } catch (error: unknown) {
    lifecycleLog(
      `Stop failed or timed out: reason=${reason}; error=${errorMessage(error)}; managed process termination requested when available`,
    );
    await terminateManagedServerProcess(processToTerminate, reason);
  }
}

async function startLanguageClient(context: ExtensionContext, reason: string): Promise<boolean> {
  try {
    const binary = getServerBinaryForStart(context);
    lifecycleLog(
      `Starting language server: reason=${reason}; source=${binarySourceLabel(binary.source, binary.platformDir)}; path=${binary.serverPath}; platform=${binary.platformDir ?? `${os.platform()}-${os.arch()}`}`,
    );

    const nextClient = createLanguageClient(context, binary);
    client = nextClient;
    indexingStatusSubscription = nextClient.onNotification(
      "phpLsp/indexingStatus",
      (status: IndexingStatus) => statusController?.update(status),
    );

    await nextClient.start();
    lastStartError = undefined;
    lifecycleLog(`Started language server: reason=${reason}; path=${binary.serverPath}`);
    return true;
  } catch (error: unknown) {
    const message = errorMessage(error);
    lastStartError = message;
    client = undefined;
    indexingStatusSubscription?.dispose();
    indexingStatusSubscription = undefined;
    statusController?.update({
      phase: "error",
      message,
    });
    window.showErrorMessage(`PHP Language Server failed to start: ${message}`);
    return false;
  }
}

async function restartLanguageClient(context: ExtensionContext): Promise<void> {
  await enqueueLifecycleOperation("restart command", async () => {
    statusController?.update({
      phase: "starting",
      message: "Restarting language server",
    });
    await stopLanguageClient("restart command");
    if (!workspace.getConfiguration("phpLsp").get<boolean>("enable", true)) {
      statusController?.update({
        phase: "ready",
        message: "Language server is disabled",
      });
      window.showInformationMessage("PHP Language Server is disabled");
      return;
    }
    if (await startLanguageClient(context, "restart command")) {
      window.showInformationMessage("PHP Language Server restarted");
    }
  });
}

async function clearCacheAndRestartLanguageClient(context: ExtensionContext): Promise<void> {
  const cacheDirs = currentWorkspaceCacheDirs();
  if (cacheDirs.length === 0) {
    window.showInformationMessage("PHP LSP cache was not cleared: no workspace folder is open");
    return;
  }

  const confirmed = await window.showWarningMessage(
    `Clear PHP LSP disk cache for ${cacheDirs.length} workspace root(s) and restart the language server?`,
    { modal: true },
    "Clear Cache and Restart",
  );
  if (confirmed !== "Clear Cache and Restart") {
    return;
  }

  await enqueueLifecycleOperation("clear cache and restart command", async () => {
    statusController?.update({
      phase: "starting",
      message: "Clearing disk cache and restarting language server",
    });

    await stopLanguageClient("clear cache and restart command");

    const failed: string[] = [];
    let removed = 0;
    for (const cacheDir of cacheDirs) {
      try {
        if (fs.existsSync(cacheDir)) {
          lifecycleLog(`Removing cache directory: ${cacheDir}`);
          await fs.promises.rm(cacheDir, { recursive: true, force: true });
          removed += 1;
        }
      } catch (error: unknown) {
        failed.push(`${cacheDir}: ${errorMessage(error)}`);
      }
    }

    if (failed.length > 0) {
      const message = `Failed to clear PHP LSP cache: ${failed.join("; ")}`;
      statusController?.update({
        phase: "error",
        message,
      });
      window.showErrorMessage(message);
      return;
    }

    if (!workspace.getConfiguration("phpLsp").get<boolean>("enable", true)) {
      statusController?.update({
        phase: "ready",
        message: "Language server is disabled",
      });
      window.showInformationMessage(
        `PHP LSP cache cleared (${cacheDirectoryCountLabel(removed)} removed). Language server is disabled.`,
      );
      return;
    }

    if (await startLanguageClient(context, "clear cache and restart command")) {
      window.showInformationMessage(
        `PHP LSP cache cleared (${cacheDirectoryCountLabel(removed)} removed) and language server restarted`,
      );
    }
  });
}

export function activate(context: ExtensionContext): void {
  const config = workspace.getConfiguration("phpLsp");

  const controller = new PhpLspStatusController(() => getExtensionSnapshot(context));
  statusController = controller;

  // Register restart command
  const restartCommand = commands.registerCommand(
    "phpLsp.restartServer",
    async () => restartLanguageClient(context),
  );

  const clearCacheCommand = commands.registerCommand(
    "phpLsp.clearCacheAndRestart",
    async () => clearCacheAndRestartLanguageClient(context),
  );

  const showStatusCommand = commands.registerCommand(
    "phpLsp.showStatus",
    async () => statusController?.showPopup(),
  );

  const showServerVersionCommand = commands.registerCommand(
    "phpLsp.showServerVersion",
    async () => showServerVersion(getExtensionSnapshot(context)),
  );

  const enableConfigSubscription = workspace.onDidChangeConfiguration(async (event) => {
    if (!event.affectsConfiguration("phpLsp")) {
      return;
    }

    const enabled = workspace.getConfiguration("phpLsp").get<boolean>("enable", true);
    if (!enabled) {
      await enqueueLifecycleOperation("disable setting changed", async () => {
        await stopLanguageClient("disable setting changed");
        statusController?.update({
          phase: "ready",
          message: "Language server is disabled",
        });
      });
    } else if (!client) {
      await enqueueLifecycleOperation("enable setting changed", async () => {
        statusController?.update({
          phase: "starting",
          message: "Starting language server",
        });
        await startLanguageClient(context, "enable setting changed");
      });
    } else {
      await notifyServerConfigurationChanged(context);
    }
  });

  context.subscriptions.push(
    controller,
    restartCommand,
    clearCacheCommand,
    showStatusCommand,
    showServerVersionCommand,
    enableConfigSubscription,
  );

  if (!config.get<boolean>("enable", true)) {
    lifecycleLog("Extension activated with phpLsp.enable=false; server start skipped");
    statusController?.update({
      phase: "ready",
      message: "Language server is disabled",
    });
    return;
  }

  // Start the client (also launches the server)
  void enqueueLifecycleOperation("extension activation", async () => {
    await startLanguageClient(context, "extension activation");
  });
}

export async function deactivate(): Promise<void> {
  await enqueueLifecycleOperation("extension deactivation", async () => {
    await stopLanguageClient("extension deactivation");
  });
  statusController?.dispose();
  statusController = undefined;
  outputChannel?.dispose();
  outputChannel = undefined;
}
