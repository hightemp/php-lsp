export interface ConfigurationInspection<T> {
  globalValue?: T;
  workspaceValue?: T;
  workspaceFolderValue?: T;
  globalLanguageValue?: T;
  workspaceLanguageValue?: T;
  workspaceFolderLanguageValue?: T;
}

export interface ConfigurationReader {
  inspect<T>(key: string): ConfigurationInspection<T> | undefined;
  get<T>(key: string, defaultValue: T): T;
}

export interface WorkspaceConfigurationSource {
  uri: string;
  configuration: ConfigurationReader;
}

export interface ClientConfigurationSnapshot {
  configurationVersion: 2;
  global: Record<string, unknown>;
  workspaceFolders: Array<{
    uri: string;
    settings: Record<string, unknown>;
  }>;
  bundledStubsPath?: string;
}

export interface StatusConfigurationResource<TConfiguration> {
  configuration: TConfiguration;
  resourceUri: string;
  workspaceFolderLabel?: string;
}

export interface StatusConfigurationSelection<TConfiguration> {
  configuration: TConfiguration;
  scopeLabel: string;
  resourceUri?: string;
}

export function selectStatusConfiguration<TConfiguration>(
  fallbackConfiguration: TConfiguration,
  activeResource?: StatusConfigurationResource<TConfiguration>,
): StatusConfigurationSelection<TConfiguration> {
  if (!activeResource) {
    return {
      configuration: fallbackConfiguration,
      scopeLabel: "workspace defaults",
    };
  }

  return {
    configuration: activeResource.configuration,
    scopeLabel: activeResource.workspaceFolderLabel ?? "outside workspace fallback",
    resourceUri: activeResource.resourceUri,
  };
}

function hasExplicitValue(config: ConfigurationReader, key: string): boolean {
  const inspected = config.inspect<unknown>(key);
  return inspected !== undefined && (
    inspected.globalValue !== undefined
    || inspected.workspaceValue !== undefined
    || inspected.workspaceFolderValue !== undefined
    || inspected.globalLanguageValue !== undefined
    || inspected.workspaceLanguageValue !== undefined
    || inspected.workspaceFolderLanguageValue !== undefined
  );
}

function setExplicitValue<T>(
  target: Record<string, unknown>,
  config: ConfigurationReader,
  key: string,
  optionKey: string,
  defaultValue: T,
): void {
  if (hasExplicitValue(config, key)) {
    target[optionKey] = config.get<T>(key, defaultValue);
  }
}

export function buildExplicitClientSettings(
  config: ConfigurationReader,
  stubsPath: string | undefined,
): Record<string, unknown> {
  const options: Record<string, unknown> = {};

  setExplicitValue(options, config, "phpVersion", "phpVersion", "8.2");
  setExplicitValue(options, config, "diagnostics.mode", "diagnosticsMode", "basic-semantic");
  setExplicitValue(options, config, "diagnostics.severity", "diagnosticsSeverity", {});
  setExplicitValue(
    options,
    config,
    "diagnostics.memberTypeNodeBudget",
    "diagnosticsMemberTypeNodeBudget",
    512,
  );
  setExplicitValue(
    options,
    config,
    "diagnostics.partialAnalysisDiagnostic",
    "diagnosticsPartialAnalysisDiagnostic",
    true,
  );
  setExplicitValue(options, config, "composer.enabled", "composerEnabled", true);
  setExplicitValue(options, config, "indexVendor", "indexVendor", true);
  setExplicitValue(options, config, "includePaths", "includePaths", []);
  setExplicitValue(options, config, "excludePaths", "excludePaths", []);
  setExplicitValue(options, config, "indexing.maxFiles", "indexingMaxFiles", 100000);
  setExplicitValue(options, config, "indexing.maxEntries", "indexingMaxEntries", 1000000);
  setExplicitValue(options, config, "stubs.extensions", "stubExtensions", []);
  setExplicitValue(options, config, "logLevel", "logLevel", "info");
  setExplicitValue(options, config, "allowProjectCommands", "allowProjectCommands", false);
  setExplicitValue(options, config, "formatting.provider", "formattingProvider", "auto");
  setExplicitValue(options, config, "formatting.command", "formattingCommand", "");
  setExplicitValue(options, config, "formatting.timeoutMs", "formattingTimeoutMs", 30000);
  setExplicitValue(options, config, "phpstan.enabled", "phpstanEnabled", false);
  setExplicitValue(
    options,
    config,
    "phpstan.command",
    "phpstanCommand",
    "vendor/bin/phpstan analyse --error-format=json --no-progress --no-interaction {file}",
  );
  setExplicitValue(options, config, "phpstan.timeoutMs", "phpstanTimeoutMs", 30000);
  setExplicitValue(options, config, "psalm.enabled", "psalmEnabled", false);
  setExplicitValue(
    options,
    config,
    "psalm.command",
    "psalmCommand",
    "vendor/bin/psalm --output-format=json --no-progress {file}",
  );
  setExplicitValue(options, config, "psalm.timeoutMs", "psalmTimeoutMs", 30000);
  setExplicitValue(
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

/**
 * Build the versioned configuration payload consumed by the server.
 *
 * VS Code resource settings are resolved independently for every workspace
 * folder. Schema defaults remain omitted so a project `.php-lsp.toml` value is
 * revealed again when a user override is removed.
 */
export function buildClientConfigurationSnapshot(
  globalConfiguration: ConfigurationReader,
  workspaceFolders: readonly WorkspaceConfigurationSource[],
  stubsPath: string | undefined,
): ClientConfigurationSnapshot {
  const snapshot: ClientConfigurationSnapshot = {
    configurationVersion: 2,
    global: buildExplicitClientSettings(globalConfiguration, undefined),
    workspaceFolders: workspaceFolders.map((folder) => ({
      uri: folder.uri,
      settings: buildExplicitClientSettings(folder.configuration, undefined),
    })),
  };

  if (stubsPath) {
    snapshot.bundledStubsPath = stubsPath;
  }

  return snapshot;
}
