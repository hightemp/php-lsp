use crate::server::{
    collect_php_files, compute_diagnostics_with_runtime_config,
    diagnostic_budget_config_from_settings, discover_workspace_root_config,
    lazy_resolvable_diagnostic_fqn, load_configured_stubs, load_effective_configuration_settings,
    normalize_config_paths, parse_vendor_autoload_map, path_is_excluded,
    resolve_vendor_paths_from_map, workspace_index_directories, DiagnosticBudgetConfig,
    DiagnosticSeverityConfig, DiagnosticsMode, DiagnosticsRuntimeConfig, PhpVersion,
    VendorAutoloadMap, VENDOR_PRELOAD_ENTRYPOINT_LIMIT,
};
use crate::util::uri::path_to_uri;
use php_lsp_index::workspace::WorkspaceIndex;
use php_lsp_parser::parser::FileParser;
use php_lsp_parser::references::collect_symbol_references_in_file;
use php_lsp_parser::semantic::collect_aliased_class_fqns;
use php_lsp_parser::symbols::extract_file_symbols;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tower_lsp::ls_types::{Diagnostic, DiagnosticSeverity, NumberOrString};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalyzeFormat {
    Table,
    Json,
    Github,
}

impl AnalyzeFormat {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "table" => Some(Self::Table),
            "json" => Some(Self::Json),
            "github" => Some(Self::Github),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalyzeSeverity {
    All,
    Hint,
    Info,
    Warning,
    Error,
}

impl AnalyzeSeverity {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "all" => Some(Self::All),
            "hint" => Some(Self::Hint),
            "info" | "information" => Some(Self::Info),
            "warning" | "warn" => Some(Self::Warning),
            "error" => Some(Self::Error),
            _ => None,
        }
    }

    fn includes(self, severity: Option<DiagnosticSeverity>) -> bool {
        match self {
            Self::All => true,
            Self::Hint => matches!(
                severity,
                Some(
                    DiagnosticSeverity::ERROR
                        | DiagnosticSeverity::WARNING
                        | DiagnosticSeverity::INFORMATION
                        | DiagnosticSeverity::HINT
                )
            ),
            Self::Info => matches!(
                severity,
                Some(
                    DiagnosticSeverity::ERROR
                        | DiagnosticSeverity::WARNING
                        | DiagnosticSeverity::INFORMATION
                )
            ),
            Self::Warning => matches!(
                severity,
                Some(DiagnosticSeverity::ERROR | DiagnosticSeverity::WARNING)
            ),
            Self::Error => severity == Some(DiagnosticSeverity::ERROR),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalyzeArgs {
    pub path: Option<PathBuf>,
    pub project_root: Option<PathBuf>,
    pub severity: AnalyzeSeverity,
    pub format: AnalyzeFormat,
}

impl Default for AnalyzeArgs {
    fn default() -> Self {
        Self {
            path: None,
            project_root: None,
            severity: AnalyzeSeverity::All,
            format: AnalyzeFormat::Table,
        }
    }
}

#[derive(Debug)]
pub struct AnalyzeCliResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug)]
pub struct AnalyzeError {
    message: String,
}

impl AnalyzeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for AnalyzeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for AnalyzeError {}

struct ParsedAnalyzeFile {
    path: PathBuf,
    uri: String,
    parser: FileParser,
}

#[derive(Debug)]
struct AnalyzeRuntimeConfig {
    php_version: PhpVersion,
    diagnostics_mode: DiagnosticsMode,
    diagnostic_severity: DiagnosticSeverityConfig,
    diagnostic_budget: DiagnosticBudgetConfig,
    composer_enabled: bool,
    index_vendor: bool,
    stubs_path: Option<PathBuf>,
    stub_extensions: Option<Vec<String>>,
    include_paths: Vec<PathBuf>,
    exclude_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
struct AnalyzeDiagnostic {
    path: PathBuf,
    uri: String,
    diagnostic: Diagnostic,
}

#[derive(Debug)]
struct AnalyzeReport {
    project_root: PathBuf,
    target: PathBuf,
    files_analyzed: usize,
    diagnostics: Vec<AnalyzeDiagnostic>,
}

struct AnalyzeLazyIndexContext<'a> {
    index: &'a WorkspaceIndex,
    project_root: &'a Path,
    namespace_map: Option<&'a php_lsp_index::composer::NamespaceMap>,
    runtime_config: &'a AnalyzeRuntimeConfig,
    vendor_map: Option<&'a VendorAutoloadMap>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonAnalyzeReport {
    schema_version: u32,
    project_root: String,
    target: String,
    summary: JsonAnalyzeSummary,
    diagnostics: Vec<JsonAnalyzeDiagnostic>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonAnalyzeSummary {
    files_analyzed: usize,
    diagnostics: usize,
    errors: usize,
    warnings: usize,
    information: usize,
    hints: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonAnalyzeDiagnostic {
    path: String,
    uri: String,
    range: JsonAnalyzeRange,
    severity: String,
    source: Option<String>,
    code: Option<String>,
    message: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonAnalyzeRange {
    start: JsonAnalyzePosition,
    end: JsonAnalyzePosition,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonAnalyzePosition {
    line: u32,
    character: u32,
}

pub fn parse_analyze_args(raw_args: Vec<String>) -> Result<AnalyzeArgs, AnalyzeError> {
    let mut parsed = AnalyzeArgs::default();
    let mut iter = raw_args.into_iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--project-root" => {
                let value = iter
                    .next()
                    .ok_or_else(|| AnalyzeError::new("Missing value for --project-root"))?;
                parsed.project_root = Some(PathBuf::from(value));
            }
            "--severity" => {
                let value = iter
                    .next()
                    .ok_or_else(|| AnalyzeError::new("Missing value for --severity"))?;
                parsed.severity = AnalyzeSeverity::parse(&value).ok_or_else(|| {
                    AnalyzeError::new(format!(
                        "Invalid --severity `{value}`; expected all, hint, info, warning, or error"
                    ))
                })?;
            }
            "--format" => {
                let value = iter
                    .next()
                    .ok_or_else(|| AnalyzeError::new("Missing value for --format"))?;
                parsed.format = AnalyzeFormat::parse(&value).ok_or_else(|| {
                    AnalyzeError::new(format!(
                        "Invalid --format `{value}`; expected table, json, or github"
                    ))
                })?;
            }
            "--help" | "-h" => {
                return Err(AnalyzeError::new(analyze_help()));
            }
            value if value.starts_with('-') => {
                return Err(AnalyzeError::new(format!(
                    "Unknown analyze option `{value}`"
                )));
            }
            value => {
                if parsed.path.is_some() {
                    return Err(AnalyzeError::new(format!(
                        "Unexpected extra analyze path `{value}`"
                    )));
                }
                parsed.path = Some(PathBuf::from(value));
            }
        }
    }

    Ok(parsed)
}

pub fn analyze_help() -> &'static str {
    "Usage:\n  php-lsp analyze [PATH] [--project-root <DIR>] [--severity <all|hint|info|warning|error>] [--format <table|json|github>]"
}

pub fn run_analyze_cli(raw_args: Vec<String>) -> AnalyzeCliResult {
    if raw_args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h" | "help"))
    {
        return AnalyzeCliResult {
            exit_code: 0,
            stdout: format!("{}\n", analyze_help()),
            stderr: String::new(),
        };
    }

    let args = match parse_analyze_args(raw_args) {
        Ok(args) => args,
        Err(err) => {
            return AnalyzeCliResult {
                exit_code: 1,
                stdout: String::new(),
                stderr: format!("{err}\n"),
            };
        }
    };

    match run_analyze(&args) {
        Ok(report) => {
            let stdout = render_report(&report, args.format);
            let exit_code = if report.diagnostics.is_empty() { 0 } else { 2 };
            AnalyzeCliResult {
                exit_code,
                stdout,
                stderr: String::new(),
            }
        }
        Err(err) => AnalyzeCliResult {
            exit_code: 1,
            stdout: String::new(),
            stderr: format!("{err}\n"),
        },
    }
}

fn run_analyze(args: &AnalyzeArgs) -> Result<AnalyzeReport, AnalyzeError> {
    let cwd = std::env::current_dir()
        .map_err(|err| AnalyzeError::new(format!("Failed to read current directory: {err}")))?;
    let explicit_project_root = args.project_root.is_some();
    let default_target = args.path.is_none();
    let requested_project_root = args.project_root.clone().unwrap_or_else(|| cwd.clone());
    let requested_project_root = resolve_existing_path(&cwd, &requested_project_root)?;
    let requested_target = match args.path.as_ref() {
        Some(path) if path.is_absolute() => path.clone(),
        Some(path) if explicit_project_root => requested_project_root.join(path),
        Some(path) => cwd.join(path),
        None => requested_project_root.clone(),
    };
    let requested_target = resolve_existing_path(&cwd, &requested_target)?;

    let (settings, messages) = load_effective_configuration_settings(
        std::slice::from_ref(&requested_project_root),
        &serde_json::json!({}),
    );
    if let Some(message) = messages.iter().find(|message| message.contains("failed")) {
        return Err(AnalyzeError::new(message.clone()));
    }

    let runtime_config = analyze_runtime_config(&settings);
    let workspace_config =
        discover_workspace_root_config(&requested_project_root, runtime_config.composer_enabled);
    let project_root = workspace_config.root;
    let requested_target = if default_target {
        project_root.clone()
    } else {
        requested_target
    };
    let workspace_files = collect_workspace_analyze_files(
        &project_root,
        workspace_config.namespace_map.as_ref(),
        &runtime_config.include_paths,
        &runtime_config.exclude_paths,
    );
    let target_files = collect_target_analyze_files(
        &requested_target,
        &project_root,
        &runtime_config.exclude_paths,
    )?;

    let mut all_files = workspace_files.clone();
    for file in &target_files {
        push_unique_path(&mut all_files, file.clone());
    }
    all_files.sort();

    let index = WorkspaceIndex::new();
    load_configured_stubs(
        &index,
        &project_root,
        runtime_config.stubs_path.clone(),
        runtime_config.stub_extensions.clone(),
        runtime_config.php_version,
        false,
    );
    let vendor_autoload_map = runtime_config
        .index_vendor
        .then(|| parse_vendor_autoload_map(&project_root.join("vendor")))
        .flatten();
    if let Some(vendor_map) = vendor_autoload_map.as_ref() {
        preload_analyze_vendor_entrypoints(&index, &project_root, &runtime_config, vendor_map);
    }

    let mut parsed_by_path = HashMap::new();
    for path in all_files {
        let parsed = parse_analyze_file(&path)?;
        index_analyze_file(&index, &parsed);
        parsed_by_path.insert(path, parsed);
    }

    let lazy_index_context = AnalyzeLazyIndexContext {
        index: &index,
        project_root: &project_root,
        namespace_map: workspace_config.namespace_map.as_ref(),
        runtime_config: &runtime_config,
        vendor_map: vendor_autoload_map.as_ref(),
    };

    for target_file in &target_files {
        let parsed = parsed_by_path.get(target_file).ok_or_else(|| {
            AnalyzeError::new(format!("Failed to parse {}", target_file.display()))
        })?;
        pre_resolve_analyze_file_dependencies(parsed, &lazy_index_context);
    }

    let mut diagnostics = Vec::new();
    for target_file in &target_files {
        let parsed = parsed_by_path.get(target_file).ok_or_else(|| {
            AnalyzeError::new(format!("Failed to parse {}", target_file.display()))
        })?;
        let file_diagnostics = compute_diagnostics_with_runtime_config(
            &parsed.uri,
            &parsed.parser,
            &index,
            DiagnosticsRuntimeConfig {
                mode: runtime_config.diagnostics_mode,
                severity: runtime_config.diagnostic_severity,
                budget: runtime_config.diagnostic_budget,
                php_version: runtime_config.php_version,
            },
            None,
        );
        let file_diagnostics =
            filter_analyze_lazy_resolved_symbol_diagnostics(file_diagnostics, &lazy_index_context);
        diagnostics.extend(file_diagnostics.into_iter().filter_map(|diagnostic| {
            args.severity
                .includes(diagnostic.severity)
                .then(|| AnalyzeDiagnostic {
                    path: parsed.path.clone(),
                    uri: parsed.uri.clone(),
                    diagnostic,
                })
        }));
    }

    diagnostics.sort_by(|left, right| {
        (
            display_path(&left.path, &project_root),
            left.diagnostic.range.start.line,
            left.diagnostic.range.start.character,
            severity_sort_key(left.diagnostic.severity),
            left.diagnostic.message.as_str(),
        )
            .cmp(&(
                display_path(&right.path, &project_root),
                right.diagnostic.range.start.line,
                right.diagnostic.range.start.character,
                severity_sort_key(right.diagnostic.severity),
                right.diagnostic.message.as_str(),
            ))
    });

    Ok(AnalyzeReport {
        project_root,
        target: requested_target,
        files_analyzed: target_files.len(),
        diagnostics,
    })
}

fn analyze_runtime_config(settings: &serde_json::Value) -> AnalyzeRuntimeConfig {
    let settings = settings.get("phpLsp").unwrap_or(settings);
    let php_version = settings_string(settings, "phpVersion", &["phpVersion"])
        .and_then(PhpVersion::parse)
        .unwrap_or(PhpVersion::DEFAULT);
    let diagnostics_mode = settings_string(settings, "diagnosticsMode", &["diagnostics", "mode"])
        .and_then(DiagnosticsMode::parse)
        .unwrap_or_default();
    let diagnostic_severity = settings_value(
        settings,
        "diagnosticsSeverity",
        &["diagnostics", "severity"],
    )
    .and_then(DiagnosticSeverityConfig::parse)
    .unwrap_or_default();
    let diagnostic_budget = diagnostic_budget_config_from_settings(settings);
    let composer_enabled =
        settings_bool(settings, "composerEnabled", &["composer", "enabled"]).unwrap_or(true);
    let index_vendor = settings_bool(settings, "indexVendor", &["indexVendor"]).unwrap_or(true);
    let stubs_path = settings_string_any(
        settings,
        "stubsPath",
        &[&["stubs", "path"][..], &["bundledStubsPath"][..]],
    )
    .and_then(|path| {
        let trimmed = path.trim();
        (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
    });
    let stub_extensions =
        settings_string_array(settings, "stubExtensions", &["stubs", "extensions"]);
    let include_paths = settings_string_array(settings, "includePaths", &["includePaths"])
        .map(normalize_config_paths)
        .unwrap_or_default();
    let exclude_paths = settings_string_array(settings, "excludePaths", &["excludePaths"])
        .map(normalize_config_paths)
        .unwrap_or_default();

    AnalyzeRuntimeConfig {
        php_version,
        diagnostics_mode,
        diagnostic_severity,
        diagnostic_budget,
        composer_enabled,
        index_vendor,
        stubs_path,
        stub_extensions,
        include_paths,
        exclude_paths,
    }
}

fn settings_value<'a>(
    settings: &'a serde_json::Value,
    flat_key: &str,
    nested_path: &[&str],
) -> Option<&'a serde_json::Value> {
    if let Some(value) = settings.get(flat_key) {
        return Some(value);
    }
    let mut current = settings;
    for key in nested_path {
        current = current.get(*key)?;
    }
    Some(current)
}

fn settings_string<'a>(
    settings: &'a serde_json::Value,
    flat_key: &str,
    nested_path: &[&str],
) -> Option<&'a str> {
    settings_value(settings, flat_key, nested_path).and_then(|value| value.as_str())
}

fn settings_string_any<'a>(
    settings: &'a serde_json::Value,
    flat_key: &str,
    nested_paths: &[&[&str]],
) -> Option<&'a str> {
    if let Some(value) = settings.get(flat_key).and_then(|value| value.as_str()) {
        return Some(value);
    }
    for nested_path in nested_paths {
        if let Some(value) = settings_string(settings, flat_key, nested_path) {
            return Some(value);
        }
    }
    None
}

fn settings_bool(
    settings: &serde_json::Value,
    flat_key: &str,
    nested_path: &[&str],
) -> Option<bool> {
    settings_value(settings, flat_key, nested_path).and_then(|value| value.as_bool())
}

fn settings_string_array(
    settings: &serde_json::Value,
    flat_key: &str,
    nested_path: &[&str],
) -> Option<Vec<String>> {
    let values = settings_value(settings, flat_key, nested_path)?.as_array()?;
    Some(
        values
            .iter()
            .filter_map(|value| value.as_str())
            .map(str::to_string)
            .collect(),
    )
}

fn resolve_existing_path(cwd: &Path, path: &Path) -> Result<PathBuf, AnalyzeError> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    path.canonicalize()
        .map_err(|err| AnalyzeError::new(format!("Invalid path {}: {err}", path.display())))
}

fn collect_workspace_analyze_files(
    project_root: &Path,
    namespace_map: Option<&php_lsp_index::composer::NamespaceMap>,
    include_paths: &[PathBuf],
    exclude_paths: &[PathBuf],
) -> Vec<PathBuf> {
    let source_dirs = workspace_index_directories(project_root, namespace_map, include_paths);
    let mut files = collect_php_files(&source_dirs, project_root, exclude_paths);
    if let Some(namespace_map) = namespace_map {
        for file_path in &namespace_map.files {
            let abs = if file_path.is_absolute() {
                file_path.clone()
            } else {
                project_root.join(file_path)
            };
            if abs.exists() {
                push_unique_path(&mut files, abs);
            }
        }
    }
    files.sort();
    files
}

fn collect_target_analyze_files(
    target: &Path,
    project_root: &Path,
    exclude_paths: &[PathBuf],
) -> Result<Vec<PathBuf>, AnalyzeError> {
    if target.is_file() {
        return if target.extension().and_then(|ext| ext.to_str()) == Some("php") {
            Ok(vec![target.to_path_buf()])
        } else {
            Err(AnalyzeError::new(format!(
                "Analyze target is not a PHP file: {}",
                target.display()
            )))
        };
    }
    if target.is_dir() {
        let mut files = collect_php_files(&[target.to_path_buf()], project_root, exclude_paths);
        files.sort();
        return Ok(files);
    }

    Err(AnalyzeError::new(format!(
        "Analyze target does not exist: {}",
        target.display()
    )))
}

fn parse_analyze_file(path: &Path) -> Result<ParsedAnalyzeFile, AnalyzeError> {
    let bytes = std::fs::read(path)
        .map_err(|err| AnalyzeError::new(format!("Failed to read {}: {err}", path.display())))?;
    let source = String::from_utf8_lossy(&bytes).into_owned();
    let mut parser = FileParser::new();
    parser.parse_full(&source);
    if parser.tree().is_none() {
        return Err(AnalyzeError::new(format!(
            "Parser did not produce a syntax tree for {}",
            path.display()
        )));
    }
    Ok(ParsedAnalyzeFile {
        path: path.to_path_buf(),
        uri: path_to_uri(path).map_err(|err| AnalyzeError::new(err.to_string()))?,
        parser,
    })
}

fn index_analyze_file(index: &WorkspaceIndex, parsed: &ParsedAnalyzeFile) {
    let source = parsed.parser.source();
    let Some(tree) = parsed.parser.tree() else {
        return;
    };
    let file_symbols = extract_file_symbols(tree, &source, &parsed.uri);
    let references = collect_symbol_references_in_file(tree, &source, &file_symbols);
    index.update_file_with_references(&parsed.uri, file_symbols, references);
}

fn parse_and_index_analyze_php_file(index: &WorkspaceIndex, file_path: &Path) -> bool {
    if !file_path.is_file() || file_path.extension().and_then(|ext| ext.to_str()) != Some("php") {
        return false;
    }

    let Ok(parsed) = parse_analyze_file(file_path) else {
        return false;
    };
    index_analyze_file(index, &parsed);
    true
}

fn preload_analyze_vendor_entrypoints(
    index: &WorkspaceIndex,
    project_root: &Path,
    runtime_config: &AnalyzeRuntimeConfig,
    vendor_map: &VendorAutoloadMap,
) {
    for file_path in vendor_map
        .files
        .iter()
        .take(VENDOR_PRELOAD_ENTRYPOINT_LIMIT)
    {
        if path_is_excluded(file_path, project_root, &runtime_config.exclude_paths) {
            continue;
        }
        parse_and_index_analyze_php_file(index, file_path);
    }
}

fn pre_resolve_analyze_file_dependencies(
    parsed: &ParsedAnalyzeFile,
    context: &AnalyzeLazyIndexContext<'_>,
) {
    let Some(tree) = parsed.parser.tree() else {
        return;
    };
    let source = parsed.parser.source();
    let file_symbols = context
        .index
        .file_symbols
        .get(&parsed.uri)
        .map(|entry| entry.value().clone())
        .unwrap_or_default();

    let mut fqns = Vec::new();
    for use_statement in &file_symbols.use_statements {
        if use_statement.kind == php_lsp_types::UseKind::Class && use_statement.fqn.contains('\\') {
            push_unique_string(&mut fqns, use_statement.fqn.clone());
        }
    }
    for fqn in collect_aliased_class_fqns(tree, &source, &file_symbols) {
        push_unique_string(&mut fqns, fqn);
    }

    for fqn in fqns {
        analyze_index_class_dependencies(context, &fqn);
    }
}

fn filter_analyze_lazy_resolved_symbol_diagnostics(
    diagnostics: Vec<Diagnostic>,
    context: &AnalyzeLazyIndexContext<'_>,
) -> Vec<Diagnostic> {
    let mut filtered = Vec::with_capacity(diagnostics.len());

    for diagnostic in diagnostics {
        if diagnostic.source.as_deref() == Some("php-lsp") {
            if let Some(fqn) = lazy_resolvable_diagnostic_fqn(&diagnostic.message) {
                analyze_index_class_dependencies(context, &fqn);
                if context.index.resolve_fqn(&fqn).is_some() {
                    continue;
                }
            }
        }
        filtered.push(diagnostic);
    }

    filtered
}

fn analyze_index_class_dependencies(
    context: &AnalyzeLazyIndexContext<'_>,
    class_or_member_fqn: &str,
) {
    let mut visited = HashSet::new();
    analyze_index_class_dependencies_inner(context, class_or_member_fqn, &mut visited, 0);
}

fn analyze_index_class_dependencies_inner(
    context: &AnalyzeLazyIndexContext<'_>,
    class_or_member_fqn: &str,
    visited: &mut HashSet<String>,
    depth: usize,
) {
    const MAX_DEPTH: usize = 10;
    if depth >= MAX_DEPTH {
        return;
    }

    let class_fqn = analyze_lazy_class_fqn(class_or_member_fqn);
    if class_fqn.is_empty() || !visited.insert(class_fqn.clone()) {
        return;
    }

    analyze_index_class(context, &class_fqn);

    let parent_fqns: Vec<String> = context
        .index
        .types
        .get(&class_fqn)
        .map(|symbol| {
            symbol
                .extends
                .iter()
                .chain(symbol.implements.iter())
                .chain(symbol.traits.iter())
                .cloned()
                .collect()
        })
        .unwrap_or_default();

    for parent_fqn in parent_fqns {
        analyze_index_class_dependencies_inner(context, &parent_fqn, visited, depth + 1);
    }
}

fn analyze_index_class(context: &AnalyzeLazyIndexContext<'_>, class_fqn: &str) -> bool {
    if context.index.types.contains_key(class_fqn) {
        return false;
    }

    let mut paths = context
        .namespace_map
        .map(|map| map.resolve_class_to_paths(class_fqn))
        .unwrap_or_default();
    if paths.is_empty() {
        if let Some(vendor_map) = context.vendor_map {
            if let Some(vendor_paths) = resolve_vendor_paths_from_map(class_fqn, vendor_map) {
                paths.extend(vendor_paths);
            }
        }
    }

    for path in paths {
        let abs = if path.is_absolute() {
            path
        } else {
            context.project_root.join(path)
        };
        if path_is_excluded(
            &abs,
            context.project_root,
            &context.runtime_config.exclude_paths,
        ) {
            continue;
        }
        if parse_and_index_analyze_php_file(context.index, &abs)
            && context.index.types.contains_key(class_fqn)
        {
            return true;
        }
    }

    false
}

fn analyze_lazy_class_fqn(fqn: &str) -> String {
    let fqn = fqn.trim().trim_start_matches('\\');
    fqn.rsplit_once("::")
        .map(|(class_fqn, _)| class_fqn)
        .unwrap_or(fqn)
        .to_string()
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

fn push_unique_string(values: &mut Vec<String>, value: String) {
    if !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

fn render_report(report: &AnalyzeReport, format: AnalyzeFormat) -> String {
    match format {
        AnalyzeFormat::Table => render_table_report(report),
        AnalyzeFormat::Json => render_json_report(report),
        AnalyzeFormat::Github => render_github_report(report),
    }
}

fn render_table_report(report: &AnalyzeReport) -> String {
    if report.diagnostics.is_empty() {
        return "No diagnostics found.\n".to_string();
    }

    let mut out = String::new();
    for item in &report.diagnostics {
        let diagnostic = &item.diagnostic;
        let path = display_path(&item.path, &report.project_root);
        let line = diagnostic.range.start.line + 1;
        let column = diagnostic.range.start.character + 1;
        let severity = severity_name(diagnostic.severity);
        let source = diagnostic.source.as_deref().unwrap_or("unknown");
        let code = diagnostic_code(diagnostic).unwrap_or_else(|| "-".to_string());
        let message = compact_message(&diagnostic.message);
        out.push_str(&format!(
            "{path}:{line}:{column}: {severity}: {message} [{source}/{code}]\n"
        ));
    }
    out
}

fn render_json_report(report: &AnalyzeReport) -> String {
    let json_report = JsonAnalyzeReport {
        schema_version: 1,
        project_root: report.project_root.display().to_string(),
        target: report.target.display().to_string(),
        summary: json_summary(report),
        diagnostics: report
            .diagnostics
            .iter()
            .map(|item| JsonAnalyzeDiagnostic {
                path: display_path(&item.path, &report.project_root),
                uri: item.uri.clone(),
                range: JsonAnalyzeRange {
                    start: JsonAnalyzePosition {
                        line: item.diagnostic.range.start.line,
                        character: item.diagnostic.range.start.character,
                    },
                    end: JsonAnalyzePosition {
                        line: item.diagnostic.range.end.line,
                        character: item.diagnostic.range.end.character,
                    },
                },
                severity: severity_name(item.diagnostic.severity).to_string(),
                source: item.diagnostic.source.clone(),
                code: diagnostic_code(&item.diagnostic),
                message: item.diagnostic.message.clone(),
            })
            .collect(),
    };
    let mut out = serde_json::to_string_pretty(&json_report)
        .expect("JSON serialization for analyze report should not fail");
    out.push('\n');
    out
}

fn render_github_report(report: &AnalyzeReport) -> String {
    let mut out = String::new();
    for item in &report.diagnostics {
        let diagnostic = &item.diagnostic;
        let annotation = match diagnostic.severity {
            Some(DiagnosticSeverity::ERROR) => "error",
            Some(DiagnosticSeverity::WARNING) => "warning",
            _ => "notice",
        };
        let path = github_escape_property(&display_path(&item.path, &report.project_root));
        let title = github_escape_property(
            &diagnostic
                .source
                .clone()
                .unwrap_or_else(|| "php-lsp".to_string()),
        );
        let message = github_escape_message(&diagnostic.message);
        out.push_str(&format!(
            "::{annotation} file={path},line={},col={},endLine={},endColumn={},title={}::{}\n",
            diagnostic.range.start.line + 1,
            diagnostic.range.start.character + 1,
            diagnostic.range.end.line + 1,
            diagnostic.range.end.character + 1,
            title,
            message
        ));
    }
    out
}

fn json_summary(report: &AnalyzeReport) -> JsonAnalyzeSummary {
    JsonAnalyzeSummary {
        files_analyzed: report.files_analyzed,
        diagnostics: report.diagnostics.len(),
        errors: report
            .diagnostics
            .iter()
            .filter(|item| item.diagnostic.severity == Some(DiagnosticSeverity::ERROR))
            .count(),
        warnings: report
            .diagnostics
            .iter()
            .filter(|item| item.diagnostic.severity == Some(DiagnosticSeverity::WARNING))
            .count(),
        information: report
            .diagnostics
            .iter()
            .filter(|item| item.diagnostic.severity == Some(DiagnosticSeverity::INFORMATION))
            .count(),
        hints: report
            .diagnostics
            .iter()
            .filter(|item| item.diagnostic.severity == Some(DiagnosticSeverity::HINT))
            .count(),
    }
}

fn display_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn severity_name(severity: Option<DiagnosticSeverity>) -> &'static str {
    match severity {
        Some(DiagnosticSeverity::ERROR) => "error",
        Some(DiagnosticSeverity::WARNING) => "warning",
        Some(DiagnosticSeverity::INFORMATION) => "information",
        Some(DiagnosticSeverity::HINT) => "hint",
        _ => "unknown",
    }
}

fn severity_sort_key(severity: Option<DiagnosticSeverity>) -> u8 {
    match severity {
        Some(DiagnosticSeverity::ERROR) => 0,
        Some(DiagnosticSeverity::WARNING) => 1,
        Some(DiagnosticSeverity::INFORMATION) => 2,
        Some(DiagnosticSeverity::HINT) => 3,
        _ => 4,
    }
}

fn diagnostic_code(diagnostic: &Diagnostic) -> Option<String> {
    match diagnostic.code.as_ref()? {
        NumberOrString::String(value) => Some(value.clone()),
        NumberOrString::Number(value) => Some(value.to_string()),
    }
}

fn compact_message(message: &str) -> String {
    message.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn github_escape_property(value: &str) -> String {
    github_escape_message(value)
        .replace(':', "%3A")
        .replace(',', "%2C")
}

fn github_escape_message(value: &str) -> String {
    value
        .replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn parse_analyze_args_accepts_path_project_root_severity_and_format() {
        let args = parse_analyze_args(vec![
            "src".to_string(),
            "--project-root".to_string(),
            "/tmp/project".to_string(),
            "--severity".to_string(),
            "warning".to_string(),
            "--format".to_string(),
            "json".to_string(),
        ])
        .unwrap();

        assert_eq!(args.path, Some(PathBuf::from("src")));
        assert_eq!(args.project_root, Some(PathBuf::from("/tmp/project")));
        assert_eq!(args.severity, AnalyzeSeverity::Warning);
        assert_eq!(args.format, AnalyzeFormat::Json);
    }

    #[test]
    fn analyze_json_output_has_stable_shape() {
        let root = temp_dir("json-shape");
        std::fs::write(
            root.join("Broken.php"),
            "<?php\nnamespace App;\nfunction demo(): void { new MissingClass(); }\n",
        )
        .unwrap();

        let result = run_analyze_cli(vec![
            "--project-root".to_string(),
            root.display().to_string(),
            "--format".to_string(),
            "json".to_string(),
        ]);

        assert_eq!(result.exit_code, 2, "stderr: {}", result.stderr);
        let value: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
        assert_eq!(value["schemaVersion"], 1);
        assert_eq!(value["summary"]["filesAnalyzed"], 1);
        assert_eq!(value["summary"]["diagnostics"], 1);
        assert_eq!(value["diagnostics"][0]["path"], "Broken.php");
        assert_eq!(value["diagnostics"][0]["severity"], "warning");
        assert!(value["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("Unknown class"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn analyze_exit_codes_report_clean_diagnostics_and_errors() {
        let clean_root = temp_dir("exit-clean");
        std::fs::write(clean_root.join("Clean.php"), "<?php\nclass Clean {}\n").unwrap();
        let clean = run_analyze_cli(vec![
            "--project-root".to_string(),
            clean_root.display().to_string(),
        ]);
        assert_eq!(clean.exit_code, 0, "stderr: {}", clean.stderr);
        assert!(clean.stdout.contains("No diagnostics found."));

        let broken_root = temp_dir("exit-broken");
        std::fs::write(
            broken_root.join("Broken.php"),
            "<?php\nnamespace App;\nfunction broken(): void { new MissingClass(); }\n",
        )
        .unwrap();
        let broken = run_analyze_cli(vec![
            "--project-root".to_string(),
            broken_root.display().to_string(),
            "--severity".to_string(),
            "warning".to_string(),
        ]);
        assert_eq!(broken.exit_code, 2, "stderr: {}", broken.stderr);

        let invalid = run_analyze_cli(vec!["/path/that/does/not/exist".to_string()]);
        assert_eq!(invalid.exit_code, 1);

        let _ = std::fs::remove_dir_all(clean_root);
        let _ = std::fs::remove_dir_all(broken_root);
    }

    #[test]
    fn analyze_resolves_vendor_psr4_symbols_from_composer_installed_metadata() {
        let root = temp_dir("vendor-psr4");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("vendor/composer")).unwrap();
        std::fs::create_dir_all(root.join("vendor/acme/library/src")).unwrap();

        std::fs::write(
            root.join("composer.json"),
            r#"{
                "autoload": {
                    "psr-4": {
                        "App\\": "src/"
                    }
                }
            }"#,
        )
        .unwrap();
        std::fs::write(
            root.join("vendor/composer/installed.json"),
            serde_json::json!({
                "packages": [
                    {
                        "name": "acme/library",
                        "install-path": "../acme/library",
                        "autoload": {
                            "psr-4": {
                                "Vendor\\Package\\": "src/"
                            }
                        }
                    }
                ]
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            root.join("vendor/acme/library/src/ExternalThing.php"),
            "<?php\nnamespace Vendor\\Package;\nclass ExternalThing {}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/Service.php"),
            "<?php\nnamespace App;\nuse Vendor\\Package\\ExternalThing;\nfinal class Service { public function build(): ExternalThing { return new ExternalThing(); } }\n",
        )
        .unwrap();

        let result = run_analyze_cli(vec![
            "src/Service.php".to_string(),
            "--project-root".to_string(),
            root.display().to_string(),
            "--severity".to_string(),
            "all".to_string(),
            "--format".to_string(),
            "json".to_string(),
        ]);

        assert_eq!(
            result.exit_code, 0,
            "stdout: {}\nstderr: {}",
            result.stdout, result.stderr
        );
        let value: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
        assert_eq!(value["summary"]["diagnostics"], 0, "{}", result.stdout);

        let _ = std::fs::remove_dir_all(root);
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "php-lsp-analyze-{label}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
