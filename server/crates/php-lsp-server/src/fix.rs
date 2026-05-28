use crate::server::{
    build_organize_imports_edit, collect_php_files, compute_diagnostics_with_config,
    discover_workspace_root_config, is_unused_import_diagnostic,
    load_effective_configuration_settings, normalize_config_paths, return_type_hint,
    workspace_index_directories, DiagnosticSeverityConfig, DiagnosticsMode, PhpVersion,
};
use crate::util::lsp_text::{lsp_position_to_byte, text_at_lsp_range};
use crate::util::uri::path_to_uri;
use php_lsp_index::workspace::WorkspaceIndex;
use php_lsp_parser::parser::FileParser;
use php_lsp_parser::references::collect_symbol_references_in_file;
use php_lsp_parser::return_type::find_missing_return_type_candidates;
use php_lsp_parser::symbols::extract_file_symbols;
use php_lsp_parser::utf16::Utf16LineIndex;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tower_lsp::ls_types::{Diagnostic, Position, Range, TextEdit, Uri, WorkspaceEdit};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixFormat {
    Table,
    Json,
}

impl FixFormat {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "table" => Some(Self::Table),
            "json" => Some(Self::Json),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FixRule {
    UnusedImports,
    OrganizeImports,
    AddReturnType,
}

impl FixRule {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "unused-imports" | "unused_imports" | "unusedimports" => Some(Self::UnusedImports),
            "organize-imports" | "organize_imports" | "organizeimports" => {
                Some(Self::OrganizeImports)
            }
            "add-return-type" | "add_return_type" | "addreturntype" | "phpdoc-return-type"
            | "phpdoc_return_type" => Some(Self::AddReturnType),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::UnusedImports => "unused-imports",
            Self::OrganizeImports => "organize-imports",
            Self::AddReturnType => "add-return-type",
        }
    }
}

const DEFAULT_FIX_RULES: &[FixRule] = &[FixRule::UnusedImports, FixRule::AddReturnType];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixArgs {
    pub path: Option<PathBuf>,
    pub project_root: Option<PathBuf>,
    pub dry_run: bool,
    pub format: FixFormat,
    pub rules: Vec<FixRule>,
}

impl Default for FixArgs {
    fn default() -> Self {
        Self {
            path: None,
            project_root: None,
            dry_run: false,
            format: FixFormat::Table,
            rules: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub struct FixCliResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug)]
pub struct FixError {
    message: String,
}

impl FixError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for FixError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for FixError {}

struct ParsedFixFile {
    path: PathBuf,
    uri: String,
    parser: FileParser,
    file_symbols: php_lsp_types::FileSymbols,
}

#[derive(Debug)]
struct FixRuntimeConfig {
    php_version: PhpVersion,
    diagnostics_mode: DiagnosticsMode,
    diagnostic_severity: DiagnosticSeverityConfig,
    composer_enabled: bool,
    include_paths: Vec<PathBuf>,
    exclude_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
struct FixEdit {
    range: Range,
    old_text: String,
    new_text: String,
}

#[derive(Debug, Clone)]
struct FixAction {
    rule: FixRule,
    title: String,
    edits: Vec<FixEdit>,
}

#[derive(Debug)]
struct FixFileReport {
    path: PathBuf,
    uri: String,
    actions: Vec<FixAction>,
}

#[derive(Debug)]
struct FixReport {
    project_root: PathBuf,
    target: PathBuf,
    dry_run: bool,
    requested_rules: Vec<FixRule>,
    files_analyzed: usize,
    files: Vec<FixFileReport>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonFixReport {
    schema_version: u32,
    project_root: String,
    target: String,
    dry_run: bool,
    rules: Vec<String>,
    summary: JsonFixSummary,
    files: Vec<JsonFixFile>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonFixSummary {
    files_analyzed: usize,
    files_with_changes: usize,
    fixes: usize,
    edits: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonFixFile {
    path: String,
    uri: String,
    fixes: Vec<JsonFixAction>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonFixAction {
    rule: String,
    title: String,
    edits: Vec<JsonFixEdit>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonFixEdit {
    range: JsonFixRange,
    old_text: String,
    new_text: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonFixRange {
    start: JsonFixPosition,
    end: JsonFixPosition,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonFixPosition {
    line: u32,
    character: u32,
}

impl FixReport {
    fn total_fixes(&self) -> usize {
        self.files.iter().map(|file| file.actions.len()).sum()
    }

    fn total_edits(&self) -> usize {
        self.files
            .iter()
            .flat_map(|file| file.actions.iter())
            .map(|action| action.edits.len())
            .sum()
    }
}

pub fn parse_fix_args(raw_args: Vec<String>) -> Result<FixArgs, FixError> {
    let mut parsed = FixArgs::default();
    let mut iter = raw_args.into_iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--dry-run" => {
                parsed.dry_run = true;
            }
            "--project-root" => {
                let value = iter
                    .next()
                    .ok_or_else(|| FixError::new("Missing value for --project-root"))?;
                parsed.project_root = Some(PathBuf::from(value));
            }
            "--rule" => {
                let value = iter
                    .next()
                    .ok_or_else(|| FixError::new("Missing value for --rule"))?;
                let rule = FixRule::parse(&value).ok_or_else(|| {
                    FixError::new(format!(
                        "Invalid --rule `{value}`; expected unused-imports, organize-imports, or add-return-type"
                    ))
                })?;
                push_unique_rule(&mut parsed.rules, rule);
            }
            "--format" => {
                let value = iter
                    .next()
                    .ok_or_else(|| FixError::new("Missing value for --format"))?;
                parsed.format = FixFormat::parse(&value).ok_or_else(|| {
                    FixError::new(format!(
                        "Invalid --format `{value}`; expected table or json"
                    ))
                })?;
            }
            "--help" | "-h" => {
                return Err(FixError::new(fix_help()));
            }
            value if value.starts_with('-') => {
                return Err(FixError::new(format!("Unknown fix option `{value}`")));
            }
            value => {
                if parsed.path.is_some() {
                    return Err(FixError::new(format!(
                        "Unexpected extra fix path `{value}`"
                    )));
                }
                parsed.path = Some(PathBuf::from(value));
            }
        }
    }

    if parsed.rules.is_empty() {
        parsed.rules = DEFAULT_FIX_RULES.to_vec();
    }

    Ok(parsed)
}

pub fn fix_help() -> &'static str {
    "Usage:\n  php-lsp fix [PATH] --dry-run [--project-root <DIR>] [--rule <unused-imports|organize-imports|add-return-type>]... [--format <table|json>]"
}

pub fn run_fix_cli(raw_args: Vec<String>) -> FixCliResult {
    if raw_args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h" | "help"))
    {
        return FixCliResult {
            exit_code: 0,
            stdout: format!("{}\n", fix_help()),
            stderr: String::new(),
        };
    }

    let args = match parse_fix_args(raw_args) {
        Ok(args) => args,
        Err(err) => {
            return FixCliResult {
                exit_code: 1,
                stdout: String::new(),
                stderr: format!("{err}\n"),
            };
        }
    };

    if !args.dry_run {
        return FixCliResult {
            exit_code: 1,
            stdout: String::new(),
            stderr: "php-lsp fix currently requires --dry-run and will not write files.\n"
                .to_string(),
        };
    }

    match run_fix(&args) {
        Ok(report) => {
            let stdout = render_report(&report, args.format);
            let exit_code = if report.total_edits() == 0 { 0 } else { 2 };
            FixCliResult {
                exit_code,
                stdout,
                stderr: String::new(),
            }
        }
        Err(err) => FixCliResult {
            exit_code: 1,
            stdout: String::new(),
            stderr: format!("{err}\n"),
        },
    }
}

fn run_fix(args: &FixArgs) -> Result<FixReport, FixError> {
    let cwd = std::env::current_dir()
        .map_err(|err| FixError::new(format!("Failed to read current directory: {err}")))?;
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
        return Err(FixError::new(message.clone()));
    }

    let runtime_config = fix_runtime_config(&settings);
    let workspace_config =
        discover_workspace_root_config(&requested_project_root, runtime_config.composer_enabled);
    let project_root = workspace_config.root;
    let requested_target = if default_target {
        project_root.clone()
    } else {
        requested_target
    };
    let workspace_files = collect_workspace_fix_files(
        &project_root,
        workspace_config.namespace_map.as_ref(),
        &runtime_config.include_paths,
        &runtime_config.exclude_paths,
    );
    let target_files = collect_target_fix_files(
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
    let mut parsed_by_path = HashMap::new();
    for path in all_files {
        let parsed = parse_fix_file(&path)?;
        let source = parsed.parser.source();
        let tree = parsed.parser.tree().expect("parsed file has a tree");
        let references = collect_symbol_references_in_file(tree, &source, &parsed.file_symbols);
        index.update_file_with_references(&parsed.uri, parsed.file_symbols.clone(), references);
        parsed_by_path.insert(path, parsed);
    }

    let mut files = Vec::new();
    for target_file in &target_files {
        let parsed = parsed_by_path
            .get(target_file)
            .ok_or_else(|| FixError::new(format!("Failed to parse {}", target_file.display())))?;
        if let Some(file_report) = collect_file_fixes(parsed, &index, &runtime_config, &args.rules)?
        {
            files.push(file_report);
        }
    }

    files.sort_by(|left, right| {
        display_path(&left.path, &project_root).cmp(&display_path(&right.path, &project_root))
    });

    Ok(FixReport {
        project_root,
        target: requested_target,
        dry_run: args.dry_run,
        requested_rules: args.rules.clone(),
        files_analyzed: target_files.len(),
        files,
    })
}

fn collect_file_fixes(
    parsed: &ParsedFixFile,
    index: &WorkspaceIndex,
    runtime_config: &FixRuntimeConfig,
    rules: &[FixRule],
) -> Result<Option<FixFileReport>, FixError> {
    let uri = parsed
        .uri
        .parse::<Uri>()
        .map_err(|err| FixError::new(format!("Invalid URI {}: {err}", parsed.uri)))?;
    let source = parsed.parser.source();
    let mut actions = Vec::new();

    if rules.contains(&FixRule::OrganizeImports) {
        if let Some(action) = organize_imports_action(
            uri.clone(),
            &parsed.uri,
            &source,
            &parsed.file_symbols,
            FixRule::OrganizeImports,
            "Organize imports",
        )? {
            actions.push(action);
        }
    } else if rules.contains(&FixRule::UnusedImports) {
        let diagnostics = compute_file_diagnostics(parsed, index, runtime_config);
        if diagnostics.iter().any(is_unused_import_diagnostic) {
            if let Some(action) = organize_imports_action(
                uri.clone(),
                &parsed.uri,
                &source,
                &parsed.file_symbols,
                FixRule::UnusedImports,
                "Remove unused imports",
            )? {
                actions.push(action);
            }
        }
    }

    if rules.contains(&FixRule::AddReturnType) {
        actions.extend(add_return_type_actions(
            &source,
            parsed.parser.tree().expect("parsed file has a tree"),
            runtime_config.php_version,
        ));
    }

    if actions.is_empty() {
        return Ok(None);
    }

    let edits = actions
        .iter()
        .flat_map(|action| action.edits.iter().cloned())
        .collect::<Vec<_>>();
    let new_source = apply_fix_edits(&source, &edits)?;
    if new_source == source {
        return Ok(None);
    }

    Ok(Some(FixFileReport {
        path: parsed.path.clone(),
        uri: parsed.uri.clone(),
        actions,
    }))
}

fn organize_imports_action(
    uri: Uri,
    uri_str: &str,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    rule: FixRule,
    title: &str,
) -> Result<Option<FixAction>, FixError> {
    let Some(workspace_edit) = build_organize_imports_edit(uri, source, file_symbols) else {
        return Ok(None);
    };
    let edits = workspace_edit_to_fix_edits(uri_str, source, workspace_edit)?;
    if edits.is_empty() {
        return Ok(None);
    }
    Ok(Some(FixAction {
        rule,
        title: title.to_string(),
        edits,
    }))
}

fn add_return_type_actions(
    source: &str,
    tree: &tree_sitter::Tree,
    php_version: PhpVersion,
) -> Vec<FixAction> {
    let utf16_index = Utf16LineIndex::new(source);
    find_missing_return_type_candidates(tree, source, (0, 0, u32::MAX, u32::MAX))
        .into_iter()
        .filter_map(|candidate| {
            let hint = return_type_hint(&candidate.return_type, php_version)?;
            let position = Position::new(
                candidate.insert_position.0,
                utf16_index
                    .byte_col_to_utf16(candidate.insert_position.0, candidate.insert_position.1),
            );
            Some(FixAction {
                rule: FixRule::AddReturnType,
                title: format!("Add return type `{}` to `{}`", hint, candidate.name),
                edits: vec![FixEdit {
                    range: Range {
                        start: position,
                        end: position,
                    },
                    old_text: String::new(),
                    new_text: format!(": {hint}"),
                }],
            })
        })
        .collect()
}

fn compute_file_diagnostics(
    parsed: &ParsedFixFile,
    index: &WorkspaceIndex,
    runtime_config: &FixRuntimeConfig,
) -> Vec<Diagnostic> {
    compute_diagnostics_with_config(
        &parsed.uri,
        &parsed.parser,
        index,
        runtime_config.diagnostics_mode,
        runtime_config.diagnostic_severity,
        runtime_config.php_version,
    )
}

fn workspace_edit_to_fix_edits(
    uri_str: &str,
    source: &str,
    workspace_edit: WorkspaceEdit,
) -> Result<Vec<FixEdit>, FixError> {
    let mut edits = Vec::new();
    if let Some(changes) = workspace_edit.changes {
        for (uri, text_edits) in changes {
            if uri.as_str() != uri_str {
                continue;
            }
            for edit in text_edits {
                edits.push(fix_edit_from_text_edit(source, edit)?);
            }
        }
    }
    Ok(edits)
}

fn fix_edit_from_text_edit(source: &str, edit: TextEdit) -> Result<FixEdit, FixError> {
    let old_text = text_at_lsp_range(source, edit.range)
        .ok_or_else(|| FixError::new("Generated fix contains an invalid text edit range"))?
        .to_string();
    Ok(FixEdit {
        range: edit.range,
        old_text,
        new_text: edit.new_text,
    })
}

fn apply_fix_edits(source: &str, edits: &[FixEdit]) -> Result<String, FixError> {
    let mut spans = Vec::new();
    for edit in edits {
        let start = lsp_position_to_byte(source, edit.range.start).ok_or_else(|| {
            FixError::new("Generated fix contains an invalid text edit start position")
        })?;
        let end = lsp_position_to_byte(source, edit.range.end).ok_or_else(|| {
            FixError::new("Generated fix contains an invalid text edit end position")
        })?;
        if end < start {
            return Err(FixError::new(
                "Generated fix contains an invalid reversed text edit range",
            ));
        }
        spans.push((start, end, edit.new_text.as_str()));
    }

    let mut ascending = spans.clone();
    ascending.sort_by_key(|(start, end, _)| (*start, *end));
    for window in ascending.windows(2) {
        if window[0].1 > window[1].0 {
            return Err(FixError::new(
                "Generated fixes overlap; refusing to build a dry-run patch",
            ));
        }
    }

    spans.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
    let mut output = source.to_string();
    for (start, end, new_text) in spans {
        output.replace_range(start..end, new_text);
    }
    Ok(output)
}

fn fix_runtime_config(settings: &serde_json::Value) -> FixRuntimeConfig {
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
    let composer_enabled =
        settings_bool(settings, "composerEnabled", &["composer", "enabled"]).unwrap_or(true);
    let include_paths = settings_string_array(settings, "includePaths", &["includePaths"])
        .map(normalize_config_paths)
        .unwrap_or_default();
    let exclude_paths = settings_string_array(settings, "excludePaths", &["excludePaths"])
        .map(normalize_config_paths)
        .unwrap_or_default();

    FixRuntimeConfig {
        php_version,
        diagnostics_mode,
        diagnostic_severity,
        composer_enabled,
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

fn resolve_existing_path(cwd: &Path, path: &Path) -> Result<PathBuf, FixError> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    path.canonicalize()
        .map_err(|err| FixError::new(format!("Invalid path {}: {err}", path.display())))
}

fn collect_workspace_fix_files(
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

fn collect_target_fix_files(
    target: &Path,
    project_root: &Path,
    exclude_paths: &[PathBuf],
) -> Result<Vec<PathBuf>, FixError> {
    if target.is_file() {
        return if target.extension().and_then(|ext| ext.to_str()) == Some("php") {
            Ok(vec![target.to_path_buf()])
        } else {
            Err(FixError::new(format!(
                "Fix target is not a PHP file: {}",
                target.display()
            )))
        };
    }
    if target.is_dir() {
        let mut files = collect_php_files(&[target.to_path_buf()], project_root, exclude_paths);
        files.sort();
        return Ok(files);
    }

    Err(FixError::new(format!(
        "Fix target does not exist: {}",
        target.display()
    )))
}

fn parse_fix_file(path: &Path) -> Result<ParsedFixFile, FixError> {
    let bytes = std::fs::read(path)
        .map_err(|err| FixError::new(format!("Failed to read {}: {err}", path.display())))?;
    let source = String::from_utf8_lossy(&bytes).into_owned();
    let mut parser = FileParser::new();
    parser.parse_full(&source);
    let tree = parser.tree().ok_or_else(|| {
        FixError::new(format!(
            "Parser did not produce a syntax tree for {}",
            path.display()
        ))
    })?;
    let uri = path_to_uri(path);
    let file_symbols = extract_file_symbols(tree, &source, &uri);
    Ok(ParsedFixFile {
        path: path.to_path_buf(),
        uri,
        parser,
        file_symbols,
    })
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

fn push_unique_rule(rules: &mut Vec<FixRule>, rule: FixRule) {
    if !rules.contains(&rule) {
        rules.push(rule);
    }
}

fn render_report(report: &FixReport, format: FixFormat) -> String {
    match format {
        FixFormat::Table => render_table_report(report),
        FixFormat::Json => render_json_report(report),
    }
}

fn render_table_report(report: &FixReport) -> String {
    if report.total_edits() == 0 {
        return "No fixes available.\n".to_string();
    }

    let mut out = format!(
        "Dry run: would apply {} fixes ({} edits) across {} files.\n",
        report.total_fixes(),
        report.total_edits(),
        report.files.len()
    );
    for file in &report.files {
        let path = display_path(&file.path, &report.project_root);
        for action in &file.actions {
            for edit in &action.edits {
                out.push_str(&format!(
                    "{}:{}:{}: {}: {}: {}\n",
                    path,
                    edit.range.start.line + 1,
                    edit.range.start.character + 1,
                    action.rule.as_str(),
                    action.title,
                    edit_summary(edit)
                ));
            }
        }
    }
    out
}

fn render_json_report(report: &FixReport) -> String {
    let json_report = JsonFixReport {
        schema_version: 1,
        project_root: report.project_root.display().to_string(),
        target: report.target.display().to_string(),
        dry_run: report.dry_run,
        rules: report
            .requested_rules
            .iter()
            .map(|rule| rule.as_str().to_string())
            .collect(),
        summary: JsonFixSummary {
            files_analyzed: report.files_analyzed,
            files_with_changes: report.files.len(),
            fixes: report.total_fixes(),
            edits: report.total_edits(),
        },
        files: report
            .files
            .iter()
            .map(|file| JsonFixFile {
                path: display_path(&file.path, &report.project_root),
                uri: file.uri.clone(),
                fixes: file
                    .actions
                    .iter()
                    .map(|action| JsonFixAction {
                        rule: action.rule.as_str().to_string(),
                        title: action.title.clone(),
                        edits: action
                            .edits
                            .iter()
                            .map(|edit| JsonFixEdit {
                                range: JsonFixRange {
                                    start: JsonFixPosition {
                                        line: edit.range.start.line,
                                        character: edit.range.start.character,
                                    },
                                    end: JsonFixPosition {
                                        line: edit.range.end.line,
                                        character: edit.range.end.character,
                                    },
                                },
                                old_text: edit.old_text.clone(),
                                new_text: edit.new_text.clone(),
                            })
                            .collect(),
                    })
                    .collect(),
            })
            .collect(),
    };
    let mut out = serde_json::to_string_pretty(&json_report)
        .expect("JSON serialization for fix report should not fail");
    out.push('\n');
    out
}

fn edit_summary(edit: &FixEdit) -> String {
    if edit.old_text.is_empty() {
        return format!("insert `{}`", preview_text(&edit.new_text));
    }
    if edit.new_text.is_empty() {
        return format!("delete `{}`", preview_text(&edit.old_text));
    }
    format!(
        "replace `{}` with `{}`",
        preview_text(&edit.old_text),
        preview_text(&edit.new_text)
    )
}

fn preview_text(text: &str) -> String {
    let mut preview = text
        .replace('\\', "\\\\")
        .replace('\r', "\\r")
        .replace('\n', "\\n");
    const MAX_PREVIEW_CHARS: usize = 160;
    if preview.chars().count() > MAX_PREVIEW_CHARS {
        preview = preview.chars().take(MAX_PREVIEW_CHARS).collect();
        preview.push_str("...");
    }
    preview
}

fn display_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn parse_fix_args_accepts_path_project_root_rules_and_format() {
        let args = parse_fix_args(vec![
            "src".to_string(),
            "--dry-run".to_string(),
            "--project-root".to_string(),
            "/tmp/project".to_string(),
            "--rule".to_string(),
            "organize-imports".to_string(),
            "--rule".to_string(),
            "add-return-type".to_string(),
            "--format".to_string(),
            "json".to_string(),
        ])
        .unwrap();

        assert_eq!(args.path, Some(PathBuf::from("src")));
        assert_eq!(args.project_root, Some(PathBuf::from("/tmp/project")));
        assert!(args.dry_run);
        assert_eq!(
            args.rules,
            vec![FixRule::OrganizeImports, FixRule::AddReturnType]
        );
        assert_eq!(args.format, FixFormat::Json);
    }

    #[test]
    fn fix_json_output_has_stable_shape_and_dry_run_does_not_write() {
        let root = temp_dir("json-shape");
        let path = root.join("FixMe.php");
        let original = r#"<?php
namespace App;

use App\Unused;
use App\Used;

/** @return string */
function label($value) {
    return $value;
}

echo Used::class;
"#;
        std::fs::write(&path, original).unwrap();

        let result = run_fix_cli(vec![
            "--project-root".to_string(),
            root.display().to_string(),
            "--dry-run".to_string(),
            "--format".to_string(),
            "json".to_string(),
        ]);

        assert_eq!(result.exit_code, 2, "stderr: {}", result.stderr);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);

        let value: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
        assert_eq!(value["schemaVersion"], 1);
        assert_eq!(value["dryRun"], true);
        assert_eq!(value["summary"]["filesAnalyzed"], 1);
        assert_eq!(value["summary"]["filesWithChanges"], 1);
        assert_eq!(value["files"][0]["path"], "FixMe.php");

        let rules = value["files"][0]["fixes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|fix| fix["rule"].as_str())
            .collect::<Vec<_>>();
        assert!(rules.contains(&"unused-imports"), "rules: {:?}", rules);
        assert!(rules.contains(&"add-return-type"), "rules: {:?}", rules);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn fix_report_is_idempotent_after_applying_generated_edits() {
        let root = temp_dir("idempotent");
        let path = root.join("FixMe.php");
        std::fs::write(
            &path,
            r#"<?php
namespace App;

use App\Unused;
use App\Used;

/** @return string */
function label($value) {
    return $value;
}

echo Used::class;
"#,
        )
        .unwrap();

        let args = FixArgs {
            project_root: Some(root.clone()),
            dry_run: true,
            ..FixArgs::default()
        };
        let first = run_fix(&FixArgs {
            rules: DEFAULT_FIX_RULES.to_vec(),
            ..args.clone()
        })
        .unwrap();
        assert_eq!(first.total_fixes(), 2);
        assert_eq!(first.files.len(), 1);
        let original = std::fs::read_to_string(&path).unwrap();
        let edits = first.files[0]
            .actions
            .iter()
            .flat_map(|action| action.edits.iter().cloned())
            .collect::<Vec<_>>();
        let new_source = apply_fix_edits(&original, &edits).unwrap();
        std::fs::write(&path, new_source).unwrap();

        let second = run_fix(&FixArgs {
            rules: DEFAULT_FIX_RULES.to_vec(),
            ..args
        })
        .unwrap();
        assert_eq!(second.total_edits(), 0);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn fix_cli_requires_dry_run() {
        let result = run_fix_cli(vec![]);
        assert_eq!(result.exit_code, 1);
        assert!(result.stderr.contains("requires --dry-run"));
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "php-lsp-fix-{label}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
