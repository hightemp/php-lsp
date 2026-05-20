import * as path from "path";
import * as os from "os";
import {
  workspace,
  commands,
  window,
  ExtensionContext,
  StatusBarAlignment,
  StatusBarItem,
  QuickPickItem,
  Disposable,
  MarkdownString,
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
  serverPath: string;
  stubsPath?: string;
  platformDir?: string;
  workspaceFolders: string[];
  phpVersion: string;
  diagnosticsMode: string;
  composerEnabled: boolean;
  indexVendor: boolean;
  phpstanEnabled: boolean;
  psalmEnabled: boolean;
  formattingProvider: string;
  includePaths: string[];
  excludePaths: string[];
}

interface StatusQuickPickItem extends QuickPickItem {
  action?: "restart" | "output" | "settings";
}

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
        detail: snapshot.formattingProvider === "none" ? "Document formatting is disabled" : "External formatter is configured",
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
        description: snapshot.platformDir ?? "custom",
        detail: snapshot.serverPath,
      },
      {
        label: "$(debug-restart) Restart language server",
        action: "restart",
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

    if (selected?.action === "restart") {
      await commands.executeCommand("phpLsp.restartServer");
    } else if (selected?.action === "output") {
      client?.outputChannel.show(true);
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

function getExtensionSnapshot(serverPath: string, stubsPath: string | undefined): ExtensionSnapshot {
  const config = workspace.getConfiguration("phpLsp");
  return {
    serverPath,
    stubsPath,
    platformDir: getPlatformDir(),
    workspaceFolders: workspace.workspaceFolders?.map((folder) => folder.uri.fsPath) ?? [],
    phpVersion: config.get<string>("phpVersion", "8.2"),
    diagnosticsMode: config.get<string>("diagnostics.mode", "basic-semantic"),
    composerEnabled: config.get<boolean>("composer.enabled", true),
    indexVendor: config.get<boolean>("indexVendor", true),
    phpstanEnabled: config.get<boolean>("phpstan.enabled", false),
    psalmEnabled: config.get<boolean>("psalm.enabled", false),
    formattingProvider: config.get<string>("formatting.provider", "none"),
    includePaths: config.get<string[]>("includePaths", []),
    excludePaths: config.get<string[]>("excludePaths", []),
  };
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

/**
 * Determine the path to the php-lsp server binary.
 */
function getServerPath(context: ExtensionContext): string {
  // Check user-configured path first
  const config = workspace.getConfiguration("phpLsp");
  const customPath = config.get<string>("serverPath", "");
  if (customPath) {
    return customPath;
  }

  // Use bundled binary from bin/<platform>/ subdirectory
  const platformDir = getPlatformDir();
  if (!platformDir) {
    const msg = `Unsupported platform: ${os.platform()}-${os.arch()}`;
    window.showErrorMessage(`PHP Language Server: ${msg}`);
    throw new Error(msg);
  }

  const binaryName = os.platform() === "win32" ? "php-lsp.exe" : "php-lsp";
  return context.asAbsolutePath(path.join("bin", platformDir, binaryName));
}

/**
 * Determine the path to bundled phpstorm-stubs.
 */
function getStubsPath(context: ExtensionContext): string | undefined {
  const stubsPath = context.asAbsolutePath("stubs");
  const fs = require("fs");
  if (fs.existsSync(stubsPath)) {
    return stubsPath;
  }
  return undefined;
}

export function activate(context: ExtensionContext): void {
  const config = workspace.getConfiguration("phpLsp");
  if (!config.get<boolean>("enable", true)) {
    return;
  }

  const serverPath = getServerPath(context);
  const stubsPath = getStubsPath(context);

  const serverOptions: ServerOptions = {
    run: {
      command: serverPath,
      transport: TransportKind.stdio,
    },
    debug: {
      command: serverPath,
      transport: TransportKind.stdio,
      args: ["--debug"],
    },
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [
      { scheme: "file", language: "php" },
      { scheme: "untitled", language: "php" },
    ],
    synchronize: {
      configurationSection: "phpLsp",
      fileEvents: workspace.createFileSystemWatcher("**/*.php"),
    },
    initializationOptions: {
      phpVersion: config.get<string>("phpVersion", "8.2"),
      diagnosticsMode: config.get<string>("diagnostics.mode", "basic-semantic"),
      composerEnabled: config.get<boolean>("composer.enabled", true),
      indexVendor: config.get<boolean>("indexVendor", true),
      includePaths: config.get<string[]>("includePaths", []),
      excludePaths: config.get<string[]>("excludePaths", []),
      stubExtensions: config.get<string[]>("stubs.extensions", []),
      logLevel: config.get<string>("logLevel", "info"),
      formattingProvider: config.get<string>("formatting.provider", "none"),
      formattingCommand: config.get<string>("formatting.command", ""),
      phpstanEnabled: config.get<boolean>("phpstan.enabled", false),
      phpstanCommand: config.get<string>(
        "phpstan.command",
        "vendor/bin/phpstan analyse --error-format=json --no-progress --no-interaction {file}",
      ),
      phpstanTimeoutMs: config.get<number>("phpstan.timeoutMs", 30000),
      psalmEnabled: config.get<boolean>("psalm.enabled", false),
      psalmCommand: config.get<string>(
        "psalm.command",
        "vendor/bin/psalm --output-format=json --no-progress {file}",
      ),
      psalmTimeoutMs: config.get<number>("psalm.timeoutMs", 30000),
      stubsPath: stubsPath,
    },
  };

  client = new LanguageClient(
    "phpLsp",
    "PHP Language Server",
    serverOptions,
    clientOptions,
  );

  const controller = new PhpLspStatusController(() => getExtensionSnapshot(serverPath, stubsPath));
  statusController = controller;

  // Register restart command
  const restartCommand = commands.registerCommand(
    "phpLsp.restartServer",
    async () => {
      if (client) {
        statusController?.update({
          phase: "starting",
          message: "Restarting language server",
        });
        await client.restart();
        window.showInformationMessage("PHP Language Server restarted");
      }
    },
  );

  const showStatusCommand = commands.registerCommand(
    "phpLsp.showStatus",
    async () => statusController?.showPopup(),
  );

  const indexingStatusSubscription = client.onNotification(
    "phpLsp/indexingStatus",
    (status: IndexingStatus) => statusController?.update(status),
  );

  context.subscriptions.push(controller, restartCommand, showStatusCommand, indexingStatusSubscription);

  // Start the client (also launches the server)
  client.start().catch((error: unknown) => {
    const message = error instanceof Error ? error.message : String(error);
    statusController?.update({
      phase: "error",
      message,
    });
    window.showErrorMessage(`PHP Language Server failed to start: ${message}`);
  });
}

export async function deactivate(): Promise<void> {
  if (client) {
    await client.stop();
    client = undefined;
  }
  statusController?.dispose();
  statusController = undefined;
}
