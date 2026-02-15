import * as path from "path";
import * as os from "os";
import {
  workspace,
  commands,
  window,
  ExtensionContext,
} from "vscode";
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  TransportKind,
} from "vscode-languageclient/node";

let client: LanguageClient | undefined;

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

  // Use bundled binary from extension's bin/ folder
  const platform = os.platform();
  const binaryName = platform === "win32" ? "php-lsp.exe" : "php-lsp";
  return context.asAbsolutePath(path.join("bin", binaryName));
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
      fileEvents: workspace.createFileSystemWatcher("**/*.php"),
    },
    initializationOptions: {
      phpVersion: config.get<string>("phpVersion", "8.2"),
      diagnosticsMode: config.get<string>("diagnostics.mode", "basic-semantic"),
      composerEnabled: config.get<boolean>("composer.enabled", true),
      indexVendor: config.get<boolean>("indexVendor", true),
      stubExtensions: config.get<string[]>("stubs.extensions", []),
      logLevel: config.get<string>("logLevel", "info"),
      stubsPath: stubsPath,
    },
  };

  client = new LanguageClient(
    "phpLsp",
    "PHP Language Server",
    serverOptions,
    clientOptions,
  );

  // Register restart command
  const restartCommand = commands.registerCommand(
    "phpLsp.restartServer",
    async () => {
      if (client) {
        await client.restart();
        window.showInformationMessage("PHP Language Server restarted");
      }
    },
  );

  context.subscriptions.push(restartCommand);

  // Start the client (also launches the server)
  client.start();
}

export async function deactivate(): Promise<void> {
  if (client) {
    await client.stop();
    client = undefined;
  }
}
