//! LSP server implementation — LanguageServer trait.

use crate::config::{
    global_config_candidates, load_toml_settings, merge_json_objects, normalize_client_settings,
    PROJECT_CONFIG_FILE_NAME,
};
use dashmap::DashMap;
use php_lsp_completion::context::detect_context;
use php_lsp_completion::provider::provide_completions;
use php_lsp_index::cache::{self, CacheNamespace, CacheSourceFile, IndexCacheConfig};
use php_lsp_index::composer::{parse_composer_json, NamespaceMap};
use php_lsp_index::stubs;
use php_lsp_index::workspace::WorkspaceIndex;
use php_lsp_parser::diagnostics::extract_syntax_errors;
use php_lsp_parser::parser::FileParser;
use php_lsp_parser::phpdoc::parse_phpdoc;
use php_lsp_parser::references::{
    collect_symbol_references_in_file, find_references_in_file,
    find_variable_references_at_position,
};
use php_lsp_parser::resolve::{
    infer_property_type_from_assignments, infer_variable_type_at_position,
    infer_variable_type_at_position_with_resolver, local_variable_names_at_position,
    resolve_class_name_pub, symbol_at_position, symbol_at_position_with_resolver,
    variable_definition_at_position, variable_hover_info_at_position, RefKind, SymbolAtPosition,
};
use php_lsp_parser::return_type::{
    find_missing_return_type_candidates, MissingReturnTypeCandidate,
};
use php_lsp_parser::semantic::{
    collect_aliased_class_fqns, extract_semantic_diagnostics, SemanticDiagnostic,
    SemanticDiagnosticKind,
};
use php_lsp_parser::semantic_tokens::{
    extract_semantic_tokens, SEMANTIC_TOKEN_MODIFIERS, SEMANTIC_TOKEN_TYPES,
};
use php_lsp_parser::signature_help::signature_help_context_at_position;
use php_lsp_parser::symbols::extract_file_symbols;
use php_lsp_parser::utf16::{range_byte_to_utf16, utf16_col_to_byte, Utf16LineIndex};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, Notify};
use tokio::task::{JoinHandle, JoinSet};
use tower_lsp::jsonrpc::Result;
use tower_lsp::ls_types::request::{GotoImplementationParams, GotoImplementationResponse};
use tower_lsp::ls_types::*;
use tower_lsp::{Client, LanguageServer};

struct PhpLspIndexingStatusNotification;

const DID_CHANGE_DIAGNOSTICS_DEBOUNCE_MS: u64 = 180;
const HEAVY_REQUEST_YIELD_INTERVAL: usize = 32;
const FILE_IO_SLOW_WARNING_MS: u64 = 100;
const FILE_IO_TIMEOUT_MS: u64 = 15_000;
const DIAGNOSTIC_PHASE_SLOW_WARNING_MS: u64 = 500;
const MEMBER_TYPE_DIAGNOSTIC_NODE_LIMIT: usize = 64;
const DIAGNOSTIC_THREAD_STACK_SIZE: usize = 16 * 1024 * 1024;

fn document_version_is_newer(current: Option<i32>, incoming: i32) -> bool {
    current.is_none_or(|current| incoming > current)
}

async fn cooperative_heavy_request_yield(index: usize) {
    if index % HEAVY_REQUEST_YIELD_INTERVAL == 0 {
        tokio::task::yield_now().await;
    }
}

async fn run_file_io_blocking<T, F>(
    label: &'static str,
    path_label: String,
    op: F,
) -> std::result::Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let started = Instant::now();
    let task = tokio::task::spawn_blocking(op);
    let result = match tokio::time::timeout(Duration::from_millis(FILE_IO_TIMEOUT_MS), task).await {
        Ok(Ok(result)) => result,
        Ok(Err(err)) => {
            let message = format!("{} task failed for {}: {}", label, path_label, err);
            tracing::warn!("{}", message);
            return Err(message);
        }
        Err(_) => {
            let message = format!(
                "{} timed out after {} ms for {}",
                label, FILE_IO_TIMEOUT_MS, path_label
            );
            tracing::warn!("{}", message);
            return Err(message);
        }
    };

    let elapsed = started.elapsed();
    if elapsed >= Duration::from_millis(FILE_IO_SLOW_WARNING_MS) {
        tracing::warn!(
            "{} took {} ms for {}",
            label,
            elapsed.as_millis(),
            path_label
        );
    }

    Ok(result)
}

async fn read_file_to_string_blocking(
    path: PathBuf,
    label: &'static str,
) -> std::io::Result<String> {
    let path_label = path.display().to_string();
    match run_file_io_blocking(label, path_label.clone(), move || {
        std::fs::read_to_string(&path)
    })
    .await
    {
        Ok(Ok(source)) => Ok(source),
        Ok(Err(err)) => {
            tracing::debug!("{} failed for {}: {}", label, path_label, err);
            Err(err)
        }
        Err(message) => Err(std::io::Error::other(message)),
    }
}

#[derive(Clone, Debug)]
struct OperationCancellationToken {
    cancelled: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl OperationCancellationToken {
    fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(Notify::new()),
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    fn is_same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.cancelled, &other.cancelled)
    }

    async fn cancelled(&self) {
        while !self.is_cancelled() {
            self.notify.notified().await;
        }
    }
}

impl tower_lsp::ls_types::notification::Notification for PhpLspIndexingStatusNotification {
    type Params = serde_json::Value;

    const METHOD: &'static str = "phpLsp/indexingStatus";
}

async fn send_indexing_status(client: &Client, params: serde_json::Value) {
    client
        .send_notification::<PhpLspIndexingStatusNotification>(params)
        .await;
}

fn elapsed_ms(started_at: Instant) -> u64 {
    started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

fn indexing_parse_concurrency() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(4)
        .clamp(1, MAX_INDEXING_PARSE_CONCURRENCY)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct PhpVersion {
    major: u16,
    minor: u16,
}

impl PhpVersion {
    const DEFAULT: Self = Self { major: 8, minor: 2 };

    fn parse(raw: &str) -> Option<Self> {
        let mut parts = raw.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().unwrap_or("0").parse().ok()?;
        Some(Self { major, minor })
    }

    fn at_least(self, major: u16, minor: u16) -> bool {
        self >= Self { major, minor }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FormattingConfig {
    provider: String,
    command: Option<String>,
    timeout_ms: u64,
}

impl Default for FormattingConfig {
    fn default() -> Self {
        Self {
            provider: "none".to_string(),
            command: None,
            timeout_ms: 30_000,
        }
    }
}

impl FormattingConfig {
    fn from_options(
        provider: Option<&str>,
        command: Option<&str>,
        timeout_ms: Option<u64>,
    ) -> Self {
        let provider = provider.unwrap_or("none").trim().to_ascii_lowercase();
        let command = command
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        Self {
            provider,
            command,
            timeout_ms: timeout_ms.unwrap_or(30_000).max(1_000),
        }
    }

    fn command_template(&self) -> Option<String> {
        match self.provider.as_str() {
            "none" => None,
            "custom" => self.command.clone(),
            "php-cs-fixer" => self
                .command
                .clone()
                .or_else(|| Some("php-cs-fixer fix --using-cache=no --quiet {file}".to_string())),
            "phpcbf" => self
                .command
                .clone()
                .or_else(|| Some("phpcbf {file}".to_string())),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PhpStanConfig {
    enabled: bool,
    command: String,
    timeout_ms: u64,
    memory_limit: Option<String>,
}

impl Default for PhpStanConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            command: "vendor/bin/phpstan analyse --error-format=json --no-progress --no-interaction {file}"
                .to_string(),
            timeout_ms: 30_000,
            memory_limit: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PsalmConfig {
    enabled: bool,
    command: String,
    timeout_ms: u64,
}

impl Default for PsalmConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            command: "vendor/bin/psalm --output-format=json --no-progress {file}".to_string(),
            timeout_ms: 30_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum DiagnosticsMode {
    Off,
    SyntaxOnly,
    #[default]
    BasicSemantic,
}

impl DiagnosticsMode {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "syntax-only" | "syntax" => Some(Self::SyntaxOnly),
            "basic-semantic" | "semantic" => Some(Self::BasicSemantic),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiagnosticCategory {
    UnknownSymbols,
    Unused,
    DuplicateSymbols,
    Members,
    TypeCompatibility,
    OverrideSignatures,
    PhpVersion,
}

impl DiagnosticCategory {
    fn code(self) -> &'static str {
        match self {
            Self::UnknownSymbols => "php-lsp.unknownSymbols",
            Self::Unused => "php-lsp.unused",
            Self::DuplicateSymbols => "php-lsp.duplicateSymbols",
            Self::Members => "php-lsp.members",
            Self::TypeCompatibility => "php-lsp.typeCompatibility",
            Self::OverrideSignatures => "php-lsp.overrideSignatures",
            Self::PhpVersion => "php-lsp.phpVersion",
        }
    }

    fn parse(key: &str) -> Option<Self> {
        match key
            .chars()
            .filter(|ch| *ch != '-' && *ch != '_' && *ch != '.')
            .flat_map(char::to_lowercase)
            .collect::<String>()
            .as_str()
        {
            "unknownsymbols" | "symbols" => Some(Self::UnknownSymbols),
            "unused" | "unusedcode" => Some(Self::Unused),
            "duplicatesymbols" | "duplicates" => Some(Self::DuplicateSymbols),
            "members" | "memberaccess" => Some(Self::Members),
            "typecompatibility" | "types" => Some(Self::TypeCompatibility),
            "overridesignatures" | "overrides" => Some(Self::OverrideSignatures),
            "phpversion" | "version" => Some(Self::PhpVersion),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DiagnosticLevel(Option<DiagnosticSeverity>);

impl DiagnosticLevel {
    fn parse(value: &serde_json::Value) -> Option<Self> {
        let raw = value.as_str()?.trim().to_ascii_lowercase();
        match raw.as_str() {
            "off" | "none" | "disabled" => Some(Self(None)),
            "error" => Some(Self(Some(DiagnosticSeverity::ERROR))),
            "warning" | "warn" => Some(Self(Some(DiagnosticSeverity::WARNING))),
            "information" | "info" => Some(Self(Some(DiagnosticSeverity::INFORMATION))),
            "hint" => Some(Self(Some(DiagnosticSeverity::HINT))),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DiagnosticSeverityConfig {
    unknown_symbols: DiagnosticLevel,
    unused: DiagnosticLevel,
    duplicate_symbols: DiagnosticLevel,
    members: DiagnosticLevel,
    type_compatibility: DiagnosticLevel,
    override_signatures: DiagnosticLevel,
    php_version: DiagnosticLevel,
}

impl Default for DiagnosticSeverityConfig {
    fn default() -> Self {
        let warning = DiagnosticLevel(Some(DiagnosticSeverity::WARNING));
        Self {
            unknown_symbols: warning,
            unused: warning,
            duplicate_symbols: warning,
            members: warning,
            type_compatibility: warning,
            override_signatures: warning,
            php_version: warning,
        }
    }
}

impl DiagnosticSeverityConfig {
    fn parse(value: &serde_json::Value) -> Option<Self> {
        if let Some(level) = DiagnosticLevel::parse(value) {
            return Some(Self::all(level));
        }

        let object = value.as_object()?;
        let mut config = Self::default();
        for (key, value) in object {
            let Some(category) = DiagnosticCategory::parse(key) else {
                continue;
            };
            let Some(level) = DiagnosticLevel::parse(value) else {
                continue;
            };
            config.set(category, level);
        }
        Some(config)
    }

    fn all(level: DiagnosticLevel) -> Self {
        Self {
            unknown_symbols: level,
            unused: level,
            duplicate_symbols: level,
            members: level,
            type_compatibility: level,
            override_signatures: level,
            php_version: level,
        }
    }

    fn set(&mut self, category: DiagnosticCategory, level: DiagnosticLevel) {
        match category {
            DiagnosticCategory::UnknownSymbols => self.unknown_symbols = level,
            DiagnosticCategory::Unused => self.unused = level,
            DiagnosticCategory::DuplicateSymbols => self.duplicate_symbols = level,
            DiagnosticCategory::Members => self.members = level,
            DiagnosticCategory::TypeCompatibility => self.type_compatibility = level,
            DiagnosticCategory::OverrideSignatures => self.override_signatures = level,
            DiagnosticCategory::PhpVersion => self.php_version = level,
        }
    }

    fn level(self, category: DiagnosticCategory) -> DiagnosticLevel {
        match category {
            DiagnosticCategory::UnknownSymbols => self.unknown_symbols,
            DiagnosticCategory::Unused => self.unused,
            DiagnosticCategory::DuplicateSymbols => self.duplicate_symbols,
            DiagnosticCategory::Members => self.members,
            DiagnosticCategory::TypeCompatibility => self.type_compatibility,
            DiagnosticCategory::OverrideSignatures => self.override_signatures,
            DiagnosticCategory::PhpVersion => self.php_version,
        }
    }

    fn severity(self, category: DiagnosticCategory) -> Option<DiagnosticSeverity> {
        self.level(category).0
    }
}

#[derive(Debug, Default)]
struct AppliedConfiguration {
    diagnostics_changed: bool,
    stubs_changed: bool,
    indexing_changed: bool,
}

#[derive(Debug, Clone)]
struct WorkspaceIndexingOptions {
    include_paths: Vec<PathBuf>,
    exclude_paths: Vec<PathBuf>,
    cache_config: IndexCacheConfig,
    work_done_progress_supported: bool,
}

#[derive(Debug, Clone)]
struct SemanticTokensSnapshot {
    result_id: String,
    data: Vec<SemanticToken>,
}

#[derive(Debug, Default)]
struct SemanticTokensCache {
    next_result_id: u64,
    by_uri: HashMap<String, SemanticTokensSnapshot>,
}

#[derive(Debug, Clone)]
struct WorkspaceRootConfig {
    root: PathBuf,
    namespace_map: Option<NamespaceMap>,
}

const VENDOR_FILE_LRU_CAPACITY: usize = 512;
const VENDOR_PRELOAD_ENTRYPOINT_LIMIT: usize = 16;
const MAX_INDEXING_PARSE_CONCURRENCY: usize = 8;

#[derive(Debug, Clone)]
struct VendorPsr4Mapping {
    prefix: String,
    directories: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default)]
struct VendorAutoloadMap {
    psr4: Vec<VendorPsr4Mapping>,
    files: Vec<PathBuf>,
}

#[derive(Debug)]
struct WorkspaceParseResult {
    path: PathBuf,
    uri: String,
    file_symbols: Option<php_lsp_types::FileSymbols>,
    references: Vec<php_lsp_types::SymbolReference>,
    symbol_count: usize,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct VendorAutoloadCacheEntry {
    map: VendorAutoloadMap,
}

#[derive(Debug, Default)]
struct VendorAutoloadCache {
    by_vendor_dir: HashMap<PathBuf, VendorAutoloadCacheEntry>,
}

impl VendorAutoloadCache {
    fn clear(&mut self) {
        self.by_vendor_dir.clear();
    }
}

#[derive(Debug)]
struct VendorFileLru {
    capacity: usize,
    uris: VecDeque<String>,
}

impl Default for VendorFileLru {
    fn default() -> Self {
        Self {
            capacity: VENDOR_FILE_LRU_CAPACITY,
            uris: VecDeque::new(),
        }
    }
}

impl VendorFileLru {
    #[cfg(test)]
    fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            uris: VecDeque::new(),
        }
    }

    fn touch(&mut self, uri: String) -> Vec<String> {
        if let Some(position) = self.uris.iter().position(|existing| existing == &uri) {
            self.uris.remove(position);
        }
        self.uris.push_back(uri);

        let mut evicted = Vec::new();
        while self.uris.len() > self.capacity {
            if let Some(uri) = self.uris.pop_front() {
                evicted.push(uri);
            }
        }
        evicted
    }

    fn remove(&mut self, uri: &str) {
        if let Some(position) = self.uris.iter().position(|existing| existing == uri) {
            self.uris.remove(position);
        }
    }

    fn clear(&mut self) -> Vec<String> {
        self.uris.drain(..).collect()
    }
}

impl SemanticTokensCache {
    fn store(&mut self, uri: &str, data: Vec<SemanticToken>) -> SemanticTokensSnapshot {
        self.next_result_id += 1;
        let snapshot = SemanticTokensSnapshot {
            result_id: format!("semantic-tokens-{}", self.next_result_id),
            data,
        };
        self.by_uri.insert(uri.to_string(), snapshot.clone());
        snapshot
    }

    fn previous_data(&self, uri: &str, result_id: &str) -> Option<Vec<SemanticToken>> {
        let snapshot = self.by_uri.get(uri)?;
        (snapshot.result_id == result_id).then(|| snapshot.data.clone())
    }

    fn remove(&mut self, uri: &str) {
        self.by_uri.remove(uri);
    }
}

fn php_lsp_settings(settings: &serde_json::Value) -> &serde_json::Value {
    settings.get("phpLsp").unwrap_or(settings)
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

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::RootDir | Component::Prefix(_) | Component::ParentDir => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

fn normalize_config_paths(paths: Vec<String>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .filter_map(|path| {
            let path = path.trim();
            (!path.is_empty()).then(|| normalize_path(Path::new(path)))
        })
        .collect()
}

fn settings_u64(settings: &serde_json::Value, flat_key: &str, nested_path: &[&str]) -> Option<u64> {
    settings_value(settings, flat_key, nested_path).and_then(|value| value.as_u64())
}

fn settings_string_aliases<'a>(
    settings: &'a serde_json::Value,
    flat_key: &str,
    nested_paths: &[&[&str]],
) -> Option<&'a str> {
    if let Some(value) = settings.get(flat_key).and_then(|value| value.as_str()) {
        return Some(value);
    }
    for path in nested_paths {
        let mut current = settings;
        let mut found = true;
        for key in *path {
            match current.get(*key) {
                Some(value) => current = value,
                None => {
                    found = false;
                    break;
                }
            }
        }
        if found {
            if let Some(value) = current.as_str() {
                return Some(value);
            }
        }
    }
    None
}

fn settings_u64_aliases(
    settings: &serde_json::Value,
    flat_key: &str,
    nested_paths: &[&[&str]],
) -> Option<u64> {
    if let Some(value) = settings.get(flat_key).and_then(|value| value.as_u64()) {
        return Some(value);
    }
    for path in nested_paths {
        let mut current = settings;
        let mut found = true;
        for key in *path {
            match current.get(*key) {
                Some(value) => current = value,
                None => {
                    found = false;
                    break;
                }
            }
        }
        if found {
            if let Some(value) = current.as_u64() {
                return Some(value);
            }
        }
    }
    None
}

/// Main LSP backend holding all state.
pub struct PhpLspBackend {
    /// Client handle for sending notifications to VS Code.
    client: Client,
    /// Open document parsers (URI string → FileParser).
    open_files: Arc<DashMap<String, FileParser>>,
    /// Latest LSP document version observed for each open document.
    document_versions: Arc<DashMap<String, i32>>,
    /// Per-document debounce tasks for fast diagnostics after didChange.
    diagnostic_debounce_tasks: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
    /// Per-document external analyzer runs that can be cancelled by newer document events.
    analyzer_runs: Arc<Mutex<HashMap<String, OperationCancellationToken>>>,
    /// Current background workspace indexing run.
    indexing_run: Arc<Mutex<Option<OperationCancellationToken>>>,
    /// Global workspace symbol index.
    index: Arc<WorkspaceIndex>,
    /// Workspace root path (set during initialize).
    workspace_root: Mutex<Option<PathBuf>>,
    /// Workspace roots from initialize/workspaceFolders after composer discovery.
    workspace_roots: Mutex<Vec<PathBuf>>,
    /// Namespace map from composer.json.
    namespace_map: Mutex<Option<NamespaceMap>>,
    /// Per-workspace composer namespace maps and effective roots.
    workspace_configs: Mutex<Vec<WorkspaceRootConfig>>,
    /// Trace level from InitializeParams (off/messages/verbose).
    trace_level: Mutex<TraceValue>,
    /// Last explicit client initialization/configuration settings.
    client_settings: Mutex<serde_json::Value>,
    /// Path to bundled phpstorm-stubs (from client initializationOptions).
    stubs_path: Mutex<Option<PathBuf>>,
    /// Target PHP version from client initializationOptions.
    php_version: Mutex<PhpVersion>,
    /// Diagnostics level from phpLsp.diagnostics.mode.
    diagnostics_mode: Mutex<DiagnosticsMode>,
    /// Per-category severity controls for php-lsp diagnostics.
    diagnostic_severity: Mutex<DiagnosticSeverityConfig>,
    /// PHPStan subprocess diagnostics configuration.
    phpstan_config: Mutex<PhpStanConfig>,
    /// Psalm subprocess diagnostics configuration.
    psalm_config: Mutex<PsalmConfig>,
    /// Whether composer.json autoload discovery is enabled.
    composer_enabled: Mutex<bool>,
    /// Whether lazy vendor indexing is enabled.
    index_vendor: Mutex<bool>,
    /// Additional files/directories included in workspace indexing.
    include_paths: Mutex<Vec<PathBuf>>,
    /// Files/directories excluded from workspace indexing.
    exclude_paths: Mutex<Vec<PathBuf>>,
    /// Configured phpstorm-stubs extension directory names.
    stub_extensions: Mutex<Vec<String>>,
    /// Configured server log level label.
    log_level: Mutex<String>,
    /// Whether the client advertised window/workDoneProgress support.
    work_done_progress_supported: Mutex<bool>,
    /// External formatter configuration.
    formatting_config: Mutex<FormattingConfig>,
    /// Last semantic token snapshots used for full/delta requests.
    semantic_tokens_cache: Mutex<SemanticTokensCache>,
    /// Parsed Composer vendor metadata keyed by vendor directory.
    vendor_autoload_cache: Arc<Mutex<VendorAutoloadCache>>,
    /// Bounded set of lazy-indexed vendor files currently kept in the symbol index.
    vendor_file_lru: Arc<Mutex<VendorFileLru>>,
}

impl PhpLspBackend {
    pub fn new(client: Client) -> Self {
        PhpLspBackend {
            client,
            open_files: Arc::new(DashMap::new()),
            document_versions: Arc::new(DashMap::new()),
            diagnostic_debounce_tasks: Arc::new(Mutex::new(HashMap::new())),
            analyzer_runs: Arc::new(Mutex::new(HashMap::new())),
            indexing_run: Arc::new(Mutex::new(None)),
            index: Arc::new(WorkspaceIndex::new()),
            workspace_root: Mutex::new(None),
            workspace_roots: Mutex::new(Vec::new()),
            namespace_map: Mutex::new(None),
            workspace_configs: Mutex::new(Vec::new()),
            trace_level: Mutex::new(TraceValue::Off),
            client_settings: Mutex::new(serde_json::json!({})),
            stubs_path: Mutex::new(None),
            php_version: Mutex::new(PhpVersion::DEFAULT),
            diagnostics_mode: Mutex::new(DiagnosticsMode::default()),
            diagnostic_severity: Mutex::new(DiagnosticSeverityConfig::default()),
            phpstan_config: Mutex::new(PhpStanConfig::default()),
            psalm_config: Mutex::new(PsalmConfig::default()),
            composer_enabled: Mutex::new(true),
            index_vendor: Mutex::new(true),
            include_paths: Mutex::new(Vec::new()),
            exclude_paths: Mutex::new(Vec::new()),
            stub_extensions: Mutex::new(Vec::new()),
            log_level: Mutex::new("info".to_string()),
            work_done_progress_supported: Mutex::new(false),
            formatting_config: Mutex::new(FormattingConfig::default()),
            semantic_tokens_cache: Mutex::new(SemanticTokensCache::default()),
            vendor_autoload_cache: Arc::new(Mutex::new(VendorAutoloadCache::default())),
            vendor_file_lru: Arc::new(Mutex::new(VendorFileLru::default())),
        }
    }

    /// Log a message to the client if trace level is verbose.
    async fn log_trace(&self, message: &str) {
        let level = *self.trace_level.lock().await;
        if level == TraceValue::Verbose {
            tracing::trace!("{}", message);
            self.client.log_message(MessageType::LOG, message).await;
        }
    }

    fn current_document_version(&self, uri_str: &str) -> Option<i32> {
        self.document_versions.get(uri_str).map(|version| *version)
    }

    fn accept_document_version(&self, uri_str: &str, incoming: i32) -> bool {
        let current = self.current_document_version(uri_str);
        if !document_version_is_newer(current, incoming) {
            tracing::debug!(
                "Ignoring stale didChange for {}: incoming version {}, current version {:?}",
                uri_str,
                incoming,
                current
            );
            return false;
        }

        self.document_versions.insert(uri_str.to_string(), incoming);
        true
    }

    async fn cancel_debounced_diagnostics(&self, uri_str: &str) {
        if let Some(handle) = self.diagnostic_debounce_tasks.lock().await.remove(uri_str) {
            handle.abort();
        }
    }

    async fn start_analyzer_run(&self, uri_str: &str) -> OperationCancellationToken {
        let token = OperationCancellationToken::new();
        if let Some(previous) = self
            .analyzer_runs
            .lock()
            .await
            .insert(uri_str.to_string(), token.clone())
        {
            previous.cancel();
        }
        token
    }

    async fn finish_analyzer_run(&self, uri_str: &str, token: &OperationCancellationToken) {
        let mut runs = self.analyzer_runs.lock().await;
        if runs
            .get(uri_str)
            .is_some_and(|current| current.is_same(token))
        {
            runs.remove(uri_str);
        }
    }

    async fn cancel_analyzer_run(&self, uri_str: &str) {
        if let Some(token) = self.analyzer_runs.lock().await.remove(uri_str) {
            token.cancel();
        }
    }

    async fn start_indexing_run(&self) -> OperationCancellationToken {
        let token = OperationCancellationToken::new();
        if let Some(previous) = self.indexing_run.lock().await.replace(token.clone()) {
            previous.cancel();
        }
        token
    }

    async fn schedule_fast_diagnostics(&self, uri: Uri, version: i32) {
        let uri_str = uri.as_str().to_string();
        let client = self.client.clone();
        let open_files = self.open_files.clone();
        let document_versions = self.document_versions.clone();
        let index = self.index.clone();
        let diagnostics_mode = *self.diagnostics_mode.lock().await;
        let diagnostic_severity = *self.diagnostic_severity.lock().await;
        let php_version = *self.php_version.lock().await;
        let debounce = Duration::from_millis(DID_CHANGE_DIAGNOSTICS_DEBOUNCE_MS);
        let task_uri_str = uri_str.clone();

        let handle = tokio::spawn(async move {
            tokio::time::sleep(debounce).await;

            if document_versions.get(&task_uri_str).map(|current| *current) != Some(version) {
                return;
            }

            let diagnostics = compute_open_file_diagnostics(
                &task_uri_str,
                &open_files,
                &index,
                diagnostics_mode,
                diagnostic_severity,
                php_version,
            );

            if document_versions.get(&task_uri_str).map(|current| *current) == Some(version) {
                client
                    .publish_diagnostics(uri, diagnostics, Some(version))
                    .await;
            }
        });

        if let Some(previous) = self
            .diagnostic_debounce_tasks
            .lock()
            .await
            .insert(uri_str, handle)
        {
            previous.abort();
        }
    }

    async fn apply_configuration_settings(
        &self,
        raw_settings: &serde_json::Value,
    ) -> AppliedConfiguration {
        let settings = php_lsp_settings(raw_settings);
        let mut applied = AppliedConfiguration::default();

        if let Some(raw_php_version) = settings_string(settings, "phpVersion", &["phpVersion"]) {
            if let Some(parsed) = PhpVersion::parse(raw_php_version) {
                let mut php_version = self.php_version.lock().await;
                if *php_version != parsed {
                    *php_version = parsed;
                    applied.diagnostics_changed = true;
                    applied.stubs_changed = true;
                }
            } else {
                tracing::warn!("Ignoring invalid phpVersion: {}", raw_php_version);
            }
        }

        if let Some(raw_mode) =
            settings_string(settings, "diagnosticsMode", &["diagnostics", "mode"])
        {
            if let Some(parsed) = DiagnosticsMode::parse(raw_mode) {
                let mut diagnostics_mode = self.diagnostics_mode.lock().await;
                if *diagnostics_mode != parsed {
                    *diagnostics_mode = parsed;
                    applied.diagnostics_changed = true;
                }
            } else {
                tracing::warn!("Ignoring invalid diagnostics mode: {}", raw_mode);
            }
        }

        if let Some(raw_severity) = settings_value(
            settings,
            "diagnosticsSeverity",
            &["diagnostics", "severity"],
        ) {
            if let Some(parsed) = DiagnosticSeverityConfig::parse(raw_severity) {
                let mut diagnostic_severity = self.diagnostic_severity.lock().await;
                if *diagnostic_severity != parsed {
                    *diagnostic_severity = parsed;
                    applied.diagnostics_changed = true;
                }
            } else {
                tracing::warn!("Ignoring invalid diagnostics severity settings: {raw_severity}");
            }
        }

        if let Some(enabled) = settings_bool(settings, "composerEnabled", &["composer", "enabled"])
        {
            let mut composer_enabled = self.composer_enabled.lock().await;
            if *composer_enabled != enabled {
                *composer_enabled = enabled;
                applied.indexing_changed = true;
            }
        }

        if let Some(enabled) = settings_bool(settings, "indexVendor", &["indexVendor"]) {
            let changed = {
                let mut index_vendor = self.index_vendor.lock().await;
                if *index_vendor != enabled {
                    *index_vendor = enabled;
                    true
                } else {
                    false
                }
            };
            if changed {
                applied.indexing_changed = true;
                if !enabled {
                    self.vendor_autoload_cache.lock().await.clear();
                    let evicted = self.vendor_file_lru.lock().await.clear();
                    for uri in evicted {
                        self.index.remove_file(&uri);
                    }
                    let roots = self.workspace_roots.lock().await.clone();
                    remove_indexed_vendor_symbols(&self.index, &roots);
                }
            }
        }

        if let Some(paths) = settings_string_array(settings, "includePaths", &["includePaths"]) {
            let paths = normalize_config_paths(paths);
            let mut include_paths = self.include_paths.lock().await;
            if *include_paths != paths {
                *include_paths = paths;
                applied.indexing_changed = true;
            }
        }

        if let Some(paths) = settings_string_array(settings, "excludePaths", &["excludePaths"]) {
            let paths = normalize_config_paths(paths);
            let mut exclude_paths = self.exclude_paths.lock().await;
            if *exclude_paths != paths {
                *exclude_paths = paths;
                applied.indexing_changed = true;
            }
        }

        if let Some(extensions) =
            settings_string_array(settings, "stubExtensions", &["stubs", "extensions"])
        {
            let mut stub_extensions = self.stub_extensions.lock().await;
            if *stub_extensions != extensions {
                *stub_extensions = extensions;
                applied.stubs_changed = true;
            }
        }

        if let Some(log_level) = settings_string(settings, "logLevel", &["logLevel"]) {
            *self.log_level.lock().await = log_level.trim().to_ascii_lowercase();
        }

        if let Some(stubs_path) = settings_string_aliases(
            settings,
            "stubsPath",
            &[&["stubs", "path"], &["bundledStubsPath"]],
        ) {
            let next_path = if stubs_path.trim().is_empty() {
                None
            } else {
                Some(PathBuf::from(stubs_path))
            };
            let mut current_path = self.stubs_path.lock().await;
            if *current_path != next_path {
                *current_path = next_path;
                applied.stubs_changed = true;
            }
        }

        let formatting_provider =
            settings_string(settings, "formattingProvider", &["formatting", "provider"]);
        let formatting_command =
            settings_value(settings, "formattingCommand", &["formatting", "command"])
                .and_then(|value| value.as_str());
        let formatting_timeout_ms = settings_u64_aliases(
            settings,
            "formattingTimeoutMs",
            &[&["formatting", "timeoutMs"], &["formatting", "timeout"]],
        );
        if formatting_provider.is_some()
            || formatting_command.is_some()
            || formatting_timeout_ms.is_some()
        {
            let current = self.formatting_config.lock().await.clone();
            let next_config = {
                let provider = formatting_provider.unwrap_or(&current.provider);
                let command = if formatting_command.is_some() {
                    formatting_command
                } else if formatting_provider.is_some() && provider != current.provider {
                    None
                } else {
                    current.command.as_deref()
                };
                FormattingConfig::from_options(
                    Some(provider),
                    command,
                    formatting_timeout_ms.or(Some(current.timeout_ms)),
                )
            };
            *self.formatting_config.lock().await = next_config;
        }

        let phpstan_enabled = settings_bool(settings, "phpstanEnabled", &["phpstan", "enabled"]);
        let phpstan_command = settings_string(settings, "phpstanCommand", &["phpstan", "command"]);
        let phpstan_timeout_ms =
            settings_u64(settings, "phpstanTimeoutMs", &["phpstan", "timeoutMs"]);
        let phpstan_memory_limit = settings_string_aliases(
            settings,
            "phpstanMemoryLimit",
            &[&["phpstan", "memoryLimit"], &["phpstan", "memory_limit"]],
        );

        if phpstan_enabled.is_some()
            || phpstan_command.is_some()
            || phpstan_timeout_ms.is_some()
            || phpstan_memory_limit.is_some()
        {
            let current = self.phpstan_config.lock().await.clone();
            let mut next_config = current.clone();
            if let Some(enabled) = phpstan_enabled {
                next_config.enabled = enabled;
            }
            if let Some(command) = phpstan_command {
                let command = command.trim();
                if command.is_empty() {
                    next_config.command = PhpStanConfig::default().command;
                } else {
                    next_config.command = command.to_string();
                }
            }
            if let Some(timeout_ms) = phpstan_timeout_ms {
                next_config.timeout_ms = timeout_ms.max(1_000);
            }
            if let Some(memory_limit) = phpstan_memory_limit {
                let memory_limit = memory_limit.trim();
                next_config.memory_limit =
                    (!memory_limit.is_empty()).then(|| memory_limit.to_string());
            }

            if next_config != current {
                *self.phpstan_config.lock().await = next_config;
                applied.diagnostics_changed = true;
            }
        }

        let psalm_enabled = settings_bool(settings, "psalmEnabled", &["psalm", "enabled"]);
        let psalm_command = settings_string(settings, "psalmCommand", &["psalm", "command"]);
        let psalm_timeout_ms = settings_u64(settings, "psalmTimeoutMs", &["psalm", "timeoutMs"]);

        if psalm_enabled.is_some() || psalm_command.is_some() || psalm_timeout_ms.is_some() {
            let current = self.psalm_config.lock().await.clone();
            let mut next_config = current.clone();
            if let Some(enabled) = psalm_enabled {
                next_config.enabled = enabled;
            }
            if let Some(command) = psalm_command {
                let command = command.trim();
                if command.is_empty() {
                    next_config.command = PsalmConfig::default().command;
                } else {
                    next_config.command = command.to_string();
                }
            }
            if let Some(timeout_ms) = psalm_timeout_ms {
                next_config.timeout_ms = timeout_ms.max(1_000);
            }

            if next_config != current {
                *self.psalm_config.lock().await = next_config;
                applied.diagnostics_changed = true;
            }
        }

        applied
    }

    async fn apply_effective_configuration_settings(
        &self,
        client_settings: &serde_json::Value,
        workspace_roots: &[PathBuf],
    ) -> AppliedConfiguration {
        let (settings, messages) =
            load_effective_configuration_settings(workspace_roots, client_settings);
        for message in messages {
            if message.contains("failed") {
                tracing::warn!("{}", message);
                self.client.log_message(MessageType::WARNING, message).await;
            } else {
                tracing::info!("{}", message);
                self.client.log_message(MessageType::INFO, message).await;
            }
        }
        self.apply_configuration_settings(&settings).await
    }

    async fn apply_configuration_side_effects(&self, applied: AppliedConfiguration) {
        if applied.stubs_changed {
            self.reload_configured_stubs().await;
        }
        if applied.indexing_changed {
            self.reindex_workspaces().await;
        }
        if applied.diagnostics_changed || applied.stubs_changed {
            self.republish_open_diagnostics().await;
        }
    }

    async fn reload_effective_configuration(&self) {
        let client_settings = self.client_settings.lock().await.clone();
        let workspace_roots = self.workspace_roots.lock().await.clone();
        let applied = self
            .apply_effective_configuration_settings(&client_settings, &workspace_roots)
            .await;
        self.apply_configuration_side_effects(applied).await;
    }

    async fn reload_configured_stubs(&self) {
        let Some(root) = self.workspace_root.lock().await.clone() else {
            return;
        };
        let root_label = root.display().to_string();
        let index = self.index.clone();
        let client_stubs_path = self.stubs_path.lock().await.clone();
        let stub_extensions = self.stub_extensions.lock().await.clone();
        let php_version = *self.php_version.lock().await;

        send_indexing_status(
            &self.client,
            serde_json::json!({
                "phase": "loadingStubs",
                "root": root_label,
                "message": "Reloading PHP stubs"
            }),
        )
        .await;

        let loaded = tokio::task::spawn_blocking(move || {
            load_configured_stubs(
                &index,
                &root,
                client_stubs_path,
                stub_extensions,
                php_version,
                true,
            )
        })
        .await
        .unwrap_or(0);

        send_indexing_status(
            &self.client,
            serde_json::json!({
                "phase": "stubsLoaded",
                "root": root_label,
                "message": format!("Reloaded {} stub files", loaded),
                "stubFiles": loaded
            }),
        )
        .await;

        self.client
            .log_message(
                MessageType::INFO,
                format!("php-lsp: reloaded {} stub files", loaded),
            )
            .await;
    }

    async fn reindex_workspaces(&self) {
        let roots = self.workspace_roots.lock().await.clone();
        if roots.is_empty() {
            return;
        }

        let composer_enabled = *self.composer_enabled.lock().await;
        let configs = dedup_workspace_configs(
            roots
                .iter()
                .map(|root| discover_workspace_root_config(root, composer_enabled))
                .collect(),
        );
        let effective_roots: Vec<PathBuf> =
            configs.iter().map(|config| config.root.clone()).collect();

        if let Some(first_root) = effective_roots.first() {
            *self.workspace_root.lock().await = Some(first_root.clone());
        }
        *self.workspace_roots.lock().await = effective_roots.clone();
        *self.workspace_configs.lock().await = configs.clone();
        *self.namespace_map.lock().await = configs
            .iter()
            .find_map(|config| config.namespace_map.clone());

        let removed = remove_indexed_file_symbols(&self.index, &effective_roots);
        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "php-lsp: reindexing workspace after indexing configuration change (removed {} indexed files)",
                    removed
                ),
            )
            .await;

        let client = self.client.clone();
        let index = self.index.clone();
        let open_files = self.open_files.clone();
        let reindex_document_versions = self.document_versions.clone();
        let reindex_index = self.index.clone();
        let reindex_client = self.client.clone();
        let diagnostics_mode = *self.diagnostics_mode.lock().await;
        let diagnostic_severity = *self.diagnostic_severity.lock().await;
        let php_version = *self.php_version.lock().await;
        let index_vendor = *self.index_vendor.lock().await;
        let vendor_autoload_cache = self.vendor_autoload_cache.clone();
        let vendor_file_lru = self.vendor_file_lru.clone();
        let work_done_progress_supported = *self.work_done_progress_supported.lock().await;
        let include_paths = self.include_paths.lock().await.clone();
        let exclude_paths = self.exclude_paths.lock().await.clone();
        let stub_extensions = self.stub_extensions.lock().await.clone();
        let client_stubs_path = self.stubs_path.lock().await.clone();
        let cache_config = workspace_index_cache_config(
            configs.first().map(|config| config.root.as_path()),
            php_version,
            &include_paths,
            &exclude_paths,
            &stub_extensions,
            client_stubs_path.as_deref(),
        );
        let indexing_options = WorkspaceIndexingOptions {
            include_paths,
            exclude_paths,
            cache_config,
            work_done_progress_supported,
        };
        let indexing_token = self.start_indexing_run().await;
        tokio::spawn(async move {
            for config in &configs {
                if indexing_token.is_cancelled() {
                    return;
                }
                if let Err(e) = index_workspace(
                    &client,
                    &index,
                    &config.root,
                    config.namespace_map.as_ref(),
                    &indexing_options,
                    &indexing_token,
                )
                .await
                {
                    tracing::error!("Workspace reindexing failed: {}", e);
                    send_indexing_status(
                        &client,
                        serde_json::json!({
                            "phase": "error",
                            "root": config.root.display().to_string(),
                            "message": format!("Workspace reindexing failed: {}", e)
                        }),
                    )
                    .await;
                    client
                        .log_message(
                            MessageType::ERROR,
                            format!("Workspace reindexing failed: {}", e),
                        )
                        .await;
                    return;
                }
                if indexing_token.is_cancelled() {
                    return;
                }

                if index_vendor {
                    preload_vendor_entrypoints(
                        index.clone(),
                        &config.root,
                        &indexing_options.exclude_paths,
                        php_version,
                        &vendor_autoload_cache,
                        &vendor_file_lru,
                    )
                    .await;
                }
            }

            if indexing_token.is_cancelled() {
                return;
            }
            for entry in open_files.iter() {
                let uri_str = entry.key().clone();
                if let Ok(uri) = uri_str.parse::<Uri>() {
                    let version = reindex_document_versions
                        .get(&uri_str)
                        .map(|current| *current);
                    let diags = compute_diagnostics_with_config(
                        &uri_str,
                        &entry,
                        &reindex_index,
                        diagnostics_mode,
                        diagnostic_severity,
                        php_version,
                    );
                    if reindex_document_versions
                        .get(&uri_str)
                        .map(|current| *current)
                        == version
                    {
                        reindex_client
                            .publish_diagnostics(uri, diags, version)
                            .await;
                    }
                }
            }
        });
    }

    async fn republish_open_diagnostics(&self) {
        let open_uris: Vec<Uri> = self
            .open_files
            .iter()
            .filter_map(|entry| entry.key().parse::<Uri>().ok())
            .collect();

        for uri in open_uris {
            self.publish_diagnostics(&uri).await;
        }
    }

    async fn workspace_root_for_uri(&self, uri_str: &str) -> Option<PathBuf> {
        let roots = self.workspace_roots.lock().await.clone();
        if let Some(path) = uri_to_path(uri_str) {
            if let Some(root) = roots
                .iter()
                .filter(|root| path.starts_with(root))
                .max_by_key(|root| root.components().count())
            {
                return Some(root.clone());
            }
        }

        if let Some(root) = roots.into_iter().next() {
            return Some(root);
        }

        self.workspace_root.lock().await.clone()
    }

    async fn touch_vendor_file_lru(&self, file_path: &Path) {
        touch_vendor_file_lru(&self.index, &self.vendor_file_lru, file_path).await;
    }

    /// Resolve a member's type from the workspace index (for cross-file type resolution).
    ///
    /// For properties (`member_name` starts with `$`): returns the property type FQN.
    /// For methods: returns the method's return type FQN.
    ///
    /// Walks the class hierarchy to find inherited members.
    fn resolve_member_type(&self, class_fqn: &str, member_name: &str) -> Option<String> {
        resolve_member_type_from_index(&self.index, class_fqn, member_name)
    }

    fn resolve_completion_member_type(
        &self,
        class_fqn: &str,
        member_name: &str,
        file_symbols: &php_lsp_types::FileSymbols,
    ) -> Option<String> {
        self.resolve_member_type(class_fqn, member_name)
            .or_else(|| {
                let member_fqn = format!("{}::{}", class_fqn, member_name);
                let bare_name = member_name.strip_prefix('$').unwrap_or(member_name);
                file_symbols.symbols.iter().find_map(|sym| {
                    if sym.fqn == member_fqn
                        || (sym.parent_fqn.as_deref() == Some(class_fqn)
                            && (sym.name == member_name || sym.name == bare_name))
                    {
                        symbol_return_type_fqn(&self.index, class_fqn, sym)
                    } else {
                        None
                    }
                })
            })
    }

    fn infer_completion_object_type(
        &self,
        object_expr: &str,
        tree: &tree_sitter::Tree,
        source: &str,
        file_symbols: &php_lsp_types::FileSymbols,
        line: u32,
        byte_col: u32,
    ) -> Option<String> {
        let object_expr = object_expr.trim();
        if let Some(class_fqn) = infer_new_expression_type(object_expr, file_symbols) {
            return Some(class_fqn);
        }

        if object_expr.contains("->") || object_expr.contains("?->") {
            return self.infer_completion_member_chain_type(
                object_expr,
                tree,
                source,
                file_symbols,
                line,
                byte_col,
            );
        }

        if object_expr == "$this" {
            current_class_fqn_at_range(file_symbols, (line, byte_col, line, byte_col))
                .or_else(|| current_class_fqn(file_symbols))
        } else if object_expr.starts_with('$') {
            self.infer_completion_variable_type(
                tree,
                source,
                file_symbols,
                line,
                byte_col,
                object_expr,
            )
        } else {
            None
        }
    }

    fn infer_completion_variable_type(
        &self,
        tree: &tree_sitter::Tree,
        source: &str,
        file_symbols: &php_lsp_types::FileSymbols,
        line: u32,
        byte_col: u32,
        var_name: &str,
    ) -> Option<String> {
        let resolve_member_type = |class_fqn: &str, member_name: &str| {
            self.resolve_completion_member_type(class_fqn, member_name, file_symbols)
        };
        infer_variable_type_at_position_with_resolver(
            tree,
            source,
            file_symbols,
            line,
            byte_col,
            var_name,
            &resolve_member_type,
        )
    }

    fn infer_completion_member_chain_type(
        &self,
        object_expr: &str,
        tree: &tree_sitter::Tree,
        source: &str,
        file_symbols: &php_lsp_types::FileSymbols,
        line: u32,
        byte_col: u32,
    ) -> Option<String> {
        let normalized = object_expr.replace("?->", "->");
        let mut parts = normalized.split("->");
        let base_expr = parts.next()?.trim();
        let mut class_fqn = if base_expr == "$this" {
            current_class_fqn_at_range(file_symbols, (line, byte_col, line, byte_col))
                .or_else(|| current_class_fqn(file_symbols))?
        } else if base_expr.starts_with('$') {
            self.infer_completion_variable_type(
                tree,
                source,
                file_symbols,
                line,
                byte_col,
                base_expr,
            )?
        } else {
            infer_new_expression_type(base_expr, file_symbols)?
        };

        for raw_member in parts {
            let member = raw_member.trim();
            if member.is_empty() {
                return None;
            }

            let is_method_call = member.contains('(');
            let member_name = member
                .split('(')
                .next()
                .unwrap_or(member)
                .trim()
                .trim_start_matches('$');
            if member_name.is_empty() {
                return None;
            }

            let lookup_name = if is_method_call {
                member_name.to_string()
            } else {
                format!("${}", member_name)
            };
            class_fqn =
                self.resolve_completion_member_type(&class_fqn, &lookup_name, file_symbols)?;
        }

        Some(class_fqn)
    }

    /// Resolve a FQN, falling back to lazy vendor indexing if not found.
    async fn resolve_fqn_lazy(
        &self,
        fqn: &str,
    ) -> Option<std::sync::Arc<php_lsp_types::SymbolInfo>> {
        // Try direct lookup first
        if let Some(sym) = self.index.resolve_fqn(fqn) {
            return Some(sym);
        }

        // For member FQNs like "Class::method", extract the class part
        // so PSR-4 resolution works (PSR-4 maps class names, not members).
        let class_fqn = if let Some((cls, _member)) = fqn.rsplit_once("::") {
            cls
        } else {
            fqn
        };

        self.lazy_index_class_dependencies(class_fqn).await;

        // Retry resolution with the full FQN
        if let Some(sym) = self.index.resolve_fqn(fqn) {
            return Some(sym);
        }

        None
    }

    /// Lazy-index a single class FQN by finding its file via PSR-4/vendor mappings.
    /// Returns true if a new file was indexed.
    async fn lazy_index_class(&self, class_fqn: &str) -> bool {
        // Skip if already in the index
        if self.index.types.contains_key(class_fqn) {
            return false;
        }

        let index_vendor = *self.index_vendor.lock().await;
        let mut configs = self.workspace_configs.lock().await.clone();
        let exclude_paths = self.exclude_paths.lock().await.clone();
        let php_version = *self.php_version.lock().await;
        if configs.is_empty() {
            let root = self.workspace_root.lock().await.clone();
            let namespace_map = self.namespace_map.lock().await.clone();
            if let Some(root) = root {
                configs.push(WorkspaceRootConfig {
                    root,
                    namespace_map,
                });
            }
        }

        for config in configs {
            let mut all_paths = config
                .namespace_map
                .as_ref()
                .map(|ns_map| ns_map.resolve_class_to_paths(class_fqn))
                .unwrap_or_default();

            let vendor_dir = config.root.join("vendor");
            if index_vendor && vendor_dir.is_dir() && all_paths.is_empty() {
                if let Some(vendor_map) =
                    cached_vendor_autoload_map(&self.vendor_autoload_cache, &vendor_dir).await
                {
                    if let Some(vendor_paths) =
                        resolve_vendor_paths_from_map(class_fqn, &vendor_map)
                    {
                        all_paths.extend(vendor_paths);
                    }
                }
            }

            for path in &all_paths {
                let abs = if path.is_absolute() {
                    path.clone()
                } else {
                    config.root.join(path)
                };

                if path_is_excluded(&abs, &config.root, &exclude_paths) {
                    continue;
                }

                let is_vendor_file = abs.starts_with(config.root.join("vendor"));
                let vendor_cache_config = is_vendor_file
                    .then(|| vendor_index_cache_config(&config.root, php_version, &exclude_paths));
                if let Some(cache_config) = vendor_cache_config.as_ref() {
                    if load_cached_vendor_file_blocking(
                        self.index.clone(),
                        config.root.clone(),
                        abs.clone(),
                        cache_config.clone(),
                    )
                    .await
                    {
                        self.touch_vendor_file_lru(&abs).await;
                        tracing::debug!("Lazy-indexed vendor file from cache: {}", abs.display());
                        return true;
                    }
                }

                if parse_and_index_php_file_blocking(
                    self.index.clone(),
                    abs.clone(),
                    "lazy PHP file index",
                )
                .await
                {
                    if is_vendor_file {
                        self.touch_vendor_file_lru(&abs).await;
                    }
                    tracing::debug!("Lazy-indexed file: {}", abs.display());
                    return true;
                }
            }
        }

        false
    }

    /// Recursively lazy-index parent classes (extends + implements) up to a depth limit.
    fn lazy_index_parents<'a>(
        &'a self,
        class_fqn: &'a str,
        depth: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            const MAX_DEPTH: usize = 10;
            if depth >= MAX_DEPTH {
                return;
            }

            // Get the class from the index to read its extends/implements
            let parent_fqns: Vec<String> = if let Some(sym) = self.index.types.get(class_fqn) {
                sym.extends
                    .iter()
                    .chain(sym.implements.iter())
                    .chain(sym.traits.iter())
                    .cloned()
                    .collect()
            } else {
                return;
            };

            for parent_fqn in parent_fqns {
                // Lazy-index the parent class file
                self.lazy_index_class(&parent_fqn).await;
                // Recurse into the parent's parents
                self.lazy_index_parents(&parent_fqn, depth + 1).await;
            }
        })
    }

    /// Lazy-index simple class return types from already-indexed members.
    async fn lazy_index_member_return_types(&self, class_fqn: &str) {
        let return_fqns: Vec<String> = self
            .index
            .get_members(class_fqn)
            .into_iter()
            .filter_map(|sym| {
                let owner_fqn = sym.parent_fqn.as_deref().unwrap_or(class_fqn);
                symbol_return_type_fqn(&self.index, owner_fqn, &sym)
            })
            .filter(|fqn| fqn.contains('\\') && !self.index.types.contains_key(fqn.as_str()))
            .collect();

        for return_fqn in return_fqns {
            self.lazy_index_class(&return_fqn).await;
            self.lazy_index_parents(&return_fqn, 0).await;
        }
    }

    async fn lazy_index_class_dependencies(&self, class_fqn: &str) {
        self.lazy_index_class(class_fqn).await;
        self.lazy_index_parents(class_fqn, 0).await;
        self.lazy_index_member_return_types(class_fqn).await;
    }

    /// Resolve symbol from index with fallback for global built-ins.
    fn resolve_fqn_with_fallback(
        &self,
        fqn: &str,
        ref_kind: RefKind,
    ) -> Option<std::sync::Arc<php_lsp_types::SymbolInfo>> {
        if let Some(sym) = self.index.resolve_fqn(fqn) {
            return Some(sym);
        }
        if ref_kind == RefKind::FunctionCall || ref_kind == RefKind::GlobalConstant {
            if let Some((_, short_name)) = fqn.rsplit_once('\\') {
                if let Some(sym) = self.index.resolve_fqn(short_name) {
                    return Some(sym);
                }
            }
        }
        None
    }

    /// Fallback for `$this->prop->member()` when the declared property type
    /// doesn't have `member`. Scans the file for `$this->prop = <expr>`
    /// assignments, infers the RHS type, and tries to resolve the member on that
    /// type instead.
    async fn try_property_assignment_type_fallback(
        &self,
        uri_str: &str,
        prop_name: &str,
        member_name: &str,
    ) -> Option<GotoDefinitionResponse> {
        use php_lsp_parser::resolve::infer_property_type_from_assignments;

        let inferred_types = {
            let parser = match self.open_files.get(uri_str) {
                Some(p) => p,
                None => {
                    tracing::debug!("Property fallback: file not open: {}", uri_str);
                    return None;
                }
            };
            let tree = match parser.tree() {
                Some(t) => t,
                None => {
                    tracing::debug!("Property fallback: no tree for {}", uri_str);
                    return None;
                }
            };
            let source = parser.source();

            let file_symbols = self
                .index
                .file_symbols
                .get(uri_str)
                .map(|entry| entry.value().clone())
                .unwrap_or_default();

            let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
                self.resolve_member_type(class_fqn, member_name)
            };

            let result = infer_property_type_from_assignments(
                tree,
                &source,
                prop_name,
                &file_symbols,
                Some(&resolver),
            );
            tracing::debug!(
                "Property fallback: infer_property_type_from_assignments('{}') = {:?}",
                prop_name,
                result
            );
            result
        };

        for assigned_type in &inferred_types {
            let fallback_fqn = format!("{}::{}", assigned_type, member_name);
            tracing::debug!(
                "Property assignment fallback: $this->{} assigned type '{}', trying '{}'",
                prop_name,
                assigned_type,
                fallback_fqn
            );

            if let Some(sym) = self.resolve_fqn_lazy(&fallback_fqn).await {
                if let Ok(target_uri) = sym.uri.parse::<Uri>() {
                    let range = Range {
                        start: Position::new(sym.selection_range.0, sym.selection_range.1),
                        end: Position::new(sym.selection_range.2, sym.selection_range.3),
                    };
                    return Some(GotoDefinitionResponse::Scalar(Location {
                        uri: target_uri,
                        range,
                    }));
                }
            }
        }

        None
    }

    /// Resolve symbol lazily with fallback for global built-ins.
    async fn resolve_fqn_lazy_with_fallback(
        &self,
        fqn: &str,
        ref_kind: RefKind,
    ) -> Option<std::sync::Arc<php_lsp_types::SymbolInfo>> {
        if let Some(sym) = self.resolve_fqn_lazy(fqn).await {
            return Some(sym);
        }
        if ref_kind == RefKind::FunctionCall || ref_kind == RefKind::GlobalConstant {
            if let Some((_, short_name)) = fqn.rsplit_once('\\') {
                if let Some(sym) = self.resolve_fqn_lazy(short_name).await {
                    return Some(sym);
                }
            }
        }
        None
    }

    fn import_declaration_at_position(
        &self,
        uri: &Uri,
        pos: Position,
    ) -> Option<GotoDefinitionResponse> {
        let uri_str = uri.as_str().to_string();
        let parser = self.open_files.get(&uri_str)?;
        let tree = parser.tree()?;
        let source = parser.source();
        let byte_col = utf16_col_to_byte(&source, pos.line, pos.character);
        let file_symbols = extract_file_symbols(tree, &source, &uri_str);
        let sym = symbol_at_position(tree, &source, pos.line, byte_col, &file_symbols)?;
        let use_stmt = imported_use_statement_for_symbol(&file_symbols, &sym)?;
        let range = range_byte_to_utf16(&source, use_stmt.range);

        Some(GotoDefinitionResponse::Scalar(Location {
            uri: uri.clone(),
            range: Range {
                start: Position::new(range.0, range.1),
                end: Position::new(range.2, range.3),
            },
        }))
    }

    fn file_symbols_for_uri(&self, uri_str: &str) -> Option<php_lsp_types::FileSymbols> {
        if let Some(file_symbols) = self.index.file_symbols.get(uri_str) {
            return Some(file_symbols.value().clone());
        }

        let parser = self.open_files.get(uri_str)?;
        let tree = parser.tree()?;
        let source = parser.source();
        Some(extract_file_symbols(tree, &source, uri_str))
    }

    async fn source_for_uri(&self, uri_str: &str, label: &'static str) -> Option<String> {
        if let Some(parser) = self.open_files.get(uri_str) {
            return Some(parser.source());
        }

        let path = uri_to_path(uri_str)?;
        read_file_to_string_blocking(path, label).await.ok()
    }

    async fn phpdoc_virtual_member_location(
        &self,
        member: &PhpDocVirtualMember,
    ) -> Option<Location> {
        let source = self
            .source_for_uri(&member.owner.uri, "phpdoc virtual member source read")
            .await?;
        let doc_comment = member.owner.doc_comment.as_ref()?;
        let doc_start = source.find(doc_comment)?;
        let range = phpdoc_virtual_member_range(&source, doc_comment, doc_start, member)?;
        let utf16_range = range_byte_to_utf16(&source, range);
        Some(Location {
            uri: member.owner.uri.parse::<Uri>().ok()?,
            range: Range {
                start: Position::new(utf16_range.0, utf16_range.1),
                end: Position::new(utf16_range.2, utf16_range.3),
            },
        })
    }

    fn type_definition_fqn_for_symbol(
        &self,
        symbol: &php_lsp_types::SymbolInfo,
        fallback_file_symbols: &php_lsp_types::FileSymbols,
    ) -> Option<String> {
        if matches!(
            symbol.kind,
            php_lsp_types::PhpSymbolKind::Class
                | php_lsp_types::PhpSymbolKind::Interface
                | php_lsp_types::PhpSymbolKind::Trait
                | php_lsp_types::PhpSymbolKind::Enum
        ) {
            return Some(symbol.fqn.clone());
        }

        let return_type = symbol.signature.as_ref()?.return_type.as_ref()?;
        let declaring_file_symbols = self
            .file_symbols_for_uri(&symbol.uri)
            .unwrap_or_else(|| fallback_file_symbols.clone());

        first_type_definition_fqn(
            return_type,
            &declaring_file_symbols,
            symbol.parent_fqn.as_deref(),
        )
    }

    async fn location_for_type_fqn(&self, fqn: &str) -> Option<Location> {
        if is_builtin_type_name(fqn) {
            return None;
        }

        let symbol = self
            .resolve_fqn_lazy_with_fallback(fqn, RefKind::ClassName)
            .await?;
        let uri = symbol.uri.parse::<Uri>().ok()?;
        Some(Location {
            uri,
            range: Range {
                start: Position::new(symbol.selection_range.0, symbol.selection_range.1),
                end: Position::new(symbol.selection_range.2, symbol.selection_range.3),
            },
        })
    }

    fn reference_locations_for_symbol(
        &self,
        target_fqn: &str,
        target_kind: php_lsp_types::PhpSymbolKind,
        include_declaration: bool,
    ) -> Vec<Location> {
        let mut locations = Vec::new();
        let indexed_references: Vec<_> = self
            .index
            .file_references
            .iter()
            .map(|entry| entry.key().clone())
            .collect();

        for file_uri in indexed_references {
            for reference in
                self.references_for_file(&file_uri, target_fqn, target_kind, include_declaration)
            {
                if let Ok(uri) = file_uri.parse::<Uri>() {
                    locations.push(Location {
                        uri,
                        range: Range {
                            start: Position::new(reference.range.0, reference.range.1),
                            end: Position::new(reference.range.2, reference.range.3),
                        },
                    });
                }
            }
        }

        locations
    }

    async fn phpstan_diagnostics_for_uri(
        &self,
        uri: &Uri,
        cancellation: OperationCancellationToken,
    ) -> Vec<Diagnostic> {
        let config = self.phpstan_config.lock().await.clone();
        if !config.enabled {
            return vec![];
        }

        if !uri_is_php_file(uri) {
            return vec![];
        }

        let Some(file_path) = uri_to_path(uri.as_str()) else {
            return vec![];
        };
        if !file_path.exists() {
            return vec![];
        }

        let workspace_root = self.workspace_root_for_uri(uri.as_str()).await;
        match run_phpstan_for_file(config, file_path, workspace_root, Some(cancellation)).await {
            Ok(diagnostics) => diagnostics,
            Err(message) => {
                if message.contains("command cancelled") {
                    tracing::debug!(
                        "PHPStan diagnostics cancelled for {}: {}",
                        uri.as_str(),
                        message
                    );
                    return vec![];
                }
                tracing::warn!(
                    "PHPStan diagnostics failed for {}: {}",
                    uri.as_str(),
                    message
                );
                self.client
                    .log_message(
                        MessageType::WARNING,
                        format!("php-lsp PHPStan diagnostics failed: {}", message),
                    )
                    .await;
                vec![]
            }
        }
    }

    async fn psalm_diagnostics_for_uri(
        &self,
        uri: &Uri,
        cancellation: OperationCancellationToken,
    ) -> Vec<Diagnostic> {
        let config = self.psalm_config.lock().await.clone();
        if !config.enabled {
            return vec![];
        }

        if !uri_is_php_file(uri) {
            return vec![];
        }

        let Some(file_path) = uri_to_path(uri.as_str()) else {
            return vec![];
        };
        if !file_path.exists() {
            return vec![];
        }

        let workspace_root = self.workspace_root_for_uri(uri.as_str()).await;
        match run_psalm_for_file(config, file_path, workspace_root, Some(cancellation)).await {
            Ok(diagnostics) => diagnostics,
            Err(message) => {
                if message.contains("command cancelled") {
                    tracing::debug!(
                        "Psalm diagnostics cancelled for {}: {}",
                        uri.as_str(),
                        message
                    );
                    return vec![];
                }
                tracing::warn!("Psalm diagnostics failed for {}: {}", uri.as_str(), message);
                self.client
                    .log_message(
                        MessageType::WARNING,
                        format!("php-lsp Psalm diagnostics failed: {}", message),
                    )
                    .await;
                vec![]
            }
        }
    }

    fn references_for_file(
        &self,
        file_uri: &str,
        target_fqn: &str,
        target_kind: php_lsp_types::PhpSymbolKind,
        include_declaration: bool,
    ) -> Vec<php_lsp_types::SymbolReference> {
        let mut refs = if let Some(parser) = self.open_files.get(file_uri) {
            current_parser_symbol_references(file_uri, &parser)
        } else {
            self.index
                .file_references
                .get(file_uri)
                .map(|entry| entry.value().clone())
                .unwrap_or_default()
        };
        refs.retain(|reference| {
            symbol_reference_matches(reference, target_fqn, target_kind, include_declaration)
        });
        refs
    }

    /// Publish diagnostics for a file.
    async fn publish_diagnostics(&self, uri: &Uri) {
        let uri_str = uri.as_str().to_string();
        let version = self.current_document_version(&uri_str);
        let diagnostics_mode = *self.diagnostics_mode.lock().await;
        let should_preresolve_dependencies =
            diagnostics_mode == DiagnosticsMode::BasicSemantic && *self.index_vendor.lock().await;

        // Pre-resolve use statements via lazy indexing so that vendor classes
        // are available for the synchronous `compute_diagnostics` resolver.
        if should_preresolve_dependencies {
            if let Some(fs) = self.index.file_symbols.get(&uri_str) {
                let fqns_to_resolve: Vec<String> = fs
                    .use_statements
                    .iter()
                    .filter(|u| u.kind == php_lsp_types::UseKind::Class)
                    .filter(|u| u.fqn.contains('\\'))
                    .map(|u| u.fqn.clone())
                    .collect();
                drop(fs); // release DashMap ref before async calls
                for fqn in fqns_to_resolve {
                    self.lazy_index_class_dependencies(&fqn).await;
                }
            }
        }

        // Also pre-resolve: class FQNs from aliased qualified names used in code.
        // e.g. `use Symfony\...\Constraints as Assert;` → `new Assert\NotBlank`
        // → need to lazily index `Symfony\...\Constraints\NotBlank`.
        if should_preresolve_dependencies {
            if let Some(parser) = self.open_files.get(&uri_str) {
                if let Some(tree) = parser.tree() {
                    let source = parser.source();
                    if let Some(fs) = self.index.file_symbols.get(&uri_str) {
                        let alias_fqns = collect_aliased_class_fqns(tree, &source, &fs);
                        drop(fs);
                        for fqn in alias_fqns {
                            self.lazy_index_class_dependencies(&fqn).await;
                        }
                    }
                }
            }
        }

        let diagnostic_severity = *self.diagnostic_severity.lock().await;
        let php_version = *self.php_version.lock().await;
        let mut diagnostics = compute_open_file_diagnostics(
            &uri_str,
            &self.open_files,
            &self.index,
            diagnostics_mode,
            diagnostic_severity,
            php_version,
        );

        let has_syntax_errors = diagnostics.iter().any(|diagnostic| {
            diagnostic.source.as_deref() == Some("php-lsp")
                && diagnostic.severity == Some(DiagnosticSeverity::ERROR)
        });
        if diagnostics_mode == DiagnosticsMode::BasicSemantic && !has_syntax_errors {
            let analyzer_token = self.start_analyzer_run(&uri_str).await;
            diagnostics.extend(
                self.phpstan_diagnostics_for_uri(uri, analyzer_token.clone())
                    .await,
            );
            if analyzer_token.is_cancelled() {
                self.finish_analyzer_run(&uri_str, &analyzer_token).await;
                return;
            }
            diagnostics.extend(
                self.psalm_diagnostics_for_uri(uri, analyzer_token.clone())
                    .await,
            );
            if analyzer_token.is_cancelled() {
                self.finish_analyzer_run(&uri_str, &analyzer_token).await;
                return;
            }
            self.finish_analyzer_run(&uri_str, &analyzer_token).await;
        }

        if self.current_document_version(&uri_str) != version {
            tracing::debug!(
                "Skipping stale diagnostics for {}: computed for version {:?}, current {:?}",
                uri_str,
                version,
                self.current_document_version(&uri_str)
            );
            return;
        }

        self.client
            .publish_diagnostics(uri.clone(), diagnostics, version)
            .await;
    }

    async fn path_is_excluded_by_config(&self, path: &Path) -> bool {
        let exclude_paths = self.exclude_paths.lock().await.clone();
        if exclude_paths.is_empty() {
            return false;
        }

        let mut roots: Vec<PathBuf> = self
            .workspace_configs
            .lock()
            .await
            .iter()
            .map(|config| config.root.clone())
            .collect();

        if roots.is_empty() {
            if let Some(root) = self.workspace_root.lock().await.clone() {
                roots.push(root);
            }
        }

        roots
            .iter()
            .any(|root| path_is_excluded(path, root, &exclude_paths))
    }

    /// Reindex one changed PHP file from the open buffer when available,
    /// otherwise from disk.
    async fn reindex_php_file(&self, uri: &Uri) {
        let uri_str = uri.as_str().to_string();
        if !uri_is_php_file(uri) {
            return;
        }

        if let Some(path) = uri_to_path(&uri_str) {
            if self.path_is_excluded_by_config(&path).await {
                self.index.remove_file(&uri_str);
                self.semantic_tokens_cache.lock().await.remove(&uri_str);
                return;
            }
        }

        let open_file_symbols = {
            self.open_files.get(&uri_str).and_then(|parser| {
                let tree = parser.tree()?;
                let source = parser.source();
                let file_symbols = extract_file_symbols(tree, &source, &uri_str);
                let references = collect_symbol_references_in_file(tree, &source, &file_symbols);
                Some((file_symbols, references))
            })
        };

        if let Some((file_symbols, references)) = open_file_symbols {
            self.index
                .update_file_with_references(&uri_str, file_symbols, references);
            self.semantic_tokens_cache.lock().await.remove(&uri_str);
            self.publish_diagnostics(uri).await;
            return;
        }

        let Some(path) = uri_to_path(&uri_str) else {
            return;
        };

        match parse_workspace_file_for_index_blocking(path.clone(), "watched PHP file reindex")
            .await
        {
            Ok(parsed) => {
                if let Some(file_symbols) = parsed.file_symbols {
                    self.index.update_file_with_references(
                        &parsed.uri,
                        file_symbols,
                        parsed.references,
                    );
                } else {
                    if let Some(error) = parsed.error {
                        tracing::debug!(
                            "Failed to reindex watched PHP file {}, removing from index: {}",
                            path.display(),
                            error
                        );
                    }
                    self.index.remove_file(&uri_str);
                }
            }
            Err(message) => {
                tracing::warn!(
                    "Failed to schedule watched PHP file reindex for {}, removing from index: {}",
                    path.display(),
                    message
                );
                self.index.remove_file(&uri_str);
            }
        }

        self.semantic_tokens_cache.lock().await.remove(&uri_str);
    }

    /// Remove one PHP file from all server-side caches/indexes.
    async fn remove_php_file(&self, uri: &Uri) {
        if !uri_is_php_file(uri) {
            return;
        }

        let uri_str = uri.as_str().to_string();
        self.index.remove_file(&uri_str);
        self.vendor_file_lru.lock().await.remove(&uri_str);
        self.open_files.remove(&uri_str);
        self.document_versions.remove(&uri_str);
        self.cancel_debounced_diagnostics(&uri_str).await;
        self.cancel_analyzer_run(&uri_str).await;
        self.semantic_tokens_cache.lock().await.remove(&uri_str);
        self.client
            .publish_diagnostics(uri.clone(), vec![], None)
            .await;
    }

    async fn rename_php_file(&self, old_uri: &Uri, new_uri: &Uri) {
        let old_is_php = uri_is_php_file(old_uri);
        let new_is_php = uri_is_php_file(new_uri);

        if !old_is_php && !new_is_php {
            return;
        }

        let old_uri_str = old_uri.as_str().to_string();
        let moved_parser = self
            .open_files
            .remove(&old_uri_str)
            .map(|(_, parser)| parser);
        let moved_version = self
            .document_versions
            .remove(&old_uri_str)
            .map(|(_, version)| version);
        self.cancel_debounced_diagnostics(&old_uri_str).await;
        self.cancel_analyzer_run(&old_uri_str).await;
        self.cancel_analyzer_run(new_uri.as_str()).await;
        if old_is_php {
            self.index.remove_file(&old_uri_str);
            self.vendor_file_lru.lock().await.remove(&old_uri_str);
            self.semantic_tokens_cache.lock().await.remove(&old_uri_str);
            self.client
                .publish_diagnostics(old_uri.clone(), vec![], None)
                .await;
        }

        if !new_is_php {
            return;
        }

        let new_excluded = if let Some(path) = uri_to_path(new_uri.as_str()) {
            self.path_is_excluded_by_config(&path).await
        } else {
            false
        };
        if new_excluded {
            if let Some(parser) = moved_parser {
                let new_uri_str = new_uri.as_str().to_string();
                self.open_files.insert(new_uri_str.clone(), parser);
                if let Some(version) = moved_version {
                    self.document_versions.insert(new_uri_str, version);
                }
            }
            self.index.remove_file(new_uri.as_str());
            self.semantic_tokens_cache
                .lock()
                .await
                .remove(new_uri.as_str());
            return;
        }

        if let Some(parser) = moved_parser {
            let new_uri_str = new_uri.as_str().to_string();
            if let Some(tree) = parser.tree() {
                let source = parser.source();
                let file_symbols = extract_file_symbols(tree, &source, &new_uri_str);
                let references = collect_symbol_references_in_file(tree, &source, &file_symbols);
                self.index
                    .update_file_with_references(&new_uri_str, file_symbols, references);
            }
            self.open_files.insert(new_uri_str.clone(), parser);
            if let Some(version) = moved_version {
                self.document_versions.insert(new_uri_str.clone(), version);
            }
            self.semantic_tokens_cache.lock().await.remove(&new_uri_str);
            self.publish_diagnostics(new_uri).await;
        } else {
            self.reindex_php_file(new_uri).await;
        }
    }
}

fn normalize_variable_new_name(new_name: &str) -> Option<String> {
    let trimmed = new_name.trim();
    if trimmed.is_empty() {
        return None;
    }

    let raw = trimmed.strip_prefix('$').unwrap_or(trimmed);
    if raw.is_empty() {
        return None;
    }

    let mut chars = raw.chars();
    let first = chars.next()?;
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return None;
    }
    if !chars.all(|c| c == '_' || c.is_ascii_alphanumeric()) {
        return None;
    }

    Some(format!("${}", raw))
}

fn normalize_property_new_name(new_name: &str) -> Option<String> {
    let var = normalize_variable_new_name(new_name)?;
    Some(var.trim_start_matches('$').to_string())
}

fn is_renameable_variable(var_name: &str) -> bool {
    !matches!(
        var_name,
        "$this"
            | "$GLOBALS"
            | "$_SERVER"
            | "$_GET"
            | "$_POST"
            | "$_FILES"
            | "$_COOKIE"
            | "$_SESSION"
            | "$_REQUEST"
            | "$_ENV"
            | "$http_response_header"
            | "$argc"
            | "$argv"
    )
}

fn line_byte_col_to_byte(source: &str, line: u32, byte_col: u32) -> Option<usize> {
    let mut offset = 0usize;

    for (current_line, l) in source.split_inclusive('\n').enumerate() {
        if current_line as u32 == line {
            let col = byte_col as usize;
            return (col <= l.len()).then_some(offset + col);
        }
        offset += l.len();
    }

    None
}

fn starts_with_assignment_operator(text: &str) -> bool {
    matches!(
        text.as_bytes(),
        [b'=', rest @ ..] if !matches!(rest.first(), Some(b'=' | b'>'))
    ) || text.starts_with("+=")
        || text.starts_with("-=")
        || text.starts_with("*=")
        || text.starts_with("/=")
        || text.starts_with("%=")
        || text.starts_with(".=")
        || text.starts_with("&=")
        || text.starts_with("|=")
        || text.starts_with("^=")
        || text.starts_with("??=")
        || text.starts_with("<<=")
        || text.starts_with(">>=")
}

fn is_declaration_like_write(before_trimmed: &str, after_trimmed: &str) -> bool {
    let segment = before_trimmed
        .rsplit([';', '{', '}'])
        .next()
        .unwrap_or(before_trimmed)
        .trim_start();
    let declaration_tail = after_trimmed.starts_with([',', ')', ';', '=']);

    declaration_tail
        && (segment.contains("function ")
            || segment.starts_with("public ")
            || segment.starts_with("protected ")
            || segment.starts_with("private ")
            || segment.starts_with("readonly ")
            || segment.starts_with("static ")
            || segment.starts_with("var "))
}

fn is_write_reference(source: &str, range: (u32, u32, u32, u32)) -> bool {
    let Some(start) = line_byte_col_to_byte(source, range.0, range.1) else {
        return false;
    };
    let Some(end) = line_byte_col_to_byte(source, range.2, range.3) else {
        return false;
    };
    if start > end || end > source.len() {
        return false;
    }

    let line_start = source[..start].rfind('\n').map(|idx| idx + 1).unwrap_or(0);
    let line_end = source[end..]
        .find('\n')
        .map(|idx| end + idx)
        .unwrap_or(source.len());
    let before_trimmed = source[line_start..start].trim_end();
    let after_trimmed = source[end..line_end].trim_start();

    starts_with_assignment_operator(after_trimmed)
        || after_trimmed.starts_with("++")
        || after_trimmed.starts_with("--")
        || before_trimmed.ends_with("++")
        || before_trimmed.ends_with("--")
        || is_declaration_like_write(before_trimmed, after_trimmed)
}

fn document_highlight_kind(
    source: &str,
    range: (u32, u32, u32, u32),
    read_write_capable: bool,
) -> DocumentHighlightKind {
    if !read_write_capable {
        return DocumentHighlightKind::TEXT;
    }

    if is_write_reference(source, range) {
        DocumentHighlightKind::WRITE
    } else {
        DocumentHighlightKind::READ
    }
}

fn document_highlight_from_range(
    source: &str,
    range: (u32, u32, u32, u32),
    read_write_capable: bool,
) -> DocumentHighlight {
    let rng = range_byte_to_utf16(source, range);
    DocumentHighlight {
        range: Range {
            start: Position::new(rng.0, rng.1),
            end: Position::new(rng.2, rng.3),
        },
        kind: Some(document_highlight_kind(source, range, read_write_capable)),
    }
}

fn selection_range_from_byte_ranges(
    source: &str,
    byte_ranges: Vec<(u32, u32, u32, u32)>,
) -> Option<SelectionRange> {
    let mut parent = None;

    for byte_range in byte_ranges.into_iter().rev() {
        let range = range_byte_to_utf16(source, byte_range);
        parent = Some(Box::new(SelectionRange {
            range: Range {
                start: Position::new(range.0, range.1),
                end: Position::new(range.2, range.3),
            },
            parent,
        }));
    }

    parent.map(|selection_range| *selection_range)
}

fn node_byte_range(node: tree_sitter::Node) -> (u32, u32, u32, u32) {
    let start = node.start_position();
    let end = node.end_position();
    (
        start.row as u32,
        start.column as u32,
        end.row as u32,
        end.column as u32,
    )
}

fn node_text<'a>(source: &'a str, node: tree_sitter::Node) -> &'a str {
    source.get(node.byte_range()).unwrap_or("")
}

fn enclosing_linked_edit_construct(mut node: tree_sitter::Node) -> Option<tree_sitter::Node> {
    loop {
        if matches!(
            node.kind(),
            "namespace_definition"
                | "namespace_use_declaration"
                | "namespace_use_clause"
                | "namespace_use_group"
        ) {
            return Some(node);
        }
        node = node.parent()?;
    }
}

fn collect_matching_name_ranges(
    node: tree_sitter::Node,
    source: &str,
    target: &str,
    ranges: &mut Vec<(u32, u32, u32, u32)>,
) {
    if node.kind() == "name" && node_text(source, node) == target {
        ranges.push(node_byte_range(node));
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_matching_name_ranges(child, source, target, ranges);
    }
}

fn linked_editing_ranges_for_namespace_or_use(
    source: &str,
    node: tree_sitter::Node,
) -> Option<Vec<(u32, u32, u32, u32)>> {
    if node.kind() != "name" {
        return None;
    }

    let target = node_text(source, node);
    if target.is_empty() {
        return None;
    }

    let construct = enclosing_linked_edit_construct(node)?;
    let mut ranges = Vec::new();
    collect_matching_name_ranges(construct, source, target, &mut ranges);
    ranges.sort_unstable();
    ranges.dedup();

    (ranges.len() >= 2).then_some(ranges)
}

fn php_symbol_kind_for_ref_kind(ref_kind: RefKind) -> Option<php_lsp_types::PhpSymbolKind> {
    match ref_kind {
        RefKind::ClassName | RefKind::Constructor => Some(php_lsp_types::PhpSymbolKind::Class),
        RefKind::FunctionCall => Some(php_lsp_types::PhpSymbolKind::Function),
        RefKind::MethodCall => Some(php_lsp_types::PhpSymbolKind::Method),
        RefKind::PropertyAccess | RefKind::StaticPropertyAccess => {
            Some(php_lsp_types::PhpSymbolKind::Property)
        }
        RefKind::ClassConstant => Some(php_lsp_types::PhpSymbolKind::ClassConstant),
        RefKind::GlobalConstant => Some(php_lsp_types::PhpSymbolKind::GlobalConstant),
        RefKind::Variable | RefKind::NamespaceName | RefKind::Unknown => None,
    }
}

fn format_signature_param(param: &php_lsp_types::ParamInfo) -> String {
    let mut label = String::new();
    if let Some(ref type_info) = param.type_info {
        label.push_str(&type_info.to_string());
        label.push(' ');
    }
    if param.is_variadic {
        label.push_str("...");
    }
    if param.is_by_ref {
        label.push('&');
    }
    if param.name.starts_with('$') {
        label.push_str(&param.name);
    } else {
        label.push('$');
        label.push_str(&param.name);
    }
    if let Some(ref default) = param.default_value {
        label.push_str(" = ");
        label.push_str(default);
    }
    label
}

fn build_signature_help(
    sym: &php_lsp_types::SymbolInfo,
    active_parameter: usize,
) -> Option<SignatureHelp> {
    let sig = sym.signature.as_ref()?;
    let param_labels: Vec<String> = sig.params.iter().map(format_signature_param).collect();

    let mut label = String::new();
    label.push_str(&sym.fqn);
    label.push('(');
    label.push_str(&param_labels.join(", "));
    label.push(')');
    if let Some(ref ret) = sig.return_type {
        label.push_str(": ");
        label.push_str(&ret.to_string());
    }

    let phpdoc = sym.doc_comment.as_ref().map(|doc| parse_phpdoc(doc));
    let documentation = phpdoc.as_ref().and_then(|doc| {
        let mut parts = Vec::new();
        if let Some(ref summary) = doc.summary {
            parts.push(summary.clone());
        }
        if let Some(ref ret) = doc.return_type {
            parts.push(format!("@return `{}`", ret));
        }
        if parts.is_empty() {
            None
        } else {
            Some(Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: parts.join("\n\n"),
            }))
        }
    });

    let parameters: Vec<ParameterInformation> = sig
        .params
        .iter()
        .zip(param_labels.iter())
        .map(|(param, label)| {
            let documentation = phpdoc.as_ref().and_then(|doc| {
                doc.params
                    .iter()
                    .find(|p| p.name == param.name)
                    .and_then(|p| {
                        let mut parts = Vec::new();
                        if let Some(ref type_info) = p.type_info {
                            parts.push(format!("`{}`", type_info));
                        }
                        if let Some(ref desc) = p.description {
                            parts.push(desc.clone());
                        }
                        if parts.is_empty() {
                            None
                        } else {
                            Some(Documentation::MarkupContent(MarkupContent {
                                kind: MarkupKind::Markdown,
                                value: parts.join(" — "),
                            }))
                        }
                    })
            });

            ParameterInformation {
                label: ParameterLabel::Simple(label.clone()),
                documentation,
            }
        })
        .collect();

    let active_parameter = if sig.params.is_empty() {
        None
    } else {
        Some(active_parameter.min(sig.params.len() - 1) as u32)
    };

    Some(SignatureHelp {
        signatures: vec![SignatureInformation {
            label,
            documentation,
            parameters: Some(parameters),
            active_parameter,
        }],
        active_signature: Some(0),
        active_parameter,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ImportKind {
    Class,
    Function,
    Constant,
}

fn code_action_kind_allowed(only: Option<&Vec<CodeActionKind>>, kind: &CodeActionKind) -> bool {
    only.map(|kinds| kinds.is_empty() || kinds.iter().any(|k| k == kind))
        .unwrap_or(true)
}

fn unknown_symbol_from_diagnostic(message: &str) -> Option<(ImportKind, String)> {
    if let Some(fqn) = message.strip_prefix("Unknown class: ") {
        return Some((ImportKind::Class, fqn.to_string()));
    }
    if let Some(fqn) = message.strip_prefix("Unknown function: ") {
        return Some((ImportKind::Function, fqn.to_string()));
    }
    None
}

fn short_name(fqn: &str) -> &str {
    fqn.trim_start_matches('\\')
        .rsplit('\\')
        .next()
        .unwrap_or(fqn)
}

fn use_kind_for_ref_kind(ref_kind: RefKind) -> Option<php_lsp_types::UseKind> {
    match ref_kind {
        RefKind::ClassName | RefKind::Constructor => Some(php_lsp_types::UseKind::Class),
        RefKind::FunctionCall => Some(php_lsp_types::UseKind::Function),
        RefKind::GlobalConstant => Some(php_lsp_types::UseKind::Constant),
        _ => None,
    }
}

fn import_target_fqn(sym: &SymbolAtPosition) -> &str {
    if sym.ref_kind == RefKind::Constructor {
        sym.fqn
            .strip_suffix("::__construct")
            .unwrap_or(sym.fqn.as_str())
    } else {
        sym.fqn.as_str()
    }
}

fn imported_use_statement_for_symbol<'a>(
    file_symbols: &'a php_lsp_types::FileSymbols,
    sym: &SymbolAtPosition,
) -> Option<&'a php_lsp_types::UseStatement> {
    let use_kind = use_kind_for_ref_kind(sym.ref_kind)?;
    let target_fqn = import_target_fqn(sym).trim_start_matches('\\');

    file_symbols.use_statements.iter().find(|use_stmt| {
        use_stmt.kind == use_kind && use_stmt.fqn.trim_start_matches('\\') == target_fqn
    })
}

fn is_builtin_type_name(name: &str) -> bool {
    matches!(
        name.trim_start_matches('\\').to_ascii_lowercase().as_str(),
        "int"
            | "float"
            | "string"
            | "bool"
            | "boolean"
            | "array"
            | "object"
            | "null"
            | "void"
            | "never"
            | "mixed"
            | "callable"
            | "iterable"
            | "true"
            | "false"
            | "resource"
    )
}

fn first_type_definition_fqn(
    type_info: &php_lsp_types::TypeInfo,
    file_symbols: &php_lsp_types::FileSymbols,
    current_class_fqn: Option<&str>,
) -> Option<String> {
    match type_info {
        php_lsp_types::TypeInfo::Simple(name) => {
            if is_builtin_type_name(name) {
                None
            } else {
                Some(resolve_class_name_pub(name, file_symbols))
            }
        }
        php_lsp_types::TypeInfo::Nullable(inner) => {
            first_type_definition_fqn(inner, file_symbols, current_class_fqn)
        }
        php_lsp_types::TypeInfo::Union(types) | php_lsp_types::TypeInfo::Intersection(types) => {
            types
                .iter()
                .find_map(|ty| first_type_definition_fqn(ty, file_symbols, current_class_fqn))
        }
        php_lsp_types::TypeInfo::Generic { base, args } => {
            if !is_builtin_type_name(base) {
                Some(resolve_class_name_pub(base, file_symbols))
            } else {
                args.iter()
                    .find_map(|ty| first_type_definition_fqn(ty, file_symbols, current_class_fqn))
            }
        }
        php_lsp_types::TypeInfo::ClassString(Some(inner)) => {
            first_type_definition_fqn(inner, file_symbols, current_class_fqn)
        }
        php_lsp_types::TypeInfo::ArrayShape(items) => items.iter().find_map(|item| {
            first_type_definition_fqn(&item.value, file_symbols, current_class_fqn)
        }),
        php_lsp_types::TypeInfo::Callable {
            params,
            return_type,
        } => return_type
            .as_deref()
            .and_then(|ty| first_type_definition_fqn(ty, file_symbols, current_class_fqn))
            .or_else(|| {
                params
                    .iter()
                    .find_map(|ty| first_type_definition_fqn(ty, file_symbols, current_class_fqn))
            }),
        php_lsp_types::TypeInfo::Self_ | php_lsp_types::TypeInfo::Static_ => {
            current_class_fqn.map(str::to_string)
        }
        php_lsp_types::TypeInfo::Parent_ => current_class_fqn.and_then(|class_fqn| {
            file_symbols
                .symbols
                .iter()
                .find(|sym| sym.fqn == class_fqn)
                .and_then(|sym| sym.extends.first().cloned())
        }),
        php_lsp_types::TypeInfo::Void
        | php_lsp_types::TypeInfo::Never
        | php_lsp_types::TypeInfo::Mixed
        | php_lsp_types::TypeInfo::ClassString(None)
        | php_lsp_types::TypeInfo::LiteralString(_)
        | php_lsp_types::TypeInfo::LiteralInt(_)
        | php_lsp_types::TypeInfo::LiteralFloat(_)
        | php_lsp_types::TypeInfo::LiteralBool(_)
        | php_lsp_types::TypeInfo::LiteralNull => None,
    }
}

fn use_kind_matches(import_kind: ImportKind, use_kind: php_lsp_types::UseKind) -> bool {
    matches!(
        (import_kind, use_kind),
        (ImportKind::Class, php_lsp_types::UseKind::Class)
            | (ImportKind::Function, php_lsp_types::UseKind::Function)
            | (ImportKind::Constant, php_lsp_types::UseKind::Constant)
    )
}

fn import_kind_from_use_kind(use_kind: php_lsp_types::UseKind) -> ImportKind {
    match use_kind {
        php_lsp_types::UseKind::Class => ImportKind::Class,
        php_lsp_types::UseKind::Function => ImportKind::Function,
        php_lsp_types::UseKind::Constant => ImportKind::Constant,
    }
}

fn existing_use_alias(use_stmt: &php_lsp_types::UseStatement) -> String {
    use_stmt
        .alias
        .clone()
        .unwrap_or_else(|| short_name(&use_stmt.fqn).to_string())
}

fn used_import_aliases(
    file_symbols: &php_lsp_types::FileSymbols,
    import_kind: ImportKind,
) -> std::collections::HashSet<String> {
    let mut aliases = std::collections::HashSet::new();
    for use_stmt in &file_symbols.use_statements {
        if use_kind_matches(import_kind, use_stmt.kind) {
            aliases.insert(existing_use_alias(use_stmt));
        }
    }
    if import_kind == ImportKind::Class {
        for sym in &file_symbols.symbols {
            if matches!(
                sym.kind,
                php_lsp_types::PhpSymbolKind::Class
                    | php_lsp_types::PhpSymbolKind::Interface
                    | php_lsp_types::PhpSymbolKind::Trait
                    | php_lsp_types::PhpSymbolKind::Enum
            ) {
                aliases.insert(sym.name.clone());
            }
        }
    }
    aliases
}

fn unique_import_alias(base: &str, used: &std::collections::HashSet<String>) -> String {
    let mut candidate = format!("{}Import", base);
    let mut suffix = 2usize;
    while used.contains(&candidate) {
        candidate = format!("{}Import{}", base, suffix);
        suffix += 1;
    }
    candidate
}

fn existing_import_for_fqn<'a>(
    file_symbols: &'a php_lsp_types::FileSymbols,
    fqn: &str,
    import_kind: ImportKind,
) -> Option<&'a php_lsp_types::UseStatement> {
    file_symbols
        .use_statements
        .iter()
        .find(|use_stmt| use_kind_matches(import_kind, use_stmt.kind) && use_stmt.fqn == fqn)
}

fn line_is_blank(source: &str, line: u32) -> bool {
    source
        .lines()
        .nth(line as usize)
        .map(|line| line.trim().is_empty())
        .unwrap_or(false)
}

fn find_use_insert_line(source: &str, file_symbols: &php_lsp_types::FileSymbols) -> u32 {
    if let Some(last_use_line) = file_symbols
        .use_statements
        .iter()
        .map(|use_stmt| use_stmt.range.2)
        .max()
    {
        return last_use_line + 1;
    }

    for (idx, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("namespace ") && (trimmed.contains(';') || trimmed.contains('{')) {
            return idx as u32 + 1;
        }
    }

    if source
        .lines()
        .next()
        .is_some_and(|line| line.trim() == "<?php")
    {
        1
    } else {
        0
    }
}

fn build_use_statement(import_fqn: &str, import_kind: ImportKind, alias: Option<&str>) -> String {
    let import_fqn = import_fqn.trim_start_matches('\\');
    let prefix = match import_kind {
        ImportKind::Class => "use",
        ImportKind::Function => "use function",
        ImportKind::Constant => "use const",
    };
    match alias {
        Some(alias) => format!("{} {} as {};", prefix, import_fqn, alias),
        None => format!("{} {};", prefix, import_fqn),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct OrganizableImport {
    fqn: String,
    alias: Option<String>,
    kind: ImportKind,
}

fn import_kind_sort_key(kind: ImportKind) -> u8 {
    match kind {
        ImportKind::Class => 0,
        ImportKind::Function => 1,
        ImportKind::Constant => 2,
    }
}

fn source_line(source: &str, line: u32) -> Option<&str> {
    source.lines().nth(line as usize)
}

fn is_simple_use_statement_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with("use ")
        && trimmed.ends_with(';')
        && !trimmed.contains('{')
        && !trimmed.contains('}')
}

fn find_organizable_use_block(
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
) -> Option<(u32, u32)> {
    let start_line = file_symbols
        .use_statements
        .iter()
        .map(|use_stmt| use_stmt.range.0)
        .min()?;
    let end_line = file_symbols
        .use_statements
        .iter()
        .map(|use_stmt| use_stmt.range.2)
        .max()?
        + 1;

    for use_stmt in &file_symbols.use_statements {
        if use_stmt.range.0 != use_stmt.range.2 {
            return None;
        }
        let line = source_line(source, use_stmt.range.0)?;
        if !is_simple_use_statement_line(line) {
            return None;
        }
    }

    for line_idx in start_line..end_line {
        let line = source_line(source, line_idx)?;
        let trimmed = line.trim();
        if !trimmed.is_empty() && !is_simple_use_statement_line(line) {
            return None;
        }
    }

    Some((start_line, end_line))
}

fn source_without_line_range(source: &str, start_line: u32, end_line: u32) -> String {
    let mut result = String::with_capacity(source.len());
    for (line_idx, line) in source.split_inclusive('\n').enumerate() {
        if (start_line as usize..end_line as usize).contains(&line_idx) {
            result.push('\n');
        } else {
            result.push_str(line);
        }
    }
    result
}

fn is_php_identifier_char(ch: char) -> bool {
    ch == '_' || ch == '$' || ch.is_ascii_alphanumeric() || !ch.is_ascii()
}

fn has_identifier_boundaries(source: &str, start: usize, end: usize) -> bool {
    let before_ok = source[..start]
        .chars()
        .next_back()
        .map(|ch| !is_php_identifier_char(ch))
        .unwrap_or(true);
    let after_ok = source[end..]
        .chars()
        .next()
        .map(|ch| !is_php_identifier_char(ch))
        .unwrap_or(true);
    before_ok && after_ok
}

fn contains_php_identifier(source: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }

    let mut offset = 0usize;
    while let Some(relative) = source[offset..].find(name) {
        let start = offset + relative;
        let end = start + name.len();
        if has_identifier_boundaries(source, start, end) {
            return true;
        }
        offset = end;
    }

    false
}

fn contains_php_function_call(source: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }

    let mut offset = 0usize;
    while let Some(relative) = source[offset..].find(name) {
        let start = offset + relative;
        let end = start + name.len();
        if has_identifier_boundaries(source, start, end) {
            let after_name = source[end..].trim_start();
            if after_name.starts_with('(') {
                return true;
            }
        }
        offset = end;
    }

    false
}

fn import_is_used(source_without_imports: &str, import: &OrganizableImport) -> bool {
    let alias = import
        .alias
        .as_deref()
        .unwrap_or_else(|| short_name(&import.fqn));

    match import.kind {
        ImportKind::Class => contains_php_identifier(source_without_imports, alias),
        ImportKind::Function => contains_php_function_call(source_without_imports, alias),
        ImportKind::Constant => contains_php_identifier(source_without_imports, alias),
    }
}

fn build_organize_imports_edit(
    uri: Uri,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
) -> Option<WorkspaceEdit> {
    if file_symbols.use_statements.is_empty() {
        return None;
    }

    let (start_line, end_line) = find_organizable_use_block(source, file_symbols)?;
    let source_without_imports = source_without_line_range(source, start_line, end_line);

    let mut imports: Vec<OrganizableImport> = file_symbols
        .use_statements
        .iter()
        .map(|use_stmt| OrganizableImport {
            fqn: use_stmt.fqn.trim_start_matches('\\').to_string(),
            alias: use_stmt.alias.clone(),
            kind: import_kind_from_use_kind(use_stmt.kind),
        })
        .filter(|import| import_is_used(&source_without_imports, import))
        .collect();

    imports.sort_by(|a, b| {
        import_kind_sort_key(a.kind)
            .cmp(&import_kind_sort_key(b.kind))
            .then_with(|| a.fqn.to_lowercase().cmp(&b.fqn.to_lowercase()))
            .then_with(|| a.alias.cmp(&b.alias))
    });
    imports.dedup();

    let mut groups = Vec::new();
    for kind in [
        ImportKind::Class,
        ImportKind::Function,
        ImportKind::Constant,
    ] {
        let lines: Vec<String> = imports
            .iter()
            .filter(|import| import.kind == kind)
            .map(|import| build_use_statement(&import.fqn, import.kind, import.alias.as_deref()))
            .collect();
        if !lines.is_empty() {
            groups.push(lines.join("\n"));
        }
    }

    let mut new_text = groups.join("\n\n");
    if !new_text.is_empty() {
        new_text.push('\n');
        if !line_is_blank(source, end_line) {
            new_text.push('\n');
        }
    }

    let range = Range {
        start: Position::new(start_line, 0),
        end: Position::new(end_line, 0),
    };
    if text_at_lsp_range(source, range)
        .map(|old_text| old_text == new_text)
        .unwrap_or(false)
    {
        return None;
    }

    let mut changes = std::collections::HashMap::new();
    changes.insert(uri, vec![TextEdit { range, new_text }]);
    Some(WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    })
}

fn lsp_range_to_byte_range(source: &str, range: Range) -> (u32, u32, u32, u32) {
    (
        range.start.line,
        utf16_col_to_byte(source, range.start.line, range.start.character),
        range.end.line,
        utf16_col_to_byte(source, range.end.line, range.end.character),
    )
}

fn simple_return_type_hint_is_supported(
    name: &str,
    php_version: PhpVersion,
    in_union: bool,
) -> bool {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('$')
        || trimmed.contains(['<', '>', '[', ']', '(', ')', ',', ' '])
    {
        return false;
    }

    let lower = trimmed.trim_start_matches('\\').to_ascii_lowercase();
    match lower.as_str() {
        "void" => false,
        "never" => php_version.at_least(8, 1),
        "mixed" => php_version.at_least(8, 0),
        "static" => php_version.at_least(8, 0),
        "false" | "null" => {
            if in_union {
                php_version.at_least(8, 0)
            } else {
                php_version.at_least(8, 2)
            }
        }
        "true" => php_version.at_least(8, 2),
        "resource" => false,
        _ => true,
    }
}

fn is_intersection_member_type(type_info: &php_lsp_types::TypeInfo) -> bool {
    let php_lsp_types::TypeInfo::Simple(name) = type_info else {
        return false;
    };
    let lower = name.trim_start_matches('\\').to_ascii_lowercase();
    !matches!(
        lower.as_str(),
        "array"
            | "bool"
            | "callable"
            | "false"
            | "float"
            | "int"
            | "iterable"
            | "mixed"
            | "never"
            | "null"
            | "object"
            | "resource"
            | "string"
            | "true"
            | "void"
    ) && simple_return_type_hint_is_supported(name, PhpVersion::DEFAULT, false)
}

fn return_type_hint_is_supported(
    type_info: &php_lsp_types::TypeInfo,
    php_version: PhpVersion,
    in_union: bool,
) -> bool {
    match type_info {
        php_lsp_types::TypeInfo::Simple(name) => {
            simple_return_type_hint_is_supported(name, php_version, in_union)
        }
        php_lsp_types::TypeInfo::Union(types) => {
            php_version.at_least(8, 0)
                && types
                    .iter()
                    .all(|t| !matches!(t, php_lsp_types::TypeInfo::Void))
                && types
                    .iter()
                    .all(|t| return_type_hint_is_supported(t, php_version, true))
        }
        php_lsp_types::TypeInfo::Intersection(types) => {
            php_version.at_least(8, 1) && types.iter().all(is_intersection_member_type)
        }
        php_lsp_types::TypeInfo::Nullable(inner) => {
            php_version.at_least(7, 1)
                && !matches!(
                    inner.as_ref(),
                    php_lsp_types::TypeInfo::Mixed
                        | php_lsp_types::TypeInfo::Never
                        | php_lsp_types::TypeInfo::Void
                        | php_lsp_types::TypeInfo::Union(_)
                        | php_lsp_types::TypeInfo::Intersection(_)
                )
                && return_type_hint_is_supported(inner, php_version, false)
        }
        php_lsp_types::TypeInfo::Void => php_version.at_least(7, 1),
        php_lsp_types::TypeInfo::Never => php_version.at_least(8, 1),
        php_lsp_types::TypeInfo::Mixed => php_version.at_least(8, 0),
        php_lsp_types::TypeInfo::Self_ | php_lsp_types::TypeInfo::Parent_ => true,
        php_lsp_types::TypeInfo::Static_ => php_version.at_least(8, 0),
        php_lsp_types::TypeInfo::LiteralBool(value) => simple_return_type_hint_is_supported(
            if *value { "true" } else { "false" },
            php_version,
            in_union,
        ),
        php_lsp_types::TypeInfo::LiteralNull => {
            simple_return_type_hint_is_supported("null", php_version, in_union)
        }
        php_lsp_types::TypeInfo::Generic { .. }
        | php_lsp_types::TypeInfo::ArrayShape(_)
        | php_lsp_types::TypeInfo::Callable { .. }
        | php_lsp_types::TypeInfo::ClassString(_)
        | php_lsp_types::TypeInfo::LiteralString(_)
        | php_lsp_types::TypeInfo::LiteralInt(_)
        | php_lsp_types::TypeInfo::LiteralFloat(_) => false,
    }
}

fn return_type_hint(
    type_info: &php_lsp_types::TypeInfo,
    php_version: PhpVersion,
) -> Option<String> {
    if return_type_hint_is_supported(type_info, php_version, false) {
        Some(type_info.to_string())
    } else {
        None
    }
}

fn build_add_return_type_action(
    uri: Uri,
    candidate: &MissingReturnTypeCandidate,
    php_version: PhpVersion,
    request_range: Range,
    document_version: Option<i32>,
) -> Option<CodeActionOrCommand> {
    let hint = return_type_hint(&candidate.return_type, php_version)?;
    let data = serde_json::to_value(CodeActionData {
        action_kind: CodeActionDataKind::AddReturnType,
        uri: uri.as_str().to_string(),
        range: request_range,
        document_version,
        extra: CodeActionDataExtra::AddReturnType {
            hint: hint.clone(),
            insert_position: CodeActionInsertPosition {
                line: candidate.insert_position.0,
                byte_character: candidate.insert_position.1,
            },
        },
    })
    .ok()?;

    Some(CodeActionOrCommand::CodeAction(CodeAction {
        title: format!("Add return type `{}`", hint),
        kind: Some(CodeActionKind::REFACTOR_REWRITE),
        diagnostics: None,
        edit: None,
        command: None,
        is_preferred: Some(false),
        disabled: None,
        data: Some(data),
    }))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum CodeActionDataKind {
    AddReturnType,
    ImplementMissingMethods,
    GenerateConstructor,
    GenerateAccessor,
    ChangeVisibility,
    PromoteConstructorParameter,
    UpdatePhpDoc,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodeActionData {
    action_kind: CodeActionDataKind,
    uri: String,
    range: Range,
    document_version: Option<i32>,
    extra: CodeActionDataExtra,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum CodeActionDataExtra {
    AddReturnType {
        hint: String,
        insert_position: CodeActionInsertPosition,
    },
    ImplementMissingMethods {
        class_fqn: String,
    },
    GenerateConstructor {
        class_fqn: String,
    },
    GenerateAccessor {
        property_fqn: String,
        accessor_kind: AccessorKind,
        method_name: String,
    },
    ChangeVisibility {
        symbol_fqn: String,
        target_visibility: php_lsp_types::Visibility,
    },
    PromoteConstructorParameter {
        property_fqn: String,
    },
    UpdatePhpDoc {
        symbol_fqn: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum AccessorKind {
    Getter,
    Setter,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodeActionInsertPosition {
    line: u32,
    byte_character: u32,
}

fn empty_workspace_edit() -> WorkspaceEdit {
    WorkspaceEdit {
        changes: Some(HashMap::new()),
        document_changes: None,
        change_annotations: None,
    }
}

fn add_return_type_edit(
    uri: Uri,
    source: &str,
    hint: &str,
    insert_position: CodeActionInsertPosition,
) -> WorkspaceEdit {
    let utf16_index = Utf16LineIndex::new(source);
    let position = Position::new(
        insert_position.line,
        utf16_index.byte_col_to_utf16(insert_position.line, insert_position.byte_character),
    );

    let mut changes = HashMap::new();
    changes.insert(
        uri,
        vec![TextEdit {
            range: Range {
                start: position,
                end: position,
            },
            new_text: format!(": {}", hint),
        }],
    );

    WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    }
}

fn build_implement_missing_methods_action(
    uri: Uri,
    class_sym: &php_lsp_types::SymbolInfo,
    missing_methods: &[Arc<php_lsp_types::SymbolInfo>],
    request_range: Range,
    document_version: Option<i32>,
) -> Option<CodeActionOrCommand> {
    if missing_methods.is_empty() {
        return None;
    }

    let data = serde_json::to_value(CodeActionData {
        action_kind: CodeActionDataKind::ImplementMissingMethods,
        uri: uri.as_str().to_string(),
        range: request_range,
        document_version,
        extra: CodeActionDataExtra::ImplementMissingMethods {
            class_fqn: class_sym.fqn.clone(),
        },
    })
    .ok()?;

    let title = if missing_methods.len() == 1 {
        format!("Implement missing method `{}`", missing_methods[0].name)
    } else {
        format!("Implement {} missing methods", missing_methods.len())
    };

    Some(CodeActionOrCommand::CodeAction(CodeAction {
        title,
        kind: Some(CodeActionKind::QUICKFIX),
        diagnostics: None,
        edit: None,
        command: None,
        is_preferred: Some(false),
        disabled: None,
        data: Some(data),
    }))
}

fn build_generate_constructor_action(
    uri: Uri,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    class_sym: &php_lsp_types::SymbolInfo,
    request_range: Range,
    document_version: Option<i32>,
) -> Option<CodeActionOrCommand> {
    if direct_method_name_exists(file_symbols, &class_sym.fqn, "__construct")
        || constructor_generation_properties(source, file_symbols, &class_sym.fqn).is_empty()
    {
        return None;
    }

    let data = serde_json::to_value(CodeActionData {
        action_kind: CodeActionDataKind::GenerateConstructor,
        uri: uri.as_str().to_string(),
        range: request_range,
        document_version,
        extra: CodeActionDataExtra::GenerateConstructor {
            class_fqn: class_sym.fqn.clone(),
        },
    })
    .ok()?;

    Some(CodeActionOrCommand::CodeAction(CodeAction {
        title: "Generate constructor".to_string(),
        kind: Some(CodeActionKind::REFACTOR_REWRITE),
        diagnostics: None,
        edit: None,
        command: None,
        is_preferred: Some(false),
        disabled: None,
        data: Some(data),
    }))
}

fn build_generate_accessor_action(
    uri: Uri,
    property: &php_lsp_types::SymbolInfo,
    accessor_kind: AccessorKind,
    method_name: String,
    request_range: Range,
    document_version: Option<i32>,
) -> Option<CodeActionOrCommand> {
    if accessor_kind == AccessorKind::Setter && property.modifiers.is_readonly {
        return None;
    }

    let data = serde_json::to_value(CodeActionData {
        action_kind: CodeActionDataKind::GenerateAccessor,
        uri: uri.as_str().to_string(),
        range: request_range,
        document_version,
        extra: CodeActionDataExtra::GenerateAccessor {
            property_fqn: property.fqn.clone(),
            accessor_kind,
            method_name: method_name.clone(),
        },
    })
    .ok()?;

    let accessor_label = match accessor_kind {
        AccessorKind::Getter => "getter",
        AccessorKind::Setter => "setter",
    };

    Some(CodeActionOrCommand::CodeAction(CodeAction {
        title: format!("Generate {} `{}`", accessor_label, method_name),
        kind: Some(CodeActionKind::REFACTOR_REWRITE),
        diagnostics: None,
        edit: None,
        command: None,
        is_preferred: Some(false),
        disabled: None,
        data: Some(data),
    }))
}

fn build_generate_accessor_actions(
    uri: Uri,
    index: &WorkspaceIndex,
    property: &php_lsp_types::SymbolInfo,
    request_range: Range,
    document_version: Option<i32>,
) -> Vec<CodeActionOrCommand> {
    let Some(class_fqn) = property.parent_fqn.as_deref() else {
        return Vec::new();
    };

    let mut actions = Vec::new();
    let getter = getter_name(property);
    if !member_method_name_exists(index, class_fqn, &getter) {
        if let Some(action) = build_generate_accessor_action(
            uri.clone(),
            property,
            AccessorKind::Getter,
            getter,
            request_range,
            document_version,
        ) {
            actions.push(action);
        }
    }

    let setter = setter_name(property);
    if !property.modifiers.is_readonly && !member_method_name_exists(index, class_fqn, &setter) {
        if let Some(action) = build_generate_accessor_action(
            uri,
            property,
            AccessorKind::Setter,
            setter,
            request_range,
            document_version,
        ) {
            actions.push(action);
        }
    }

    actions
}

fn visibility_text(visibility: php_lsp_types::Visibility) -> &'static str {
    match visibility {
        php_lsp_types::Visibility::Public => "public",
        php_lsp_types::Visibility::Protected => "protected",
        php_lsp_types::Visibility::Private => "private",
    }
}

fn member_symbol_at_range(
    file_symbols: &php_lsp_types::FileSymbols,
    range: (u32, u32, u32, u32),
) -> Option<&php_lsp_types::SymbolInfo> {
    file_symbols
        .symbols
        .iter()
        .filter(|sym| {
            matches!(
                sym.kind,
                php_lsp_types::PhpSymbolKind::Method
                    | php_lsp_types::PhpSymbolKind::Property
                    | php_lsp_types::PhpSymbolKind::ClassConstant
            )
        })
        .find(|sym| {
            byte_range_contains(sym.range, range) || byte_ranges_overlap(sym.selection_range, range)
        })
}

fn symbol_supports_visibility_change(symbol: &php_lsp_types::SymbolInfo) -> bool {
    matches!(
        symbol.kind,
        php_lsp_types::PhpSymbolKind::Method
            | php_lsp_types::PhpSymbolKind::Property
            | php_lsp_types::PhpSymbolKind::ClassConstant
    ) && !symbol.modifiers.is_builtin
}

fn build_change_visibility_actions(
    uri: Uri,
    symbol: &php_lsp_types::SymbolInfo,
    request_range: Range,
    document_version: Option<i32>,
) -> Vec<CodeActionOrCommand> {
    if !symbol_supports_visibility_change(symbol) {
        return Vec::new();
    }

    [
        php_lsp_types::Visibility::Public,
        php_lsp_types::Visibility::Protected,
        php_lsp_types::Visibility::Private,
    ]
    .into_iter()
    .filter(|visibility| *visibility != symbol.visibility)
    .filter_map(|target_visibility| {
        let data = serde_json::to_value(CodeActionData {
            action_kind: CodeActionDataKind::ChangeVisibility,
            uri: uri.as_str().to_string(),
            range: request_range,
            document_version,
            extra: CodeActionDataExtra::ChangeVisibility {
                symbol_fqn: symbol.fqn.clone(),
                target_visibility,
            },
        })
        .ok()?;

        Some(CodeActionOrCommand::CodeAction(CodeAction {
            title: format!(
                "Change visibility to {}",
                visibility_text(target_visibility)
            ),
            kind: Some(CodeActionKind::REFACTOR_REWRITE),
            diagnostics: None,
            edit: None,
            command: None,
            is_preferred: Some(false),
            disabled: None,
            data: Some(data),
        }))
    })
    .collect()
}

fn concrete_class_symbol_at_range(
    file_symbols: &php_lsp_types::FileSymbols,
    range: (u32, u32, u32, u32),
) -> Option<&php_lsp_types::SymbolInfo> {
    file_symbols.symbols.iter().find(|sym| {
        sym.kind == php_lsp_types::PhpSymbolKind::Class
            && !sym.modifiers.is_abstract
            && byte_range_contains(sym.range, range)
    })
}

fn direct_method_symbols_from_file<'a>(
    file_symbols: &'a php_lsp_types::FileSymbols,
    type_fqn: &str,
) -> Vec<&'a php_lsp_types::SymbolInfo> {
    file_symbols
        .symbols
        .iter()
        .filter(|sym| {
            sym.kind == php_lsp_types::PhpSymbolKind::Method
                && sym.parent_fqn.as_deref() == Some(type_fqn)
        })
        .collect()
}

fn direct_member_symbols_from_index(
    index: &WorkspaceIndex,
    type_fqn: &str,
) -> Vec<Arc<php_lsp_types::SymbolInfo>> {
    let mut members = Vec::new();
    for entry in index.file_symbols.iter() {
        for sym in &entry.value().symbols {
            if sym.parent_fqn.as_deref() == Some(type_fqn) {
                members.push(Arc::new(sym.clone()));
            }
        }
    }
    members
}

fn normalized_method_name(name: &str) -> String {
    name.to_ascii_lowercase()
}

fn collect_concrete_methods_from_type(
    index: &WorkspaceIndex,
    type_fqn: &str,
    implemented: &mut HashSet<String>,
    visited: &mut HashSet<String>,
) {
    let normalized_type = type_fqn.trim_start_matches('\\').to_string();
    if !visited.insert(normalized_type.clone()) {
        return;
    }

    let Some(type_sym) = index
        .types
        .get(&normalized_type)
        .map(|entry| entry.value().clone())
    else {
        return;
    };

    for member in direct_member_symbols_from_index(index, &normalized_type) {
        if member.kind == php_lsp_types::PhpSymbolKind::Method && !member.modifiers.is_abstract {
            implemented.insert(normalized_method_name(&member.name));
        }
    }

    for trait_fqn in &type_sym.traits {
        collect_concrete_methods_from_type(index, trait_fqn, implemented, visited);
    }
    for parent_fqn in &type_sym.extends {
        collect_concrete_methods_from_type(index, parent_fqn, implemented, visited);
    }
}

fn collect_required_methods_from_type(
    index: &WorkspaceIndex,
    type_fqn: &str,
    required: &mut Vec<Arc<php_lsp_types::SymbolInfo>>,
    seen: &mut HashSet<String>,
    visited: &mut HashSet<String>,
) {
    let normalized_type = type_fqn.trim_start_matches('\\').to_string();
    if !visited.insert(normalized_type.clone()) {
        return;
    }

    let Some(type_sym) = index
        .types
        .get(&normalized_type)
        .map(|entry| entry.value().clone())
    else {
        return;
    };

    for member in direct_member_symbols_from_index(index, &normalized_type) {
        let required_method = match type_sym.kind {
            php_lsp_types::PhpSymbolKind::Interface => {
                member.kind == php_lsp_types::PhpSymbolKind::Method
            }
            php_lsp_types::PhpSymbolKind::Class | php_lsp_types::PhpSymbolKind::Trait => {
                member.kind == php_lsp_types::PhpSymbolKind::Method && member.modifiers.is_abstract
            }
            _ => false,
        };

        if required_method && seen.insert(normalized_method_name(&member.name)) {
            required.push(member);
        }
    }

    for trait_fqn in &type_sym.traits {
        collect_required_methods_from_type(index, trait_fqn, required, seen, visited);
    }
    for parent_fqn in &type_sym.extends {
        collect_required_methods_from_type(index, parent_fqn, required, seen, visited);
    }
    for iface_fqn in &type_sym.implements {
        collect_required_methods_from_type(index, iface_fqn, required, seen, visited);
    }
}

fn missing_implementation_methods(
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    class_sym: &php_lsp_types::SymbolInfo,
) -> Vec<Arc<php_lsp_types::SymbolInfo>> {
    if class_sym.kind != php_lsp_types::PhpSymbolKind::Class || class_sym.modifiers.is_abstract {
        return Vec::new();
    }

    let mut implemented = HashSet::new();
    for method in direct_method_symbols_from_file(file_symbols, &class_sym.fqn) {
        implemented.insert(normalized_method_name(&method.name));
    }

    let mut concrete_visited = HashSet::new();
    for trait_fqn in &class_sym.traits {
        collect_concrete_methods_from_type(
            index,
            trait_fqn,
            &mut implemented,
            &mut concrete_visited,
        );
    }
    for parent_fqn in &class_sym.extends {
        collect_concrete_methods_from_type(
            index,
            parent_fqn,
            &mut implemented,
            &mut concrete_visited,
        );
    }

    let mut required = Vec::new();
    let mut seen_required = HashSet::new();
    let mut required_visited = HashSet::new();
    for trait_fqn in &class_sym.traits {
        collect_required_methods_from_type(
            index,
            trait_fqn,
            &mut required,
            &mut seen_required,
            &mut required_visited,
        );
    }
    for parent_fqn in &class_sym.extends {
        collect_required_methods_from_type(
            index,
            parent_fqn,
            &mut required,
            &mut seen_required,
            &mut required_visited,
        );
    }
    for iface_fqn in &class_sym.implements {
        collect_required_methods_from_type(
            index,
            iface_fqn,
            &mut required,
            &mut seen_required,
            &mut required_visited,
        );
    }

    let mut missing = Vec::new();
    for method in required {
        let name = normalized_method_name(&method.name);
        if implemented.insert(name) {
            missing.push(method);
        }
    }

    missing.sort_by(|left, right| {
        normalized_method_name(&left.name)
            .cmp(&normalized_method_name(&right.name))
            .then_with(|| left.fqn.cmp(&right.fqn))
    });
    missing
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypeHintPosition {
    Parameter,
    Return,
}

fn native_type_hint_text(
    type_info: &php_lsp_types::TypeInfo,
    php_version: PhpVersion,
    position: TypeHintPosition,
) -> Option<String> {
    use php_lsp_types::TypeInfo;

    match type_info {
        TypeInfo::Simple(name) => Some(name.clone()),
        TypeInfo::Self_ | TypeInfo::Parent_ => Some(type_info.to_string()),
        TypeInfo::Static_ if position == TypeHintPosition::Return && php_version.at_least(8, 0) => {
            Some("static".to_string())
        }
        TypeInfo::Mixed if php_version.at_least(8, 0) => Some("mixed".to_string()),
        TypeInfo::Void if position == TypeHintPosition::Return => Some("void".to_string()),
        TypeInfo::Never if position == TypeHintPosition::Return && php_version.at_least(8, 1) => {
            Some("never".to_string())
        }
        TypeInfo::Nullable(inner) => match inner.as_ref() {
            TypeInfo::Mixed | TypeInfo::Void | TypeInfo::Never | TypeInfo::Nullable(_) => None,
            _ => native_type_hint_text(inner, php_version, position)
                .map(|inner| format!("?{}", inner)),
        },
        TypeInfo::Union(types) if php_version.at_least(8, 0) => {
            let parts = types
                .iter()
                .map(|ty| native_type_hint_text(ty, php_version, position))
                .collect::<Option<Vec<_>>>()?;
            if parts.iter().any(|part| part == "void") {
                None
            } else {
                Some(parts.join("|"))
            }
        }
        TypeInfo::Intersection(types) if php_version.at_least(8, 1) => {
            let parts = types
                .iter()
                .map(|ty| match ty {
                    TypeInfo::Simple(_) | TypeInfo::Self_ | TypeInfo::Parent_ => {
                        native_type_hint_text(ty, php_version, position)
                    }
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()?;
            Some(parts.join("&"))
        }
        TypeInfo::LiteralNull if php_version.at_least(8, 2) => Some("null".to_string()),
        TypeInfo::LiteralBool(value)
            if position == TypeHintPosition::Return && php_version.at_least(8, 2) =>
        {
            Some(if *value { "true" } else { "false" }.to_string())
        }
        _ => None,
    }
}

fn render_method_param(param: &php_lsp_types::ParamInfo, php_version: PhpVersion) -> String {
    let mut text = String::new();
    if let Some(type_info) = &param.type_info {
        if let Some(type_text) =
            native_type_hint_text(type_info, php_version, TypeHintPosition::Parameter)
        {
            text.push_str(&type_text);
            text.push(' ');
        }
    }
    if param.is_by_ref {
        text.push('&');
    }
    if param.is_variadic {
        text.push_str("...");
    }
    text.push('$');
    text.push_str(&param.name);
    if !param.is_variadic {
        if let Some(default_value) = param.default_value.as_deref() {
            text.push_str(" = ");
            text.push_str(default_value);
        }
    }
    text
}

fn render_missing_method_stub(
    method: &php_lsp_types::SymbolInfo,
    method_indent: &str,
    body_indent: &str,
    php_version: PhpVersion,
) -> String {
    let visibility = match method.visibility {
        php_lsp_types::Visibility::Public => "public",
        php_lsp_types::Visibility::Protected => "protected",
        php_lsp_types::Visibility::Private => "private",
    };

    let signature = method
        .signature
        .clone()
        .unwrap_or(php_lsp_types::Signature {
            params: Vec::new(),
            return_type: None,
        });
    let params = signature
        .params
        .iter()
        .map(|param| render_method_param(param, php_version))
        .collect::<Vec<_>>()
        .join(", ");

    let mut text = String::new();
    text.push_str(method_indent);
    text.push_str(visibility);
    text.push(' ');
    if method.modifiers.is_static {
        text.push_str("static ");
    }
    text.push_str("function ");
    text.push_str(&method.name);
    text.push('(');
    text.push_str(&params);
    text.push(')');
    if let Some(return_type) = signature.return_type.as_ref().and_then(|return_type| {
        native_type_hint_text(return_type, php_version, TypeHintPosition::Return)
    }) {
        text.push_str(": ");
        text.push_str(&return_type);
    }
    text.push('\n');
    text.push_str(method_indent);
    text.push_str("{\n");
    text.push_str(body_indent);
    text.push_str("throw new \\BadMethodCallException('Not implemented yet.');\n");
    text.push_str(method_indent);
    text.push_str("}\n");
    text
}

fn line_start_offsets(source: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    for (idx, byte) in source.bytes().enumerate() {
        if byte == b'\n' {
            offsets.push(idx + 1);
        }
    }
    offsets
}

fn byte_offset_for_line_col(source: &str, line: u32, byte_col: u32) -> Option<usize> {
    let offsets = line_start_offsets(source);
    let start = *offsets.get(line as usize)?;
    Some((start + byte_col as usize).min(source.len()))
}

fn line_col_for_byte_offset(source: &str, offset: usize) -> (u32, u32) {
    let offsets = line_start_offsets(source);
    let line_idx = offsets
        .partition_point(|line_start| *line_start <= offset)
        .saturating_sub(1);
    let line_start = offsets.get(line_idx).copied().unwrap_or(0);
    (line_idx as u32, offset.saturating_sub(line_start) as u32)
}

fn class_closing_brace_position(
    source: &str,
    class_sym: &php_lsp_types::SymbolInfo,
) -> Option<(u32, u32)> {
    let start = byte_offset_for_line_col(source, class_sym.range.0, class_sym.range.1)?;
    let end = byte_offset_for_line_col(source, class_sym.range.2, class_sym.range.3)?;
    let class_text = source.get(start..end)?;
    let closing_relative = class_text.rfind('}')?;
    Some(line_col_for_byte_offset(source, start + closing_relative))
}

fn line_text(source: &str, line: u32) -> &str {
    source.lines().nth(line as usize).unwrap_or("")
}

fn line_prefix_by_byte_col(line_text: &str, byte_col: u32) -> &str {
    let end = (byte_col as usize).min(line_text.len());
    line_text.get(..end).unwrap_or("")
}

fn leading_ascii_whitespace(text: &str) -> String {
    text.chars()
        .take_while(|ch| *ch == ' ' || *ch == '\t')
        .collect()
}

fn method_insertion_needs_leading_blank(source: &str, closing_line: u32, closing_col: u32) -> bool {
    let close_line_text = line_text(source, closing_line);
    if !line_prefix_by_byte_col(close_line_text, closing_col)
        .trim()
        .is_empty()
    {
        return true;
    }

    let lines = source.lines().collect::<Vec<_>>();
    for line in lines
        .get(..closing_line as usize)
        .unwrap_or(&[])
        .iter()
        .rev()
    {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        return !trimmed.ends_with('{');
    }

    false
}

struct ClassMethodInsertion {
    position: Position,
    method_indent: String,
    body_indent: String,
    needs_leading_blank: bool,
}

fn class_method_insertion(
    source: &str,
    class_sym: &php_lsp_types::SymbolInfo,
) -> Option<ClassMethodInsertion> {
    let (closing_line, closing_col) = class_closing_brace_position(source, class_sym)?;
    let utf16_index = Utf16LineIndex::new(source);
    let position = Position::new(
        closing_line,
        utf16_index.byte_col_to_utf16(closing_line, closing_col),
    );
    let close_line = line_text(source, closing_line);
    let close_indent = leading_ascii_whitespace(line_prefix_by_byte_col(close_line, closing_col));
    let method_indent = format!("{}    ", close_indent);
    let body_indent = format!("{}    ", method_indent);

    Some(ClassMethodInsertion {
        position,
        method_indent,
        body_indent,
        needs_leading_blank: method_insertion_needs_leading_blank(
            source,
            closing_line,
            closing_col,
        ),
    })
}

fn generated_methods_workspace_edit(
    uri: Uri,
    insertion: ClassMethodInsertion,
    rendered_methods: Vec<String>,
) -> WorkspaceEdit {
    let mut new_text = String::new();
    if insertion.needs_leading_blank {
        new_text.push('\n');
    }
    for (idx, method) in rendered_methods.into_iter().enumerate() {
        if idx > 0 {
            new_text.push('\n');
        }
        new_text.push_str(&method);
    }

    let mut changes = HashMap::new();
    changes.insert(
        uri,
        vec![TextEdit {
            range: Range {
                start: insertion.position,
                end: insertion.position,
            },
            new_text,
        }],
    );

    WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    }
}

fn implement_missing_methods_edit(
    uri: Uri,
    source: &str,
    class_sym: &php_lsp_types::SymbolInfo,
    missing_methods: &[Arc<php_lsp_types::SymbolInfo>],
    php_version: PhpVersion,
) -> Option<WorkspaceEdit> {
    if missing_methods.is_empty() {
        return Some(empty_workspace_edit());
    }

    let insertion = class_method_insertion(source, class_sym)?;
    let rendered_methods = missing_methods
        .iter()
        .map(|method| {
            render_missing_method_stub(
                method,
                &insertion.method_indent,
                &insertion.body_indent,
                php_version,
            )
        })
        .collect();

    Some(generated_methods_workspace_edit(
        uri,
        insertion,
        rendered_methods,
    ))
}

fn direct_property_symbols_from_file<'a>(
    file_symbols: &'a php_lsp_types::FileSymbols,
    type_fqn: &str,
) -> Vec<&'a php_lsp_types::SymbolInfo> {
    file_symbols
        .symbols
        .iter()
        .filter(|sym| {
            sym.kind == php_lsp_types::PhpSymbolKind::Property
                && sym.parent_fqn.as_deref() == Some(type_fqn)
        })
        .collect()
}

fn property_symbol_at_range(
    file_symbols: &php_lsp_types::FileSymbols,
    range: (u32, u32, u32, u32),
) -> Option<&php_lsp_types::SymbolInfo> {
    file_symbols
        .symbols
        .iter()
        .filter(|sym| sym.kind == php_lsp_types::PhpSymbolKind::Property)
        .find(|sym| {
            byte_range_contains(sym.range, range) || byte_ranges_overlap(sym.selection_range, range)
        })
}

fn direct_method_name_exists(
    file_symbols: &php_lsp_types::FileSymbols,
    class_fqn: &str,
    method_name: &str,
) -> bool {
    let wanted = normalized_method_name(method_name);
    direct_method_symbols_from_file(file_symbols, class_fqn)
        .iter()
        .any(|method| normalized_method_name(&method.name) == wanted)
}

fn member_method_name_exists(index: &WorkspaceIndex, class_fqn: &str, method_name: &str) -> bool {
    index
        .resolve_member(&format!("{}::{}", class_fqn, method_name))
        .is_some_and(|sym| sym.kind == php_lsp_types::PhpSymbolKind::Method)
}

fn property_type_info(property: &php_lsp_types::SymbolInfo) -> Option<&php_lsp_types::TypeInfo> {
    property
        .signature
        .as_ref()
        .and_then(|signature| signature.return_type.as_ref())
}

fn type_info_contains_bool(type_info: &php_lsp_types::TypeInfo) -> bool {
    use php_lsp_types::TypeInfo;

    match type_info {
        TypeInfo::Simple(name) => matches!(name.to_ascii_lowercase().as_str(), "bool" | "boolean"),
        TypeInfo::Nullable(inner) => type_info_contains_bool(inner),
        TypeInfo::Union(types) => types.iter().any(type_info_contains_bool),
        _ => false,
    }
}

fn property_is_bool(property: &php_lsp_types::SymbolInfo) -> bool {
    property_type_info(property).is_some_and(type_info_contains_bool)
}

fn studly_identifier(raw: &str) -> String {
    let mut result = String::new();
    for part in raw
        .trim_start_matches('$')
        .split(['_', '-'])
        .filter(|part| !part.is_empty())
    {
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            result.extend(first.to_uppercase());
            result.push_str(chars.as_str());
        }
    }

    if result.is_empty() {
        "Value".to_string()
    } else {
        result
    }
}

fn bool_getter_name(property_name: &str) -> String {
    let mut chars = property_name.chars();
    let starts_with_is = chars.next() == Some('i')
        && chars.next() == Some('s')
        && chars.next().is_some_and(|ch| ch.is_ascii_uppercase());
    if starts_with_is {
        property_name.to_string()
    } else {
        format!("is{}", studly_identifier(property_name))
    }
}

fn getter_name(property: &php_lsp_types::SymbolInfo) -> String {
    if property_is_bool(property) {
        bool_getter_name(&property.name)
    } else {
        format!("get{}", studly_identifier(&property.name))
    }
}

fn setter_name(property: &php_lsp_types::SymbolInfo) -> String {
    format!("set{}", studly_identifier(&property.name))
}

fn property_default_value(source: &str, property: &php_lsp_types::SymbolInfo) -> Option<String> {
    let start = byte_offset_for_line_col(source, property.range.0, property.range.1)?;
    let end = byte_offset_for_line_col(source, property.range.2, property.range.3)?;
    let declaration = source.get(start..end)?;
    let needle = format!("${}", property.name);
    let name_start = declaration.find(&needle)?;
    let after_name = declaration.get(name_start + needle.len()..)?;
    let equals_offset = after_name.find('=')?;
    let before_equals = after_name.get(..equals_offset)?;
    if before_equals.contains(',') || before_equals.contains(';') {
        return None;
    }

    let mut value = String::new();
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for ch in after_name[equals_offset + 1..].chars() {
        if let Some(active_quote) = quote {
            value.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active_quote {
                quote = None;
            }
            continue;
        }

        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                value.push(ch);
            }
            '(' => {
                paren_depth += 1;
                value.push(ch);
            }
            ')' => {
                paren_depth = paren_depth.saturating_sub(1);
                value.push(ch);
            }
            '[' => {
                bracket_depth += 1;
                value.push(ch);
            }
            ']' => {
                bracket_depth = bracket_depth.saturating_sub(1);
                value.push(ch);
            }
            '{' => {
                brace_depth += 1;
                value.push(ch);
            }
            '}' => {
                brace_depth = brace_depth.saturating_sub(1);
                value.push(ch);
            }
            ',' | ';' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => break,
            _ => value.push(ch),
        }
    }

    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

struct ConstructorProperty<'a> {
    symbol: &'a php_lsp_types::SymbolInfo,
    default_value: Option<String>,
    param_default: Option<String>,
}

fn constructor_generation_properties<'a>(
    source: &str,
    file_symbols: &'a php_lsp_types::FileSymbols,
    class_fqn: &str,
) -> Vec<ConstructorProperty<'a>> {
    let mut properties: Vec<_> = direct_property_symbols_from_file(file_symbols, class_fqn)
        .into_iter()
        .filter(|property| !property.modifiers.is_static)
        .map(|property| ConstructorProperty {
            symbol: property,
            default_value: property_default_value(source, property),
            param_default: None,
        })
        .collect();

    properties.sort_by_key(|property| property.symbol.selection_range);

    let mut has_later_required = false;
    for property in properties.iter_mut().rev() {
        if let Some(default_value) = property.default_value.clone() {
            if !has_later_required {
                property.param_default = Some(default_value);
            }
        } else {
            has_later_required = true;
        }
    }

    properties
}

fn render_constructor_param(property: &ConstructorProperty<'_>, php_version: PhpVersion) -> String {
    let mut text = String::new();
    if let Some(type_info) = property_type_info(property.symbol) {
        if let Some(type_text) =
            native_type_hint_text(type_info, php_version, TypeHintPosition::Parameter)
        {
            text.push_str(&type_text);
            text.push(' ');
        }
    }
    text.push('$');
    text.push_str(&property.symbol.name);
    if let Some(default_value) = property.param_default.as_deref() {
        text.push_str(" = ");
        text.push_str(default_value);
    }
    text
}

fn render_constructor_method(
    properties: &[ConstructorProperty<'_>],
    method_indent: &str,
    body_indent: &str,
    php_version: PhpVersion,
) -> String {
    let params = properties
        .iter()
        .map(|property| render_constructor_param(property, php_version))
        .collect::<Vec<_>>()
        .join(", ");

    let mut text = String::new();
    text.push_str(method_indent);
    text.push_str("public function __construct(");
    text.push_str(&params);
    text.push_str(")\n");
    text.push_str(method_indent);
    text.push_str("{\n");
    for property in properties {
        text.push_str(body_indent);
        text.push_str("$this->");
        text.push_str(&property.symbol.name);
        text.push_str(" = $");
        text.push_str(&property.symbol.name);
        text.push_str(";\n");
    }
    text.push_str(method_indent);
    text.push_str("}\n");
    text
}

fn render_accessor_method(
    property: &php_lsp_types::SymbolInfo,
    accessor_kind: AccessorKind,
    method_name: &str,
    method_indent: &str,
    body_indent: &str,
    php_version: PhpVersion,
) -> String {
    let is_static = property.modifiers.is_static;
    let type_hint = property_type_info(property);
    let mut text = String::new();
    text.push_str(method_indent);
    text.push_str("public ");
    if is_static {
        text.push_str("static ");
    }
    text.push_str("function ");
    text.push_str(method_name);

    match accessor_kind {
        AccessorKind::Getter => {
            text.push_str("()");
            if let Some(return_type) = type_hint.and_then(|type_info| {
                native_type_hint_text(type_info, php_version, TypeHintPosition::Return)
            }) {
                text.push_str(": ");
                text.push_str(&return_type);
            }
            text.push('\n');
            text.push_str(method_indent);
            text.push_str("{\n");
            text.push_str(body_indent);
            text.push_str("return ");
            if is_static {
                text.push_str("self::$");
            } else {
                text.push_str("$this->");
            }
            text.push_str(&property.name);
            text.push_str(";\n");
            text.push_str(method_indent);
            text.push_str("}\n");
        }
        AccessorKind::Setter => {
            text.push('(');
            if let Some(param_type) = type_hint.and_then(|type_info| {
                native_type_hint_text(type_info, php_version, TypeHintPosition::Parameter)
            }) {
                text.push_str(&param_type);
                text.push(' ');
            }
            text.push('$');
            text.push_str(&property.name);
            text.push_str("): void\n");
            text.push_str(method_indent);
            text.push_str("{\n");
            text.push_str(body_indent);
            if is_static {
                text.push_str("self::$");
            } else {
                text.push_str("$this->");
            }
            text.push_str(&property.name);
            text.push_str(" = $");
            text.push_str(&property.name);
            text.push_str(";\n");
            text.push_str(method_indent);
            text.push_str("}\n");
        }
    }

    text
}

fn generate_constructor_edit(
    uri: Uri,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    class_sym: &php_lsp_types::SymbolInfo,
    php_version: PhpVersion,
) -> Option<WorkspaceEdit> {
    if direct_method_name_exists(file_symbols, &class_sym.fqn, "__construct") {
        return Some(empty_workspace_edit());
    }
    let properties = constructor_generation_properties(source, file_symbols, &class_sym.fqn);
    if properties.is_empty() {
        return Some(empty_workspace_edit());
    }

    let insertion = class_method_insertion(source, class_sym)?;
    let constructor = render_constructor_method(
        &properties,
        &insertion.method_indent,
        &insertion.body_indent,
        php_version,
    );
    Some(generated_methods_workspace_edit(
        uri,
        insertion,
        vec![constructor],
    ))
}

fn generate_accessor_edit(
    uri: Uri,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    property: &php_lsp_types::SymbolInfo,
    accessor_kind: AccessorKind,
    method_name: &str,
    php_version: PhpVersion,
) -> Option<WorkspaceEdit> {
    if accessor_kind == AccessorKind::Setter && property.modifiers.is_readonly {
        return Some(empty_workspace_edit());
    }

    let class_fqn = property.parent_fqn.as_deref()?;
    if direct_method_name_exists(file_symbols, class_fqn, method_name) {
        return Some(empty_workspace_edit());
    }

    let class_sym = file_symbols
        .symbols
        .iter()
        .find(|sym| sym.fqn == class_fqn && sym.kind == php_lsp_types::PhpSymbolKind::Class)?;
    let insertion = class_method_insertion(source, class_sym)?;
    let accessor = render_accessor_method(
        property,
        accessor_kind,
        method_name,
        &insertion.method_indent,
        &insertion.body_indent,
        php_version,
    );

    Some(generated_methods_workspace_edit(
        uri,
        insertion,
        vec![accessor],
    ))
}

fn lsp_range_for_byte_offsets(source: &str, start: usize, end: usize) -> Range {
    let (start_line, start_byte_col) = line_col_for_byte_offset(source, start);
    let (end_line, end_byte_col) = line_col_for_byte_offset(source, end);
    let utf16_index = Utf16LineIndex::new(source);
    Range {
        start: Position::new(
            start_line,
            utf16_index.byte_col_to_utf16(start_line, start_byte_col),
        ),
        end: Position::new(
            end_line,
            utf16_index.byte_col_to_utf16(end_line, end_byte_col),
        ),
    }
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn find_visibility_token(
    source: &str,
    symbol: &php_lsp_types::SymbolInfo,
) -> Option<(usize, usize)> {
    let start = byte_offset_for_line_col(source, symbol.range.0, symbol.range.1)?;
    let end = byte_offset_for_line_col(source, symbol.range.2, symbol.range.3)?;
    let text = source.get(start..end)?;
    for keyword in ["public", "protected", "private"] {
        let mut search_offset = 0usize;
        while let Some(relative) = text.get(search_offset..)?.find(keyword) {
            let token_start = search_offset + relative;
            let token_end = token_start + keyword.len();
            let before = token_start
                .checked_sub(1)
                .and_then(|idx| text.as_bytes().get(idx))
                .copied();
            let after = text.as_bytes().get(token_end).copied();
            if before.is_none_or(|byte| !is_ident_byte(byte))
                && after.is_none_or(|byte| !is_ident_byte(byte))
            {
                return Some((start + token_start, start + token_end));
            }
            search_offset = token_end;
        }
    }
    None
}

fn change_visibility_edit(
    uri: Uri,
    source: &str,
    symbol: &php_lsp_types::SymbolInfo,
    target_visibility: php_lsp_types::Visibility,
) -> Option<WorkspaceEdit> {
    if !symbol_supports_visibility_change(symbol) || symbol.visibility == target_visibility {
        return Some(empty_workspace_edit());
    }

    let (start, end, new_text) =
        if let Some((token_start, token_end)) = find_visibility_token(source, symbol) {
            (
                token_start,
                token_end,
                visibility_text(target_visibility).to_string(),
            )
        } else {
            let insert_at = byte_offset_for_line_col(source, symbol.range.0, symbol.range.1)?;
            (
                insert_at,
                insert_at,
                format!("{} ", visibility_text(target_visibility)),
            )
        };

    let mut changes = HashMap::new();
    changes.insert(
        uri,
        vec![TextEdit {
            range: lsp_range_for_byte_offsets(source, start, end),
            new_text,
        }],
    );

    Some(WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    })
}

fn line_full_span(source: &str, start: usize, end: usize) -> (usize, usize) {
    let line_start = source[..start].rfind('\n').map(|idx| idx + 1).unwrap_or(0);
    let line_end = source[end..]
        .find('\n')
        .map(|idx| end + idx + 1)
        .unwrap_or(source.len());
    (line_start, line_end)
}

fn find_matching_delimiter(
    text: &str,
    open_offset: usize,
    open: char,
    close: char,
) -> Option<usize> {
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (idx, ch) in text
        .char_indices()
        .skip_while(|(idx, _)| *idx < open_offset)
    {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active_quote {
                quote = None;
            }
            continue;
        }

        match ch {
            '\'' | '"' => quote = Some(ch),
            _ if ch == open => depth += 1,
            _ if ch == close => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }
    None
}

fn split_top_level_spans(text: &str, base_offset: usize) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for (idx, ch) in text.char_indices() {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active_quote {
                quote = None;
            }
            continue;
        }

        match ch {
            '\'' | '"' => quote = Some(ch),
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            ',' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => {
                spans.push((base_offset + start, base_offset + idx));
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }

    spans.push((base_offset + start, base_offset + text.len()));
    spans
}

fn variable_name_in_parameter(param_text: &str) -> Option<String> {
    let bytes = param_text.as_bytes();
    let mut idx = 0usize;
    while idx < bytes.len() {
        if bytes[idx] == b'$' {
            let start = idx + 1;
            let mut end = start;
            while end < bytes.len() && is_ident_byte(bytes[end]) {
                end += 1;
            }
            if end > start {
                return Some(param_text[start..end].to_string());
            }
        }
        idx += 1;
    }
    None
}

fn constructor_symbol<'a>(
    file_symbols: &'a php_lsp_types::FileSymbols,
    class_fqn: &str,
) -> Option<&'a php_lsp_types::SymbolInfo> {
    direct_method_symbols_from_file(file_symbols, class_fqn)
        .into_iter()
        .find(|method| method.name.eq_ignore_ascii_case("__construct"))
}

#[derive(Clone)]
struct ConstructorParamSpan {
    name: String,
    start: usize,
    end: usize,
    text: String,
}

fn constructor_param_spans(
    source: &str,
    constructor: &php_lsp_types::SymbolInfo,
) -> Option<Vec<ConstructorParamSpan>> {
    let start = byte_offset_for_line_col(source, constructor.range.0, constructor.range.1)?;
    let end = byte_offset_for_line_col(source, constructor.range.2, constructor.range.3)?;
    let method_text = source.get(start..end)?;
    let open_relative = method_text.find('(')?;
    let close_relative = find_matching_delimiter(method_text, open_relative, '(', ')')?;
    let params_start = start + open_relative + 1;
    let params_end = start + close_relative;
    let params_text = source.get(params_start..params_end)?;

    Some(
        split_top_level_spans(params_text, params_start)
            .into_iter()
            .filter_map(|(span_start, span_end)| {
                let raw = source.get(span_start..span_end)?;
                let text = raw.trim();
                if text.is_empty() {
                    return None;
                }
                let leading_ws = raw.len().saturating_sub(raw.trim_start().len());
                let trailing_ws = raw.len().saturating_sub(raw.trim_end().len());
                let trimmed_start = span_start + leading_ws;
                let trimmed_end = span_end.saturating_sub(trailing_ws);
                Some(ConstructorParamSpan {
                    name: variable_name_in_parameter(text)?,
                    start: trimmed_start,
                    end: trimmed_end,
                    text: text.to_string(),
                })
            })
            .collect(),
    )
}

fn constructor_body_span(
    source: &str,
    constructor: &php_lsp_types::SymbolInfo,
) -> Option<(usize, usize)> {
    let start = byte_offset_for_line_col(source, constructor.range.0, constructor.range.1)?;
    let end = byte_offset_for_line_col(source, constructor.range.2, constructor.range.3)?;
    let method_text = source.get(start..end)?;
    let open_paren = method_text.find('(')?;
    let close_paren = find_matching_delimiter(method_text, open_paren, '(', ')')?;
    let after_params = method_text.get(close_paren..)?;
    let open_brace_relative = after_params.find('{')? + close_paren;
    let close_brace_relative = find_matching_delimiter(method_text, open_brace_relative, '{', '}')?;
    Some((
        start + open_brace_relative + 1,
        start + close_brace_relative,
    ))
}

fn property_declaration_is_safe_to_remove(
    source: &str,
    property: &php_lsp_types::SymbolInfo,
) -> bool {
    if property.doc_comment.is_some() {
        return false;
    }
    let Some(start) = byte_offset_for_line_col(source, property.range.0, property.range.1) else {
        return false;
    };
    let Some(end) = byte_offset_for_line_col(source, property.range.2, property.range.3) else {
        return false;
    };
    let Some(text) = source.get(start..end) else {
        return false;
    };
    if text.contains("#[") {
        return false;
    }
    let before_semicolon = text
        .split_once(';')
        .map(|(before, _)| before)
        .unwrap_or(text);
    !before_semicolon.contains(',')
}

fn property_promotion_prefix(property: &php_lsp_types::SymbolInfo) -> String {
    let mut parts = vec![visibility_text(property.visibility)];
    if property.modifiers.is_readonly {
        parts.push("readonly");
    }
    parts.join(" ")
}

fn parameter_is_already_promoted(param_text: &str) -> bool {
    let before_var = param_text.split('$').next().unwrap_or("");
    before_var
        .split_whitespace()
        .any(|part| matches!(part, "public" | "protected" | "private"))
}

fn find_constructor_assignment_line(
    source: &str,
    constructor: &php_lsp_types::SymbolInfo,
    property_name: &str,
) -> Option<(usize, usize)> {
    let (body_start, body_end) = constructor_body_span(source, constructor)?;
    let body = source.get(body_start..body_end)?;
    let expected = format!("$this->{} = ${};", property_name, property_name);
    let mut matches = Vec::new();
    let mut cursor = body_start;
    for line in body.split_inclusive('\n') {
        let line_start = cursor;
        let line_end = cursor + line.len();
        cursor = line_end;
        let trimmed = line.trim();
        if trimmed == expected {
            matches.push((line_start, line_end));
        } else if trimmed.contains(&format!("$this->{}", property_name)) && trimmed.contains('=') {
            return None;
        }
    }

    if matches.len() == 1 {
        matches.into_iter().next()
    } else {
        None
    }
}

struct PromoteConstructorParameterPlan {
    property_delete: (usize, usize),
    param_replace: (usize, usize, String),
    assignment_delete: (usize, usize),
}

fn promote_constructor_parameter_plan(
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    property: &php_lsp_types::SymbolInfo,
) -> Option<PromoteConstructorParameterPlan> {
    if property.kind != php_lsp_types::PhpSymbolKind::Property
        || property.modifiers.is_static
        || !property_declaration_is_safe_to_remove(source, property)
    {
        return None;
    }
    let class_fqn = property.parent_fqn.as_deref()?;
    let constructor = constructor_symbol(file_symbols, class_fqn)?;
    let param = constructor_param_spans(source, constructor)?
        .into_iter()
        .find(|param| param.name == property.name)?;
    if parameter_is_already_promoted(&param.text) {
        return None;
    }

    let property_start = byte_offset_for_line_col(source, property.range.0, property.range.1)?;
    let property_end = byte_offset_for_line_col(source, property.range.2, property.range.3)?;
    let property_delete = line_full_span(source, property_start, property_end);
    let assignment_delete = find_constructor_assignment_line(source, constructor, &property.name)?;
    let promoted_param = format!("{} {}", property_promotion_prefix(property), param.text);

    Some(PromoteConstructorParameterPlan {
        property_delete,
        param_replace: (param.start, param.end, promoted_param),
        assignment_delete,
    })
}

fn property_for_constructor_param_at_range<'a>(
    source: &str,
    file_symbols: &'a php_lsp_types::FileSymbols,
    range: (u32, u32, u32, u32),
) -> Option<&'a php_lsp_types::SymbolInfo> {
    let point = byte_offset_for_line_col(source, range.0, range.1)?;
    for class_sym in file_symbols
        .symbols
        .iter()
        .filter(|sym| sym.kind == php_lsp_types::PhpSymbolKind::Class)
    {
        let Some(constructor) = constructor_symbol(file_symbols, &class_sym.fqn) else {
            continue;
        };
        let Some(param) = constructor_param_spans(source, constructor).and_then(|params| {
            params
                .into_iter()
                .find(|param| point >= param.start && point <= param.end)
        }) else {
            continue;
        };
        if let Some(property) = direct_property_symbols_from_file(file_symbols, &class_sym.fqn)
            .into_iter()
            .find(|property| property.name == param.name)
        {
            return Some(property);
        }
    }
    None
}

fn build_promote_constructor_parameter_action(
    uri: Uri,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    property: &php_lsp_types::SymbolInfo,
    request_range: Range,
    document_version: Option<i32>,
) -> Option<CodeActionOrCommand> {
    promote_constructor_parameter_plan(source, file_symbols, property)?;
    let data = serde_json::to_value(CodeActionData {
        action_kind: CodeActionDataKind::PromoteConstructorParameter,
        uri: uri.as_str().to_string(),
        range: request_range,
        document_version,
        extra: CodeActionDataExtra::PromoteConstructorParameter {
            property_fqn: property.fqn.clone(),
        },
    })
    .ok()?;

    Some(CodeActionOrCommand::CodeAction(CodeAction {
        title: format!("Promote constructor parameter `${}`", property.name),
        kind: Some(CodeActionKind::REFACTOR_REWRITE),
        diagnostics: None,
        edit: None,
        command: None,
        is_preferred: Some(false),
        disabled: None,
        data: Some(data),
    }))
}

fn promote_constructor_parameter_edit(
    uri: Uri,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    property: &php_lsp_types::SymbolInfo,
) -> Option<WorkspaceEdit> {
    let plan = promote_constructor_parameter_plan(source, file_symbols, property)?;
    let mut edits = vec![
        TextEdit {
            range: lsp_range_for_byte_offsets(source, plan.param_replace.0, plan.param_replace.1),
            new_text: plan.param_replace.2,
        },
        TextEdit {
            range: lsp_range_for_byte_offsets(
                source,
                plan.assignment_delete.0,
                plan.assignment_delete.1,
            ),
            new_text: String::new(),
        },
        TextEdit {
            range: lsp_range_for_byte_offsets(
                source,
                plan.property_delete.0,
                plan.property_delete.1,
            ),
            new_text: String::new(),
        },
    ];
    edits.sort_by(|left, right| {
        (right.range.start.line, right.range.start.character)
            .cmp(&(left.range.start.line, left.range.start.character))
    });

    let mut changes = HashMap::new();
    changes.insert(uri, edits);
    Some(WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    })
}

fn callable_symbol_at_range(
    file_symbols: &php_lsp_types::FileSymbols,
    range: (u32, u32, u32, u32),
) -> Option<&php_lsp_types::SymbolInfo> {
    file_symbols
        .symbols
        .iter()
        .filter(|sym| {
            matches!(
                sym.kind,
                php_lsp_types::PhpSymbolKind::Function | php_lsp_types::PhpSymbolKind::Method
            ) && !sym.modifiers.is_builtin
        })
        .find(|sym| {
            byte_range_contains(sym.range, range) || byte_ranges_overlap(sym.selection_range, range)
        })
}

#[derive(Clone)]
struct DesiredPhpDocParam {
    name: String,
    type_text: String,
    variable_text: String,
    description: Option<String>,
}

#[derive(Clone, PartialEq, Eq)]
enum PhpDocReturnUpdate {
    Preserve,
    Remove,
    Replace(String),
}

struct UpdatePhpDocPlan {
    start: usize,
    end: usize,
    new_text: String,
}

fn phpdoc_line_starts_with_tag(line: &str, tag: &str) -> bool {
    let trimmed = line.trim_start();
    let Some(rest) = trimmed.strip_prefix(tag) else {
        return false;
    };
    rest.is_empty() || rest.chars().next().is_some_and(|ch| ch.is_whitespace())
}

fn phpdoc_line_is_tag(line: &str) -> bool {
    line.trim_start().starts_with('@')
}

fn phpdoc_content_lines(doc_comment: &str) -> Vec<String> {
    let raw_lines: Vec<&str> = doc_comment.lines().collect();
    let mut lines = Vec::new();

    for raw in raw_lines.iter() {
        let trimmed_start = raw.trim_start();
        if let Some(rest) = trimmed_start.strip_prefix("/**") {
            let rest = rest.trim_start();
            let rest = rest.strip_suffix("*/").map(str::trim_end).unwrap_or(rest);
            if !rest.is_empty() {
                lines.push(rest.to_string());
            }
            continue;
        }

        if trimmed_start.starts_with("*/") {
            continue;
        }

        if let Some(rest) = trimmed_start.strip_prefix('*') {
            lines.push(
                rest.strip_prefix(' ')
                    .unwrap_or(rest)
                    .trim_end()
                    .to_string(),
            );
        } else {
            lines.push(trimmed_start.trim_end().to_string());
        }
    }

    lines
}

fn normalize_phpdoc_content_lines(lines: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut previous_blank = true;

    for line in lines {
        let line = line.trim_end().to_string();
        let is_blank = line.trim().is_empty();
        if is_blank {
            if !previous_blank {
                out.push(String::new());
            }
            previous_blank = true;
        } else {
            out.push(line);
            previous_blank = false;
        }
    }

    while out.last().is_some_and(|line| line.trim().is_empty()) {
        out.pop();
    }

    out
}

fn phpdoc_managed_insert_index(lines: &[String]) -> usize {
    lines
        .iter()
        .position(|line| phpdoc_line_is_tag(line))
        .unwrap_or(lines.len())
}

fn render_phpdoc_param_line(param: &DesiredPhpDocParam) -> String {
    let mut line = format!(
        "@param {} {}",
        param.type_text.trim(),
        param.variable_text.trim()
    );
    if let Some(description) = param.description.as_deref().filter(|desc| !desc.is_empty()) {
        line.push(' ');
        line.push_str(description);
    }
    line
}

fn render_managed_phpdoc_lines(
    params: &[DesiredPhpDocParam],
    return_update: &PhpDocReturnUpdate,
) -> Vec<String> {
    let mut lines = params
        .iter()
        .map(render_phpdoc_param_line)
        .collect::<Vec<_>>();
    if let PhpDocReturnUpdate::Replace(return_type) = return_update {
        lines.push(format!("@return {}", return_type));
    }
    lines
}

fn update_phpdoc_content_lines(
    existing_lines: Vec<String>,
    managed_lines: Vec<String>,
    manage_return: bool,
) -> Vec<String> {
    let mut filtered = Vec::new();
    let mut insert_at = None;

    for line in existing_lines {
        let managed = phpdoc_line_starts_with_tag(&line, "@param")
            || (manage_return && phpdoc_line_starts_with_tag(&line, "@return"));
        if managed {
            if insert_at.is_none() {
                insert_at = Some(filtered.len());
            }
            continue;
        }
        filtered.push(line);
    }

    let insert_at = insert_at.unwrap_or_else(|| phpdoc_managed_insert_index(&filtered));
    let mut out = Vec::new();
    out.extend(filtered[..insert_at].iter().cloned());
    if !managed_lines.is_empty() {
        if out.last().is_some_and(|line| !line.trim().is_empty()) {
            out.push(String::new());
        }
        out.extend(managed_lines);
    }
    out.extend(filtered[insert_at..].iter().cloned());

    normalize_phpdoc_content_lines(out)
}

fn render_phpdoc_comment(indent: &str, content_lines: &[String]) -> String {
    let mut text = String::new();
    text.push_str(indent);
    text.push_str("/**\n");
    for line in content_lines {
        text.push_str(indent);
        if line.trim().is_empty() {
            text.push_str(" *\n");
        } else {
            text.push_str(" * ");
            text.push_str(line);
            text.push('\n');
        }
    }
    text.push_str(indent);
    text.push_str(" */");
    text
}

fn line_start_offset(source: &str, offset: usize) -> usize {
    source[..offset].rfind('\n').map(|idx| idx + 1).unwrap_or(0)
}

fn line_end_offset(source: &str, offset: usize) -> usize {
    source[offset..]
        .find('\n')
        .map(|idx| offset + idx)
        .unwrap_or(source.len())
}

fn line_indent_at_offset(source: &str, offset: usize) -> String {
    let line_start = line_start_offset(source, offset);
    let line_end = line_end_offset(source, line_start);
    leading_ascii_whitespace(source.get(line_start..line_end).unwrap_or(""))
}

fn symbol_doc_comment_span(
    source: &str,
    symbol: &php_lsp_types::SymbolInfo,
) -> Option<(usize, usize)> {
    let doc_comment = symbol.doc_comment.as_deref()?;
    let declaration_start = byte_offset_for_line_col(source, symbol.range.0, symbol.range.1)?;
    let search = source.get(..declaration_start)?;
    let start = search.rfind(doc_comment)?;
    Some((start, start + doc_comment.len()))
}

fn symbol_has_native_return_type(source: &str, symbol: &php_lsp_types::SymbolInfo) -> bool {
    if !matches!(
        symbol.kind,
        php_lsp_types::PhpSymbolKind::Function | php_lsp_types::PhpSymbolKind::Method
    ) {
        return false;
    }

    let Some(start) = byte_offset_for_line_col(source, symbol.range.0, symbol.range.1) else {
        return false;
    };
    let Some(end) = byte_offset_for_line_col(source, symbol.range.2, symbol.range.3) else {
        return false;
    };
    let Some(text) = source.get(start..end) else {
        return false;
    };
    let Some(open_paren) = text.find('(') else {
        return false;
    };
    let Some(close_paren) = find_matching_delimiter(text, open_paren, '(', ')') else {
        return false;
    };
    text.get(close_paren + 1..)
        .is_some_and(|after_params| after_params.trim_start().starts_with(':'))
}

fn phpdoc_return_update(source: &str, symbol: &php_lsp_types::SymbolInfo) -> PhpDocReturnUpdate {
    if !symbol_has_native_return_type(source, symbol) {
        return PhpDocReturnUpdate::Preserve;
    }

    match symbol
        .signature
        .as_ref()
        .and_then(|sig| sig.return_type.as_ref())
    {
        Some(php_lsp_types::TypeInfo::Void) => PhpDocReturnUpdate::Remove,
        Some(return_type) => PhpDocReturnUpdate::Replace(return_type.to_string()),
        None => PhpDocReturnUpdate::Preserve,
    }
}

fn phpdoc_param_variable_text(param: &php_lsp_types::ParamInfo) -> String {
    let mut text = String::new();
    if param.is_by_ref {
        text.push('&');
    }
    if param.is_variadic {
        text.push_str("...");
    }
    text.push('$');
    text.push_str(&param.name);
    text
}

fn desired_phpdoc_params(
    signature: &php_lsp_types::Signature,
    existing_doc: Option<&php_lsp_types::PhpDoc>,
) -> Vec<DesiredPhpDocParam> {
    let has_native_param_types = signature
        .params
        .iter()
        .any(|param| param.type_info.is_some());
    let has_existing_param_tags = existing_doc.is_some_and(|doc| !doc.params.is_empty());
    if !has_existing_param_tags && !has_native_param_types {
        return Vec::new();
    }

    let mut existing_by_name = HashMap::new();
    if let Some(doc) = existing_doc {
        for param in &doc.params {
            existing_by_name.entry(param.name.clone()).or_insert(param);
        }
    }

    signature
        .params
        .iter()
        .map(|param| {
            let existing = existing_by_name.get(&param.name).copied();
            let type_text = param
                .type_info
                .as_ref()
                .map(ToString::to_string)
                .or_else(|| {
                    existing
                        .and_then(|doc_param| doc_param.type_info.as_ref())
                        .map(ToString::to_string)
                })
                .unwrap_or_else(|| "mixed".to_string());

            DesiredPhpDocParam {
                name: param.name.clone(),
                type_text,
                variable_text: phpdoc_param_variable_text(param),
                description: existing.and_then(|doc_param| doc_param.description.clone()),
            }
        })
        .collect()
}

fn phpdoc_params_need_update(
    existing_doc: Option<&php_lsp_types::PhpDoc>,
    desired_params: &[DesiredPhpDocParam],
) -> bool {
    let Some(existing_doc) = existing_doc else {
        return !desired_params.is_empty();
    };
    if existing_doc.params.len() != desired_params.len() {
        return true;
    }

    existing_doc
        .params
        .iter()
        .zip(desired_params.iter())
        .any(|(existing, desired)| {
            existing.name != desired.name
                || existing
                    .type_info
                    .as_ref()
                    .map(ToString::to_string)
                    .as_deref()
                    != Some(desired.type_text.as_str())
        })
}

fn phpdoc_return_needs_update(
    existing_doc: Option<&php_lsp_types::PhpDoc>,
    return_update: &PhpDocReturnUpdate,
) -> bool {
    match return_update {
        PhpDocReturnUpdate::Preserve => false,
        PhpDocReturnUpdate::Remove => existing_doc.is_some_and(|doc| doc.return_type.is_some()),
        PhpDocReturnUpdate::Replace(return_type) => {
            existing_doc
                .and_then(|doc| doc.return_type.as_ref())
                .map(ToString::to_string)
                .as_deref()
                != Some(return_type.as_str())
        }
    }
}

fn update_phpdoc_existing_plan(
    source: &str,
    symbol: &php_lsp_types::SymbolInfo,
    desired_params: &[DesiredPhpDocParam],
    return_update: &PhpDocReturnUpdate,
) -> Option<UpdatePhpDocPlan> {
    let doc_comment = symbol.doc_comment.as_deref()?;
    let (doc_start, doc_end) = symbol_doc_comment_span(source, symbol)?;
    let manage_return = !matches!(return_update, PhpDocReturnUpdate::Preserve);
    let managed_lines = render_managed_phpdoc_lines(desired_params, return_update);
    let content_lines = update_phpdoc_content_lines(
        phpdoc_content_lines(doc_comment),
        managed_lines,
        manage_return,
    );

    let line_start = line_start_offset(source, doc_start);
    let line_end = line_end_offset(source, doc_end);
    let line_prefix = source.get(line_start..doc_start).unwrap_or("");
    let line_suffix = source.get(doc_end..line_end).unwrap_or("");
    let starts_standalone = line_prefix.trim().is_empty();
    let ends_standalone = line_suffix.trim().is_empty();

    if content_lines.is_empty() {
        let (start, end) = if starts_standalone && ends_standalone {
            line_full_span(source, doc_start, doc_end)
        } else {
            (doc_start, doc_end)
        };
        return Some(UpdatePhpDocPlan {
            start,
            end,
            new_text: String::new(),
        });
    }

    let start = if starts_standalone {
        line_start
    } else {
        doc_start
    };
    let indent = if starts_standalone { line_prefix } else { "" };

    Some(UpdatePhpDocPlan {
        start,
        end: doc_end,
        new_text: render_phpdoc_comment(indent, &content_lines),
    })
}

fn update_phpdoc_create_plan(
    source: &str,
    symbol: &php_lsp_types::SymbolInfo,
    desired_params: &[DesiredPhpDocParam],
    return_update: &PhpDocReturnUpdate,
) -> Option<UpdatePhpDocPlan> {
    let managed_lines = render_managed_phpdoc_lines(desired_params, return_update);
    if managed_lines.is_empty() {
        return None;
    }

    let declaration_start = byte_offset_for_line_col(source, symbol.range.0, symbol.range.1)?;
    let insert_at = line_start_offset(source, declaration_start);
    let indent = line_indent_at_offset(source, declaration_start);
    let mut new_text = render_phpdoc_comment(&indent, &managed_lines);
    new_text.push('\n');

    Some(UpdatePhpDocPlan {
        start: insert_at,
        end: insert_at,
        new_text,
    })
}

fn update_phpdoc_from_signature_plan(
    source: &str,
    symbol: &php_lsp_types::SymbolInfo,
) -> Option<UpdatePhpDocPlan> {
    if !matches!(
        symbol.kind,
        php_lsp_types::PhpSymbolKind::Function | php_lsp_types::PhpSymbolKind::Method
    ) || symbol.modifiers.is_builtin
    {
        return None;
    }

    let signature = symbol.signature.as_ref()?;
    let existing_doc = symbol.doc_comment.as_deref().map(parse_phpdoc);
    let desired_params = desired_phpdoc_params(signature, existing_doc.as_ref());
    let return_update = phpdoc_return_update(source, symbol);
    let params_need_update = phpdoc_params_need_update(existing_doc.as_ref(), &desired_params);
    let return_needs_update = phpdoc_return_needs_update(existing_doc.as_ref(), &return_update);

    if !params_need_update && !return_needs_update {
        return None;
    }

    if symbol.doc_comment.is_some() {
        update_phpdoc_existing_plan(source, symbol, &desired_params, &return_update)
    } else {
        update_phpdoc_create_plan(source, symbol, &desired_params, &return_update)
    }
}

fn build_update_phpdoc_from_signature_action(
    uri: Uri,
    source: &str,
    symbol: &php_lsp_types::SymbolInfo,
    request_range: Range,
    document_version: Option<i32>,
) -> Option<CodeActionOrCommand> {
    update_phpdoc_from_signature_plan(source, symbol)?;
    let data = serde_json::to_value(CodeActionData {
        action_kind: CodeActionDataKind::UpdatePhpDoc,
        uri: uri.as_str().to_string(),
        range: request_range,
        document_version,
        extra: CodeActionDataExtra::UpdatePhpDoc {
            symbol_fqn: symbol.fqn.clone(),
        },
    })
    .ok()?;

    Some(CodeActionOrCommand::CodeAction(CodeAction {
        title: "Update PHPDoc from signature".to_string(),
        kind: Some(CodeActionKind::REFACTOR_REWRITE),
        diagnostics: None,
        edit: None,
        command: None,
        is_preferred: Some(false),
        disabled: None,
        data: Some(data),
    }))
}

fn update_phpdoc_from_signature_edit(
    uri: Uri,
    source: &str,
    symbol: &php_lsp_types::SymbolInfo,
) -> Option<WorkspaceEdit> {
    let plan = update_phpdoc_from_signature_plan(source, symbol)?;
    let mut changes = HashMap::new();
    changes.insert(
        uri,
        vec![TextEdit {
            range: lsp_range_for_byte_offsets(source, plan.start, plan.end),
            new_text: plan.new_text,
        }],
    );

    Some(WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    })
}

fn semantic_tokens_legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: SEMANTIC_TOKEN_TYPES
            .iter()
            .map(|token_type| SemanticTokenType::from(*token_type))
            .collect(),
        token_modifiers: SEMANTIC_TOKEN_MODIFIERS
            .iter()
            .map(|modifier| SemanticTokenModifier::from(*modifier))
            .collect(),
    }
}

fn php_file_operation_registration_options() -> FileOperationRegistrationOptions {
    FileOperationRegistrationOptions {
        filters: vec![FileOperationFilter {
            scheme: Some("file".to_string()),
            pattern: FileOperationPattern {
                glob: "**/*.php".to_string(),
                matches: Some(FileOperationPatternKind::File),
                options: None,
            },
        }],
    }
}

fn semantic_tokens_for_parser(parser: &FileParser) -> Option<Vec<SemanticToken>> {
    let tree = parser.tree()?;
    let source = parser.source();
    Some(
        extract_semantic_tokens(tree, &source)
            .into_iter()
            .map(|token| SemanticToken {
                delta_line: token.delta_line,
                delta_start: token.delta_start,
                length: token.length,
                token_type: token.token_type,
                token_modifiers_bitset: token.token_modifiers_bitset,
            })
            .collect(),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AbsoluteSemanticToken {
    line: u32,
    start: u32,
    length: u32,
    token_type: u32,
    token_modifiers_bitset: u32,
}

fn semantic_tokens_for_parser_range(
    parser: &FileParser,
    range: Range,
) -> Option<Vec<SemanticToken>> {
    let tokens = semantic_tokens_for_parser(parser)?;
    let absolute_tokens = decode_semantic_tokens(&tokens);
    let range_tokens: Vec<_> = absolute_tokens
        .into_iter()
        .filter(|token| semantic_token_overlaps_range(*token, range))
        .collect();
    Some(encode_semantic_tokens(&range_tokens))
}

fn decode_semantic_tokens(tokens: &[SemanticToken]) -> Vec<AbsoluteSemanticToken> {
    let mut line = 0u32;
    let mut start = 0u32;
    tokens
        .iter()
        .map(|token| {
            line = line.saturating_add(token.delta_line);
            if token.delta_line == 0 {
                start = start.saturating_add(token.delta_start);
            } else {
                start = token.delta_start;
            }
            AbsoluteSemanticToken {
                line,
                start,
                length: token.length,
                token_type: token.token_type,
                token_modifiers_bitset: token.token_modifiers_bitset,
            }
        })
        .collect()
}

fn encode_semantic_tokens(tokens: &[AbsoluteSemanticToken]) -> Vec<SemanticToken> {
    let mut previous_line = 0u32;
    let mut previous_start = 0u32;

    tokens
        .iter()
        .enumerate()
        .map(|(index, token)| {
            let delta_line = if index == 0 {
                token.line
            } else {
                token.line.saturating_sub(previous_line)
            };
            let delta_start = if delta_line == 0 {
                token.start.saturating_sub(previous_start)
            } else {
                token.start
            };
            previous_line = token.line;
            previous_start = token.start;
            SemanticToken {
                delta_line,
                delta_start,
                length: token.length,
                token_type: token.token_type,
                token_modifiers_bitset: token.token_modifiers_bitset,
            }
        })
        .collect()
}

fn semantic_token_overlaps_range(token: AbsoluteSemanticToken, range: Range) -> bool {
    let token_start = Position::new(token.line, token.start);
    let token_end = Position::new(token.line, token.start.saturating_add(token.length));
    position_before(token_start, range.end) && position_before(range.start, token_end)
}

fn position_before(left: Position, right: Position) -> bool {
    left.line < right.line || (left.line == right.line && left.character < right.character)
}

fn semantic_tokens_flat_len(token_count: usize) -> u32 {
    u32::try_from(token_count.saturating_mul(5)).unwrap_or(u32::MAX)
}

fn semantic_tokens_delta_edits(
    previous: &[SemanticToken],
    current: &[SemanticToken],
) -> Vec<SemanticTokensEdit> {
    if previous == current {
        return vec![];
    }

    let mut common_prefix = 0usize;
    while common_prefix < previous.len()
        && common_prefix < current.len()
        && previous[common_prefix] == current[common_prefix]
    {
        common_prefix += 1;
    }

    let mut common_suffix = 0usize;
    while common_suffix < previous.len().saturating_sub(common_prefix)
        && common_suffix < current.len().saturating_sub(common_prefix)
        && previous[previous.len() - 1 - common_suffix]
            == current[current.len() - 1 - common_suffix]
    {
        common_suffix += 1;
    }

    let delete_count = previous.len() - common_prefix - common_suffix;
    let insert_end = current.len() - common_suffix;
    let inserted = current[common_prefix..insert_end].to_vec();

    vec![SemanticTokensEdit {
        start: semantic_tokens_flat_len(common_prefix),
        delete_count: semantic_tokens_flat_len(delete_count),
        data: (!inserted.is_empty()).then_some(inserted),
    }]
}

fn full_document_range(source: &str) -> Range {
    let mut line = 0u32;
    let mut character = 0u32;

    for ch in source.chars() {
        if ch == '\n' {
            line += 1;
            character = 0;
        } else {
            character += ch.len_utf16() as u32;
        }
    }

    Range {
        start: Position::new(0, 0),
        end: Position::new(line, character),
    }
}

#[derive(Debug, Clone)]
struct WorkspaceSymbolCandidate {
    score: i64,
    symbol: php_lsp_types::SymbolInfo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceSymbolKindFilter {
    Type,
    Class,
    Interface,
    Trait,
    Enum,
    Function,
    Method,
    Property,
    Constant,
}

fn workspace_symbol_candidates(
    index: &WorkspaceIndex,
    raw_query: &str,
) -> Vec<WorkspaceSymbolCandidate> {
    let (kind_filter, query) = parse_workspace_symbol_query(raw_query);
    if query.is_empty() && kind_filter.is_none() {
        return Vec::new();
    }

    let mut candidates = Vec::new();
    for file_symbols in index.file_symbols.iter() {
        for symbol in &file_symbols.symbols {
            if symbol.modifiers.is_builtin {
                continue;
            }
            if !kind_filter.is_none_or(|filter| workspace_symbol_kind_matches(symbol.kind, filter))
            {
                continue;
            }
            let Some(score) = workspace_symbol_score(symbol, &query) else {
                continue;
            };
            candidates.push(WorkspaceSymbolCandidate {
                score,
                symbol: symbol.clone(),
            });
        }
    }

    candidates.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| {
                workspace_symbol_kind_rank(left.symbol.kind)
                    .cmp(&workspace_symbol_kind_rank(right.symbol.kind))
            })
            .then_with(|| left.symbol.fqn.cmp(&right.symbol.fqn))
    });
    candidates
}

fn parse_workspace_symbol_query(raw_query: &str) -> (Option<WorkspaceSymbolKindFilter>, String) {
    let query = raw_query.trim();
    if let Some((prefix, rest)) = query.split_once(':') {
        if let Some(filter) = parse_workspace_symbol_kind_filter(prefix) {
            return (Some(filter), rest.trim().to_string());
        }
    }

    if let Some((prefix, rest)) = query.split_once(char::is_whitespace) {
        if let Some(filter) = parse_workspace_symbol_kind_filter(prefix) {
            return (Some(filter), rest.trim().to_string());
        }
    }

    (None, query.to_string())
}

fn parse_workspace_symbol_kind_filter(raw: &str) -> Option<WorkspaceSymbolKindFilter> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "type" | "types" => Some(WorkspaceSymbolKindFilter::Type),
        "class" | "classes" => Some(WorkspaceSymbolKindFilter::Class),
        "interface" | "interfaces" => Some(WorkspaceSymbolKindFilter::Interface),
        "trait" | "traits" => Some(WorkspaceSymbolKindFilter::Trait),
        "enum" | "enums" => Some(WorkspaceSymbolKindFilter::Enum),
        "function" | "functions" | "fn" => Some(WorkspaceSymbolKindFilter::Function),
        "method" | "methods" => Some(WorkspaceSymbolKindFilter::Method),
        "property" | "properties" | "prop" | "props" => Some(WorkspaceSymbolKindFilter::Property),
        "const" | "constant" | "constants" => Some(WorkspaceSymbolKindFilter::Constant),
        _ => None,
    }
}

fn workspace_symbol_kind_matches(
    kind: php_lsp_types::PhpSymbolKind,
    filter: WorkspaceSymbolKindFilter,
) -> bool {
    match filter {
        WorkspaceSymbolKindFilter::Type => matches!(
            kind,
            php_lsp_types::PhpSymbolKind::Class
                | php_lsp_types::PhpSymbolKind::Interface
                | php_lsp_types::PhpSymbolKind::Trait
                | php_lsp_types::PhpSymbolKind::Enum
        ),
        WorkspaceSymbolKindFilter::Class => kind == php_lsp_types::PhpSymbolKind::Class,
        WorkspaceSymbolKindFilter::Interface => kind == php_lsp_types::PhpSymbolKind::Interface,
        WorkspaceSymbolKindFilter::Trait => kind == php_lsp_types::PhpSymbolKind::Trait,
        WorkspaceSymbolKindFilter::Enum => kind == php_lsp_types::PhpSymbolKind::Enum,
        WorkspaceSymbolKindFilter::Function => kind == php_lsp_types::PhpSymbolKind::Function,
        WorkspaceSymbolKindFilter::Method => kind == php_lsp_types::PhpSymbolKind::Method,
        WorkspaceSymbolKindFilter::Property => kind == php_lsp_types::PhpSymbolKind::Property,
        WorkspaceSymbolKindFilter::Constant => matches!(
            kind,
            php_lsp_types::PhpSymbolKind::ClassConstant
                | php_lsp_types::PhpSymbolKind::GlobalConstant
                | php_lsp_types::PhpSymbolKind::EnumCase
        ),
    }
}

fn workspace_symbol_score(symbol: &php_lsp_types::SymbolInfo, query: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(1_000 + workspace_symbol_kind_bonus(symbol.kind));
    }

    let mut best_score = fuzzy_text_score(&symbol.name, query);
    if let Some(fqn_score) = fuzzy_text_score(&symbol.fqn, query) {
        let qualified_bonus = if query.contains('\\') { 700 } else { 100 };
        best_score = Some(
            best_score
                .unwrap_or(i64::MIN)
                .max(fqn_score + qualified_bonus),
        );
    }
    if let Some(container) = workspace_symbol_container_name(symbol) {
        if container
            .to_ascii_lowercase()
            .contains(&query.to_ascii_lowercase())
        {
            best_score = Some(best_score.unwrap_or(i64::MIN).max(5_500));
        }
    }

    Some(best_score? + workspace_symbol_kind_bonus(symbol.kind))
}

fn fuzzy_text_score(text: &str, query: &str) -> Option<i64> {
    let text_lower = text.to_ascii_lowercase();
    let query_lower = query.to_ascii_lowercase();
    if query_lower.is_empty() {
        return Some(1_000);
    }
    if text_lower == query_lower {
        return Some(10_000);
    }
    if text_lower.starts_with(&query_lower) {
        return Some(9_000 - text_lower.len().saturating_sub(query_lower.len()) as i64);
    }
    if let Some(index) = text_lower.find(&query_lower) {
        return Some(7_000 - (index as i64 * 10));
    }

    fuzzy_abbreviation_score(&text_lower, &query_lower)
}

fn fuzzy_abbreviation_score(text: &str, query: &str) -> Option<i64> {
    let mut score = 4_000i64;
    let mut last_match_index: Option<usize> = None;
    let mut search_from = 0usize;

    for query_char in query.chars() {
        let relative_index = text[search_from..].find(query_char)?;
        let absolute_index = search_from + relative_index;
        if let Some(last_match_index) = last_match_index {
            let gap = absolute_index.saturating_sub(last_match_index + 1);
            score -= gap as i64 * 8;
        } else {
            score -= absolute_index as i64 * 4;
        }
        if absolute_index == 0
            || text[..absolute_index]
                .chars()
                .last()
                .is_some_and(|ch| ch == '\\' || ch == '_' || ch == '-' || ch.is_whitespace())
        {
            score += 80;
        }
        last_match_index = Some(absolute_index);
        search_from = absolute_index + query_char.len_utf8();
    }

    Some(score - text.len() as i64)
}

fn workspace_symbol_kind_bonus(kind: php_lsp_types::PhpSymbolKind) -> i64 {
    match kind {
        php_lsp_types::PhpSymbolKind::Class => 90,
        php_lsp_types::PhpSymbolKind::Enum => 85,
        php_lsp_types::PhpSymbolKind::Interface => 80,
        php_lsp_types::PhpSymbolKind::Trait => 70,
        php_lsp_types::PhpSymbolKind::Function => 60,
        php_lsp_types::PhpSymbolKind::Method => 40,
        php_lsp_types::PhpSymbolKind::Property => 30,
        php_lsp_types::PhpSymbolKind::ClassConstant
        | php_lsp_types::PhpSymbolKind::GlobalConstant
        | php_lsp_types::PhpSymbolKind::EnumCase => 20,
        php_lsp_types::PhpSymbolKind::Namespace => 10,
    }
}

fn workspace_symbol_kind_rank(kind: php_lsp_types::PhpSymbolKind) -> u8 {
    match kind {
        php_lsp_types::PhpSymbolKind::Class => 0,
        php_lsp_types::PhpSymbolKind::Enum => 1,
        php_lsp_types::PhpSymbolKind::Interface => 2,
        php_lsp_types::PhpSymbolKind::Trait => 3,
        php_lsp_types::PhpSymbolKind::Function => 4,
        php_lsp_types::PhpSymbolKind::Method => 5,
        php_lsp_types::PhpSymbolKind::Property => 6,
        php_lsp_types::PhpSymbolKind::ClassConstant
        | php_lsp_types::PhpSymbolKind::GlobalConstant
        | php_lsp_types::PhpSymbolKind::EnumCase => 7,
        php_lsp_types::PhpSymbolKind::Namespace => 8,
    }
}

fn workspace_symbol_container_name(symbol: &php_lsp_types::SymbolInfo) -> Option<String> {
    symbol.parent_fqn.clone().or_else(|| {
        let fqn = &symbol.fqn;
        fqn.rfind('\\').map(|index| fqn[..index].to_string())
    })
}

async fn workspace_symbol_source_for_uri(
    uri_str: &str,
    open_files: &DashMap<String, FileParser>,
    source_cache: &mut HashMap<String, Option<String>>,
) -> Option<String> {
    if let Some(cached) = source_cache.get(uri_str) {
        return cached.clone();
    }

    let source = { open_files.get(uri_str).map(|parser| parser.source()) };
    let source = if source.is_some() {
        source
    } else if let Some(path) = uri_to_path(uri_str) {
        read_file_to_string_blocking(path, "workspace/symbol source read")
            .await
            .ok()
    } else {
        None
    };

    source_cache.insert(uri_str.to_string(), source.clone());
    source
}

async fn workspace_symbol_information(
    symbol: &php_lsp_types::SymbolInfo,
    open_files: &DashMap<String, FileParser>,
    source_cache: &mut HashMap<String, Option<String>>,
) -> Option<SymbolInformation> {
    let uri: Uri = symbol.uri.parse().ok()?;
    let source = workspace_symbol_source_for_uri(&symbol.uri, open_files, source_cache).await;
    let range = workspace_symbol_lsp_range(source.as_deref(), symbol.range);

    #[allow(deprecated)]
    Some(SymbolInformation {
        name: symbol.name.clone(),
        kind: php_kind_to_lsp(symbol.kind),
        tags: if symbol.modifiers.is_deprecated {
            Some(vec![SymbolTag::DEPRECATED])
        } else {
            None
        },
        deprecated: None,
        location: Location { uri, range },
        container_name: workspace_symbol_container_name(symbol),
    })
}

fn workspace_symbol_lsp_range(source: Option<&str>, range: (u32, u32, u32, u32)) -> Range {
    let range = source
        .map(|source| range_byte_to_utf16(source, range))
        .unwrap_or(range);
    Range {
        start: Position::new(range.0, range.1),
        end: Position::new(range.2, range.3),
    }
}

fn shell_escape(value: &str) -> String {
    if cfg!(windows) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn build_formatter_shell_command(template: &str, file_path: &Path) -> String {
    let escaped_file = shell_escape(&file_path.to_string_lossy());
    if template.contains("{file}") {
        template.replace("{file}", &escaped_file)
    } else {
        format!("{} {}", template, escaped_file)
    }
}

fn run_formatter_shell_command(
    command: &str,
    current_dir: Option<&Path>,
    timeout_ms: u64,
) -> std::result::Result<std::process::Output, String> {
    let mut process = if cfg!(windows) {
        let mut command_process = std::process::Command::new("cmd");
        command_process.arg("/C").arg(command);
        command_process
    } else {
        let mut command_process = std::process::Command::new("sh");
        command_process.arg("-c").arg(command);
        command_process
    };

    if let Some(current_dir) = current_dir {
        process.current_dir(current_dir);
    }

    process
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = process
        .spawn()
        .map_err(|err| format!("failed to spawn formatter command: {}", err))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_reader = stdout.map(|mut stdout| {
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = std::io::Read::read_to_end(&mut stdout, &mut buffer);
            buffer
        })
    });
    let stderr_reader = stderr.map(|mut stderr| {
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = std::io::Read::read_to_end(&mut stderr, &mut buffer);
            buffer
        })
    });
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = stdout_reader
                    .map(|reader| reader.join().unwrap_or_default())
                    .unwrap_or_default();
                let stderr = stderr_reader
                    .map(|reader| reader.join().unwrap_or_default())
                    .unwrap_or_default();
                return Ok(std::process::Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            Ok(None) => {
                if started.elapsed() >= Duration::from_millis(timeout_ms) {
                    let _ = child.kill();
                    let _ = child.wait();
                    if let Some(reader) = stdout_reader {
                        let _ = reader.join();
                    }
                    if let Some(reader) = stderr_reader {
                        let _ = reader.join();
                    }
                    return Err(format!(
                        "formatter command timed out after {}ms",
                        timeout_ms
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => return Err(format!("failed to poll formatter command: {}", err)),
        }
    }
}

fn build_analyzer_shell_command(template: &str, file_path: &Path) -> String {
    let escaped_file = shell_escape(&file_path.to_string_lossy());
    if template.contains("{file}") {
        template.replace("{file}", &escaped_file)
    } else {
        format!("{} {}", template, escaped_file)
    }
}

fn build_phpstan_shell_command(config: &PhpStanConfig, file_path: &Path) -> String {
    let mut template = config.command.clone();
    if let Some(memory_limit) = config.memory_limit.as_deref() {
        if template.contains("{memory_limit}") {
            template = template.replace("{memory_limit}", &shell_escape(memory_limit));
        } else if !template.contains("--memory-limit") {
            template.push_str(" --memory-limit=");
            template.push_str(&shell_escape(memory_limit));
        }
    } else if template.contains("{memory_limit}") {
        template = template.replace("{memory_limit}", "");
    }

    build_analyzer_shell_command(&template, file_path)
}

async fn run_shell_command_with_timeout(
    label: &str,
    command: &str,
    current_dir: Option<&Path>,
    timeout_ms: u64,
    cancellation: Option<OperationCancellationToken>,
) -> std::result::Result<std::process::Output, String> {
    let mut process = if cfg!(windows) {
        let mut command_process = tokio::process::Command::new("cmd");
        command_process.arg("/C").arg(command);
        command_process
    } else {
        let mut command_process = tokio::process::Command::new("sh");
        command_process.arg("-c").arg(command);
        command_process
    };

    if let Some(current_dir) = current_dir {
        process.current_dir(current_dir);
    }

    process
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    process.kill_on_drop(true);
    let child = process
        .spawn()
        .map_err(|err| format!("failed to start {} command: {}", label, err))?;

    let wait = child.wait_with_output();
    tokio::pin!(wait);
    let timeout = tokio::time::sleep(Duration::from_millis(timeout_ms));
    tokio::pin!(timeout);

    let output = if let Some(cancellation) = cancellation {
        tokio::select! {
            result = &mut wait => result,
            _ = &mut timeout => {
                return Err(format!("{} command timed out after {}ms", label, timeout_ms));
            }
            _ = cancellation.cancelled() => {
                return Err(format!("{} command cancelled", label));
            }
        }
    } else {
        tokio::select! {
            result = &mut wait => result,
            _ = &mut timeout => {
                return Err(format!("{} command timed out after {}ms", label, timeout_ms));
            }
        }
    };

    output.map_err(|err| format!("failed to wait for {} command: {}", label, err))
}

fn phpstan_json_message_line(message: &serde_json::Value) -> Option<u32> {
    message
        .get("line")
        .and_then(|value| value.as_u64())
        .and_then(|line| u32::try_from(line).ok())
}

fn phpstan_json_message_u32(message: &serde_json::Value, key: &str) -> Option<u32> {
    message
        .get(key)
        .and_then(|value| value.as_u64())
        .and_then(|value| u32::try_from(value).ok())
}

fn phpstan_file_key_matches(key: &str, target: &Path) -> bool {
    let key_path = PathBuf::from(key);
    if key_path == target {
        return true;
    }

    if let (Ok(key_canonical), Ok(target_canonical)) =
        (key_path.canonicalize(), target.canonicalize())
    {
        return key_canonical == target_canonical;
    }

    false
}

fn phpstan_message_to_diagnostic(message: &serde_json::Value) -> Option<Diagnostic> {
    let raw_message = message.get("message")?.as_str()?;
    let line = phpstan_json_message_line(message).unwrap_or(1).max(1);
    let start_line = line - 1;
    let start_character = phpstan_json_message_u32(message, "column")
        .unwrap_or(1)
        .saturating_sub(1);
    let end_line = phpstan_json_message_u32(message, "endLine")
        .unwrap_or(line)
        .max(1)
        - 1;
    let end_character = phpstan_json_message_u32(message, "endColumn")
        .map(|column| column.saturating_sub(1))
        .unwrap_or(start_character + 1);

    let tip = message
        .get("tip")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let diagnostic_message = if let Some(tip) = tip {
        format!("{}\n\n{}", raw_message, tip)
    } else {
        raw_message.to_string()
    };

    Some(Diagnostic {
        range: Range {
            start: Position::new(start_line, start_character),
            end: Position::new(end_line, end_character),
        },
        severity: Some(DiagnosticSeverity::ERROR),
        code: message
            .get("identifier")
            .and_then(|value| value.as_str())
            .map(|identifier| NumberOrString::String(identifier.to_string())),
        source: Some("phpstan".to_string()),
        message: diagnostic_message,
        ..Default::default()
    })
}

fn parse_phpstan_json_diagnostics(
    stdout: &str,
    file_path: &Path,
) -> std::result::Result<Vec<Diagnostic>, String> {
    let value: serde_json::Value =
        serde_json::from_str(stdout).map_err(|err| format!("invalid PHPStan JSON: {}", err))?;
    let Some(files) = value.get("files").and_then(|files| files.as_object()) else {
        return Ok(vec![]);
    };

    let mut diagnostics = Vec::new();
    for (file_key, file_value) in files {
        if files.len() != 1 && !phpstan_file_key_matches(file_key, file_path) {
            continue;
        }

        let Some(messages) = file_value
            .get("messages")
            .and_then(|value| value.as_array())
        else {
            continue;
        };

        diagnostics.extend(messages.iter().filter_map(phpstan_message_to_diagnostic));
    }

    Ok(diagnostics)
}

async fn run_phpstan_for_file(
    config: PhpStanConfig,
    file_path: PathBuf,
    workspace_root: Option<PathBuf>,
    cancellation: Option<OperationCancellationToken>,
) -> std::result::Result<Vec<Diagnostic>, String> {
    let command = build_phpstan_shell_command(&config, &file_path);
    let output = run_shell_command_with_timeout(
        "PHPStan",
        &command,
        workspace_root.as_deref(),
        config.timeout_ms,
        cancellation,
    )
    .await?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    if stdout.trim().is_empty() {
        if output.status.success() {
            return Ok(vec![]);
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        let details = stderr.trim();
        return Err(if details.is_empty() {
            format!("PHPStan command exited with {}", output.status)
        } else {
            format!("PHPStan command exited with {}: {}", output.status, details)
        });
    }

    parse_phpstan_json_diagnostics(&stdout, &file_path).map_err(|err| {
        if output.status.success() {
            err
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let details = stderr.trim();
            if details.is_empty() {
                format!("{} (exit {})", err, output.status)
            } else {
                format!("{} (exit {}: {})", err, output.status, details)
            }
        }
    })
}

fn psalm_issue_u32(issue: &serde_json::Value, key: &str) -> Option<u32> {
    issue
        .get(key)
        .and_then(|value| value.as_u64())
        .and_then(|value| u32::try_from(value).ok())
}

fn psalm_issue_path_matches(issue: &serde_json::Value, target: &Path) -> bool {
    let Some(path) = issue
        .get("file_path")
        .or_else(|| issue.get("file_name"))
        .and_then(|value| value.as_str())
    else {
        return true;
    };

    phpstan_file_key_matches(path, target)
}

fn psalm_severity(issue: &serde_json::Value) -> DiagnosticSeverity {
    match issue
        .get("severity")
        .and_then(|value| value.as_str())
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("info") => DiagnosticSeverity::INFORMATION,
        Some("warning") => DiagnosticSeverity::WARNING,
        Some("error") | None => DiagnosticSeverity::ERROR,
        _ => DiagnosticSeverity::ERROR,
    }
}

fn psalm_issue_to_diagnostic(issue: &serde_json::Value) -> Option<Diagnostic> {
    let message = issue.get("message")?.as_str()?.to_string();
    let line_from = psalm_issue_u32(issue, "line_from").unwrap_or(1).max(1);
    let line_to = psalm_issue_u32(issue, "line_to")
        .unwrap_or(line_from)
        .max(1);
    let start_character = psalm_issue_u32(issue, "column_from")
        .unwrap_or(1)
        .saturating_sub(1);
    let end_character = psalm_issue_u32(issue, "column_to")
        .map(|column| column.saturating_sub(1))
        .unwrap_or(start_character + 1);

    Some(Diagnostic {
        range: Range {
            start: Position::new(line_from - 1, start_character),
            end: Position::new(line_to - 1, end_character),
        },
        severity: Some(psalm_severity(issue)),
        code: issue
            .get("type")
            .and_then(|value| value.as_str())
            .or_else(|| issue.get("shortcode").and_then(|value| value.as_str()))
            .map(|code| NumberOrString::String(code.to_string())),
        source: Some("psalm".to_string()),
        message,
        ..Default::default()
    })
}

fn parse_psalm_json_diagnostics(
    stdout: &str,
    file_path: &Path,
) -> std::result::Result<Vec<Diagnostic>, String> {
    let value: serde_json::Value =
        serde_json::from_str(stdout).map_err(|err| format!("invalid Psalm JSON: {}", err))?;
    let issues = value
        .as_array()
        .or_else(|| value.get("issues").and_then(|issues| issues.as_array()))
        .or_else(|| value.get("errors").and_then(|errors| errors.as_array()));

    let Some(issues) = issues else {
        return Ok(vec![]);
    };

    Ok(issues
        .iter()
        .filter(|issue| psalm_issue_path_matches(issue, file_path))
        .filter_map(psalm_issue_to_diagnostic)
        .collect())
}

async fn run_psalm_for_file(
    config: PsalmConfig,
    file_path: PathBuf,
    workspace_root: Option<PathBuf>,
    cancellation: Option<OperationCancellationToken>,
) -> std::result::Result<Vec<Diagnostic>, String> {
    let command = build_analyzer_shell_command(&config.command, &file_path);
    let output = run_shell_command_with_timeout(
        "Psalm",
        &command,
        workspace_root.as_deref(),
        config.timeout_ms,
        cancellation,
    )
    .await?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    if stdout.trim().is_empty() {
        if output.status.success() {
            return Ok(vec![]);
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        let details = stderr.trim();
        return Err(if details.is_empty() {
            format!("Psalm command exited with {}", output.status)
        } else {
            format!("Psalm command exited with {}: {}", output.status, details)
        });
    }

    parse_psalm_json_diagnostics(&stdout, &file_path).map_err(|err| {
        if output.status.success() {
            err
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let details = stderr.trim();
            if details.is_empty() {
                format!("{} (exit {})", err, output.status)
            } else {
                format!("{} (exit {}: {})", err, output.status, details)
            }
        }
    })
}

fn temp_format_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!("php-lsp-format-{}-{}", std::process::id(), nanos))
}

fn run_external_formatter(
    source: String,
    config: FormattingConfig,
    workspace_root: Option<PathBuf>,
) -> std::result::Result<Option<String>, String> {
    let Some(template) = config.command_template() else {
        return Ok(None);
    };

    let temp_dir = temp_format_dir();
    let file_path = temp_dir.join("input.php");
    std::fs::create_dir_all(&temp_dir)
        .map_err(|err| format!("failed to create formatter temp dir: {}", err))?;
    std::fs::write(&file_path, &source)
        .map_err(|err| format!("failed to write formatter temp file: {}", err))?;

    let command = build_formatter_shell_command(&template, &file_path);
    let output =
        run_formatter_shell_command(&command, workspace_root.as_deref(), config.timeout_ms)
            .map_err(|err| format!("failed to run formatter command: {}", err));
    let formatted = std::fs::read_to_string(&file_path)
        .map_err(|err| format!("failed to read formatter temp file: {}", err));
    let _ = std::fs::remove_dir_all(&temp_dir);

    let output = output?;
    let formatted = formatted?;

    if !output.status.success() && formatted == source {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let details = if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else {
            stdout.trim().to_string()
        };
        return Err(if details.is_empty() {
            format!("formatter command exited with {}", output.status)
        } else {
            format!(
                "formatter command exited with {}: {}",
                output.status, details
            )
        });
    }

    if formatted == source {
        Ok(None)
    } else {
        Ok(Some(formatted))
    }
}

fn range_formatter_input(fragment: &str) -> (String, bool) {
    if fragment.trim_start().starts_with("<?php") {
        (fragment.to_string(), false)
    } else {
        (format!("<?php\n{}", fragment), true)
    }
}

fn strip_range_formatter_wrapper(formatted: String, was_wrapped: bool) -> String {
    if !was_wrapped {
        return formatted;
    }

    formatted
        .strip_prefix("<?php\n")
        .or_else(|| formatted.strip_prefix("<?php\r\n"))
        .unwrap_or(&formatted)
        .to_string()
}

fn formatting_source_line(source: &str, line: u32) -> Option<&str> {
    source.split('\n').nth(line as usize)
}

fn leading_indent(line: &str) -> &str {
    let indent_end = line
        .char_indices()
        .find(|(_, ch)| !matches!(ch, ' ' | '\t'))
        .map(|(idx, _)| idx)
        .unwrap_or(line.len());
    &line[..indent_end]
}

fn utf16_len(text: &str) -> u32 {
    text.chars().map(|ch| ch.len_utf16() as u32).sum()
}

fn formatting_indent_unit(options: &FormattingOptions) -> String {
    if options.insert_spaces {
        " ".repeat(options.tab_size.max(1) as usize)
    } else {
        "\t".to_string()
    }
}

fn brace_delta(line: &str) -> isize {
    let mut delta = 0isize;
    for ch in line.chars() {
        match ch {
            '{' => delta += 1,
            '}' => delta -= 1,
            _ => {}
        }
    }
    delta
}

fn brace_depth_before_line(source: &str, line: u32) -> usize {
    let mut depth = 0isize;
    for row in source.split('\n').take(line as usize) {
        depth = (depth + brace_delta(row)).max(0);
    }
    depth as usize
}

fn on_type_indent_edit(source: &str, line: u32, options: &FormattingOptions) -> Option<TextEdit> {
    let current_line = formatting_source_line(source, line)?;
    let current_indent = leading_indent(current_line);
    let trimmed = current_line.trim_start_matches([' ', '\t']);
    let mut depth = brace_depth_before_line(source, line);
    if trimmed.starts_with('}') {
        depth = depth.saturating_sub(1);
    }

    let desired_indent = formatting_indent_unit(options).repeat(depth);
    if desired_indent == current_indent {
        return None;
    }

    Some(TextEdit {
        range: Range {
            start: Position::new(line, 0),
            end: Position::new(line, utf16_len(current_indent)),
        },
        new_text: desired_indent,
    })
}

fn lsp_position_to_byte(source: &str, position: Position) -> Option<usize> {
    let byte_col = utf16_col_to_byte(source, position.line, position.character) as usize;
    let mut offset = 0usize;

    for (current_line, row) in source.split_inclusive('\n').enumerate() {
        if current_line as u32 == position.line {
            return Some(offset + byte_col.min(row.len()));
        }
        offset += row.len();
    }

    if position.line as usize == source.lines().count() {
        Some(source.len())
    } else {
        None
    }
}

fn text_at_lsp_range(source: &str, range: Range) -> Option<&str> {
    let start = lsp_position_to_byte(source, range.start)?;
    let end = lsp_position_to_byte(source, range.end)?;
    source.get(start..end)
}

fn build_add_import_edit(
    uri: Uri,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    import_fqn: &str,
    import_kind: ImportKind,
    diagnostic_range: Range,
) -> Option<(WorkspaceEdit, Option<String>)> {
    if let Some(existing) = existing_import_for_fqn(file_symbols, import_fqn, import_kind) {
        if let Some(alias) = existing.alias.clone() {
            let edit = TextEdit {
                range: diagnostic_range,
                new_text: alias.clone(),
            };
            let mut changes = std::collections::HashMap::new();
            changes.insert(uri, vec![edit]);
            return Some((
                WorkspaceEdit {
                    changes: Some(changes),
                    document_changes: None,
                    change_annotations: None,
                },
                Some(alias),
            ));
        }
        return None;
    }

    let import_short_name = short_name(import_fqn);
    let used_aliases = used_import_aliases(file_symbols, import_kind);
    let alias = if used_aliases.contains(import_short_name) {
        Some(unique_import_alias(import_short_name, &used_aliases))
    } else {
        None
    };

    let insert_line = find_use_insert_line(source, file_symbols);
    let needs_spacing =
        file_symbols.use_statements.is_empty() && !line_is_blank(source, insert_line);
    let mut import_text = build_use_statement(import_fqn, import_kind, alias.as_deref());
    import_text.push('\n');
    if needs_spacing {
        import_text.push('\n');
    }

    let mut edits = vec![TextEdit {
        range: Range {
            start: Position::new(insert_line, 0),
            end: Position::new(insert_line, 0),
        },
        new_text: import_text,
    }];

    let replacement_name = alias.as_deref().unwrap_or(import_short_name);
    if alias.is_some()
        || text_at_lsp_range(source, diagnostic_range)
            .map(|text| text.trim_start_matches('\\') != replacement_name)
            .unwrap_or(false)
    {
        edits.push(TextEdit {
            range: diagnostic_range,
            new_text: replacement_name.to_string(),
        });
    }

    let mut changes = std::collections::HashMap::new();
    changes.insert(uri, edits);
    Some((
        WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        },
        alias,
    ))
}

fn range_overlaps(a: Range, b: Range) -> bool {
    a.start <= b.end && b.start <= a.end
}

fn byte_ranges_overlap(left: (u32, u32, u32, u32), right: (u32, u32, u32, u32)) -> bool {
    (left.0, left.1) <= (right.2, right.3) && (right.0, right.1) <= (left.2, left.3)
}

fn inlay_hints(
    tree: &tree_sitter::Tree,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    requested_range: Range,
    php_version: PhpVersion,
) -> Vec<InlayHint> {
    let utf16_index = Utf16LineIndex::new(source);
    let byte_range = lsp_range_to_byte_range(source, requested_range);
    let mut hints = Vec::new();
    let ctx = InlayHintContext {
        tree,
        source,
        file_symbols,
        index,
        utf16_index: &utf16_index,
        requested_range: byte_range,
    };

    collect_call_argument_inlay_hints(&ctx, tree.root_node(), &mut hints);
    collect_phpdoc_parameter_type_inlay_hints(
        tree.root_node(),
        source,
        &utf16_index,
        byte_range,
        &mut hints,
    );
    collect_phpdoc_return_type_inlay_hints(
        tree,
        source,
        &utf16_index,
        byte_range,
        php_version,
        &mut hints,
    );

    hints.sort_by(|left, right| {
        (
            left.position.line,
            left.position.character,
            inlay_hint_label_text(&left.label),
        )
            .cmp(&(
                right.position.line,
                right.position.character,
                inlay_hint_label_text(&right.label),
            ))
    });
    hints
}

struct InlayHintContext<'a> {
    tree: &'a tree_sitter::Tree,
    source: &'a str,
    file_symbols: &'a php_lsp_types::FileSymbols,
    index: &'a WorkspaceIndex,
    utf16_index: &'a Utf16LineIndex,
    requested_range: (u32, u32, u32, u32),
}

fn collect_call_argument_inlay_hints(
    ctx: &InlayHintContext<'_>,
    node: tree_sitter::Node,
    hints: &mut Vec<InlayHint>,
) {
    if matches!(
        node.kind(),
        "function_call_expression"
            | "member_call_expression"
            | "scoped_call_expression"
            | "object_creation_expression"
    ) {
        if let Some(callable) =
            resolve_callable_for_inlay_hint(ctx.tree, node, ctx.source, ctx.file_symbols, ctx.index)
        {
            add_call_argument_inlay_hints(
                node,
                &callable,
                ctx.source,
                ctx.utf16_index,
                ctx.requested_range,
                hints,
            );
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_call_argument_inlay_hints(ctx, child, hints);
    }
}

fn resolve_callable_for_inlay_hint(
    tree: &tree_sitter::Tree,
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
) -> Option<Arc<php_lsp_types::SymbolInfo>> {
    let name_node = call_target_name_node(node)?;
    let (_, sym) = resolve_reference_symbol_at_node(tree, source, name_node, file_symbols, index)?;
    matches!(
        sym.kind,
        php_lsp_types::PhpSymbolKind::Function | php_lsp_types::PhpSymbolKind::Method
    )
    .then_some(sym)
}

fn call_target_name_node(node: tree_sitter::Node) -> Option<tree_sitter::Node> {
    match node.kind() {
        "function_call_expression" => node
            .child_by_field_name("function")
            .or_else(|| node.named_child(0)),
        "member_call_expression" | "scoped_call_expression" => member_reference_name_node(node),
        "object_creation_expression" => object_creation_class_node(node),
        _ => None,
    }
}

fn add_call_argument_inlay_hints(
    call_node: tree_sitter::Node,
    callable: &php_lsp_types::SymbolInfo,
    source: &str,
    utf16_index: &Utf16LineIndex,
    requested_range: (u32, u32, u32, u32),
    hints: &mut Vec<InlayHint>,
) {
    let Some(signature) = callable.signature.as_ref() else {
        return;
    };

    for (arg_index, argument) in call_arguments(call_node, source).into_iter().enumerate() {
        if argument.name.is_some() {
            continue;
        }
        let Some(param) = signature_param_for_arg(signature, arg_index) else {
            continue;
        };
        if param.name.is_empty() {
            continue;
        }
        let arg_range = node_range_node(argument.value_node);
        if !byte_ranges_overlap(arg_range, requested_range) {
            continue;
        }
        let start = argument.value_node.start_position();
        hints.push(InlayHint {
            position: Position::new(
                start.row as u32,
                utf16_index.byte_col_to_utf16(start.row as u32, start.column as u32),
            ),
            label: InlayHintLabel::String(format!("{}:", param.name)),
            kind: Some(InlayHintKind::PARAMETER),
            text_edits: None,
            tooltip: Some(InlayHintTooltip::String(callable.fqn.clone())),
            padding_left: Some(false),
            padding_right: Some(true),
            data: None,
        });
    }
}

fn collect_phpdoc_parameter_type_inlay_hints(
    node: tree_sitter::Node,
    source: &str,
    utf16_index: &Utf16LineIndex,
    requested_range: (u32, u32, u32, u32),
    hints: &mut Vec<InlayHint>,
) {
    if matches!(node.kind(), "function_definition" | "method_declaration") {
        add_phpdoc_parameter_type_inlay_hints(node, source, utf16_index, requested_range, hints);
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_phpdoc_parameter_type_inlay_hints(
            child,
            source,
            utf16_index,
            requested_range,
            hints,
        );
    }
}

fn add_phpdoc_parameter_type_inlay_hints(
    node: tree_sitter::Node,
    source: &str,
    utf16_index: &Utf16LineIndex,
    requested_range: (u32, u32, u32, u32),
    hints: &mut Vec<InlayHint>,
) {
    let Some(doc_comment) = doc_comment_before_node(node, source) else {
        return;
    };
    let phpdoc = parse_phpdoc(&doc_comment);
    if phpdoc.params.is_empty() {
        return;
    }

    let Some(parameters) = node.child_by_field_name("parameters") else {
        return;
    };
    let mut cursor = parameters.walk();
    for parameter in parameters.named_children(&mut cursor) {
        if !matches!(
            parameter.kind(),
            "simple_parameter" | "variadic_parameter" | "property_promotion_parameter"
        ) || parameter.child_by_field_name("type").is_some()
        {
            continue;
        }
        let Some(name_node) = parameter.child_by_field_name("name") else {
            continue;
        };
        if !byte_ranges_overlap(node_range_node(name_node), requested_range) {
            continue;
        }
        let raw_name = node_text(source, name_node);
        let name = raw_name.trim_start_matches('$');
        let Some(param_doc) = phpdoc.params.iter().find(|param| param.name == name) else {
            continue;
        };
        let Some(type_info) = param_doc.type_info.as_ref() else {
            continue;
        };
        let end = name_node.end_position();
        hints.push(InlayHint {
            position: Position::new(
                end.row as u32,
                utf16_index.byte_col_to_utf16(end.row as u32, end.column as u32),
            ),
            label: InlayHintLabel::String(format!(": {}", type_info)),
            kind: Some(InlayHintKind::TYPE),
            text_edits: None,
            tooltip: Some(InlayHintTooltip::String("PHPDoc @param".to_string())),
            padding_left: Some(false),
            padding_right: Some(false),
            data: None,
        });
    }
}

fn collect_phpdoc_return_type_inlay_hints(
    tree: &tree_sitter::Tree,
    source: &str,
    utf16_index: &Utf16LineIndex,
    requested_range: (u32, u32, u32, u32),
    php_version: PhpVersion,
    hints: &mut Vec<InlayHint>,
) {
    for candidate in find_missing_return_type_candidates(tree, source, requested_range) {
        let label = return_type_hint(&candidate.return_type, php_version)
            .unwrap_or_else(|| candidate.return_type.to_string());
        hints.push(InlayHint {
            position: Position::new(
                candidate.insert_position.0,
                utf16_index
                    .byte_col_to_utf16(candidate.insert_position.0, candidate.insert_position.1),
            ),
            label: InlayHintLabel::String(format!(": {}", label)),
            kind: Some(InlayHintKind::TYPE),
            text_edits: None,
            tooltip: Some(InlayHintTooltip::String("PHPDoc @return".to_string())),
            padding_left: Some(false),
            padding_right: Some(false),
            data: None,
        });
    }
}

fn doc_comment_before_node(node: tree_sitter::Node, source: &str) -> Option<String> {
    let mut prev = node.prev_sibling();
    while let Some(sibling) = prev {
        if sibling.kind() == "comment" {
            let text = node_text(source, sibling);
            if text.starts_with("/**") {
                return Some(text.to_string());
            }
            return None;
        }
        prev = sibling.prev_sibling();
    }
    None
}

fn inlay_hint_label_text(label: &InlayHintLabel) -> String {
    match label {
        InlayHintLabel::String(value) => value.clone(),
        InlayHintLabel::LabelParts(parts) => parts.iter().map(|part| part.value.as_str()).collect(),
    }
}

fn is_call_hierarchy_symbol_kind(kind: php_lsp_types::PhpSymbolKind) -> bool {
    matches!(
        kind,
        php_lsp_types::PhpSymbolKind::Function | php_lsp_types::PhpSymbolKind::Method
    )
}

fn is_call_hierarchy_ref_kind(ref_kind: RefKind) -> bool {
    matches!(
        ref_kind,
        RefKind::FunctionCall | RefKind::MethodCall | RefKind::Constructor
    )
}

fn call_hierarchy_item_from_symbol(sym: &php_lsp_types::SymbolInfo) -> Option<CallHierarchyItem> {
    let uri = sym.uri.parse::<Uri>().ok()?;
    Some(CallHierarchyItem {
        name: sym.name.clone(),
        kind: php_kind_to_lsp(sym.kind),
        tags: sym
            .modifiers
            .is_deprecated
            .then_some(vec![SymbolTag::DEPRECATED]),
        detail: Some(call_hierarchy_detail(sym)),
        uri,
        range: range_from_tuple(sym.range),
        selection_range: range_from_tuple(sym.selection_range),
        data: Some(serde_json::json!({
            "fqn": sym.fqn,
            "kind": call_hierarchy_kind_key(sym.kind),
        })),
    })
}

fn call_hierarchy_detail(sym: &php_lsp_types::SymbolInfo) -> String {
    if let Some(signature) = sym.signature.as_ref() {
        let params: Vec<String> = signature
            .params
            .iter()
            .map(format_signature_param)
            .collect();
        let mut detail = format!("{}({})", sym.fqn, params.join(", "));
        if let Some(return_type) = signature.return_type.as_ref() {
            detail.push_str(": ");
            detail.push_str(&return_type.to_string());
        }
        detail
    } else {
        sym.fqn.clone()
    }
}

fn call_hierarchy_kind_key(kind: php_lsp_types::PhpSymbolKind) -> &'static str {
    match kind {
        php_lsp_types::PhpSymbolKind::Function => "function",
        php_lsp_types::PhpSymbolKind::Method => "method",
        php_lsp_types::PhpSymbolKind::Class => "class",
        php_lsp_types::PhpSymbolKind::Interface => "interface",
        php_lsp_types::PhpSymbolKind::Trait => "trait",
        php_lsp_types::PhpSymbolKind::Enum => "enum",
        php_lsp_types::PhpSymbolKind::Property => "property",
        php_lsp_types::PhpSymbolKind::ClassConstant => "class_constant",
        php_lsp_types::PhpSymbolKind::GlobalConstant => "global_constant",
        php_lsp_types::PhpSymbolKind::EnumCase => "enum_case",
        php_lsp_types::PhpSymbolKind::Namespace => "namespace",
    }
}

fn call_hierarchy_kind_from_key(raw: &str) -> Option<php_lsp_types::PhpSymbolKind> {
    match raw {
        "function" => Some(php_lsp_types::PhpSymbolKind::Function),
        "method" => Some(php_lsp_types::PhpSymbolKind::Method),
        "class" => Some(php_lsp_types::PhpSymbolKind::Class),
        "interface" => Some(php_lsp_types::PhpSymbolKind::Interface),
        "trait" => Some(php_lsp_types::PhpSymbolKind::Trait),
        "enum" => Some(php_lsp_types::PhpSymbolKind::Enum),
        "property" => Some(php_lsp_types::PhpSymbolKind::Property),
        "class_constant" => Some(php_lsp_types::PhpSymbolKind::ClassConstant),
        "global_constant" => Some(php_lsp_types::PhpSymbolKind::GlobalConstant),
        "enum_case" => Some(php_lsp_types::PhpSymbolKind::EnumCase),
        "namespace" => Some(php_lsp_types::PhpSymbolKind::Namespace),
        _ => None,
    }
}

fn is_type_hierarchy_symbol_kind(kind: php_lsp_types::PhpSymbolKind) -> bool {
    matches!(
        kind,
        php_lsp_types::PhpSymbolKind::Class
            | php_lsp_types::PhpSymbolKind::Interface
            | php_lsp_types::PhpSymbolKind::Trait
            | php_lsp_types::PhpSymbolKind::Enum
    )
}

fn type_hierarchy_item_from_symbol(sym: &php_lsp_types::SymbolInfo) -> Option<TypeHierarchyItem> {
    if !is_type_hierarchy_symbol_kind(sym.kind) {
        return None;
    }
    let uri = sym.uri.parse::<Uri>().ok()?;
    Some(TypeHierarchyItem {
        name: sym.name.clone(),
        kind: php_kind_to_lsp(sym.kind),
        tags: sym.modifiers.is_deprecated.then_some(SymbolTag::DEPRECATED),
        detail: Some(sym.fqn.clone()),
        uri,
        range: range_from_tuple(sym.range),
        selection_range: range_from_tuple(sym.selection_range),
        data: Some(serde_json::json!({
            "fqn": sym.fqn,
            "kind": call_hierarchy_kind_key(sym.kind),
        })),
    })
}

fn is_code_lens_symbol_kind(kind: php_lsp_types::PhpSymbolKind) -> bool {
    matches!(
        kind,
        php_lsp_types::PhpSymbolKind::Class
            | php_lsp_types::PhpSymbolKind::Interface
            | php_lsp_types::PhpSymbolKind::Trait
            | php_lsp_types::PhpSymbolKind::Enum
            | php_lsp_types::PhpSymbolKind::Method
    )
}

fn reference_count_title(count: usize) -> String {
    if count == 1 {
        "1 reference".to_string()
    } else {
        format!("{} references", count)
    }
}

fn type_hierarchy_symbol_from_item(
    index: &WorkspaceIndex,
    item: &TypeHierarchyItem,
) -> Option<Arc<php_lsp_types::SymbolInfo>> {
    if let Some(data) = item.data.as_ref() {
        if let Some(fqn) = data.get("fqn").and_then(|value| value.as_str()) {
            if let Some(sym) = index.resolve_fqn(fqn) {
                if is_type_hierarchy_symbol_kind(sym.kind) {
                    return Some(sym);
                }
            }
        }
    }

    let uri = item.uri.as_str();
    let selection = (
        item.selection_range.start.line,
        item.selection_range.start.character,
        item.selection_range.end.line,
        item.selection_range.end.character,
    );
    index.file_symbols.get(uri).and_then(|file_symbols| {
        file_symbols
            .symbols
            .iter()
            .find(|sym| {
                sym.name == item.name
                    && sym.selection_range == selection
                    && is_type_hierarchy_symbol_kind(sym.kind)
            })
            .cloned()
            .map(Arc::new)
    })
}

fn direct_type_subtypes(
    index: &WorkspaceIndex,
    target_fqn: &str,
) -> Vec<Arc<php_lsp_types::SymbolInfo>> {
    let mut seen = HashSet::new();
    let mut subtypes = Vec::new();

    for entry in index.types.iter() {
        let sym = entry.value().clone();
        if !is_type_hierarchy_symbol_kind(sym.kind) || sym.fqn == target_fqn {
            continue;
        }
        let matches_target = sym
            .extends
            .iter()
            .chain(sym.implements.iter())
            .any(|parent| parent == target_fqn);
        if matches_target && seen.insert(sym.fqn.clone()) {
            subtypes.push(sym);
        }
    }

    subtypes.sort_by(|left, right| left.fqn.cmp(&right.fqn));
    subtypes
}

fn direct_type_parent_fqns(sym: &php_lsp_types::SymbolInfo) -> Vec<String> {
    let mut seen = HashSet::new();
    sym.extends
        .iter()
        .chain(sym.implements.iter())
        .filter_map(|fqn| seen.insert(fqn.clone()).then_some(fqn.clone()))
        .collect()
}

fn symbol_location(sym: &php_lsp_types::SymbolInfo) -> Option<Location> {
    Some(Location {
        uri: sym.uri.parse::<Uri>().ok()?,
        range: range_from_tuple(sym.selection_range),
    })
}

fn direct_symbol_by_fqn(
    index: &WorkspaceIndex,
    fqn: &str,
) -> Option<Arc<php_lsp_types::SymbolInfo>> {
    index.file_symbols.iter().find_map(|entry| {
        entry
            .value()
            .symbols
            .iter()
            .find(|sym| sym.fqn == fqn)
            .cloned()
            .map(Arc::new)
    })
}

fn implementation_type_descendants(
    index: &WorkspaceIndex,
    target_fqn: &str,
) -> Vec<Arc<php_lsp_types::SymbolInfo>> {
    let mut visited = HashSet::new();
    let mut result = Vec::new();
    collect_implementation_type_descendants(index, target_fqn, &mut visited, &mut result);
    result.sort_by(|left, right| left.fqn.cmp(&right.fqn));
    result
}

fn collect_implementation_type_descendants(
    index: &WorkspaceIndex,
    target_fqn: &str,
    visited: &mut HashSet<String>,
    result: &mut Vec<Arc<php_lsp_types::SymbolInfo>>,
) {
    if !visited.insert(target_fqn.to_string()) {
        return;
    }

    for subtype in direct_type_subtypes(index, target_fqn) {
        if matches!(
            subtype.kind,
            php_lsp_types::PhpSymbolKind::Class | php_lsp_types::PhpSymbolKind::Enum
        ) {
            result.push(subtype.clone());
        }
        collect_implementation_type_descendants(index, &subtype.fqn, visited, result);
    }
}

fn implementation_locations_for_type(
    index: &WorkspaceIndex,
    target: &php_lsp_types::SymbolInfo,
) -> Vec<Location> {
    implementation_type_descendants(index, &target.fqn)
        .into_iter()
        .filter(|sym| !sym.modifiers.is_abstract)
        .filter_map(|sym| symbol_location(&sym))
        .collect()
}

fn implementation_locations_for_method(
    index: &WorkspaceIndex,
    target: &php_lsp_types::SymbolInfo,
) -> Vec<Location> {
    let Some(parent_fqn) = target.parent_fqn.as_deref() else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut locations = Vec::new();

    for subtype in implementation_type_descendants(index, parent_fqn) {
        let member_fqn = format!("{}::{}", subtype.fqn, target.name);
        let Some(method) = direct_symbol_by_fqn(index, &member_fqn) else {
            continue;
        };
        if method.kind != php_lsp_types::PhpSymbolKind::Method || method.fqn == target.fqn {
            continue;
        }
        if seen.insert(method.fqn.clone()) {
            if let Some(location) = symbol_location(&method) {
                locations.push(location);
            }
        }
    }

    locations.sort_by(|left, right| {
        (
            left.uri.as_str(),
            left.range.start.line,
            left.range.start.character,
        )
            .cmp(&(
                right.uri.as_str(),
                right.range.start.line,
                right.range.start.character,
            ))
    });
    locations
}

fn call_hierarchy_symbol_from_item(
    index: &WorkspaceIndex,
    item: &CallHierarchyItem,
) -> Option<Arc<php_lsp_types::SymbolInfo>> {
    if let Some(data) = item.data.as_ref() {
        if let Some(fqn) = data.get("fqn").and_then(|value| value.as_str()) {
            if let Some(sym) = index.resolve_fqn(fqn) {
                return Some(sym);
            }
        }
    }

    let uri = item.uri.as_str();
    let selection = (
        item.selection_range.start.line,
        item.selection_range.start.character,
        item.selection_range.end.line,
        item.selection_range.end.character,
    );
    index.file_symbols.get(uri).and_then(|file_symbols| {
        file_symbols
            .symbols
            .iter()
            .find(|sym| {
                sym.name == item.name
                    && sym.selection_range == selection
                    && is_call_hierarchy_symbol_kind(sym.kind)
            })
            .cloned()
            .map(Arc::new)
    })
}

fn call_hierarchy_target_from_item(
    index: &WorkspaceIndex,
    item: &CallHierarchyItem,
) -> Option<(Arc<php_lsp_types::SymbolInfo>, php_lsp_types::PhpSymbolKind)> {
    let sym = call_hierarchy_symbol_from_item(index, item)?;
    let kind = item
        .data
        .as_ref()
        .and_then(|data| data.get("kind"))
        .and_then(|value| value.as_str())
        .and_then(call_hierarchy_kind_from_key)
        .unwrap_or(sym.kind);
    Some((sym, kind))
}

fn range_from_tuple(range: (u32, u32, u32, u32)) -> Range {
    Range {
        start: Position::new(range.0, range.1),
        end: Position::new(range.2, range.3),
    }
}

fn range_from_byte_range(source: &str, range: (u32, u32, u32, u32)) -> Range {
    range_from_tuple(range_byte_to_utf16(source, range))
}

fn is_document_link_include_expression(kind: &str) -> bool {
    matches!(
        kind,
        "include_expression"
            | "include_once_expression"
            | "require_expression"
            | "require_once_expression"
    )
}

fn is_static_string_literal_node(node: tree_sitter::Node) -> bool {
    if !matches!(node.kind(), "string" | "encapsed_string") {
        return false;
    }

    let mut cursor = node.walk();
    let is_static = node
        .named_children(&mut cursor)
        .all(|child| matches!(child.kind(), "string_content" | "escape_sequence"));
    is_static
}

fn unescape_static_php_string(content: &str, quote: char) -> String {
    let mut result = String::with_capacity(content.len());
    let mut chars = content.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            result.push(ch);
            continue;
        }

        let Some(escaped) = chars.next() else {
            result.push('\\');
            break;
        };

        if quote == '\'' {
            match escaped {
                '\\' | '\'' => result.push(escaped),
                other => {
                    result.push('\\');
                    result.push(other);
                }
            }
            continue;
        }

        match escaped {
            'n' => result.push('\n'),
            'r' => result.push('\r'),
            't' => result.push('\t'),
            'v' => result.push('\u{000b}'),
            'e' => result.push('\u{001b}'),
            'f' => result.push('\u{000c}'),
            '\\' | '$' | '"' => result.push(escaped),
            other => {
                result.push('\\');
                result.push(other);
            }
        }
    }
    result
}

fn static_string_literal_value(source: &str, node: tree_sitter::Node) -> Option<String> {
    if !is_static_string_literal_node(node) {
        return None;
    }

    let raw = node_text(source, node).trim();
    let mut chars = raw.char_indices();
    let (start_idx, first) = chars.next()?;
    let (quote_start, quote) = if matches!(first, 'b' | 'B') {
        let (idx, ch) = chars.next()?;
        (idx, ch)
    } else {
        (start_idx, first)
    };

    if !matches!(quote, '\'' | '"') || !raw.ends_with(quote) {
        return None;
    }

    let content_start = quote_start + quote.len_utf8();
    let content_end = raw.len().checked_sub(quote.len_utf8())?;
    if content_start > content_end {
        return None;
    }

    Some(unescape_static_php_string(
        &raw[content_start..content_end],
        quote,
    ))
}

fn binary_expression_is_concat(source: &str, node: tree_sitter::Node) -> bool {
    let Some(left) = node
        .child_by_field_name("left")
        .or_else(|| node.named_child(0))
    else {
        return false;
    };
    let Some(right) = node
        .child_by_field_name("right")
        .or_else(|| node.named_child(1))
    else {
        return false;
    };

    source
        .get(left.end_byte()..right.start_byte())
        .is_some_and(|operator| operator.contains('.'))
}

fn first_call_argument_node(node: tree_sitter::Node) -> Option<tree_sitter::Node> {
    let arguments = node.child_by_field_name("arguments").or_else(|| {
        let mut cursor = node.walk();
        let arguments = node
            .named_children(&mut cursor)
            .find(|child| child.kind() == "arguments");
        arguments
    })?;

    let mut cursor = arguments.walk();
    let first = arguments.named_children(&mut cursor).find_map(|argument| {
        argument
            .child_by_field_name("value")
            .or_else(|| argument.named_child(0))
            .or(Some(argument))
    });
    first
}

fn static_include_expression_value(
    source: &str,
    node: tree_sitter::Node,
    file_path: &Path,
    file_dir: &Path,
) -> Option<String> {
    match node.kind() {
        "string" | "encapsed_string" => static_string_literal_value(source, node),
        "binary_expression" if binary_expression_is_concat(source, node) => {
            let left = node
                .child_by_field_name("left")
                .or_else(|| node.named_child(0))?;
            let right = node
                .child_by_field_name("right")
                .or_else(|| node.named_child(1))?;
            let mut value = static_include_expression_value(source, left, file_path, file_dir)?;
            value.push_str(&static_include_expression_value(
                source, right, file_path, file_dir,
            )?);
            Some(value)
        }
        "parenthesized_expression" => {
            let inner = node.named_child(0)?;
            static_include_expression_value(source, inner, file_path, file_dir)
        }
        "function_call_expression" => {
            let function = node
                .child_by_field_name("function")
                .or_else(|| node.named_child(0))?;
            if !node_text(source, function).eq_ignore_ascii_case("dirname") {
                return None;
            }
            let argument = first_call_argument_node(node)?;
            let value = static_include_expression_value(source, argument, file_path, file_dir)?;
            Path::new(&value)
                .parent()
                .map(|parent| parent.to_string_lossy().into_owned())
        }
        _ => {
            let raw = node_text(source, node).trim();
            if raw.eq_ignore_ascii_case("__DIR__") {
                Some(file_dir.to_string_lossy().into_owned())
            } else if raw.eq_ignore_ascii_case("__FILE__") {
                Some(file_path.to_string_lossy().into_owned())
            } else {
                None
            }
        }
    }
}

fn document_link_target_path(
    source: &str,
    expression: tree_sitter::Node,
    file_path: &Path,
    file_dir: &Path,
) -> Option<PathBuf> {
    let raw_path = static_include_expression_value(source, expression, file_path, file_dir)?;
    let path = PathBuf::from(raw_path);
    let path = if path.is_absolute() {
        path
    } else {
        file_dir.join(path)
    };
    path.is_file().then_some(path)
}

fn collect_document_links(
    node: tree_sitter::Node,
    source: &str,
    file_path: &Path,
    file_dir: &Path,
    links: &mut Vec<DocumentLink>,
) {
    if is_document_link_include_expression(node.kind()) {
        if let Some(expression) = node.named_child(0) {
            if let Some(target_path) =
                document_link_target_path(source, expression, file_path, file_dir)
            {
                if let Ok(target) = path_to_uri(&target_path).parse::<Uri>() {
                    links.push(DocumentLink {
                        range: range_from_byte_range(source, node_byte_range(expression)),
                        target: Some(target),
                        tooltip: Some(target_path.display().to_string()),
                        data: None,
                    });
                }
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_document_links(child, source, file_path, file_dir, links);
    }
}

fn document_links_for_source(
    source: &str,
    tree: &tree_sitter::Tree,
    file_path: &Path,
) -> Vec<DocumentLink> {
    let Some(file_dir) = file_path.parent() else {
        return Vec::new();
    };

    let mut links = Vec::new();
    collect_document_links(tree.root_node(), source, file_path, file_dir, &mut links);
    links
}

fn is_folding_declaration_node(kind: &str) -> bool {
    matches!(
        kind,
        "namespace_definition"
            | "class_declaration"
            | "interface_declaration"
            | "trait_declaration"
            | "enum_declaration"
            | "function_definition"
            | "method_declaration"
            | "anonymous_function_creation_expression"
    )
}

fn is_declaration_parent_for_block(node: tree_sitter::Node) -> bool {
    if node.kind() != "compound_statement" {
        return false;
    }

    node.parent()
        .is_some_and(|parent| is_folding_declaration_node(parent.kind()))
}

fn folding_range_for_node(node: tree_sitter::Node, source: &str) -> Option<FoldingRange> {
    let kind = match node.kind() {
        "comment" => {
            let text = node_text(source, node).trim_start();
            if !text.starts_with("/**") {
                return None;
            }
            Some(FoldingRangeKind::Comment)
        }
        "array_creation_expression" => Some(FoldingRangeKind::Region),
        "compound_statement" if !is_declaration_parent_for_block(node) => {
            Some(FoldingRangeKind::Region)
        }
        kind if is_folding_declaration_node(kind) => None,
        _ => return None,
    };

    let start = node.start_position();
    let end = node.end_position();
    let start_line = start.row as u32;
    let end_line = end.row as u32;
    if end_line <= start_line {
        return None;
    }

    Some(FoldingRange {
        start_line,
        start_character: Some(start.column as u32),
        end_line,
        end_character: Some(end.column as u32),
        kind,
        collapsed_text: None,
    })
}

fn collect_folding_ranges(
    node: tree_sitter::Node,
    source: &str,
    ranges: &mut Vec<FoldingRange>,
    seen: &mut HashSet<(u32, Option<u32>, u32, Option<u32>)>,
) {
    if let Some(range) = folding_range_for_node(node, source) {
        let key = (
            range.start_line,
            range.start_character,
            range.end_line,
            range.end_character,
        );
        if seen.insert(key) {
            ranges.push(range);
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_folding_ranges(child, source, ranges, seen);
    }
}

fn folding_ranges(tree: &tree_sitter::Tree, source: &str) -> Vec<FoldingRange> {
    let mut ranges = Vec::new();
    let mut seen = HashSet::new();
    collect_folding_ranges(tree.root_node(), source, &mut ranges, &mut seen);
    ranges.sort_by_key(|range| {
        (
            range.start_line,
            range.start_character.unwrap_or_default(),
            range.end_line,
            range.end_character.unwrap_or_default(),
        )
    });
    ranges
}

fn incoming_call_hierarchy_for_file(
    tree: &tree_sitter::Tree,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    target_fqn: &str,
    target_kind: php_lsp_types::PhpSymbolKind,
    calls_by_caller: &mut HashMap<String, (php_lsp_types::SymbolInfo, Vec<Range>)>,
) {
    let refs = find_references_in_file(tree, source, file_symbols, target_fqn, target_kind, false);

    for reference in refs {
        let Some(caller) = containing_callable_symbol(file_symbols, reference.range) else {
            continue;
        };
        if caller.fqn == target_fqn {
            continue;
        }

        calls_by_caller
            .entry(caller.fqn.clone())
            .or_insert_with(|| (caller.clone(), Vec::new()))
            .1
            .push(range_from_byte_range(source, reference.range));
    }
}

struct OutgoingCallHierarchyContext<'a> {
    tree: &'a tree_sitter::Tree,
    source: &'a str,
    file_symbols: &'a php_lsp_types::FileSymbols,
    index: &'a WorkspaceIndex,
    caller_range: (u32, u32, u32, u32),
}

fn outgoing_call_hierarchy_for_tree(
    tree: &tree_sitter::Tree,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    caller: &php_lsp_types::SymbolInfo,
) -> Vec<CallHierarchyOutgoingCall> {
    let ctx = OutgoingCallHierarchyContext {
        tree,
        source,
        file_symbols,
        index,
        caller_range: caller.range,
    };
    let mut calls_by_target: HashMap<String, (Arc<php_lsp_types::SymbolInfo>, Vec<Range>)> =
        HashMap::new();
    collect_outgoing_call_hierarchy(tree.root_node(), &ctx, &mut calls_by_target);

    let mut calls: Vec<_> = calls_by_target
        .into_values()
        .filter_map(|(symbol, ranges)| {
            Some(CallHierarchyOutgoingCall {
                to: call_hierarchy_item_from_symbol(&symbol)?,
                from_ranges: ranges,
            })
        })
        .collect();
    calls.sort_by(|left, right| left.to.name.cmp(&right.to.name));
    calls
}

fn collect_outgoing_call_hierarchy(
    node: tree_sitter::Node,
    ctx: &OutgoingCallHierarchyContext<'_>,
    calls_by_target: &mut HashMap<String, (Arc<php_lsp_types::SymbolInfo>, Vec<Range>)>,
) {
    let node_range = node_range_node(node);
    if !byte_ranges_overlap(node_range, ctx.caller_range) {
        return;
    }

    if matches!(node.kind(), "function_definition" | "method_declaration")
        && node_range != ctx.caller_range
        && byte_range_contains(ctx.caller_range, node_range)
    {
        return;
    }

    if matches!(
        node.kind(),
        "function_call_expression"
            | "member_call_expression"
            | "scoped_call_expression"
            | "object_creation_expression"
    ) {
        if let Some(name_node) = call_target_name_node(node) {
            if let Some((_, target)) = resolve_reference_symbol_at_node(
                ctx.tree,
                ctx.source,
                name_node,
                ctx.file_symbols,
                ctx.index,
            ) {
                if is_call_hierarchy_symbol_kind(target.kind) {
                    calls_by_target
                        .entry(target.fqn.clone())
                        .or_insert_with(|| (target.clone(), Vec::new()))
                        .1
                        .push(range_from_byte_range(
                            ctx.source,
                            node_range_node(name_node),
                        ));
                }
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_outgoing_call_hierarchy(child, ctx, calls_by_target);
    }
}

/// Compute diagnostics for a file (syntax + semantic).
///
/// Extracted as a free function so it can be called both from
/// `publish_diagnostics` and from the post-indexing re-check in `initialized`.
fn compute_open_file_diagnostics(
    uri_str: &str,
    open_files: &DashMap<String, FileParser>,
    index: &Arc<WorkspaceIndex>,
    diagnostics_mode: DiagnosticsMode,
    diagnostic_severity: DiagnosticSeverityConfig,
    php_version: PhpVersion,
) -> Vec<Diagnostic> {
    if let Some(parser) = open_files.get(uri_str) {
        compute_source_diagnostics_on_dedicated_stack(
            uri_str.to_string(),
            parser.source(),
            index.clone(),
            diagnostics_mode,
            diagnostic_severity,
            php_version,
        )
    } else {
        vec![]
    }
}

fn compute_source_diagnostics_on_dedicated_stack(
    uri_str: String,
    source: String,
    index: Arc<WorkspaceIndex>,
    diagnostics_mode: DiagnosticsMode,
    diagnostic_severity: DiagnosticSeverityConfig,
    php_version: PhpVersion,
) -> Vec<Diagnostic> {
    let thread_name = format!("php-lsp-diagnostics:{uri_str}");
    let handle = match std::thread::Builder::new()
        .name(thread_name)
        .stack_size(DIAGNOSTIC_THREAD_STACK_SIZE)
        .spawn(move || {
            let mut parser = FileParser::new();
            parser.parse_full(&source);
            compute_diagnostics_with_config(
                &uri_str,
                &parser,
                &index,
                diagnostics_mode,
                diagnostic_severity,
                php_version,
            )
        }) {
        Ok(handle) => handle,
        Err(err) => {
            tracing::warn!("Failed to spawn diagnostics worker: {}", err);
            return vec![];
        }
    };

    match handle.join() {
        Ok(diagnostics) => diagnostics,
        Err(_) => {
            tracing::warn!("Diagnostics worker panicked");
            vec![]
        }
    }
}

fn current_parser_symbol_references(
    uri_str: &str,
    parser: &FileParser,
) -> Vec<php_lsp_types::SymbolReference> {
    let Some(tree) = parser.tree() else {
        return Vec::new();
    };
    let source = parser.source();
    let file_symbols = extract_file_symbols(tree, &source, uri_str);
    collect_symbol_references_in_file(tree, &source, &file_symbols)
}

fn symbol_reference_matches(
    reference: &php_lsp_types::SymbolReference,
    target_fqn: &str,
    target_kind: php_lsp_types::PhpSymbolKind,
    include_declaration: bool,
) -> bool {
    if reference.is_declaration && !include_declaration {
        return false;
    }

    if reference.target_fqn == target_fqn
        && reference_kind_matches(reference.target_kind, target_kind)
    {
        return true;
    }

    if reference.is_declaration || !reference_kind_matches(reference.target_kind, target_kind) {
        return false;
    }

    let Some(member_name) = target_fqn.rsplit_once("::").map(|(_, member)| member) else {
        return false;
    };
    matches!(
        target_kind,
        php_lsp_types::PhpSymbolKind::Method
            | php_lsp_types::PhpSymbolKind::Property
            | php_lsp_types::PhpSymbolKind::ClassConstant
            | php_lsp_types::PhpSymbolKind::EnumCase
    ) && reference.target_fqn == format!("::{}", member_name)
}

fn reference_kind_matches(
    reference_kind: php_lsp_types::PhpSymbolKind,
    target_kind: php_lsp_types::PhpSymbolKind,
) -> bool {
    if reference_kind == target_kind {
        return true;
    }

    is_class_like_kind(reference_kind) && is_class_like_kind(target_kind)
}

fn is_class_like_kind(kind: php_lsp_types::PhpSymbolKind) -> bool {
    matches!(
        kind,
        php_lsp_types::PhpSymbolKind::Class
            | php_lsp_types::PhpSymbolKind::Interface
            | php_lsp_types::PhpSymbolKind::Trait
            | php_lsp_types::PhpSymbolKind::Enum
    )
}

#[cfg(test)]
fn compute_diagnostics(
    uri_str: &str,
    parser: &FileParser,
    index: &WorkspaceIndex,
    diagnostics_mode: DiagnosticsMode,
    php_version: PhpVersion,
) -> Vec<Diagnostic> {
    compute_diagnostics_with_config(
        uri_str,
        parser,
        index,
        diagnostics_mode,
        DiagnosticSeverityConfig::default(),
        php_version,
    )
}

fn compute_diagnostics_with_config(
    uri_str: &str,
    parser: &FileParser,
    index: &WorkspaceIndex,
    diagnostics_mode: DiagnosticsMode,
    diagnostic_severity: DiagnosticSeverityConfig,
    php_version: PhpVersion,
) -> Vec<Diagnostic> {
    let diagnostics_started = Instant::now();
    if diagnostics_mode == DiagnosticsMode::Off {
        return vec![];
    }

    let tree = match parser.tree() {
        Some(t) => t,
        None => return vec![],
    };
    let source = parser.source();
    let utf16_index = Utf16LineIndex::new(&source);

    // Syntax errors (ERROR / MISSING nodes)
    let lsp_diags = extract_syntax_errors(tree, &source);
    let mut diagnostics: Vec<Diagnostic> = lsp_diags
        .into_iter()
        .map(|d| {
            // tree-sitter positions use byte columns; convert to UTF-16
            let start_char =
                utf16_index.byte_col_to_utf16(d.range.start.line, d.range.start.character);
            let end_char = utf16_index.byte_col_to_utf16(d.range.end.line, d.range.end.character);
            Diagnostic {
                range: Range {
                    start: Position::new(d.range.start.line, start_char),
                    end: Position::new(d.range.end.line, end_char),
                },
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("php-lsp".to_string()),
                message: d.message,
                ..Default::default()
            }
        })
        .collect();

    // Avoid semantic noise while the file has syntax errors.
    if !diagnostics.is_empty() {
        return diagnostics;
    }

    if diagnostics_mode == DiagnosticsMode::SyntaxOnly {
        return diagnostics;
    }

    // Semantic diagnostics (unknown class, function, unresolved use)
    let file_symbols = index
        .file_symbols
        .get(uri_str)
        .map(|entry| entry.value().clone())
        .unwrap_or_default();

    let semantic_started = Instant::now();
    let sem_diags =
        extract_semantic_diagnostics(tree, &source, &file_symbols, |fqn| index.resolve_fqn(fqn));
    warn_if_slow_diagnostic_phase(uri_str, "semantic", semantic_started);

    for sd in sem_diags {
        if let Some(diagnostic) = semantic_diagnostic_to_lsp(sd, &utf16_index, diagnostic_severity)
        {
            diagnostics.push(diagnostic);
        }
    }

    let skip_member_and_type_diagnostics =
        count_member_type_diagnostic_nodes(tree.root_node()) > MEMBER_TYPE_DIAGNOSTIC_NODE_LIMIT;

    diagnostics.extend(apply_diagnostic_category(
        workspace_duplicate_symbol_diagnostics(uri_str, &file_symbols, index, &utf16_index),
        DiagnosticCategory::DuplicateSymbols,
        diagnostic_severity,
    ));
    if diagnostic_severity
        .severity(DiagnosticCategory::Members)
        .is_some()
        && !skip_member_and_type_diagnostics
    {
        let members_started = Instant::now();
        diagnostics.extend(apply_diagnostic_category(
            member_access_diagnostics(tree, &source, &file_symbols, index, &utf16_index),
            DiagnosticCategory::Members,
            diagnostic_severity,
        ));
        warn_if_slow_diagnostic_phase(uri_str, "members", members_started);
    }
    if diagnostic_severity
        .severity(DiagnosticCategory::TypeCompatibility)
        .is_some()
        && !skip_member_and_type_diagnostics
    {
        let types_started = Instant::now();
        diagnostics.extend(apply_diagnostic_category(
            type_compatibility_diagnostics(tree, &source, &file_symbols, index, &utf16_index),
            DiagnosticCategory::TypeCompatibility,
            diagnostic_severity,
        ));
        warn_if_slow_diagnostic_phase(uri_str, "type compatibility", types_started);
    }
    diagnostics.extend(apply_diagnostic_category(
        override_signature_diagnostics(&file_symbols, index, &utf16_index),
        DiagnosticCategory::OverrideSignatures,
        diagnostic_severity,
    ));
    diagnostics.extend(apply_diagnostic_category(
        php_version_type_diagnostics(tree, &source, php_version, &utf16_index),
        DiagnosticCategory::PhpVersion,
        diagnostic_severity,
    ));

    warn_if_slow_diagnostic_phase(uri_str, "total", diagnostics_started);
    diagnostics
}

fn count_member_type_diagnostic_nodes(node: tree_sitter::Node) -> usize {
    let mut count = usize::from(matches!(
        node.kind(),
        "member_access_expression"
            | "member_call_expression"
            | "scoped_call_expression"
            | "scoped_property_access_expression"
            | "class_constant_access_expression"
            | "function_call_expression"
            | "object_creation_expression"
            | "assignment_expression"
            | "return_statement"
    ));

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        count += count_member_type_diagnostic_nodes(child);
        if count > MEMBER_TYPE_DIAGNOSTIC_NODE_LIMIT {
            break;
        }
    }
    count
}

fn warn_if_slow_diagnostic_phase(uri_str: &str, phase: &str, started: Instant) {
    let elapsed = started.elapsed();
    if elapsed >= Duration::from_millis(DIAGNOSTIC_PHASE_SLOW_WARNING_MS) {
        tracing::warn!(
            "diagnostics {} phase took {} ms for {}",
            phase,
            elapsed.as_millis(),
            uri_str
        );
    }
}

fn semantic_diagnostic_to_lsp(
    diagnostic: SemanticDiagnostic,
    utf16_index: &Utf16LineIndex,
    severity_config: DiagnosticSeverityConfig,
) -> Option<Diagnostic> {
    let category = semantic_diagnostic_category(&diagnostic.kind);
    let severity = severity_config.severity(category)?;
    Some(Diagnostic {
        range: Range {
            start: Position::new(
                diagnostic.range.0,
                utf16_index.byte_col_to_utf16(diagnostic.range.0, diagnostic.range.1),
            ),
            end: Position::new(
                diagnostic.range.2,
                utf16_index.byte_col_to_utf16(diagnostic.range.2, diagnostic.range.3),
            ),
        },
        severity: Some(severity),
        code: Some(NumberOrString::String(
            semantic_diagnostic_code(&diagnostic.kind).to_string(),
        )),
        source: Some("php-lsp".to_string()),
        message: diagnostic.message,
        ..Default::default()
    })
}

fn semantic_diagnostic_category(kind: &SemanticDiagnosticKind) -> DiagnosticCategory {
    match kind {
        SemanticDiagnosticKind::UnknownClass
        | SemanticDiagnosticKind::UnknownFunction
        | SemanticDiagnosticKind::UnresolvedUse => DiagnosticCategory::UnknownSymbols,
        SemanticDiagnosticKind::ArgumentCountMismatch => DiagnosticCategory::TypeCompatibility,
        SemanticDiagnosticKind::UndefinedVariable => DiagnosticCategory::UnknownSymbols,
        SemanticDiagnosticKind::UnusedImport
        | SemanticDiagnosticKind::UnusedVariable
        | SemanticDiagnosticKind::UnusedParameter => DiagnosticCategory::Unused,
        SemanticDiagnosticKind::DuplicateSymbol => DiagnosticCategory::DuplicateSymbols,
    }
}

fn semantic_diagnostic_code(kind: &SemanticDiagnosticKind) -> &'static str {
    match kind {
        SemanticDiagnosticKind::UnknownClass => "php-lsp.unknownClass",
        SemanticDiagnosticKind::UnknownFunction => "php-lsp.unknownFunction",
        SemanticDiagnosticKind::UnresolvedUse => "php-lsp.unresolvedUse",
        SemanticDiagnosticKind::ArgumentCountMismatch => "php-lsp.argumentCountMismatch",
        SemanticDiagnosticKind::UndefinedVariable => "php-lsp.undefinedVariable",
        SemanticDiagnosticKind::UnusedImport => "php-lsp.unusedImport",
        SemanticDiagnosticKind::UnusedVariable => "php-lsp.unusedVariable",
        SemanticDiagnosticKind::UnusedParameter => "php-lsp.unusedParameter",
        SemanticDiagnosticKind::DuplicateSymbol => "php-lsp.duplicateSymbol",
    }
}

fn apply_diagnostic_category(
    diagnostics: Vec<Diagnostic>,
    category: DiagnosticCategory,
    severity_config: DiagnosticSeverityConfig,
) -> Vec<Diagnostic> {
    let Some(severity) = severity_config.severity(category) else {
        return Vec::new();
    };

    diagnostics
        .into_iter()
        .map(|mut diagnostic| {
            diagnostic.severity = Some(severity);
            if diagnostic.code.is_none() {
                diagnostic.code = Some(NumberOrString::String(category.code().to_string()));
            }
            diagnostic
        })
        .collect()
}

fn diagnostic_at_byte_range(
    range: (u32, u32, u32, u32),
    utf16_index: &Utf16LineIndex,
    message: String,
) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position::new(range.0, utf16_index.byte_col_to_utf16(range.0, range.1)),
            end: Position::new(range.2, utf16_index.byte_col_to_utf16(range.2, range.3)),
        },
        severity: Some(DiagnosticSeverity::WARNING),
        source: Some("php-lsp".to_string()),
        message,
        ..Default::default()
    }
}

fn member_access_diagnostics(
    tree: &tree_sitter::Tree,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    walk_member_access_diagnostics(
        tree,
        tree.root_node(),
        source,
        file_symbols,
        index,
        utf16_index,
        &mut diagnostics,
    );
    diagnostics
}

fn walk_member_access_diagnostics(
    tree: &tree_sitter::Tree,
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if matches!(
        node.kind(),
        "member_access_expression"
            | "member_call_expression"
            | "scoped_call_expression"
            | "scoped_property_access_expression"
            | "class_constant_access_expression"
    ) {
        check_member_access_node(
            tree,
            node,
            source,
            file_symbols,
            index,
            utf16_index,
            diagnostics,
        );
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_member_access_diagnostics(
            tree,
            child,
            source,
            file_symbols,
            index,
            utf16_index,
            diagnostics,
        );
    }
}

fn check_member_access_node(
    tree: &tree_sitter::Tree,
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if node_inside_anonymous_class_body(node, source) {
        return;
    }

    let Some(name_node) = member_reference_name_node(node) else {
        return;
    };
    if is_magic_class_constant_access(node, name_node, source) {
        return;
    }
    let pos = name_node.start_position();
    let member_type_resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
        resolve_member_type_from_index(index, class_fqn, member_name)
    };
    let Some(sym_at_pos) = symbol_at_position_with_resolver(
        tree,
        source,
        pos.row as u32,
        pos.column as u32,
        file_symbols,
        Some(&member_type_resolver),
    ) else {
        return;
    };

    if !matches!(
        sym_at_pos.ref_kind,
        RefKind::MethodCall
            | RefKind::PropertyAccess
            | RefKind::StaticPropertyAccess
            | RefKind::ClassConstant
    ) || !sym_at_pos.fqn.contains("::")
    {
        return;
    }

    let Some(resolved) = resolve_member_for_ref_kind(index, &sym_at_pos) else {
        if is_phpunit_testcase_helper_call(&sym_at_pos, file_symbols, index)
            || is_phpunit_test_double_api_call(tree, source, file_symbols, index, &sym_at_pos)
            || is_missing_parent_constructor_call(&sym_at_pos)
            || is_enum_builtin_method_call(index, &sym_at_pos)
            || is_dynamic_member_access(index, file_symbols, &sym_at_pos)
        {
            return;
        }

        diagnostics.push(member_diagnostic(
            &sym_at_pos,
            utf16_index,
            unknown_member_message(&sym_at_pos),
        ));
        return;
    };

    if let Some(message) = static_instance_misuse_message(node.kind(), &sym_at_pos, &resolved) {
        diagnostics.push(member_diagnostic(&sym_at_pos, utf16_index, message));
    }

    if let Some(message) =
        visibility_violation_message(index, &resolved, file_symbols, sym_at_pos.range)
    {
        diagnostics.push(member_diagnostic(&sym_at_pos, utf16_index, message));
    }
}

fn member_reference_name_node(node: tree_sitter::Node) -> Option<tree_sitter::Node> {
    node.child_by_field_name("name").or_else(|| {
        if node.kind() == "class_constant_access_expression" {
            node.named_child(1)
        } else {
            None
        }
    })
}

fn is_phpunit_testcase_helper_call(
    sym_at_pos: &SymbolAtPosition,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
) -> bool {
    if sym_at_pos.ref_kind != RefKind::MethodCall
        || !file_is_phpunit_test_context(file_symbols, index)
        || !phpunit_testcase_helper_method(&sym_at_pos.name)
    {
        return false;
    }

    matches!(
        sym_at_pos.object_expr.as_deref(),
        Some("$this" | "self" | "static" | "parent")
    )
}

fn file_is_phpunit_test_context(
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
) -> bool {
    file_symbols.symbols.iter().any(|sym| {
        matches!(sym.kind, php_lsp_types::PhpSymbolKind::Class)
            && sym.extends.iter().any(|parent| {
                is_phpunit_testcase_like_fqn(parent)
                    || class_extends_or_implements(
                        index,
                        parent.trim_start_matches('\\'),
                        "PHPUnit\\Framework\\TestCase",
                        &mut Vec::new(),
                    )
            })
    }) || file_symbols.symbols.iter().any(|sym| {
        matches!(sym.kind, php_lsp_types::PhpSymbolKind::Trait)
            && (sym.name.ends_with("TestTrait")
                || sym
                    .fqn
                    .split('\\')
                    .any(|segment| segment.eq_ignore_ascii_case("Tests")))
    })
}

fn is_phpunit_testcase_like_fqn(fqn: &str) -> bool {
    let fqn = fqn.trim_start_matches('\\');
    fqn == "PHPUnit\\Framework\\TestCase" || fqn.ends_with("\\TestCase")
}

fn phpunit_testcase_helper_method(member_name: &str) -> bool {
    member_name.starts_with("assert")
        || matches!(
            member_name,
            "fail"
                | "markTestIncomplete"
                | "markTestSkipped"
                | "setUp"
                | "tearDown"
                | "createMock"
                | "createConfiguredMock"
                | "createPartialMock"
                | "createStub"
                | "createStubForIntersectionOfInterfaces"
                | "createMockForIntersectionOfInterfaces"
                | "once"
                | "never"
                | "any"
                | "exactly"
                | "atLeast"
                | "atLeastOnce"
                | "atMost"
                | "callback"
                | "anything"
                | "equalTo"
                | "identicalTo"
                | "isInstanceOf"
                | "isType"
                | "stringContains"
                | "logicalAnd"
                | "logicalOr"
                | "logicalNot"
                | "containsEqual"
                | "containsIdentical"
        )
}

fn is_phpunit_test_double_api_call(
    tree: &tree_sitter::Tree,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    sym_at_pos: &SymbolAtPosition,
) -> bool {
    if sym_at_pos.ref_kind != RefKind::MethodCall
        || !phpunit_test_double_api_method(&sym_at_pos.name)
    {
        return false;
    }

    let Some(prop_name) = sym_at_pos
        .object_expr
        .as_deref()
        .and_then(|object_expr| object_expr.strip_prefix("$this->"))
        .filter(|prop_name| !prop_name.contains("->"))
    else {
        return false;
    };

    let member_type_resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
        phpunit_testcase_factory_return_type(member_name)
            .map(str::to_string)
            .or_else(|| resolve_member_type_from_index(index, class_fqn, member_name))
    };

    infer_property_type_from_assignments(
        tree,
        source,
        prop_name,
        file_symbols,
        Some(&member_type_resolver),
    )
    .into_iter()
    .any(|class_fqn| {
        phpunit_test_double_type_has_method(&class_fqn, &sym_at_pos.name)
            || resolve_member_on_class_for_ref_kind(
                index,
                &class_fqn,
                &sym_at_pos.name,
                sym_at_pos.ref_kind,
                None,
            )
            .is_some()
    })
}

fn phpunit_testcase_factory_return_type(member_name: &str) -> Option<&'static str> {
    match member_name {
        "createMock"
        | "createConfiguredMock"
        | "createPartialMock"
        | "createMockForIntersectionOfInterfaces" => {
            Some("PHPUnit\\Framework\\MockObject\\MockObject")
        }
        "createStub" | "createStubForIntersectionOfInterfaces" => {
            Some("PHPUnit\\Framework\\MockObject\\Stub")
        }
        _ => None,
    }
}

fn phpunit_test_double_api_method(member_name: &str) -> bool {
    matches!(
        member_name,
        "expects"
            | "method"
            | "with"
            | "withAnyParameters"
            | "withConsecutive"
            | "will"
            | "willReturn"
            | "willReturnArgument"
            | "willReturnCallback"
            | "willReturnMap"
            | "willReturnOnConsecutiveCalls"
            | "willReturnReference"
            | "willReturnSelf"
            | "willThrowException"
    )
}

fn phpunit_test_double_type_has_method(class_fqn: &str, member_name: &str) -> bool {
    is_phpunit_test_double_type(class_fqn) && phpunit_test_double_api_method(member_name)
}

fn is_dynamic_member_access(
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    sym_at_pos: &SymbolAtPosition,
) -> bool {
    let Some((class_fqn, member_name)) = sym_at_pos.fqn.rsplit_once("::") else {
        return false;
    };

    if is_generic_object_type_name(class_fqn) {
        return true;
    }

    if class_has_unindexed_ancestor(index, class_fqn, &mut Vec::new()) {
        return true;
    }

    if sym_at_pos.ref_kind == RefKind::MethodCall {
        return is_doctrine_repository_dynamic_method(index, class_fqn, member_name)
            || is_laravel_eloquent_dynamic_member(
                index,
                class_fqn,
                member_name,
                sym_at_pos.ref_kind,
            )
            || is_symfony_controller_helper_method(index, class_fqn, member_name)
            || is_unindexed_imported_type(index, file_symbols, class_fqn);
    }

    if sym_at_pos.ref_kind != RefKind::PropertyAccess {
        return false;
    }

    if fqn_matches(class_fqn, "stdClass") || is_phpunit_test_double_type(class_fqn) {
        return true;
    }

    if is_laravel_eloquent_dynamic_member(index, class_fqn, member_name, sym_at_pos.ref_kind) {
        return true;
    }

    let bare_member_name = member_name.strip_prefix('$').unwrap_or(member_name);
    matches!(bare_member_name, "name" | "value")
        && index
            .types
            .get(class_fqn.trim_start_matches('\\'))
            .is_some_and(|sym| sym.kind == php_lsp_types::PhpSymbolKind::Enum)
}

fn is_missing_parent_constructor_call(sym_at_pos: &SymbolAtPosition) -> bool {
    sym_at_pos.ref_kind == RefKind::MethodCall
        && sym_at_pos.name == "__construct"
        && sym_at_pos.object_expr.as_deref() == Some("parent")
}

fn is_enum_builtin_method_call(index: &WorkspaceIndex, sym_at_pos: &SymbolAtPosition) -> bool {
    if sym_at_pos.ref_kind != RefKind::MethodCall
        || !matches!(sym_at_pos.name.as_str(), "cases" | "from" | "tryFrom")
    {
        return false;
    }

    let Some((class_fqn, _)) = sym_at_pos.fqn.rsplit_once("::") else {
        return false;
    };

    index
        .types
        .get(class_fqn.trim_start_matches('\\'))
        .is_some_and(|sym| sym.kind == php_lsp_types::PhpSymbolKind::Enum)
}

fn is_doctrine_repository_dynamic_method(
    index: &WorkspaceIndex,
    class_fqn: &str,
    member_name: &str,
) -> bool {
    class_is_or_extends(index, class_fqn, "Doctrine\\ORM\\EntityRepository")
        && (member_name.starts_with("findBy")
            || member_name.starts_with("findOneBy")
            || member_name.starts_with("countBy"))
}

fn is_symfony_controller_helper_method(
    index: &WorkspaceIndex,
    class_fqn: &str,
    member_name: &str,
) -> bool {
    if !class_is_or_extends(
        index,
        class_fqn,
        "Symfony\\Bundle\\FrameworkBundle\\Controller\\AbstractController",
    ) {
        return false;
    }

    matches!(
        member_name.to_ascii_lowercase().as_str(),
        "render"
            | "renderform"
            | "json"
            | "redirect"
            | "redirecttoroute"
            | "redirecttourl"
            | "forward"
            | "generateurl"
            | "addflash"
            | "getuser"
            | "isgranted"
            | "denyaccessunlessgranted"
            | "createform"
            | "createformbuilder"
            | "getparameter"
    )
}

fn is_laravel_eloquent_dynamic_member(
    index: &WorkspaceIndex,
    class_fqn: &str,
    member_name: &str,
    ref_kind: RefKind,
) -> bool {
    let is_model = class_is_or_extends(index, class_fqn, "Illuminate\\Database\\Eloquent\\Model");
    let is_builder =
        class_is_or_extends(index, class_fqn, "Illuminate\\Database\\Eloquent\\Builder")
            || class_is_or_extends(index, class_fqn, "Illuminate\\Database\\Query\\Builder")
            || class_is_or_extends(
                index,
                class_fqn,
                "Illuminate\\Database\\Eloquent\\Relations\\Relation",
            );

    match ref_kind {
        RefKind::MethodCall => {
            (is_model || is_builder) && is_laravel_eloquent_dynamic_method(member_name)
        }
        RefKind::PropertyAccess => is_model,
        _ => false,
    }
}

fn is_laravel_eloquent_dynamic_method(member_name: &str) -> bool {
    let lower = member_name.to_ascii_lowercase();
    lower.starts_with("where")
        || lower.starts_with("orwhere")
        || lower.starts_with("wherehas")
        || lower.starts_with("orwherehas")
        || lower.starts_with("withwherehas")
        || lower.starts_with("doesnthave")
        || lower.starts_with("ordoesnthave")
        || matches!(
            lower.as_str(),
            "query"
                | "newquery"
                | "newmodelquery"
                | "newquerywithoutrelationships"
                | "find"
                | "findorfail"
                | "findmany"
                | "first"
                | "firstorfail"
                | "firstornew"
                | "firstorcreate"
                | "updateorcreate"
                | "create"
                | "forcecreate"
                | "save"
                | "push"
                | "update"
                | "delete"
                | "destroy"
                | "restore"
                | "with"
                | "without"
                | "load"
                | "loadmissing"
                | "pluck"
                | "count"
                | "exists"
                | "paginate"
                | "simplepaginate"
        )
}

fn class_is_or_extends(index: &WorkspaceIndex, class_fqn: &str, target_class: &str) -> bool {
    fqn_matches(class_fqn, target_class)
        || class_extends_or_implements(index, class_fqn, target_class, &mut Vec::new())
}

fn class_has_unindexed_ancestor(
    index: &WorkspaceIndex,
    class_fqn: &str,
    visited: &mut Vec<String>,
) -> bool {
    let class_fqn = class_fqn.trim_start_matches('\\');
    if visited
        .iter()
        .any(|visited| fqn_matches(visited, class_fqn))
    {
        return false;
    }
    visited.push(class_fqn.to_string());

    let Some(class_sym) = index
        .types
        .get(class_fqn)
        .map(|entry| entry.value().clone())
    else {
        return false;
    };

    class_sym
        .extends
        .iter()
        .chain(class_sym.implements.iter())
        .any(|parent| {
            let parent = parent.trim_start_matches('\\');
            if parent.is_empty() || fqn_matches(parent, class_fqn) {
                return false;
            }
            !index.types.contains_key(parent)
                || class_has_unindexed_ancestor(index, parent, visited)
        })
}

fn is_unindexed_imported_type(
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    class_fqn: &str,
) -> bool {
    let normalized = class_fqn.trim_start_matches('\\');
    if index.types.contains_key(normalized) {
        return false;
    }

    file_symbols.use_statements.iter().any(|use_statement| {
        matches!(use_statement.kind, php_lsp_types::UseKind::Class)
            && use_statement.fqn.trim_start_matches('\\') == normalized
    })
}

fn is_generic_object_type_name(class_fqn: &str) -> bool {
    class_fqn
        .trim_start_matches('\\')
        .rsplit('\\')
        .next()
        .is_some_and(|name| name.eq_ignore_ascii_case("object"))
}

fn is_phpunit_test_double_type(class_fqn: &str) -> bool {
    matches!(
        class_fqn.trim_start_matches('\\'),
        "PHPUnit\\Framework\\MockObject\\MockObject"
            | "PHPUnit\\Framework\\MockObject\\Stub"
            | "PHPUnit\\Framework\\MockObject\\MockBuilder"
            | "PHPUnit\\Framework\\MockObject\\InvocationMocker"
    )
}

fn is_magic_class_constant_access(
    node: tree_sitter::Node,
    name_node: tree_sitter::Node,
    source: &str,
) -> bool {
    node.kind() == "class_constant_access_expression"
        && source[name_node.byte_range()].eq_ignore_ascii_case("class")
}

fn member_diagnostic(
    sym_at_pos: &SymbolAtPosition,
    utf16_index: &Utf16LineIndex,
    message: String,
) -> Diagnostic {
    diagnostic_at_byte_range(sym_at_pos.range, utf16_index, message)
}

fn symbol_kind_matches_ref_kind(sym: &php_lsp_types::SymbolInfo, ref_kind: RefKind) -> bool {
    matches!(
        (ref_kind, sym.kind),
        (RefKind::MethodCall, php_lsp_types::PhpSymbolKind::Method)
            | (
                RefKind::PropertyAccess,
                php_lsp_types::PhpSymbolKind::Property
            )
            | (
                RefKind::StaticPropertyAccess,
                php_lsp_types::PhpSymbolKind::Property
            )
            | (
                RefKind::ClassConstant,
                php_lsp_types::PhpSymbolKind::ClassConstant
            )
            | (
                RefKind::ClassConstant,
                php_lsp_types::PhpSymbolKind::EnumCase
            )
    )
}

fn resolve_member_for_ref_kind(
    index: &WorkspaceIndex,
    sym_at_pos: &SymbolAtPosition,
) -> Option<std::sync::Arc<php_lsp_types::SymbolInfo>> {
    if let Some(sym) = index.resolve_fqn(&sym_at_pos.fqn) {
        if symbol_kind_matches_ref_kind(&sym, sym_at_pos.ref_kind) {
            return Some(sym);
        }
    }

    let (class_fqn, member_name) = sym_at_pos.fqn.rsplit_once("::")?;
    resolve_member_on_class_for_ref_kind(
        index,
        class_fqn,
        member_name,
        sym_at_pos.ref_kind,
        Some(&sym_at_pos.fqn),
    )
}

fn resolve_member_on_class_for_ref_kind(
    index: &WorkspaceIndex,
    class_fqn: &str,
    member_name: &str,
    ref_kind: RefKind,
    exact_fqn: Option<&str>,
) -> Option<std::sync::Arc<php_lsp_types::SymbolInfo>> {
    let bare_name = member_name.strip_prefix('$').unwrap_or(member_name);
    index.get_members(class_fqn).into_iter().find(|sym| {
        symbol_kind_matches_ref_kind(sym, ref_kind)
            && (exact_fqn.is_some_and(|fqn| sym.fqn == fqn)
                || sym.name == member_name
                || sym.name == bare_name)
    })
}

fn unknown_member_message(sym_at_pos: &SymbolAtPosition) -> String {
    match sym_at_pos.ref_kind {
        RefKind::MethodCall => format!("Unknown method: {}", sym_at_pos.fqn),
        RefKind::PropertyAccess | RefKind::StaticPropertyAccess => {
            format!("Unknown property: {}", sym_at_pos.fqn)
        }
        RefKind::ClassConstant => format!("Unknown class constant: {}", sym_at_pos.fqn),
        _ => format!("Unknown member: {}", sym_at_pos.fqn),
    }
}

fn static_instance_misuse_message(
    node_kind: &str,
    sym_at_pos: &SymbolAtPosition,
    sym: &php_lsp_types::SymbolInfo,
) -> Option<String> {
    match sym.kind {
        php_lsp_types::PhpSymbolKind::Method => match (node_kind, sym.modifiers.is_static) {
            ("member_call_expression", true)
                if sym_at_pos.object_expr.as_deref() == Some("$this") =>
            {
                None
            }
            ("member_call_expression", true) => Some(format!(
                "Static method called as instance method: {}",
                sym.fqn
            )),
            ("scoped_call_expression", false)
                if matches!(
                    sym_at_pos.object_expr.as_deref(),
                    Some("self" | "static" | "parent")
                ) =>
            {
                None
            }
            ("scoped_call_expression", false) => {
                Some(format!("Instance method called statically: {}", sym.fqn))
            }
            _ => None,
        },
        php_lsp_types::PhpSymbolKind::Property => match (node_kind, sym.modifiers.is_static) {
            ("member_access_expression", true) => Some(format!(
                "Static property accessed as instance property: {}",
                sym.fqn
            )),
            ("scoped_property_access_expression", false) => Some(format!(
                "Instance property accessed statically: {}",
                sym.fqn
            )),
            _ => None,
        },
        _ => None,
    }
}

fn visibility_violation_message(
    index: &WorkspaceIndex,
    sym: &php_lsp_types::SymbolInfo,
    file_symbols: &php_lsp_types::FileSymbols,
    access_range: (u32, u32, u32, u32),
) -> Option<String> {
    let declaring_class = sym.parent_fqn.as_deref()?;
    match sym.visibility {
        php_lsp_types::Visibility::Public => None,
        php_lsp_types::Visibility::Private => {
            let current_class = current_class_fqn_at_range(file_symbols, access_range);
            let accessible = current_class.as_deref().is_some_and(|current| {
                fqn_matches(current, declaring_class)
                    || class_uses_trait(index, current, declaring_class, &mut Vec::new())
            });
            (!accessible).then(|| format!("Private member is not accessible here: {}", sym.fqn))
        }
        php_lsp_types::Visibility::Protected => {
            let current_class = current_class_fqn_at_range(file_symbols, access_range);
            let accessible = current_class.as_deref().is_some_and(|current| {
                class_can_access_protected_member(index, current, declaring_class)
            });
            (!accessible).then(|| format!("Protected member is not accessible here: {}", sym.fqn))
        }
    }
}

fn current_class_fqn_at_range(
    file_symbols: &php_lsp_types::FileSymbols,
    range: (u32, u32, u32, u32),
) -> Option<String> {
    current_class_symbol_at_range(file_symbols, range).map(|sym| sym.fqn.clone())
}

fn current_class_symbol_at_range(
    file_symbols: &php_lsp_types::FileSymbols,
    range: (u32, u32, u32, u32),
) -> Option<&php_lsp_types::SymbolInfo> {
    file_symbols.symbols.iter().find(|sym| {
        matches!(
            sym.kind,
            php_lsp_types::PhpSymbolKind::Class
                | php_lsp_types::PhpSymbolKind::Interface
                | php_lsp_types::PhpSymbolKind::Trait
                | php_lsp_types::PhpSymbolKind::Enum
        ) && byte_range_contains(sym.range, range)
    })
}

fn class_can_access_protected_member(
    index: &WorkspaceIndex,
    current_class: &str,
    declaring_class: &str,
) -> bool {
    if fqn_matches(current_class, declaring_class) {
        return true;
    }
    class_extends_or_implements(index, current_class, declaring_class, &mut Vec::new())
        || class_or_ancestor_uses_trait(index, current_class, declaring_class, &mut Vec::new())
}

fn class_extends_or_implements(
    index: &WorkspaceIndex,
    current_class: &str,
    target_class: &str,
    visited: &mut Vec<String>,
) -> bool {
    let current_class = current_class.trim_start_matches('\\');
    let target_class = target_class.trim_start_matches('\\');
    if visited
        .iter()
        .any(|visited| fqn_matches(visited, current_class))
    {
        return false;
    }
    visited.push(current_class.to_string());

    let Some(class_sym) = index
        .types
        .get(current_class)
        .map(|entry| entry.value().clone())
    else {
        return false;
    };

    class_sym
        .extends
        .iter()
        .chain(class_sym.implements.iter())
        .any(|parent| {
            fqn_matches(parent, target_class)
                || class_extends_or_implements(index, parent, target_class, visited)
        })
}

fn class_or_ancestor_uses_trait(
    index: &WorkspaceIndex,
    current_class: &str,
    target_trait: &str,
    visited: &mut Vec<String>,
) -> bool {
    let current_class = current_class.trim_start_matches('\\');
    if visited
        .iter()
        .any(|visited| fqn_matches(visited, current_class))
    {
        return false;
    }
    visited.push(current_class.to_string());

    if class_uses_trait(index, current_class, target_trait, &mut Vec::new()) {
        return true;
    }

    let Some(class_sym) = index
        .types
        .get(current_class)
        .map(|entry| entry.value().clone())
    else {
        return false;
    };

    class_sym
        .extends
        .iter()
        .any(|parent| class_or_ancestor_uses_trait(index, parent, target_trait, visited))
}

fn class_uses_trait(
    index: &WorkspaceIndex,
    current_class: &str,
    target_trait: &str,
    visited: &mut Vec<String>,
) -> bool {
    let current_class = current_class.trim_start_matches('\\');
    if visited
        .iter()
        .any(|visited| fqn_matches(visited, current_class))
    {
        return false;
    }
    visited.push(current_class.to_string());

    let Some(class_sym) = index
        .types
        .get(current_class)
        .map(|entry| entry.value().clone())
    else {
        return false;
    };

    class_sym.traits.iter().any(|used_trait| {
        fqn_matches(used_trait, target_trait)
            || class_uses_trait(index, used_trait, target_trait, visited)
    })
}

fn fqn_matches(left: &str, right: &str) -> bool {
    left.trim_start_matches('\\') == right.trim_start_matches('\\')
}

fn byte_range_contains(outer: (u32, u32, u32, u32), inner: (u32, u32, u32, u32)) -> bool {
    (inner.0 > outer.0 || (inner.0 == outer.0 && inner.1 >= outer.1))
        && (inner.2 < outer.2 || (inner.2 == outer.2 && inner.3 <= outer.3))
}

fn node_inside_anonymous_class_body(node: tree_sitter::Node, source: &str) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == "object_creation_expression" {
            let text = &source[parent.byte_range()];
            if text.trim_start().starts_with("new class") {
                return text.find('{').is_some_and(|body_start| {
                    node.start_byte() > parent.start_byte().saturating_add(body_start)
                });
            }
        }
        current = parent.parent();
    }
    false
}

#[derive(Debug, Clone)]
struct InferredExprType {
    display: String,
    comparable: String,
    range: (u32, u32, u32, u32),
}

fn type_compatibility_diagnostics(
    tree: &tree_sitter::Tree,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    walk_type_compatibility_diagnostics(
        tree,
        tree.root_node(),
        source,
        file_symbols,
        index,
        utf16_index,
        &mut diagnostics,
    );
    diagnostics
}

fn walk_type_compatibility_diagnostics(
    tree: &tree_sitter::Tree,
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match node.kind() {
        "function_call_expression" => check_function_call_type_compatibility(
            tree,
            node,
            source,
            file_symbols,
            index,
            utf16_index,
            diagnostics,
        ),
        "member_call_expression" | "scoped_call_expression" => {
            check_member_call_type_compatibility(
                tree,
                node,
                source,
                file_symbols,
                index,
                utf16_index,
                diagnostics,
            )
        }
        "object_creation_expression" => check_constructor_type_compatibility(
            tree,
            node,
            source,
            file_symbols,
            index,
            utf16_index,
            diagnostics,
        ),
        "return_statement" => check_return_type_compatibility(
            node,
            source,
            file_symbols,
            index,
            utf16_index,
            diagnostics,
        ),
        "assignment_expression" => check_property_assignment_type_compatibility(
            tree,
            node,
            source,
            file_symbols,
            index,
            utf16_index,
            diagnostics,
        ),
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_type_compatibility_diagnostics(
            tree,
            child,
            source,
            file_symbols,
            index,
            utf16_index,
            diagnostics,
        );
    }
}

fn check_function_call_type_compatibility(
    tree: &tree_sitter::Tree,
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(name_node) = node
        .child_by_field_name("function")
        .or_else(|| node.named_child(0))
    else {
        return;
    };
    let Some((_, sym)) =
        resolve_reference_symbol_at_node(tree, source, name_node, file_symbols, index)
    else {
        return;
    };

    if sym.kind == php_lsp_types::PhpSymbolKind::Function {
        check_call_argument_types(
            node,
            &sym,
            source,
            file_symbols,
            index,
            utf16_index,
            diagnostics,
        );
    }
}

fn check_member_call_type_compatibility(
    tree: &tree_sitter::Tree,
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(name_node) = member_reference_name_node(node) else {
        return;
    };
    let Some((_, sym)) =
        resolve_reference_symbol_at_node(tree, source, name_node, file_symbols, index)
    else {
        return;
    };

    if sym.kind == php_lsp_types::PhpSymbolKind::Method {
        check_call_argument_types(
            node,
            &sym,
            source,
            file_symbols,
            index,
            utf16_index,
            diagnostics,
        );
    }
}

fn check_constructor_type_compatibility(
    tree: &tree_sitter::Tree,
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(name_node) = object_creation_class_node(node) else {
        return;
    };
    let Some((_, sym)) =
        resolve_reference_symbol_at_node(tree, source, name_node, file_symbols, index)
    else {
        return;
    };

    if sym.kind == php_lsp_types::PhpSymbolKind::Method && sym.name == "__construct" {
        check_call_argument_types(
            node,
            &sym,
            source,
            file_symbols,
            index,
            utf16_index,
            diagnostics,
        );
    }
}

fn check_call_argument_types(
    call_node: tree_sitter::Node,
    callable: &php_lsp_types::SymbolInfo,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(signature) = callable.signature.as_ref() else {
        return;
    };

    let callable_file_symbols = index.file_symbols.get(&callable.uri);
    let expected_file_symbols = callable_file_symbols
        .as_ref()
        .map(|entry| entry.value())
        .unwrap_or(file_symbols);

    let arguments = call_arguments(call_node, source);
    for (arg_index, arg) in arguments.into_iter().enumerate() {
        let Some(param) = signature_param_for_call_arg(signature, arg_index, arg.name.as_deref())
        else {
            continue;
        };
        let Some(expected) = param.type_info.as_ref() else {
            continue;
        };
        let Some(actual) = infer_expression_type(arg.value_node, source, file_symbols) else {
            continue;
        };

        if !type_info_accepts_inferred_type(expected, &actual, expected_file_symbols, index) {
            diagnostics.push(diagnostic_at_byte_range(
                actual.range,
                utf16_index,
                format!(
                    "Type mismatch for {} argument ${}: expected {}, got {}",
                    callable.fqn, param.name, expected, actual.display
                ),
            ));
        }
    }
}

fn check_return_type_compatibility(
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if node_inside_anonymous_class_body(node, source) {
        return;
    }

    let Some(expr_node) = return_expression_node(node) else {
        return;
    };
    let Some(callable) = containing_callable_symbol(file_symbols, node_range_node(node)) else {
        return;
    };
    let Some(signature) = callable.signature.as_ref() else {
        return;
    };
    let Some(expected) = signature.return_type.as_ref() else {
        return;
    };
    if matches!(
        expected,
        php_lsp_types::TypeInfo::Void | php_lsp_types::TypeInfo::Mixed
    ) {
        return;
    }
    let Some(actual) = infer_expression_type(expr_node, source, file_symbols) else {
        return;
    };

    if !type_info_accepts_inferred_type(expected, &actual, file_symbols, index) {
        diagnostics.push(diagnostic_at_byte_range(
            actual.range,
            utf16_index,
            format!(
                "Return type mismatch in {}: expected {}, got {}",
                callable.fqn, expected, actual.display
            ),
        ));
    }
}

fn check_property_assignment_type_compatibility(
    tree: &tree_sitter::Tree,
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(left_node) = node
        .child_by_field_name("left")
        .or_else(|| node.named_child(0))
    else {
        return;
    };
    if !matches!(
        left_node.kind(),
        "member_access_expression" | "scoped_property_access_expression"
    ) {
        return;
    }
    let Some(right_node) = node
        .child_by_field_name("right")
        .or_else(|| node.named_child(1))
    else {
        return;
    };
    let Some(name_node) = member_reference_name_node(left_node) else {
        return;
    };
    let Some((_, property)) =
        resolve_reference_symbol_at_node(tree, source, name_node, file_symbols, index)
    else {
        return;
    };

    if property.kind != php_lsp_types::PhpSymbolKind::Property {
        return;
    }
    let Some(expected) = property
        .signature
        .as_ref()
        .and_then(|signature| signature.return_type.as_ref())
    else {
        return;
    };
    let Some(actual) = infer_expression_type(right_node, source, file_symbols) else {
        return;
    };

    let property_file_symbols = index.file_symbols.get(&property.uri);
    let expected_file_symbols = property_file_symbols
        .as_ref()
        .map(|entry| entry.value())
        .unwrap_or(file_symbols);

    if !type_info_accepts_inferred_type(expected, &actual, expected_file_symbols, index) {
        diagnostics.push(diagnostic_at_byte_range(
            actual.range,
            utf16_index,
            format!(
                "Property assignment type mismatch for {}: expected {}, got {}",
                property.fqn, expected, actual.display
            ),
        ));
    }
}

fn resolve_reference_symbol_at_node(
    tree: &tree_sitter::Tree,
    source: &str,
    node: tree_sitter::Node,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
) -> Option<(SymbolAtPosition, Arc<php_lsp_types::SymbolInfo>)> {
    let pos = node.start_position();
    let member_type_resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
        resolve_member_type_from_index(index, class_fqn, member_name)
    };
    let sym_at_pos = symbol_at_position_with_resolver(
        tree,
        source,
        pos.row as u32,
        pos.column as u32,
        file_symbols,
        Some(&member_type_resolver),
    )?;
    let resolved = index.resolve_fqn(&sym_at_pos.fqn)?;
    Some((sym_at_pos, resolved))
}

#[derive(Debug, Clone)]
struct CallArgument<'tree> {
    value_node: tree_sitter::Node<'tree>,
    name: Option<String>,
}

fn call_arguments<'tree>(
    call_node: tree_sitter::Node<'tree>,
    source: &str,
) -> Vec<CallArgument<'tree>> {
    let Some(arguments) = call_node.child_by_field_name("arguments").or_else(|| {
        let mut cursor = call_node.walk();
        let arguments = call_node
            .children(&mut cursor)
            .find(|child| child.kind() == "arguments");
        arguments
    }) else {
        return Vec::new();
    };

    let mut result = Vec::new();
    let mut cursor = arguments.walk();
    for child in arguments.named_children(&mut cursor) {
        if child.kind() == "argument" {
            result.push(CallArgument {
                value_node: argument_value_node(child).unwrap_or(child),
                name: argument_name(child, source),
            });
        }
    }
    result
}

fn argument_value_node(argument: tree_sitter::Node) -> Option<tree_sitter::Node> {
    argument.child_by_field_name("value").or_else(|| {
        let mut cursor = argument.walk();
        argument.named_children(&mut cursor).last()
    })
}

fn argument_name(argument: tree_sitter::Node, source: &str) -> Option<String> {
    if let Some(name_node) = argument.child_by_field_name("name") {
        return Some(normalize_argument_name(node_text(source, name_node)));
    }

    let text = node_text(source, argument);
    let colon_index = text.find(':')?;
    let value_start = argument_value_node(argument)
        .map(|value| value.start_byte().saturating_sub(argument.start_byte()))
        .unwrap_or(text.len());

    (colon_index < value_start).then(|| normalize_argument_name(&text[..colon_index]))
}

fn normalize_argument_name(name: &str) -> String {
    name.trim()
        .trim_start_matches('$')
        .trim_end_matches(':')
        .trim()
        .to_string()
}

fn signature_param_for_arg(
    signature: &php_lsp_types::Signature,
    arg_index: usize,
) -> Option<&php_lsp_types::ParamInfo> {
    signature
        .params
        .get(arg_index)
        .or_else(|| signature.params.last().filter(|param| param.is_variadic))
}

fn signature_param_for_call_arg<'a>(
    signature: &'a php_lsp_types::Signature,
    arg_index: usize,
    name: Option<&str>,
) -> Option<&'a php_lsp_types::ParamInfo> {
    if let Some(name) = name {
        return signature.params.iter().find(|param| {
            param
                .name
                .trim_start_matches('$')
                .eq_ignore_ascii_case(name)
        });
    }

    signature_param_for_arg(signature, arg_index)
}

fn return_expression_node(node: tree_sitter::Node) -> Option<tree_sitter::Node> {
    node.child_by_field_name("value")
        .or_else(|| node.named_child(0))
}

fn object_creation_class_node(node: tree_sitter::Node) -> Option<tree_sitter::Node> {
    let mut cursor = node.walk();
    let class_node = node.named_children(&mut cursor).find(|child| {
        matches!(
            child.kind(),
            "name" | "qualified_name" | "namespace_name" | "relative_scope"
        )
    });
    class_node
}

fn containing_callable_symbol(
    file_symbols: &php_lsp_types::FileSymbols,
    range: (u32, u32, u32, u32),
) -> Option<&php_lsp_types::SymbolInfo> {
    file_symbols
        .symbols
        .iter()
        .filter(|sym| {
            matches!(
                sym.kind,
                php_lsp_types::PhpSymbolKind::Function | php_lsp_types::PhpSymbolKind::Method
            ) && byte_range_contains(sym.range, range)
        })
        .min_by_key(|sym| {
            (
                sym.range.2.saturating_sub(sym.range.0),
                sym.range.3.saturating_sub(sym.range.1),
            )
        })
}

fn infer_expression_type(
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
) -> Option<InferredExprType> {
    let node = normalized_expression_node(node);
    let raw = source[node.byte_range()].trim();
    let lower = raw.to_ascii_lowercase();
    let kind = node.kind();
    let range = node_range_node(node);

    if kind.contains("conditional") || raw.contains(" ? ") {
        return None;
    }

    if kind == "object_creation_expression" {
        let class_node = object_creation_class_node(node)?;
        let class_name = source[class_node.byte_range()].trim();
        let fqn = resolve_class_name_pub(class_name, file_symbols);
        return Some(InferredExprType {
            display: fqn.clone(),
            comparable: fqn,
            range,
        });
    }

    if lower == "null" {
        return Some(inferred_builtin_type("null", range));
    }
    if lower == "true" || lower == "false" {
        return Some(inferred_builtin_type(&lower, range));
    }
    if raw.starts_with('"') || raw.starts_with('\'') || kind.contains("string") {
        return Some(inferred_builtin_type("string", range));
    }
    if kind.contains("array") || raw.starts_with('[') || lower.starts_with("array(") {
        return Some(inferred_builtin_type("array", range));
    }

    let numeric = lower.trim_start_matches(['+', '-']);
    if kind.contains("float") || numeric.parse::<f64>().is_ok() && numeric.contains('.') {
        return Some(inferred_builtin_type("float", range));
    }
    if kind.contains("integer") || numeric.parse::<i64>().is_ok() {
        return Some(inferred_builtin_type("int", range));
    }

    None
}

fn normalized_expression_node(mut node: tree_sitter::Node) -> tree_sitter::Node {
    loop {
        match node.kind() {
            "argument" => {
                let Some(inner) = argument_value_node(node) else {
                    return node;
                };
                node = inner;
            }
            "parenthesized_expression" | "unary_op_expression" => {
                let Some(inner) = node.named_child(0) else {
                    return node;
                };
                node = inner;
            }
            _ => return node,
        }
    }
}

fn inferred_builtin_type(name: &str, range: (u32, u32, u32, u32)) -> InferredExprType {
    InferredExprType {
        display: name.to_string(),
        comparable: name.to_string(),
        range,
    }
}

fn type_info_accepts_inferred_type(
    expected: &php_lsp_types::TypeInfo,
    actual: &InferredExprType,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
) -> bool {
    match expected {
        php_lsp_types::TypeInfo::Mixed => true,
        php_lsp_types::TypeInfo::Nullable(inner) => {
            actual.comparable == "null"
                || type_info_accepts_inferred_type(inner, actual, file_symbols, index)
        }
        php_lsp_types::TypeInfo::Union(types) => types.iter().any(|type_info| {
            type_info_accepts_inferred_type(type_info, actual, file_symbols, index)
        }),
        php_lsp_types::TypeInfo::Intersection(_) => true,
        php_lsp_types::TypeInfo::Simple(name) => {
            simple_type_accepts_inferred_type(name, actual, file_symbols, index)
        }
        php_lsp_types::TypeInfo::Generic { base, .. } => {
            simple_type_accepts_inferred_type(base, actual, file_symbols, index)
        }
        php_lsp_types::TypeInfo::ArrayShape(_) => actual.comparable == "array",
        php_lsp_types::TypeInfo::Callable { .. } => actual.comparable == "callable",
        php_lsp_types::TypeInfo::ClassString(_) => actual.comparable == "string",
        php_lsp_types::TypeInfo::LiteralString(value)
        | php_lsp_types::TypeInfo::LiteralInt(value)
        | php_lsp_types::TypeInfo::LiteralFloat(value) => actual.comparable == value.as_str(),
        php_lsp_types::TypeInfo::LiteralBool(value) => {
            actual.comparable == if *value { "true" } else { "false" }
        }
        php_lsp_types::TypeInfo::LiteralNull => actual.comparable == "null",
        php_lsp_types::TypeInfo::Self_
        | php_lsp_types::TypeInfo::Static_
        | php_lsp_types::TypeInfo::Parent_ => true,
        php_lsp_types::TypeInfo::Void | php_lsp_types::TypeInfo::Never => false,
    }
}

fn simple_type_accepts_inferred_type(
    expected: &str,
    actual: &InferredExprType,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
) -> bool {
    let expected_lower = expected.trim_start_matches('\\').to_ascii_lowercase();
    let actual_lower = actual
        .comparable
        .trim_start_matches('\\')
        .to_ascii_lowercase();

    match expected_lower.as_str() {
        "mixed" => true,
        "string" => actual_lower == "string",
        "int" => actual_lower == "int",
        "float" => actual_lower == "float" || actual_lower == "int",
        "bool" => matches!(actual_lower.as_str(), "bool" | "true" | "false"),
        "false" => actual_lower == "false",
        "true" => actual_lower == "true",
        "null" => actual_lower == "null",
        "array" => actual_lower == "array",
        "iterable" => actual_lower == "array",
        "object" => !is_builtin_comparable_type(&actual_lower),
        "callable" => true,
        "void" | "never" => false,
        _ => {
            let expected_fqn = if expected.starts_with('\\') {
                expected.trim_start_matches('\\').to_string()
            } else {
                resolve_class_name_pub(expected, file_symbols)
            };
            let actual_fqn = actual.comparable.trim_start_matches('\\');
            expected_fqn == actual_fqn
                || class_extends_or_implements(index, actual_fqn, &expected_fqn, &mut Vec::new())
        }
    }
}

fn is_builtin_comparable_type(name: &str) -> bool {
    matches!(
        name,
        "array" | "bool" | "false" | "float" | "int" | "null" | "string" | "true"
    )
}

fn node_range_node(node: tree_sitter::Node) -> (u32, u32, u32, u32) {
    let start = node.start_position();
    let end = node.end_position();
    (
        start.row as u32,
        start.column as u32,
        end.row as u32,
        end.column as u32,
    )
}

fn override_signature_diagnostics(
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for class_sym in file_symbols.symbols.iter().filter(|sym| {
        matches!(
            sym.kind,
            php_lsp_types::PhpSymbolKind::Class
                | php_lsp_types::PhpSymbolKind::Interface
                | php_lsp_types::PhpSymbolKind::Trait
        )
    }) {
        let child_methods: Vec<_> = file_symbols
            .symbols
            .iter()
            .filter(|sym| {
                sym.kind == php_lsp_types::PhpSymbolKind::Method
                    && sym.parent_fqn.as_deref() == Some(class_sym.fqn.as_str())
            })
            .collect();

        for child_method in child_methods {
            if child_method.name == "__construct" {
                continue;
            }

            let mut reported = false;
            for parent_fqn in class_sym.extends.iter().chain(class_sym.implements.iter()) {
                let parent_member_fqn = format!("{}::{}", parent_fqn, child_method.name);
                let Some(parent_method) = index.resolve_fqn(&parent_member_fqn) else {
                    continue;
                };
                if parent_method.kind != php_lsp_types::PhpSymbolKind::Method {
                    continue;
                }
                let parent_file_symbols_guard = index.file_symbols.get(&parent_method.uri);
                let parent_file_symbols: &php_lsp_types::FileSymbols =
                    match parent_file_symbols_guard.as_ref() {
                        Some(entry) => entry.value(),
                        None => file_symbols,
                    };
                if !override_signatures_are_compatible(
                    child_method,
                    &parent_method,
                    file_symbols,
                    parent_file_symbols,
                    index,
                ) {
                    diagnostics.push(diagnostic_at_byte_range(
                        child_method.selection_range,
                        utf16_index,
                        format!(
                            "Incompatible override signature: {} differs from {}",
                            child_method.fqn, parent_method.fqn
                        ),
                    ));
                    reported = true;
                    break;
                }
            }
            if reported {
                continue;
            }
        }
    }

    diagnostics
}

fn override_signatures_are_compatible(
    child_method: &php_lsp_types::SymbolInfo,
    parent_method: &php_lsp_types::SymbolInfo,
    child_file_symbols: &php_lsp_types::FileSymbols,
    parent_file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
) -> bool {
    let (Some(child_sig), Some(parent_sig)) = (
        child_method.signature.as_ref(),
        parent_method.signature.as_ref(),
    ) else {
        return true;
    };

    if child_sig.params.len() < parent_sig.params.len() {
        return false;
    }
    if child_sig
        .params
        .iter()
        .skip(parent_sig.params.len())
        .any(|param| !signature_param_is_optional(param))
    {
        return false;
    }

    for (child_param, parent_param) in child_sig.params.iter().zip(parent_sig.params.iter()) {
        if child_param.is_variadic != parent_param.is_variadic
            || child_param.is_by_ref != parent_param.is_by_ref
            || (parent_param.default_value.is_some() && child_param.default_value.is_none())
            || !override_param_type_is_compatible(
                child_param.type_info.as_ref(),
                parent_param.type_info.as_ref(),
                child_file_symbols,
                parent_file_symbols,
                child_method.parent_fqn.as_deref(),
                parent_method.parent_fqn.as_deref(),
                index,
            )
        {
            return false;
        }
    }

    match (&child_sig.return_type, &parent_sig.return_type) {
        (Some(child_return), Some(parent_return)) => override_return_type_is_compatible(
            child_return,
            parent_return,
            child_file_symbols,
            parent_file_symbols,
            child_method.parent_fqn.as_deref(),
            parent_method.parent_fqn.as_deref(),
            index,
        ),
        (None, Some(_)) => false,
        _ => true,
    }
}

fn signature_param_is_optional(param: &php_lsp_types::ParamInfo) -> bool {
    param.default_value.is_some() || param.is_variadic
}

fn override_param_type_is_compatible(
    child_type: Option<&php_lsp_types::TypeInfo>,
    parent_type: Option<&php_lsp_types::TypeInfo>,
    child_file_symbols: &php_lsp_types::FileSymbols,
    parent_file_symbols: &php_lsp_types::FileSymbols,
    child_owner_fqn: Option<&str>,
    parent_owner_fqn: Option<&str>,
    _index: &WorkspaceIndex,
) -> bool {
    match (child_type, parent_type) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(child_type), Some(parent_type)) => {
            type_info_is_mixed(child_type)
                || normalized_type_info_for_override(
                    child_type,
                    child_file_symbols,
                    child_owner_fqn,
                ) == normalized_type_info_for_override(
                    parent_type,
                    parent_file_symbols,
                    parent_owner_fqn,
                )
        }
    }
}

fn override_return_type_is_compatible(
    child_type: &php_lsp_types::TypeInfo,
    parent_type: &php_lsp_types::TypeInfo,
    child_file_symbols: &php_lsp_types::FileSymbols,
    parent_file_symbols: &php_lsp_types::FileSymbols,
    child_owner_fqn: Option<&str>,
    parent_owner_fqn: Option<&str>,
    index: &WorkspaceIndex,
) -> bool {
    if type_info_is_mixed(parent_type) {
        return true;
    }

    let child_normalized =
        normalized_type_info_for_override(child_type, child_file_symbols, child_owner_fqn);
    let parent_normalized =
        normalized_type_info_for_override(parent_type, parent_file_symbols, parent_owner_fqn);
    if child_normalized == parent_normalized {
        return true;
    }

    match (
        simple_class_fqn_for_override(child_type, child_file_symbols, child_owner_fqn),
        simple_class_fqn_for_override(parent_type, parent_file_symbols, parent_owner_fqn),
    ) {
        (Some(child_fqn), Some(parent_fqn)) => {
            class_extends_or_implements(index, &child_fqn, &parent_fqn, &mut Vec::new())
        }
        _ => false,
    }
}

fn type_info_is_mixed(type_info: &php_lsp_types::TypeInfo) -> bool {
    match type_info {
        php_lsp_types::TypeInfo::Mixed => true,
        php_lsp_types::TypeInfo::Simple(name) => name.eq_ignore_ascii_case("mixed"),
        _ => false,
    }
}

fn normalized_type_info_for_override(
    type_info: &php_lsp_types::TypeInfo,
    file_symbols: &php_lsp_types::FileSymbols,
    owner_fqn: Option<&str>,
) -> String {
    match type_info {
        php_lsp_types::TypeInfo::Simple(name) => {
            normalized_simple_type_for_override(name, file_symbols, owner_fqn)
        }
        php_lsp_types::TypeInfo::Union(types) => {
            let mut parts: Vec<_> = types
                .iter()
                .map(|type_info| {
                    normalized_type_info_for_override(type_info, file_symbols, owner_fqn)
                })
                .collect();
            parts.sort();
            format!("union({})", parts.join("|"))
        }
        php_lsp_types::TypeInfo::Intersection(types) => {
            let mut parts: Vec<_> = types
                .iter()
                .map(|type_info| {
                    normalized_type_info_for_override(type_info, file_symbols, owner_fqn)
                })
                .collect();
            parts.sort();
            format!("intersection({})", parts.join("&"))
        }
        php_lsp_types::TypeInfo::Nullable(inner) => format!(
            "?{}",
            normalized_type_info_for_override(inner, file_symbols, owner_fqn)
        ),
        php_lsp_types::TypeInfo::Generic { base, args } => {
            let base = normalized_simple_type_for_override(base, file_symbols, owner_fqn);
            let args = args
                .iter()
                .map(|type_info| {
                    normalized_type_info_for_override(type_info, file_symbols, owner_fqn)
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("{}<{}>", base, args)
        }
        php_lsp_types::TypeInfo::ArrayShape(_)
        | php_lsp_types::TypeInfo::Callable { .. }
        | php_lsp_types::TypeInfo::ClassString(_)
        | php_lsp_types::TypeInfo::LiteralString(_)
        | php_lsp_types::TypeInfo::LiteralInt(_)
        | php_lsp_types::TypeInfo::LiteralFloat(_)
        | php_lsp_types::TypeInfo::LiteralBool(_)
        | php_lsp_types::TypeInfo::LiteralNull => type_info.to_string(),
        php_lsp_types::TypeInfo::Void => "void".to_string(),
        php_lsp_types::TypeInfo::Never => "never".to_string(),
        php_lsp_types::TypeInfo::Mixed => "mixed".to_string(),
        php_lsp_types::TypeInfo::Self_ | php_lsp_types::TypeInfo::Static_ => owner_fqn
            .map(|owner| owner.trim_start_matches('\\').to_string())
            .unwrap_or_else(|| type_info.to_string()),
        php_lsp_types::TypeInfo::Parent_ => "parent".to_string(),
    }
}

fn normalized_simple_type_for_override(
    name: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    owner_fqn: Option<&str>,
) -> String {
    let lower = name.trim_start_matches('\\').to_ascii_lowercase();
    if matches!(lower.as_str(), "self" | "static") {
        return owner_fqn
            .map(|owner| owner.trim_start_matches('\\').to_string())
            .unwrap_or(lower);
    }
    if lower == "parent" {
        return lower;
    }
    if is_builtin_type_name(name) {
        return lower;
    }
    resolve_class_name_pub(name, file_symbols)
        .trim_start_matches('\\')
        .to_string()
}

fn simple_class_fqn_for_override(
    type_info: &php_lsp_types::TypeInfo,
    file_symbols: &php_lsp_types::FileSymbols,
    owner_fqn: Option<&str>,
) -> Option<String> {
    match type_info {
        php_lsp_types::TypeInfo::Simple(name) if !is_builtin_type_name(name) => {
            let lower = name.trim_start_matches('\\').to_ascii_lowercase();
            if matches!(lower.as_str(), "self" | "static") {
                return owner_fqn.map(|owner| owner.trim_start_matches('\\').to_string());
            }
            if lower == "parent" {
                return None;
            }
            Some(
                resolve_class_name_pub(name, file_symbols)
                    .trim_start_matches('\\')
                    .to_string(),
            )
        }
        php_lsp_types::TypeInfo::Self_ | php_lsp_types::TypeInfo::Static_ => {
            owner_fqn.map(|owner| owner.trim_start_matches('\\').to_string())
        }
        php_lsp_types::TypeInfo::Generic { base, .. } if !is_builtin_type_name(base) => Some(
            resolve_class_name_pub(base, file_symbols)
                .trim_start_matches('\\')
                .to_string(),
        ),
        _ => None,
    }
}

fn php_version_type_diagnostics(
    tree: &tree_sitter::Tree,
    source: &str,
    php_version: PhpVersion,
    utf16_index: &Utf16LineIndex,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    walk_php_version_type_diagnostics(
        tree.root_node(),
        source,
        php_version,
        utf16_index,
        &mut diagnostics,
    );
    diagnostics
}

fn walk_php_version_type_diagnostics(
    node: tree_sitter::Node,
    source: &str,
    php_version: PhpVersion,
    utf16_index: &Utf16LineIndex,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for (field_name, is_return_type) in [("type", false), ("return_type", true)] {
        if let Some(type_node) = node.child_by_field_name(field_name) {
            check_declared_type_php_version(
                type_node,
                source,
                php_version,
                is_return_type,
                utf16_index,
                diagnostics,
            );
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_php_version_type_diagnostics(child, source, php_version, utf16_index, diagnostics);
    }
}

fn check_declared_type_php_version(
    type_node: tree_sitter::Node,
    source: &str,
    php_version: PhpVersion,
    is_return_type: bool,
    utf16_index: &Utf16LineIndex,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let type_text = source[type_node.byte_range()].trim();
    if declared_type_hint_is_supported(type_text, php_version, is_return_type) {
        return;
    }

    diagnostics.push(diagnostic_at_byte_range(
        node_range_node(type_node),
        utf16_index,
        format!(
            "Type is not supported by PHP {}: {}",
            php_version_label(php_version),
            type_text
        ),
    ));
}

fn declared_type_hint_is_supported(
    type_text: &str,
    php_version: PhpVersion,
    is_return_type: bool,
) -> bool {
    let trimmed = type_text.trim();
    if let Some(inner) = trimmed.strip_prefix('?') {
        return php_version.at_least(7, 1)
            && !inner.contains(['|', '&'])
            && simple_declared_type_hint_is_supported(inner, php_version, false, is_return_type);
    }

    if trimmed.contains('|') {
        return php_version.at_least(8, 0)
            && trimmed.split('|').all(|part| {
                let part = part.trim();
                !matches!(part.to_ascii_lowercase().as_str(), "void" | "never")
                    && simple_declared_type_hint_is_supported(
                        part,
                        php_version,
                        true,
                        is_return_type,
                    )
            });
    }

    if trimmed.contains('&') {
        return php_version.at_least(8, 1)
            && trimmed
                .split('&')
                .all(|part| intersection_declared_type_hint_is_supported(part.trim()));
    }

    simple_declared_type_hint_is_supported(trimmed, php_version, false, is_return_type)
}

fn simple_declared_type_hint_is_supported(
    type_text: &str,
    php_version: PhpVersion,
    in_union: bool,
    is_return_type: bool,
) -> bool {
    let lower = type_text
        .trim()
        .trim_start_matches('\\')
        .to_ascii_lowercase();
    match lower.as_str() {
        "" => false,
        "void" => is_return_type && php_version.at_least(7, 1),
        "never" => is_return_type && php_version.at_least(8, 1),
        "mixed" => php_version.at_least(8, 0),
        "static" => is_return_type && php_version.at_least(8, 0),
        "false" | "null" => {
            if in_union {
                php_version.at_least(8, 0)
            } else {
                php_version.at_least(8, 2)
            }
        }
        "true" => php_version.at_least(8, 2),
        "resource" => false,
        _ => true,
    }
}

fn intersection_declared_type_hint_is_supported(type_text: &str) -> bool {
    let lower = type_text
        .trim()
        .trim_start_matches('\\')
        .to_ascii_lowercase();
    !matches!(
        lower.as_str(),
        "" | "array"
            | "bool"
            | "callable"
            | "false"
            | "float"
            | "int"
            | "iterable"
            | "mixed"
            | "never"
            | "null"
            | "object"
            | "resource"
            | "string"
            | "true"
            | "void"
    )
}

fn php_version_label(php_version: PhpVersion) -> String {
    format!("{}.{}", php_version.major, php_version.minor)
}

fn workspace_duplicate_symbol_diagnostics(
    uri_str: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for sym in &file_symbols.symbols {
        if sym.modifiers.is_builtin || !is_duplicate_checked_symbol_kind(sym.kind) {
            continue;
        }

        let has_duplicate = index.file_symbols.iter().any(|entry| {
            entry.value().symbols.iter().any(|other| {
                other.kind == sym.kind
                    && other.fqn == sym.fqn
                    && !other.modifiers.is_builtin
                    && (entry.key().as_str() != uri_str
                        || other.selection_range != sym.selection_range)
            })
        });

        if has_duplicate {
            diagnostics.push(Diagnostic {
                range: Range {
                    start: Position::new(
                        sym.selection_range.0,
                        utf16_index.byte_col_to_utf16(sym.selection_range.0, sym.selection_range.1),
                    ),
                    end: Position::new(
                        sym.selection_range.2,
                        utf16_index.byte_col_to_utf16(sym.selection_range.2, sym.selection_range.3),
                    ),
                },
                severity: Some(DiagnosticSeverity::WARNING),
                source: Some("php-lsp".to_string()),
                message: format!("Duplicate symbol: {}", sym.fqn),
                ..Default::default()
            });
        }
    }

    diagnostics
}

fn is_duplicate_checked_symbol_kind(kind: php_lsp_types::PhpSymbolKind) -> bool {
    matches!(
        kind,
        php_lsp_types::PhpSymbolKind::Class
            | php_lsp_types::PhpSymbolKind::Interface
            | php_lsp_types::PhpSymbolKind::Trait
            | php_lsp_types::PhpSymbolKind::Enum
            | php_lsp_types::PhpSymbolKind::Function
            | php_lsp_types::PhpSymbolKind::GlobalConstant
    )
}

fn current_class_fqn(file_symbols: &php_lsp_types::FileSymbols) -> Option<String> {
    file_symbols.symbols.iter().find_map(|sym| {
        matches!(
            sym.kind,
            php_lsp_types::PhpSymbolKind::Class
                | php_lsp_types::PhpSymbolKind::Interface
                | php_lsp_types::PhpSymbolKind::Trait
                | php_lsp_types::PhpSymbolKind::Enum
        )
        .then(|| sym.fqn.clone())
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhpDocVirtualMemberKind {
    Property,
    Method,
}

#[derive(Debug, Clone)]
struct PhpDocVirtualMember {
    owner: Arc<php_lsp_types::SymbolInfo>,
    name: String,
    kind: PhpDocVirtualMemberKind,
    type_info: Option<php_lsp_types::TypeInfo>,
    access: Option<php_lsp_types::PhpDocPropertyAccess>,
    return_type: Option<php_lsp_types::TypeInfo>,
    description: Option<String>,
    is_static: bool,
}

fn phpdoc_virtual_member_for_symbol(
    index: &WorkspaceIndex,
    sym: &SymbolAtPosition,
) -> Option<PhpDocVirtualMember> {
    let kind = match sym.ref_kind {
        RefKind::PropertyAccess | RefKind::StaticPropertyAccess => {
            PhpDocVirtualMemberKind::Property
        }
        RefKind::MethodCall => PhpDocVirtualMemberKind::Method,
        _ => return None,
    };
    let (class_fqn, member_name) = sym.fqn.rsplit_once("::")?;
    let member_name = member_name.trim_start_matches('$');
    phpdoc_virtual_member(index, class_fqn, member_name, kind)
}

fn phpdoc_virtual_member(
    index: &WorkspaceIndex,
    class_fqn: &str,
    member_name: &str,
    kind: PhpDocVirtualMemberKind,
) -> Option<PhpDocVirtualMember> {
    for owner in index.get_type_hierarchy_symbols(class_fqn) {
        let Some(ref doc_comment) = owner.doc_comment else {
            continue;
        };
        let phpdoc = parse_phpdoc(doc_comment);
        match kind {
            PhpDocVirtualMemberKind::Property => {
                if let Some(property) = phpdoc
                    .properties
                    .into_iter()
                    .find(|property| property.name == member_name)
                {
                    return Some(PhpDocVirtualMember {
                        owner,
                        name: property.name,
                        kind,
                        type_info: property.type_info,
                        access: Some(property.access),
                        return_type: None,
                        description: property.description,
                        is_static: false,
                    });
                }
            }
            PhpDocVirtualMemberKind::Method => {
                if let Some(method) = phpdoc
                    .methods
                    .into_iter()
                    .find(|method| method.name == member_name)
                {
                    return Some(PhpDocVirtualMember {
                        owner,
                        name: method.name,
                        kind,
                        type_info: None,
                        access: None,
                        return_type: method.return_type,
                        description: method.description,
                        is_static: method.is_static,
                    });
                }
            }
        }
    }

    None
}

fn phpdoc_property_tag(access: php_lsp_types::PhpDocPropertyAccess) -> &'static str {
    match access {
        php_lsp_types::PhpDocPropertyAccess::ReadWrite => "@property",
        php_lsp_types::PhpDocPropertyAccess::ReadOnly => "@property-read",
        php_lsp_types::PhpDocPropertyAccess::WriteOnly => "@property-write",
    }
}

fn phpdoc_virtual_completion_data(item: &CompletionItem) -> Option<(&str, &str, &str)> {
    let data = item.data.as_ref()?;
    if data.get("kind")?.as_str()? != "phpdoc-virtual-member" {
        return None;
    }
    Some((
        data.get("ownerFqn")?.as_str()?,
        data.get("memberKind")?.as_str()?,
        data.get("memberName")?.as_str()?,
    ))
}

fn phpdoc_virtual_member_markdown(member: &PhpDocVirtualMember) -> String {
    let mut content = String::new();
    content.push_str("```php\n");
    match member.kind {
        PhpDocVirtualMemberKind::Property => {
            let access = member
                .access
                .map(phpdoc_property_tag)
                .unwrap_or("@property");
            content.push_str(access);
            if let Some(ref type_info) = member.type_info {
                content.push(' ');
                content.push_str(&type_info.to_string());
            }
            content.push_str(" $");
            content.push_str(&member.name);
        }
        PhpDocVirtualMemberKind::Method => {
            content.push_str("@method ");
            if member.is_static {
                content.push_str("static ");
            }
            if let Some(ref return_type) = member.return_type {
                content.push_str(&return_type.to_string());
                content.push(' ');
            }
            content.push_str(&member.name);
            content.push_str("()");
        }
    }
    content.push_str("\n```\n");
    if let Some(ref description) = member.description {
        content.push_str("\n---\n\n");
        content.push_str(description);
        content.push('\n');
    }
    content
}

fn phpdoc_extra_markdown_sections(phpdoc: &php_lsp_types::PhpDoc) -> Vec<String> {
    let mut sections = Vec::new();

    if let Some(ref var_type) = phpdoc.var_type {
        sections.push(format!("**@var** `{}`", var_type));
    }

    if !phpdoc.throws.is_empty() {
        let throws = phpdoc
            .throws
            .iter()
            .map(|throw_type| format!("- `{}`", throw_type))
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(format!("**Throws:**\n\n{}", throws));
    }

    if !phpdoc.properties.is_empty() {
        let properties = phpdoc
            .properties
            .iter()
            .map(|property| {
                let access = phpdoc_property_tag(property.access);
                let type_info = property
                    .type_info
                    .as_ref()
                    .map(|type_info| format!(" {}", type_info))
                    .unwrap_or_default();
                let description = property
                    .description
                    .as_ref()
                    .map(|description| format!(" - {}", description))
                    .unwrap_or_default();
                format!("- `{access}{type_info} ${}`{description}", property.name)
            })
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(format!("**PHPDoc properties:**\n\n{}", properties));
    }

    if !phpdoc.methods.is_empty() {
        let methods = phpdoc
            .methods
            .iter()
            .map(|method| {
                let static_part = if method.is_static { "static " } else { "" };
                let return_type = method
                    .return_type
                    .as_ref()
                    .map(|return_type| format!("{} ", return_type))
                    .unwrap_or_default();
                let description = method
                    .description
                    .as_ref()
                    .map(|description| format!(" - {}", description))
                    .unwrap_or_default();
                format!(
                    "- `@method {static_part}{return_type}{}()`{description}",
                    method.name
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(format!("**PHPDoc methods:**\n\n{}", methods));
    }

    sections
}

fn phpdoc_virtual_member_range(
    source: &str,
    doc_comment: &str,
    doc_start: usize,
    member: &PhpDocVirtualMember,
) -> Option<(u32, u32, u32, u32)> {
    let needle = match member.kind {
        PhpDocVirtualMemberKind::Property => format!("${}", member.name),
        PhpDocVirtualMemberKind::Method => format!("{}(", member.name),
    };
    let tag = match member.kind {
        PhpDocVirtualMemberKind::Property => "@property",
        PhpDocVirtualMemberKind::Method => "@method",
    };

    let mut line_offset = 0usize;
    for line in doc_comment.split_inclusive('\n') {
        if line.contains(tag) {
            if let Some(local_start) = line.find(&needle) {
                let name_start = if member.kind == PhpDocVirtualMemberKind::Method {
                    local_start
                } else {
                    local_start + 1
                };
                let name_end = name_start + member.name.len();
                let absolute_start = doc_start + line_offset + name_start;
                let absolute_end = doc_start + line_offset + name_end;
                return Some(byte_offsets_to_range(source, absolute_start, absolute_end));
            }
        }
        line_offset += line.len();
    }

    Some(byte_offsets_to_range(
        source,
        doc_start,
        doc_start + doc_comment.len().min(3),
    ))
}

fn byte_offsets_to_range(source: &str, start: usize, end: usize) -> (u32, u32, u32, u32) {
    let (start_line, start_col) = byte_offset_to_line_col(source, start);
    let (end_line, end_col) = byte_offset_to_line_col(source, end);
    (start_line, start_col, end_line, end_col)
}

fn byte_offset_to_line_col(source: &str, byte_offset: usize) -> (u32, u32) {
    let mut line = 0u32;
    let mut line_start = 0usize;

    for (idx, ch) in source.char_indices() {
        if idx >= byte_offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            line_start = idx + ch.len_utf8();
        }
    }

    (line, byte_offset.saturating_sub(line_start) as u32)
}

fn add_local_variable_completion_items(
    items: &mut Vec<lsp_types::CompletionItem>,
    tree: &tree_sitter::Tree,
    source: &str,
    line: u32,
    byte_col: u32,
    prefix: &str,
) {
    let prefix_lower = prefix.to_ascii_lowercase();
    let mut seen: HashSet<String> = items.iter().map(|item| item.label.clone()).collect();

    for var_name in local_variable_names_at_position(tree, source, line, byte_col) {
        let name_without_dollar = var_name.trim_start_matches('$');
        if !name_without_dollar
            .to_ascii_lowercase()
            .starts_with(&prefix_lower)
        {
            continue;
        }
        if !seen.insert(var_name.clone()) {
            continue;
        }

        items.push(lsp_types::CompletionItem {
            label: var_name.clone(),
            kind: Some(lsp_types::CompletionItemKind::VARIABLE),
            sort_text: Some(format!("0102_{}", name_without_dollar.to_ascii_lowercase())),
            filter_text: Some(format!("{} {}", var_name, name_without_dollar)),
            ..Default::default()
        });
    }
}

fn infer_new_expression_type(
    expr: &str,
    file_symbols: &php_lsp_types::FileSymbols,
) -> Option<String> {
    let expr = trim_balanced_outer_parens(expr.trim());
    let rest = expr.strip_prefix("new")?;
    if !rest.chars().next().is_some_and(char::is_whitespace) {
        return None;
    }

    let rest = rest.trim_start();
    let end = rest
        .char_indices()
        .find_map(|(idx, ch)| (!ch.is_alphanumeric() && ch != '_' && ch != '\\').then_some(idx))
        .unwrap_or(rest.len());
    let class_name = rest[..end].trim();
    if class_name.is_empty() || class_name == "class" {
        return None;
    }

    Some(
        resolve_class_name_pub(class_name, file_symbols)
            .trim_start_matches('\\')
            .to_string(),
    )
}

fn trim_balanced_outer_parens(mut text: &str) -> &str {
    loop {
        let trimmed = text.trim();
        if !trimmed.starts_with('(') || !trimmed.ends_with(')') {
            return trimmed;
        }

        let mut depth = 0usize;
        let mut encloses_whole_expr = false;
        for (idx, ch) in trimmed.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        encloses_whole_expr = idx + ch.len_utf8() == trimmed.len();
                        break;
                    }
                }
                _ => {}
            }
        }

        if !encloses_whole_expr {
            return trimmed;
        }
        text = &trimmed[1..trimmed.len() - 1];
    }
}

fn resolve_member_type_from_index(
    index: &WorkspaceIndex,
    class_fqn: &str,
    member_name: &str,
) -> Option<String> {
    let member_fqn = format!("{}::{}", class_fqn, member_name);
    tracing::debug!("resolve_member_type: looking up {}", member_fqn);

    let sym = match index.resolve_fqn(&member_fqn) {
        Some(s) => s,
        None => {
            tracing::debug!("resolve_member_type: {} not found in index", member_fqn);
            return None;
        }
    };

    symbol_return_type_fqn(index, class_fqn, &sym)
}

fn symbol_return_type_fqn(
    index: &WorkspaceIndex,
    owner_fqn: &str,
    sym: &php_lsp_types::SymbolInfo,
) -> Option<String> {
    let sig = sym.signature.as_ref()?;
    let ret = sig.return_type.as_ref()?;
    tracing::debug!("resolve_member_type: {} -> return type '{}'", sym.fqn, ret);

    type_info_fqn_from_index(index, owner_fqn, &sym.uri, ret)
}

fn type_info_fqn_from_index(
    index: &WorkspaceIndex,
    owner_fqn: &str,
    uri: &str,
    type_info: &php_lsp_types::TypeInfo,
) -> Option<String> {
    match type_info {
        php_lsp_types::TypeInfo::Simple(name) => simple_type_fqn_from_index(index, uri, name),
        php_lsp_types::TypeInfo::Nullable(inner) => {
            type_info_fqn_from_index(index, owner_fqn, uri, inner)
        }
        php_lsp_types::TypeInfo::Self_ | php_lsp_types::TypeInfo::Static_ => {
            Some(owner_fqn.to_string())
        }
        php_lsp_types::TypeInfo::Generic { base, .. } if !is_builtin_type_name(base) => {
            simple_type_fqn_from_index(index, uri, base)
        }
        php_lsp_types::TypeInfo::Union(types) | php_lsp_types::TypeInfo::Intersection(types) => {
            types
                .iter()
                .find_map(|type_info| type_info_fqn_from_index(index, owner_fqn, uri, type_info))
        }
        php_lsp_types::TypeInfo::ClassString(Some(inner)) => {
            type_info_fqn_from_index(index, owner_fqn, uri, inner)
        }
        _ => None,
    }
}

fn simple_type_fqn_from_index(
    index: &WorkspaceIndex,
    uri: &str,
    type_name: &str,
) -> Option<String> {
    let type_name = type_name.trim();
    if type_name.is_empty() || type_name == "mixed" || is_builtin_type_name(type_name) {
        return None;
    }
    if type_name.contains(['|', '&', '<', '>', '{', '}', '(', ')', ',', ' ']) {
        return None;
    }
    if type_name.contains('\\') {
        return Some(type_name.trim_start_matches('\\').to_string());
    }

    if let Some(file_syms) = index.file_symbols.get(uri) {
        Some(php_lsp_parser::resolve::resolve_class_name(
            type_name, &file_syms,
        ))
    } else {
        Some(type_name.to_string())
    }
}

fn resolve_config_path(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        normalize_path(path)
    } else {
        normalize_path(&root.join(path))
    }
}

fn path_is_excluded(path: &Path, root: &Path, exclude_paths: &[PathBuf]) -> bool {
    if exclude_paths.is_empty() {
        return false;
    }

    let absolute_path = resolve_config_path(root, path);
    let relative_path = absolute_path.strip_prefix(root).ok().map(normalize_path);

    exclude_paths.iter().any(|exclude_path| {
        if exclude_path.as_os_str().is_empty() {
            return false;
        }

        let absolute_exclude = resolve_config_path(root, exclude_path);
        if absolute_path == absolute_exclude || absolute_path.starts_with(&absolute_exclude) {
            return true;
        }

        relative_path.as_ref().is_some_and(|relative_path| {
            relative_path == exclude_path || relative_path.starts_with(exclude_path)
        })
    })
}

fn workspace_index_directories(
    root: &Path,
    namespace_map: Option<&NamespaceMap>,
    include_paths: &[PathBuf],
) -> Vec<PathBuf> {
    let mut directories: Vec<PathBuf> = namespace_map
        .map(|ns_map| {
            ns_map
                .source_directories()
                .into_iter()
                .map(Path::to_path_buf)
                .collect()
        })
        .unwrap_or_default();

    if directories.is_empty() {
        directories.push(root.to_path_buf());
    }

    for include_path in include_paths {
        push_unique_path(&mut directories, include_path.clone());
    }

    directories
}

/// Collect all .php files from the given directories.
fn collect_php_files(
    directories: &[PathBuf],
    root: &Path,
    exclude_paths: &[PathBuf],
) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for dir in directories {
        let abs_dir = if dir.is_absolute() {
            dir.to_path_buf()
        } else {
            root.join(dir)
        };
        if path_is_excluded(&abs_dir, root, exclude_paths) {
            continue;
        }
        if abs_dir.is_dir() {
            collect_php_files_recursive(&abs_dir, root, exclude_paths, &mut files);
        } else if abs_dir.extension().and_then(|e| e.to_str()) == Some("php") {
            push_unique_path(&mut files, abs_dir);
        }
    }
    files
}

/// Recursively collect .php files from a directory.
fn collect_php_files_recursive(
    dir: &Path,
    root: &Path,
    exclude_paths: &[PathBuf],
    files: &mut Vec<PathBuf>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!("Failed to read directory {}: {}", dir.display(), e);
            return;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path_is_excluded(&path, root, exclude_paths) {
            continue;
        }
        if path.is_dir() {
            // Skip hidden directories and vendor
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with('.') || name_str == "vendor" || name_str == "node_modules" {
                continue;
            }
            collect_php_files_recursive(&path, root, exclude_paths, files);
        } else if path.extension().and_then(|e| e.to_str()) == Some("php") {
            push_unique_path(files, path);
        }
    }
}

/// Convert a file:// URI to a filesystem path.
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    uri.strip_prefix("file://").map(PathBuf::from)
}

fn uri_is_php_file(uri: &Uri) -> bool {
    if let Some(path) = uri_to_path(uri.as_str()) {
        return path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("php"));
    }

    uri.as_str().to_ascii_lowercase().ends_with(".php")
}

/// Convert a file path to a file:// URI.
fn path_to_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

fn workspace_roots_from_initialize(params: &InitializeParams) -> Vec<PathBuf> {
    let mut roots = Vec::new();

    if let Some(folders) = params.workspace_folders.as_ref() {
        for folder in folders {
            if let Some(path) = uri_to_path(folder.uri.as_str()) {
                push_unique_path(&mut roots, path);
            }
        }
        if !roots.is_empty() {
            return roots;
        }
    }

    #[allow(deprecated)]
    if let Some(root) = params
        .root_uri
        .as_ref()
        .and_then(|uri| uri_to_path(uri.as_str()))
        .or_else(|| params.root_path.as_ref().map(PathBuf::from))
    {
        push_unique_path(&mut roots, root);
    }

    roots
}

fn project_config_candidates(root: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(composer_json) = find_composer_json(root) {
        if let Some(composer_root) = composer_json.parent() {
            push_unique_path(
                &mut candidates,
                composer_root.join(PROJECT_CONFIG_FILE_NAME),
            );
        }
    }

    push_unique_path(&mut candidates, root.join(PROJECT_CONFIG_FILE_NAME));
    candidates
}

fn load_effective_configuration_settings(
    workspace_roots: &[PathBuf],
    client_settings: &serde_json::Value,
) -> (serde_json::Value, Vec<String>) {
    let mut effective = serde_json::json!({});
    let mut messages = Vec::new();

    if let Some(path) = global_config_candidates()
        .into_iter()
        .find(|path| path.exists())
    {
        match load_toml_settings(&path) {
            Ok(settings) => {
                merge_json_objects(&mut effective, &settings);
                messages.push(format!("Loaded global config: {}", path.display()));
            }
            Err(message) => messages.push(message),
        }
    }

    for root in workspace_roots {
        for path in project_config_candidates(root) {
            if !path.exists() {
                continue;
            }
            match load_toml_settings(&path) {
                Ok(settings) => {
                    merge_json_objects(&mut effective, &settings);
                    messages.push(format!("Loaded project config: {}", path.display()));
                    break;
                }
                Err(message) => messages.push(message),
            }
        }
    }

    let client_settings = normalize_client_settings(client_settings);
    merge_json_objects(&mut effective, &client_settings);

    (effective, messages)
}

fn uri_is_project_config_file(uri: &Uri) -> bool {
    uri_to_path(uri.as_str())
        .and_then(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
        })
        .is_some_and(|file_name| file_name == PROJECT_CONFIG_FILE_NAME)
}

fn discover_workspace_root_config(root: &Path, composer_enabled: bool) -> WorkspaceRootConfig {
    let composer_path = composer_enabled.then(|| find_composer_json(root)).flatten();

    if let Some(ref cp) = composer_path {
        let effective_root = cp.parent().unwrap_or(root).to_path_buf();
        if effective_root != root {
            tracing::info!(
                "Found composer.json in subdirectory: {}",
                effective_root.display()
            );
        }

        return match parse_composer_json(cp) {
            Ok(namespace_map) => {
                tracing::info!(
                    "Parsed composer.json with {} PSR-4 entries",
                    namespace_map.psr4.len()
                );
                WorkspaceRootConfig {
                    root: effective_root,
                    namespace_map: Some(namespace_map),
                }
            }
            Err(e) => {
                tracing::warn!("Failed to parse composer.json: {}", e);
                WorkspaceRootConfig {
                    root: root.to_path_buf(),
                    namespace_map: None,
                }
            }
        };
    }

    if !composer_enabled {
        tracing::info!("Composer support disabled, will scan all PHP files");
    } else {
        tracing::info!("No composer.json found, will scan all PHP files");
    }

    WorkspaceRootConfig {
        root: root.to_path_buf(),
        namespace_map: None,
    }
}

fn dedup_workspace_configs(configs: Vec<WorkspaceRootConfig>) -> Vec<WorkspaceRootConfig> {
    let mut roots = Vec::new();
    let mut unique = Vec::new();

    for config in configs {
        if roots.iter().any(|root| root == &config.root) {
            continue;
        }
        roots.push(config.root.clone());
        unique.push(config);
    }

    unique
}

fn remove_indexed_files_under_roots(index: &WorkspaceIndex, roots: &[PathBuf]) -> usize {
    let uris: Vec<String> = index
        .file_symbols
        .iter()
        .filter_map(|entry| {
            let path = uri_to_path(entry.key())?;
            roots
                .iter()
                .any(|root| path.starts_with(root))
                .then(|| entry.key().clone())
        })
        .collect();

    let removed = uris.len();
    for uri in uris {
        index.remove_file(&uri);
    }

    removed
}

fn remove_indexed_file_symbols(index: &WorkspaceIndex, roots: &[PathBuf]) -> usize {
    let uris: Vec<String> = index
        .file_symbols
        .iter()
        .filter(|entry| {
            entry.key().starts_with("file://")
                && uri_to_path(entry.key())
                    .map(|path| !path_is_under_vendor_roots(&path, roots))
                    .unwrap_or(true)
        })
        .map(|entry| entry.key().clone())
        .collect();

    let removed = uris.len();
    for uri in uris {
        index.remove_file(&uri);
    }

    removed
}

fn remove_indexed_vendor_symbols(index: &WorkspaceIndex, roots: &[PathBuf]) -> usize {
    let uris: Vec<String> = index
        .file_symbols
        .iter()
        .filter_map(|entry| {
            let path = uri_to_path(entry.key())?;
            path_is_under_vendor_roots(&path, roots).then(|| entry.key().clone())
        })
        .collect();

    let removed = uris.len();
    for uri in uris {
        index.remove_file(&uri);
    }
    removed
}

fn path_is_under_vendor_roots(path: &Path, roots: &[PathBuf]) -> bool {
    roots
        .iter()
        .any(|root| path.starts_with(root.join("vendor")))
}

/// Find composer.json in the workspace root or immediate subdirectories.
///
/// Searches the root first, then scans depth-1 subdirectories (skipping hidden
/// directories and common non-project dirs like `node_modules`, `vendor`).
fn find_composer_json(root: &Path) -> Option<PathBuf> {
    // Check root first
    let in_root = root.join("composer.json");
    if in_root.exists() {
        return Some(in_root);
    }

    // Scan immediate subdirectories (depth 1)
    let entries = std::fs::read_dir(root).ok()?;
    let skip_dirs = [
        "node_modules",
        "vendor",
        ".git",
        ".github",
        "docker",
        "cache",
        "logs",
        "tmp",
    ];

    let mut candidates: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if !ft.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        // Skip hidden dirs and known non-project dirs
        if name_str.starts_with('.') || skip_dirs.contains(&name_str.as_ref()) {
            continue;
        }
        let subdir_composer = entry.path().join("composer.json");
        if subdir_composer.exists() {
            candidates.push(subdir_composer);
        }
    }

    // If exactly one found, use it; if multiple, prefer the one with autoload section
    match candidates.len() {
        0 => None,
        1 => Some(candidates.into_iter().next().unwrap()),
        _ => {
            // Prefer the candidate with the most autoload entries
            for c in &candidates {
                if let Ok(content) = std::fs::read_to_string(c) {
                    if content.contains("\"autoload\"") || content.contains("\"psr-4\"") {
                        return Some(c.clone());
                    }
                }
            }
            // Fallback to first
            Some(candidates.into_iter().next().unwrap())
        }
    }
}

fn parse_vendor_autoload_map(vendor_dir: &Path) -> Option<VendorAutoloadMap> {
    let installed_json = vendor_dir.join("composer/installed.json");
    if !installed_json.exists() {
        return None;
    }

    let content = std::fs::read_to_string(&installed_json).ok()?;
    let data: serde_json::Value = serde_json::from_str(&content).ok()?;

    // installed.json can be {"packages": [...]} or just [...]
    let packages = data
        .get("packages")
        .and_then(|p| p.as_array())
        .or_else(|| data.as_array())?;

    let mut map = VendorAutoloadMap::default();

    for pkg in packages {
        let install_path = pkg
            .get("install-path")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let pkg_dir = vendor_package_dir(vendor_dir, install_path);

        if let Some(autoload) = pkg.get("autoload") {
            append_vendor_autoload(&mut map, &pkg_dir, autoload);
        }
    }

    Some(map)
}

async fn parse_vendor_autoload_map_blocking(vendor_dir: PathBuf) -> Option<VendorAutoloadMap> {
    let path_label = vendor_dir.display().to_string();
    run_file_io_blocking("vendor autoload parse", path_label, move || {
        parse_vendor_autoload_map(&vendor_dir)
    })
    .await
    .ok()
    .flatten()
}

fn append_vendor_autoload(
    map: &mut VendorAutoloadMap,
    pkg_dir: &Path,
    autoload: &serde_json::Value,
) {
    if let Some(psr4) = autoload.get("psr-4").and_then(|v| v.as_object()) {
        for (prefix, dirs) in psr4 {
            let mut directories = Vec::new();
            match dirs {
                serde_json::Value::String(dir) => {
                    directories.push(pkg_dir.join(dir));
                }
                serde_json::Value::Array(dir_list) => {
                    for dir in dir_list {
                        if let Some(dir_str) = dir.as_str() {
                            directories.push(pkg_dir.join(dir_str));
                        }
                    }
                }
                _ => {}
            }
            if !directories.is_empty() {
                map.psr4.push(VendorPsr4Mapping {
                    prefix: prefix.clone(),
                    directories,
                });
            }
        }
    }

    if let Some(files) = autoload.get("files").and_then(|value| value.as_array()) {
        for file in files {
            if let Some(file_path) = file.as_str() {
                map.files.push(pkg_dir.join(file_path));
            }
        }
    }
}

fn vendor_package_dir(vendor_dir: &Path, install_path: &str) -> PathBuf {
    if install_path.is_empty() {
        vendor_dir.to_path_buf()
    } else if install_path.starts_with("../") {
        vendor_dir.join("composer").join(install_path)
    } else {
        vendor_dir.join(install_path)
    }
}

fn resolve_vendor_paths_from_map(fqn: &str, map: &VendorAutoloadMap) -> Option<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for mapping in &map.psr4 {
        let Some(relative) = fqn.strip_prefix(mapping.prefix.as_str()) else {
            continue;
        };
        let relative_path = relative.replace('\\', "/") + ".php";
        for directory in &mapping.directories {
            push_unique_path(&mut paths, directory.join(&relative_path));
        }
    }

    if paths.is_empty() {
        None
    } else {
        Some(paths)
    }
}

async fn cached_vendor_autoload_map(
    cache: &Arc<Mutex<VendorAutoloadCache>>,
    vendor_dir: &Path,
) -> Option<VendorAutoloadMap> {
    {
        let cache = cache.lock().await;
        if let Some(entry) = cache.by_vendor_dir.get(vendor_dir) {
            return Some(entry.map.clone());
        }
    }

    let Some(map) = parse_vendor_autoload_map_blocking(vendor_dir.to_path_buf()).await else {
        cache.lock().await.by_vendor_dir.remove(vendor_dir);
        return None;
    };

    cache.lock().await.by_vendor_dir.insert(
        vendor_dir.to_path_buf(),
        VendorAutoloadCacheEntry { map: map.clone() },
    );
    Some(map)
}

/// Try to resolve a FQN to file paths by scanning vendor/composer installed packages.
#[cfg(test)]
fn resolve_vendor_paths(fqn: &str, vendor_dir: &Path) -> Option<Vec<PathBuf>> {
    let map = parse_vendor_autoload_map(vendor_dir)?;
    resolve_vendor_paths_from_map(fqn, &map)
}

/// Convert PhpSymbolKind to the ls_types SymbolKind used by tower-lsp.
fn php_kind_to_lsp(kind: php_lsp_types::PhpSymbolKind) -> SymbolKind {
    match kind {
        php_lsp_types::PhpSymbolKind::Class => SymbolKind::CLASS,
        php_lsp_types::PhpSymbolKind::Interface => SymbolKind::INTERFACE,
        php_lsp_types::PhpSymbolKind::Trait => SymbolKind::INTERFACE,
        php_lsp_types::PhpSymbolKind::Enum => SymbolKind::ENUM,
        php_lsp_types::PhpSymbolKind::Function => SymbolKind::FUNCTION,
        php_lsp_types::PhpSymbolKind::Method => SymbolKind::METHOD,
        php_lsp_types::PhpSymbolKind::Property => SymbolKind::PROPERTY,
        php_lsp_types::PhpSymbolKind::ClassConstant => SymbolKind::CONSTANT,
        php_lsp_types::PhpSymbolKind::GlobalConstant => SymbolKind::CONSTANT,
        php_lsp_types::PhpSymbolKind::EnumCase => SymbolKind::ENUM_MEMBER,
        php_lsp_types::PhpSymbolKind::Namespace => SymbolKind::NAMESPACE,
    }
}

/// Convert lsp_types::CompletionItemKind to ls_types::CompletionItemKind.
fn lsp_completion_kind_to_ls(kind: lsp_types::CompletionItemKind) -> CompletionItemKind {
    // Both crates use the same numeric values from the LSP spec
    match kind {
        lsp_types::CompletionItemKind::TEXT => CompletionItemKind::TEXT,
        lsp_types::CompletionItemKind::METHOD => CompletionItemKind::METHOD,
        lsp_types::CompletionItemKind::FUNCTION => CompletionItemKind::FUNCTION,
        lsp_types::CompletionItemKind::CONSTRUCTOR => CompletionItemKind::CONSTRUCTOR,
        lsp_types::CompletionItemKind::FIELD => CompletionItemKind::FIELD,
        lsp_types::CompletionItemKind::VARIABLE => CompletionItemKind::VARIABLE,
        lsp_types::CompletionItemKind::CLASS => CompletionItemKind::CLASS,
        lsp_types::CompletionItemKind::INTERFACE => CompletionItemKind::INTERFACE,
        lsp_types::CompletionItemKind::MODULE => CompletionItemKind::MODULE,
        lsp_types::CompletionItemKind::PROPERTY => CompletionItemKind::PROPERTY,
        lsp_types::CompletionItemKind::UNIT => CompletionItemKind::UNIT,
        lsp_types::CompletionItemKind::VALUE => CompletionItemKind::VALUE,
        lsp_types::CompletionItemKind::ENUM => CompletionItemKind::ENUM,
        lsp_types::CompletionItemKind::KEYWORD => CompletionItemKind::KEYWORD,
        lsp_types::CompletionItemKind::SNIPPET => CompletionItemKind::SNIPPET,
        lsp_types::CompletionItemKind::COLOR => CompletionItemKind::COLOR,
        lsp_types::CompletionItemKind::FILE => CompletionItemKind::FILE,
        lsp_types::CompletionItemKind::REFERENCE => CompletionItemKind::REFERENCE,
        lsp_types::CompletionItemKind::FOLDER => CompletionItemKind::FOLDER,
        lsp_types::CompletionItemKind::ENUM_MEMBER => CompletionItemKind::ENUM_MEMBER,
        lsp_types::CompletionItemKind::CONSTANT => CompletionItemKind::CONSTANT,
        lsp_types::CompletionItemKind::STRUCT => CompletionItemKind::STRUCT,
        lsp_types::CompletionItemKind::EVENT => CompletionItemKind::EVENT,
        lsp_types::CompletionItemKind::OPERATOR => CompletionItemKind::OPERATOR,
        lsp_types::CompletionItemKind::TYPE_PARAMETER => CompletionItemKind::TYPE_PARAMETER,
        _ => CompletionItemKind::TEXT,
    }
}

fn lsp_insert_text_format_to_ls(format: lsp_types::InsertTextFormat) -> InsertTextFormat {
    if format == lsp_types::InsertTextFormat::SNIPPET {
        InsertTextFormat::SNIPPET
    } else {
        InsertTextFormat::PLAIN_TEXT
    }
}

fn lsp_position_to_ls(position: lsp_types::Position) -> Position {
    Position::new(position.line, position.character)
}

fn lsp_range_to_ls(range: lsp_types::Range) -> Range {
    Range {
        start: lsp_position_to_ls(range.start),
        end: lsp_position_to_ls(range.end),
    }
}

fn lsp_text_edit_to_ls(edit: lsp_types::TextEdit) -> TextEdit {
    TextEdit {
        range: lsp_range_to_ls(edit.range),
        new_text: edit.new_text,
    }
}

fn import_kind_for_completion_symbol(sym: &php_lsp_types::SymbolInfo) -> Option<ImportKind> {
    match sym.kind {
        php_lsp_types::PhpSymbolKind::Class
        | php_lsp_types::PhpSymbolKind::Interface
        | php_lsp_types::PhpSymbolKind::Trait
        | php_lsp_types::PhpSymbolKind::Enum => Some(ImportKind::Class),
        php_lsp_types::PhpSymbolKind::Function => Some(ImportKind::Function),
        php_lsp_types::PhpSymbolKind::GlobalConstant => Some(ImportKind::Constant),
        _ => None,
    }
}

fn symbol_is_in_current_namespace(file_symbols: &php_lsp_types::FileSymbols, fqn: &str) -> bool {
    let Some(namespace) = file_symbols.namespace.as_deref() else {
        return false;
    };
    fqn.rsplit_once('\\')
        .map(|(symbol_namespace, _)| symbol_namespace == namespace)
        .unwrap_or(false)
}

fn build_completion_auto_import_edit(
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    sym: &php_lsp_types::SymbolInfo,
) -> Option<TextEdit> {
    if sym.modifiers.is_builtin || !sym.fqn.contains('\\') {
        return None;
    }
    if symbol_is_in_current_namespace(file_symbols, &sym.fqn) {
        return None;
    }

    let import_kind = import_kind_for_completion_symbol(sym)?;
    if existing_import_for_fqn(file_symbols, &sym.fqn, import_kind).is_some() {
        return None;
    }

    let import_short_name = short_name(&sym.fqn);
    let used_aliases = used_import_aliases(file_symbols, import_kind);
    if used_aliases.contains(import_short_name) {
        return None;
    }

    let insert_line = find_use_insert_line(source, file_symbols);
    let needs_spacing =
        file_symbols.use_statements.is_empty() && !line_is_blank(source, insert_line);
    let mut new_text = build_use_statement(&sym.fqn, import_kind, None);
    new_text.push('\n');
    if needs_spacing {
        new_text.push('\n');
    }

    Some(TextEdit {
        range: Range {
            start: Position::new(insert_line, 0),
            end: Position::new(insert_line, 0),
        },
        new_text,
    })
}

fn remove_stub_symbols(index: &WorkspaceIndex) {
    let stub_uris: Vec<String> = index
        .file_symbols
        .iter()
        .filter(|entry| entry.key().starts_with("phpstub://"))
        .map(|entry| entry.key().clone())
        .collect();

    for uri in stub_uris {
        index.remove_file(&uri);
    }
}

fn candidate_stubs_paths(root: &Path, client_stubs_path: Option<PathBuf>) -> Vec<PathBuf> {
    let mut candidate_paths: Vec<PathBuf> = Vec::new();

    if let Some(path) = client_stubs_path {
        candidate_paths.push(path);
    }

    candidate_paths.push(root.join("server/data/stubs"));

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidate_paths.push(dir.join("data/stubs"));
            candidate_paths.push(
                dir.join("../stubs")
                    .canonicalize()
                    .unwrap_or_else(|_| dir.join("../stubs")),
            );
        }
    }

    candidate_paths.push(PathBuf::from("/usr/share/php-lsp/stubs"));
    candidate_paths
}

fn workspace_index_cache_config(
    root: Option<&Path>,
    php_version: PhpVersion,
    include_paths: &[PathBuf],
    exclude_paths: &[PathBuf],
    stub_extensions: &[String],
    client_stubs_path: Option<&Path>,
) -> IndexCacheConfig {
    let root = root.unwrap_or_else(|| Path::new(""));
    IndexCacheConfig {
        namespace: CacheNamespace::Workspace,
        php_lsp_version: env!("CARGO_PKG_VERSION").to_string(),
        php_version: php_version_label(php_version),
        include_paths: include_paths
            .iter()
            .map(|path| cache_path_label(path))
            .collect(),
        exclude_paths: exclude_paths
            .iter()
            .map(|path| cache_path_label(path))
            .collect(),
        stub_extensions: effective_stub_extensions(stub_extensions),
        stubs_hash: stubs_cache_hash(root, client_stubs_path, stub_extensions),
    }
}

fn stubs_index_cache_config(
    stubs_path: &Path,
    php_version: PhpVersion,
    stub_extensions: &[String],
) -> IndexCacheConfig {
    IndexCacheConfig {
        namespace: CacheNamespace::Stubs,
        php_lsp_version: env!("CARGO_PKG_VERSION").to_string(),
        php_version: php_version_label(php_version),
        include_paths: vec![cache_path_label(stubs_path)],
        exclude_paths: Vec::new(),
        stub_extensions: effective_stub_extensions(stub_extensions),
        stubs_hash: stubs_cache_hash_for_path(stubs_path, stub_extensions),
    }
}

fn vendor_index_cache_config(
    root: &Path,
    php_version: PhpVersion,
    exclude_paths: &[PathBuf],
) -> IndexCacheConfig {
    IndexCacheConfig {
        namespace: CacheNamespace::Vendor,
        php_lsp_version: env!("CARGO_PKG_VERSION").to_string(),
        php_version: php_version_label(php_version),
        include_paths: vec![cache_path_label(&root.join("vendor"))],
        exclude_paths: exclude_paths
            .iter()
            .map(|path| cache_path_label(path))
            .collect(),
        stub_extensions: Vec::new(),
        stubs_hash: vendor_cache_hash(root),
    }
}

fn cache_path_label(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn effective_stub_extensions(stub_extensions: &[String]) -> Vec<String> {
    if stub_extensions.is_empty() {
        stubs::DEFAULT_EXTENSIONS
            .iter()
            .map(|ext| (*ext).to_string())
            .collect()
    } else {
        stub_extensions.to_vec()
    }
}

fn stubs_cache_hash(
    root: &Path,
    client_stubs_path: Option<&Path>,
    stub_extensions: &[String],
) -> u64 {
    let client_stubs_path = client_stubs_path.map(Path::to_path_buf);
    if let Some(stubs_root) = candidate_stubs_paths(root, client_stubs_path)
        .into_iter()
        .find(|path| path.is_dir())
    {
        return stubs_cache_hash_for_path(&stubs_root, stub_extensions);
    }

    let mut parts = vec!["stubs-cache-v1".to_string(), "root=missing".to_string()];
    for extension in effective_stub_extensions(stub_extensions) {
        parts.push(format!("extension={}:unknown", extension));
    }
    cache::stable_hash_strings(parts.iter().map(String::as_str))
}

fn stubs_cache_hash_for_path(stubs_root: &Path, stub_extensions: &[String]) -> u64 {
    let mut parts = vec![
        "stubs-cache-v1".to_string(),
        format!("root={}", cache_path_label(stubs_root)),
    ];

    for file_name in ["composer.lock", "composer.json", "PhpStormStubsMap.php"] {
        push_metadata_hash_part(&mut parts, "file", file_name, &stubs_root.join(file_name));
    }

    for extension in effective_stub_extensions(stub_extensions) {
        let path = stubs_root.join(&extension);
        if path.exists() {
            push_metadata_hash_part(&mut parts, "extension", &extension, &path);
        } else {
            parts.push(format!("extension={}:missing", extension));
        }
    }

    cache::stable_hash_strings(parts.iter().map(String::as_str))
}

fn vendor_cache_hash(root: &Path) -> u64 {
    let mut parts = vec![
        "vendor-cache-v1".to_string(),
        format!("root={}", cache_path_label(root)),
    ];
    for relative in [
        "composer.json",
        "composer.lock",
        "vendor/composer/installed.json",
        "vendor/composer/autoload_psr4.php",
    ] {
        push_metadata_hash_part(&mut parts, "file", relative, &root.join(relative));
    }
    cache::stable_hash_strings(parts.iter().map(String::as_str))
}

fn push_metadata_hash_part(parts: &mut Vec<String>, kind: &str, label: &str, path: &Path) {
    match std::fs::metadata(path) {
        Ok(metadata) => {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| format!("{}.{:09}", duration.as_secs(), duration.subsec_nanos()))
                .unwrap_or_else(|| "unknown".to_string());
            parts.push(format!(
                "{}={}:{}:{}",
                kind,
                label,
                metadata.len(),
                modified
            ));
        }
        Err(_) => parts.push(format!("{}={}:missing", kind, label)),
    }
}

fn load_configured_stubs(
    index: &WorkspaceIndex,
    root: &Path,
    client_stubs_path: Option<PathBuf>,
    stub_extensions: Vec<String>,
    php_version: PhpVersion,
    clear_existing: bool,
) -> usize {
    if clear_existing {
        remove_stub_symbols(index);
    }

    for stubs_path in candidate_stubs_paths(root, client_stubs_path) {
        if stubs_path.is_dir() {
            tracing::info!("Loading phpstorm-stubs from {}", stubs_path.display());
            let extensions = effective_stub_extensions(&stub_extensions);
            let cache_sources = collect_stub_cache_sources(&stubs_path, &extensions);
            let cache_path = cache::cache_file_path_for_namespace(root, CacheNamespace::Stubs);
            let cache_config = stubs_index_cache_config(&stubs_path, php_version, &stub_extensions);
            let stub_php_version = stubs::StubPhpVersion {
                major: php_version.major,
                minor: php_version.minor,
            };
            let cache_report = cache::load_valid_cached_sources(
                index,
                &cache_path,
                &stubs_path,
                &cache_sources,
                &cache_config,
            );
            if let Some(reason) = cache_report.miss_reason.as_deref() {
                tracing::debug!("Stubs index cache miss: {}", reason);
            }

            let mut parsed = 0;
            for source in &cache_report.parse_sources {
                let Some(ext_name) = source.relative_path.split('/').next() else {
                    continue;
                };
                if stubs::load_stub_file_for_php_version(
                    index,
                    ext_name,
                    &source.path,
                    Some(stub_php_version),
                )
                .is_some()
                {
                    parsed += 1;
                }
            }

            let cache_to_save =
                cache::build_cache_from_sources(index, &stubs_path, &cache_sources, &cache_config);
            if let Err(e) = cache::save_cache_atomic(&cache_path, &cache_to_save) {
                tracing::warn!(
                    "Failed to save stubs index cache at {}: {}",
                    cache_path.display(),
                    e
                );
            }

            let loaded = cache_report.loaded_files + parsed;
            tracing::info!(
                "Loaded {} stub files ({} from cache, {} parsed)",
                loaded,
                cache_report.loaded_files,
                parsed
            );
            return loaded;
        }
    }

    tracing::warn!("phpstorm-stubs not found, built-in completions will be limited");
    0
}

fn collect_stub_cache_sources(stubs_path: &Path, extensions: &[String]) -> Vec<CacheSourceFile> {
    let mut sources = Vec::new();
    for extension in extensions {
        for path in stubs::collect_extension_stub_files(stubs_path, extension) {
            let file_name = path
                .file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_default();
            sources.push(CacheSourceFile::new(
                path.clone(),
                stubs::stub_file_uri(extension, &path),
                format!("{}/{}", extension, file_name),
            ));
        }
    }
    sources.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    sources
}

fn read_php_source_lossy(file_path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(file_path)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn parse_and_index_php_file(index: &WorkspaceIndex, file_path: &Path) -> bool {
    let Ok(source) = read_php_source_lossy(file_path) else {
        return false;
    };
    let mut parser = FileParser::new();
    parser.parse_full(&source);
    let Some(tree) = parser.tree() else {
        return false;
    };

    let uri = path_to_uri(file_path);
    let file_symbols = extract_file_symbols(tree, &source, &uri);
    let references = collect_symbol_references_in_file(tree, &source, &file_symbols);
    index.update_file_with_references(&uri, file_symbols, references);
    true
}

fn parse_workspace_file_for_index(file_path: PathBuf) -> WorkspaceParseResult {
    let uri = path_to_uri(&file_path);
    let source = match read_php_source_lossy(&file_path) {
        Ok(source) => source,
        Err(err) => {
            return WorkspaceParseResult {
                path: file_path,
                uri,
                file_symbols: None,
                references: Vec::new(),
                symbol_count: 0,
                error: Some(format!("failed to read file: {}", err)),
            };
        }
    };

    let mut parser = FileParser::new();
    parser.parse_full(&source);
    let Some(tree) = parser.tree() else {
        return WorkspaceParseResult {
            path: file_path,
            uri,
            file_symbols: None,
            references: Vec::new(),
            symbol_count: 0,
            error: Some("parser did not produce a syntax tree".to_string()),
        };
    };

    let file_symbols = extract_file_symbols(tree, &source, &uri);
    let references = collect_symbol_references_in_file(tree, &source, &file_symbols);
    let symbol_count = file_symbols.symbols.len();
    WorkspaceParseResult {
        path: file_path,
        uri,
        file_symbols: Some(file_symbols),
        references,
        symbol_count,
        error: None,
    }
}

async fn parse_workspace_file_for_index_blocking(
    file_path: PathBuf,
    label: &'static str,
) -> std::result::Result<WorkspaceParseResult, String> {
    let path_label = file_path.display().to_string();
    run_file_io_blocking(label, path_label, move || {
        parse_workspace_file_for_index(file_path)
    })
    .await
}

async fn parse_and_index_php_file_blocking(
    index: Arc<WorkspaceIndex>,
    file_path: PathBuf,
    label: &'static str,
) -> bool {
    let path_label = file_path.display().to_string();
    match run_file_io_blocking(label, path_label.clone(), move || {
        parse_and_index_php_file(&index, &file_path)
    })
    .await
    {
        Ok(indexed) => indexed,
        Err(message) => {
            tracing::warn!("{} failed for {}: {}", label, path_label, message);
            false
        }
    }
}

fn load_cached_vendor_file(
    index: &WorkspaceIndex,
    root: &Path,
    file_path: &Path,
    config: &IndexCacheConfig,
) -> bool {
    let source = CacheSourceFile::workspace(root, file_path);
    let cache_path = cache::cache_file_path_for_namespace(root, CacheNamespace::Vendor);
    let report = cache::load_valid_cached_sources(
        index,
        &cache_path,
        root,
        std::slice::from_ref(&source),
        config,
    );

    if report.loaded_files > 0 {
        return true;
    }
    if let Some(reason) = report.miss_reason.as_deref() {
        tracing::debug!(
            "Vendor index cache miss for {}: {}",
            file_path.display(),
            reason
        );
    }
    false
}

async fn load_cached_vendor_file_blocking(
    index: Arc<WorkspaceIndex>,
    root: PathBuf,
    file_path: PathBuf,
    config: IndexCacheConfig,
) -> bool {
    let path_label = file_path.display().to_string();
    match run_file_io_blocking("vendor cache load", path_label.clone(), move || {
        load_cached_vendor_file(&index, &root, &file_path, &config)
    })
    .await
    {
        Ok(loaded) => loaded,
        Err(message) => {
            tracing::warn!("Vendor cache load failed for {}: {}", path_label, message);
            false
        }
    }
}

async fn touch_vendor_file_lru(
    index: &WorkspaceIndex,
    vendor_file_lru: &Arc<Mutex<VendorFileLru>>,
    file_path: &Path,
) {
    let uri = path_to_uri(file_path);
    let evicted = vendor_file_lru.lock().await.touch(uri);
    for uri in evicted {
        index.remove_file(&uri);
    }
}

fn save_vendor_index_cache(index: &WorkspaceIndex, root: &Path, config: &IndexCacheConfig) {
    let sources = indexed_vendor_cache_sources(index, root);
    if sources.is_empty() {
        return;
    }

    let cache_path = cache::cache_file_path_for_namespace(root, CacheNamespace::Vendor);
    let cache_to_save = cache::build_cache_from_sources(index, root, &sources, config);
    if let Err(e) = cache::save_cache_atomic(&cache_path, &cache_to_save) {
        tracing::warn!(
            "Failed to save vendor index cache at {}: {}",
            cache_path.display(),
            e
        );
    }
}

async fn save_vendor_index_cache_blocking(
    index: Arc<WorkspaceIndex>,
    root: PathBuf,
    config: IndexCacheConfig,
) {
    let path_label = root.display().to_string();
    if let Err(message) = run_file_io_blocking("vendor cache save", path_label.clone(), move || {
        save_vendor_index_cache(&index, &root, &config)
    })
    .await
    {
        tracing::warn!("Vendor cache save failed for {}: {}", path_label, message);
    }
}

fn indexed_vendor_cache_sources(index: &WorkspaceIndex, root: &Path) -> Vec<CacheSourceFile> {
    let vendor_dir = root.join("vendor");
    let mut sources: Vec<CacheSourceFile> = index
        .file_symbols
        .iter()
        .filter_map(|entry| {
            let path = uri_to_path(entry.key())?;
            (path.starts_with(&vendor_dir) && path.is_file())
                .then(|| CacheSourceFile::workspace(root, &path))
        })
        .collect();
    sources.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    sources.dedup_by(|left, right| left.relative_path == right.relative_path);
    sources
}

async fn preload_vendor_entrypoints(
    index: Arc<WorkspaceIndex>,
    root: &Path,
    exclude_paths: &[PathBuf],
    php_version: PhpVersion,
    vendor_autoload_cache: &Arc<Mutex<VendorAutoloadCache>>,
    vendor_file_lru: &Arc<Mutex<VendorFileLru>>,
) -> usize {
    let vendor_dir = root.join("vendor");
    if !vendor_dir.is_dir() {
        return 0;
    }

    let Some(autoload) = cached_vendor_autoload_map(vendor_autoload_cache, &vendor_dir).await
    else {
        return 0;
    };
    if autoload.files.is_empty() {
        return 0;
    }

    let cache_config = vendor_index_cache_config(root, php_version, exclude_paths);
    let mut loaded = 0;
    for file_path in autoload.files.iter().take(VENDOR_PRELOAD_ENTRYPOINT_LIMIT) {
        if !file_path.is_file() || path_is_excluded(file_path, root, exclude_paths) {
            continue;
        }

        let from_cache = load_cached_vendor_file_blocking(
            index.clone(),
            root.to_path_buf(),
            file_path.clone(),
            cache_config.clone(),
        )
        .await;
        if from_cache
            || parse_and_index_php_file_blocking(
                index.clone(),
                file_path.clone(),
                "vendor preload PHP file index",
            )
            .await
        {
            touch_vendor_file_lru(&index, vendor_file_lru, file_path).await;
            loaded += 1;
        }
    }

    if loaded > 0 {
        save_vendor_index_cache_blocking(index, root.to_path_buf(), cache_config).await;
        tracing::debug!(
            "Preloaded {} vendor autoload entrypoint file(s) for {}",
            loaded,
            root.display()
        );
    }
    loaded
}

/// Background workspace indexing.
///
/// Scans PHP files in the workspace and adds their symbols to the index.
async fn index_workspace(
    client: &Client,
    index: &WorkspaceIndex,
    root: &Path,
    namespace_map: Option<&NamespaceMap>,
    options: &WorkspaceIndexingOptions,
    cancellation: &OperationCancellationToken,
) -> std::result::Result<(), String> {
    let root_label = root.display().to_string();
    let started_at = Instant::now();
    if cancellation.is_cancelled() {
        tracing::debug!("Workspace indexing cancelled before start: {}", root_label);
        return Ok(());
    }

    send_indexing_status(
        client,
        serde_json::json!({
            "phase": "discovering",
            "root": root_label,
            "message": "Discovering PHP files",
            "indexedFiles": 0,
            "indexedSymbols": 0,
            "percentage": 0
        }),
    )
    .await;

    // Create progress token
    let progress_token = ProgressToken::String(format!("php-lsp-indexing-{}", root.display()));

    // Request progress support from client (with timeout to avoid hanging if client doesn't respond)
    let progress_supported = if options.work_done_progress_supported {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.create_work_done_progress(progress_token.clone()),
        )
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false)
    } else {
        false
    };

    // Start progress reporting (Bounded with percentage)
    let ongoing = if progress_supported {
        let progress = client
            .progress(progress_token, "Indexing PHP workspace")
            .with_percentage(0)
            .with_message("Discovering files...");
        Some(progress.begin().await)
    } else {
        None
    };

    // Collect PHP files
    let source_dirs = workspace_index_directories(root, namespace_map, &options.include_paths);
    let php_files = collect_php_files(&source_dirs, root, &options.exclude_paths);
    if cancellation.is_cancelled() {
        tracing::debug!(
            "Workspace indexing cancelled after discovery: {}",
            root_label
        );
        return Ok(());
    }

    // Also add explicit files from composer.json
    let mut all_files = php_files;
    if let Some(ns_map) = namespace_map {
        for file_path in &ns_map.files {
            let abs = if file_path.is_absolute() {
                file_path.clone()
            } else {
                root.join(file_path)
            };
            if abs.exists()
                && !path_is_excluded(&abs, root, &options.exclude_paths)
                && !all_files.contains(&abs)
            {
                all_files.push(abs);
            }
        }
    }
    all_files.sort();

    let total = all_files.len();
    tracing::info!("Indexing {} PHP files", total);

    let cache_path = cache::cache_file_path(root);
    let cache_report =
        cache::load_valid_cached_files(index, &cache_path, root, &all_files, &options.cache_config);
    if cancellation.is_cancelled() {
        tracing::debug!(
            "Workspace indexing cancelled after cache load: {}",
            root_label
        );
        return Ok(());
    }
    if let Some(reason) = cache_report.miss_reason.as_deref() {
        tracing::debug!(
            "Workspace index cache miss for {}: {}",
            root.display(),
            reason
        );
    } else if cache_report.loaded_files > 0 {
        tracing::info!(
            "Loaded {} PHP files from workspace index cache for {}",
            cache_report.loaded_files,
            root.display()
        );
    }
    let files_to_parse = cache_report.parse_files.clone();
    let loaded_from_cache = cache_report.loaded_files;
    let mut indexed_symbols = cache_report.indexed_symbols;

    send_indexing_status(
        client,
        serde_json::json!({
            "phase": "indexing",
            "root": root_label,
            "message": if loaded_from_cache > 0 {
                format!(
                    "Loaded {} files from cache; indexing {} changed/missing files",
                    loaded_from_cache,
                    files_to_parse.len()
                )
            } else {
                format!("Indexing {} PHP files", total)
            },
            "indexedFiles": loaded_from_cache,
            "totalFiles": total,
            "indexedSymbols": indexed_symbols,
            "percentage": if total > 0 {
                ((loaded_from_cache as f64 / total as f64) * 100.0) as u32
            } else {
                100
            },
            "elapsedMs": elapsed_ms(started_at),
            "cacheFilesLoaded": loaded_from_cache,
            "cacheFilesStale": cache_report.stale_files,
            "cacheFilesMissing": cache_report.missing_files,
            "parseConcurrency": indexing_parse_concurrency()
        }),
    )
    .await;

    if let Some(ref p) = ongoing {
        p.report_with_message(format!("Indexing {} files...", total), 0)
            .await;
    }

    let parse_concurrency = indexing_parse_concurrency();
    let mut pending_files = files_to_parse.into_iter();
    let mut parse_tasks = JoinSet::new();
    while parse_tasks.len() < parse_concurrency {
        let Some(file_path) = pending_files.next() else {
            break;
        };
        parse_tasks.spawn_blocking(move || parse_workspace_file_for_index(file_path));
    }

    let mut done = loaded_from_cache;
    let mut parse_errors = 0usize;
    while let Some(result) = parse_tasks.join_next().await {
        if cancellation.is_cancelled() {
            parse_tasks.abort_all();
            tracing::debug!(
                "Workspace indexing cancelled after {}/{} files: {}",
                done,
                total,
                root_label
            );
            return Ok(());
        }

        let parsed = match result {
            Ok(parsed) => parsed,
            Err(err) => {
                let message = format!("Workspace indexing task failed: {}", err);
                send_indexing_status(
                    client,
                    serde_json::json!({
                        "phase": "error",
                        "root": root_label,
                        "message": message,
                        "indexedFiles": done,
                        "totalFiles": total,
                        "indexedSymbols": indexed_symbols,
                        "elapsedMs": elapsed_ms(started_at)
                    }),
                )
                .await;
                return Err(message);
            }
        };

        if let Some(file_symbols) = parsed.file_symbols {
            index.update_file_with_references(&parsed.uri, file_symbols, parsed.references);
            indexed_symbols += parsed.symbol_count;

            if parsed.symbol_count > 0 {
                tracing::debug!(
                    "Indexed {}: {} symbols",
                    parsed.path.display(),
                    parsed.symbol_count
                );
            }
        } else if let Some(error) = parsed.error {
            parse_errors += 1;
            tracing::warn!("Failed to index {}: {}", parsed.path.display(), error);
        }

        done += 1;

        while parse_tasks.len() < parse_concurrency {
            if cancellation.is_cancelled() {
                parse_tasks.abort_all();
                tracing::debug!(
                    "Workspace indexing cancelled before scheduling more parse tasks: {}",
                    root_label
                );
                return Ok(());
            }
            let Some(file_path) = pending_files.next() else {
                break;
            };
            parse_tasks.spawn_blocking(move || parse_workspace_file_for_index(file_path));
        }

        if let Some(ref p) = ongoing {
            if done % 10 == 0 || done == total {
                let percentage = if total > 0 {
                    ((done as f64 / total as f64) * 100.0) as u32
                } else {
                    100
                };
                p.report_with_message(format!("Indexed {}/{} files", done, total), percentage)
                    .await;
            }
        }
        if done % 10 == 0 || done == total {
            let percentage = if total > 0 {
                ((done as f64 / total as f64) * 100.0) as u32
            } else {
                100
            };
            send_indexing_status(
                client,
                serde_json::json!({
                    "phase": "indexing",
                    "root": root_label,
                    "message": format!("Indexed {}/{} files", done, total),
                    "indexedFiles": done,
                    "totalFiles": total,
                    "indexedSymbols": indexed_symbols,
                    "indexingErrors": parse_errors,
                    "percentage": percentage,
                    "elapsedMs": elapsed_ms(started_at),
                    "parseConcurrency": parse_concurrency
                }),
            )
            .await;
        }

        if done % 50 == 0 {
            tokio::task::yield_now().await;
        }
    }

    // End progress
    if let Some(p) = ongoing {
        p.finish_with_message(format!("Indexed {} files", total))
            .await;
    }

    let cache_to_save =
        cache::build_cache_from_index(index, root, &all_files, &options.cache_config);
    if let Err(e) = cache::save_cache_atomic(&cache_path, &cache_to_save) {
        tracing::warn!(
            "Failed to save workspace index cache at {}: {}",
            cache_path.display(),
            e
        );
    }

    send_indexing_status(
        client,
        serde_json::json!({
            "phase": "ready",
            "root": root_label,
            "message": format!("Indexed {} PHP files", total),
            "indexedFiles": total,
            "totalFiles": total,
            "indexedSymbols": indexed_symbols,
            "percentage": 100,
            "elapsedMs": elapsed_ms(started_at),
            "cacheFilesLoaded": loaded_from_cache,
            "cacheFilesStale": cache_report.stale_files,
            "cacheFilesMissing": cache_report.missing_files,
            "indexingErrors": parse_errors,
            "parseConcurrency": parse_concurrency,
            "cachePath": cache_path.display().to_string()
        }),
    )
    .await;

    client
        .log_message(
            MessageType::INFO,
            format!("php-lsp: indexed {} PHP files", total),
        )
        .await;

    tracing::info!("Workspace indexing complete: {} files", total);

    Ok(())
}

impl LanguageServer for PhpLspBackend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        tracing::info!("php-lsp: initialize");

        // Store trace level from client
        if let Some(trace) = params.trace {
            *self.trace_level.lock().await = trace;
            tracing::info!("Trace level: {:?}", trace);
        }

        *self.work_done_progress_supported.lock().await = params
            .capabilities
            .window
            .as_ref()
            .and_then(|window| window.work_done_progress)
            .unwrap_or(false);

        let workspace_roots = workspace_roots_from_initialize(&params);

        if !workspace_roots.is_empty() {
            for root in &workspace_roots {
                tracing::info!("Workspace root: {}", root.display());
            }
            *self.workspace_root.lock().await = workspace_roots.first().cloned();
            *self.workspace_roots.lock().await = workspace_roots.clone();
        }

        let client_settings = params
            .initialization_options
            .unwrap_or_else(|| serde_json::json!({}));
        *self.client_settings.lock().await = client_settings.clone();
        self.apply_effective_configuration_settings(&client_settings, &workspace_roots)
            .await;

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::INCREMENTAL),
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(false),
                        })),
                        ..Default::default()
                    },
                )),
                selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
                linked_editing_range_provider: Some(LinkedEditingRangeServerCapabilities::Simple(
                    true,
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                declaration_provider: Some(DeclarationCapability::Simple(true)),
                type_definition_provider: Some(TypeDefinitionProviderCapability::Simple(true)),
                implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
                references_provider: Some(OneOf::Left(true)),
                document_highlight_provider: Some(OneOf::Left(true)),
                call_hierarchy_provider: Some(CallHierarchyServerCapability::Simple(true)),
                inlay_hint_provider: Some(OneOf::Left(true)),
                code_lens_provider: Some(CodeLensOptions {
                    resolve_provider: Some(false),
                }),
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                document_link_provider: Some(DocumentLinkOptions {
                    resolve_provider: Some(false),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                }),
                document_symbol_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                        supported: Some(true),
                        change_notifications: Some(OneOf::Left(true)),
                    }),
                    file_operations: Some({
                        let php_files = php_file_operation_registration_options();
                        WorkspaceFileOperationsServerCapabilities {
                            did_create: Some(php_files.clone()),
                            will_create: Some(php_files.clone()),
                            did_rename: Some(php_files.clone()),
                            will_rename: None,
                            did_delete: Some(php_files),
                            will_delete: Some(php_file_operation_registration_options()),
                        }
                    }),
                }),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                })),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![
                        "$".to_string(),
                        ">".to_string(),
                        ":".to_string(),
                        "\\".to_string(),
                    ]),
                    resolve_provider: Some(true),
                    ..Default::default()
                }),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
                    retrigger_characters: Some(vec![",".to_string()]),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                }),
                code_action_provider: Some(CodeActionProviderCapability::Options(
                    CodeActionOptions {
                        code_action_kinds: Some(vec![
                            CodeActionKind::QUICKFIX,
                            CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
                            CodeActionKind::REFACTOR_REWRITE,
                        ]),
                        resolve_provider: Some(true),
                        work_done_progress_options: WorkDoneProgressOptions::default(),
                    },
                )),
                document_formatting_provider: Some(OneOf::Left(true)),
                document_range_formatting_provider: Some(OneOf::Left(true)),
                document_on_type_formatting_provider: Some(DocumentOnTypeFormattingOptions {
                    first_trigger_character: "\n".to_string(),
                    more_trigger_character: Some(vec![";".to_string(), "}".to_string()]),
                }),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            work_done_progress_options: WorkDoneProgressOptions::default(),
                            legend: semantic_tokens_legend(),
                            range: Some(true),
                            full: Some(SemanticTokensFullOptions::Delta { delta: Some(true) }),
                        },
                    ),
                ),
                experimental: Some(serde_json::json!({
                    "typeHierarchyProvider": true,
                })),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "php-lsp".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        tracing::info!("php-lsp: initialized");
        self.client
            .log_message(MessageType::INFO, "php-lsp server initialized")
            .await;

        let mut roots = self.workspace_roots.lock().await.clone();
        if roots.is_empty() {
            if let Some(root) = self.workspace_root.lock().await.clone() {
                roots.push(root);
            }
        }

        if roots.is_empty() {
            tracing::warn!("No workspace root, skipping indexing");
            send_indexing_status(
                &self.client,
                serde_json::json!({
                    "phase": "ready",
                    "message": "No workspace root",
                    "indexedFiles": 0,
                    "totalFiles": 0,
                    "indexedSymbols": 0,
                    "percentage": 100
                }),
            )
            .await;
            return;
        }

        let composer_enabled = *self.composer_enabled.lock().await;
        let configs = dedup_workspace_configs(
            roots
                .iter()
                .map(|root| discover_workspace_root_config(root, composer_enabled))
                .collect(),
        );
        let effective_roots: Vec<PathBuf> =
            configs.iter().map(|config| config.root.clone()).collect();

        if let Some(first_root) = effective_roots.first() {
            *self.workspace_root.lock().await = Some(first_root.clone());
        }
        *self.workspace_roots.lock().await = effective_roots;
        *self.workspace_configs.lock().await = configs.clone();
        *self.namespace_map.lock().await = configs
            .iter()
            .find_map(|config| config.namespace_map.clone());

        // Load phpstorm-stubs for built-in PHP functions/classes.
        let stubs_index = self.index.clone();
        let stubs_root = configs
            .first()
            .map(|config| config.root.clone())
            .unwrap_or_default();
        let stubs_root_label = stubs_root.display().to_string();
        let client_stubs_path = self.stubs_path.lock().await.clone();
        let stub_extensions = self.stub_extensions.lock().await.clone();
        let php_version = *self.php_version.lock().await;

        send_indexing_status(
            &self.client,
            serde_json::json!({
                "phase": "loadingStubs",
                "root": stubs_root_label,
                "message": "Loading PHP stubs"
            }),
        )
        .await;

        let load_client_stubs_path = client_stubs_path.clone();
        let load_stub_extensions = stub_extensions.clone();
        let loaded_stubs = tokio::task::spawn_blocking(move || {
            load_configured_stubs(
                &stubs_index,
                &stubs_root,
                load_client_stubs_path,
                load_stub_extensions,
                php_version,
                false,
            )
        })
        .await
        .unwrap_or(0);

        send_indexing_status(
            &self.client,
            serde_json::json!({
                "phase": "stubsLoaded",
                "root": stubs_root_label,
                "message": format!("Loaded {} stub files", loaded_stubs),
                "stubFiles": loaded_stubs
            }),
        )
        .await;

        let client = self.client.clone();
        let index = self.index.clone();
        let open_files = self.open_files.clone();
        let reindex_document_versions = self.document_versions.clone();
        let reindex_index = self.index.clone();
        let reindex_client = self.client.clone();
        let diagnostics_mode = *self.diagnostics_mode.lock().await;
        let diagnostic_severity = *self.diagnostic_severity.lock().await;
        let index_vendor = *self.index_vendor.lock().await;
        let vendor_autoload_cache = self.vendor_autoload_cache.clone();
        let vendor_file_lru = self.vendor_file_lru.clone();
        let work_done_progress_supported = *self.work_done_progress_supported.lock().await;
        let include_paths = self.include_paths.lock().await.clone();
        let exclude_paths = self.exclude_paths.lock().await.clone();
        let cache_config = workspace_index_cache_config(
            configs.first().map(|config| config.root.as_path()),
            php_version,
            &include_paths,
            &exclude_paths,
            &stub_extensions,
            client_stubs_path.as_deref(),
        );
        let indexing_options = WorkspaceIndexingOptions {
            include_paths,
            exclude_paths,
            cache_config,
            work_done_progress_supported,
        };
        let indexing_token = self.start_indexing_run().await;
        tokio::spawn(async move {
            for config in &configs {
                if indexing_token.is_cancelled() {
                    return;
                }
                if let Err(e) = index_workspace(
                    &client,
                    &index,
                    &config.root,
                    config.namespace_map.as_ref(),
                    &indexing_options,
                    &indexing_token,
                )
                .await
                {
                    tracing::error!("Background indexing failed: {}", e);
                    send_indexing_status(
                        &client,
                        serde_json::json!({
                            "phase": "error",
                            "root": config.root.display().to_string(),
                            "message": format!("Indexing failed: {}", e)
                        }),
                    )
                    .await;
                    client
                        .log_message(MessageType::ERROR, format!("Indexing failed: {}", e))
                        .await;
                    return;
                }
                if indexing_token.is_cancelled() {
                    return;
                }

                if index_vendor {
                    preload_vendor_entrypoints(
                        index.clone(),
                        &config.root,
                        &indexing_options.exclude_paths,
                        php_version,
                        &vendor_autoload_cache,
                        &vendor_file_lru,
                    )
                    .await;
                }
            }

            // Re-publish diagnostics for all open files now that the index is populated.
            if indexing_token.is_cancelled() {
                return;
            }
            for entry in open_files.iter() {
                let uri_str = entry.key().clone();
                if let Ok(uri) = uri_str.parse::<Uri>() {
                    let version = reindex_document_versions
                        .get(&uri_str)
                        .map(|current| *current);
                    let diags = compute_diagnostics_with_config(
                        &uri_str,
                        &entry,
                        &reindex_index,
                        diagnostics_mode,
                        diagnostic_severity,
                        php_version,
                    );
                    if reindex_document_versions
                        .get(&uri_str)
                        .map(|current| *current)
                        == version
                    {
                        reindex_client
                            .publish_diagnostics(uri, diags, version)
                            .await;
                    }
                }
            }
        });
    }

    async fn did_change_workspace_folders(&self, params: DidChangeWorkspaceFoldersParams) {
        tracing::debug!("didChangeWorkspaceFolders");

        let removed_roots: Vec<PathBuf> = params
            .event
            .removed
            .iter()
            .filter_map(|folder| uri_to_path(folder.uri.as_str()))
            .collect();
        if !removed_roots.is_empty() {
            let first_root = {
                let mut roots = self.workspace_roots.lock().await;
                roots.retain(|root| {
                    !removed_roots
                        .iter()
                        .any(|removed| root.starts_with(removed))
                });
                roots.first().cloned()
            };
            let first_namespace_map = {
                let mut configs = self.workspace_configs.lock().await;
                configs.retain(|config| {
                    !removed_roots
                        .iter()
                        .any(|removed| config.root.starts_with(removed))
                });
                configs
                    .iter()
                    .find_map(|config| config.namespace_map.clone())
            };
            *self.workspace_root.lock().await = first_root;
            *self.namespace_map.lock().await = first_namespace_map;

            let removed_files = remove_indexed_files_under_roots(&self.index, &removed_roots);
            self.client
                .log_message(
                    MessageType::INFO,
                    format!(
                        "php-lsp: removed {} indexed PHP files from detached workspace folder(s)",
                        removed_files
                    ),
                )
                .await;
        }

        let added_roots: Vec<PathBuf> = params
            .event
            .added
            .iter()
            .filter_map(|folder| uri_to_path(folder.uri.as_str()))
            .collect();
        if added_roots.is_empty() {
            return;
        }

        let composer_enabled = *self.composer_enabled.lock().await;
        let added_configs = dedup_workspace_configs(
            added_roots
                .iter()
                .map(|root| discover_workspace_root_config(root, composer_enabled))
                .collect(),
        );

        let first_root = {
            let mut roots = self.workspace_roots.lock().await;
            for config in &added_configs {
                push_unique_path(&mut roots, config.root.clone());
            }
            roots.first().cloned()
        };
        let mut workspace_root = self.workspace_root.lock().await;
        if workspace_root.is_none() {
            *workspace_root = first_root;
        }
        drop(workspace_root);

        let first_namespace_map = {
            let mut configs = self.workspace_configs.lock().await;
            for config in &added_configs {
                if !configs.iter().any(|existing| existing.root == config.root) {
                    configs.push(config.clone());
                }
            }
            configs
                .iter()
                .find_map(|config| config.namespace_map.clone())
        };
        *self.namespace_map.lock().await = first_namespace_map;

        let client = self.client.clone();
        let index = self.index.clone();
        let work_done_progress_supported = *self.work_done_progress_supported.lock().await;
        let include_paths = self.include_paths.lock().await.clone();
        let exclude_paths = self.exclude_paths.lock().await.clone();
        let php_version = *self.php_version.lock().await;
        let index_vendor = *self.index_vendor.lock().await;
        let vendor_autoload_cache = self.vendor_autoload_cache.clone();
        let vendor_file_lru = self.vendor_file_lru.clone();
        let stub_extensions = self.stub_extensions.lock().await.clone();
        let client_stubs_path = self.stubs_path.lock().await.clone();
        let cache_config = workspace_index_cache_config(
            added_configs.first().map(|config| config.root.as_path()),
            php_version,
            &include_paths,
            &exclude_paths,
            &stub_extensions,
            client_stubs_path.as_deref(),
        );
        let indexing_options = WorkspaceIndexingOptions {
            include_paths,
            exclude_paths,
            cache_config,
            work_done_progress_supported,
        };
        let indexing_token = self.start_indexing_run().await;
        tokio::spawn(async move {
            for config in &added_configs {
                if indexing_token.is_cancelled() {
                    return;
                }
                if let Err(e) = index_workspace(
                    &client,
                    &index,
                    &config.root,
                    config.namespace_map.as_ref(),
                    &indexing_options,
                    &indexing_token,
                )
                .await
                {
                    tracing::error!("Workspace folder indexing failed: {}", e);
                    send_indexing_status(
                        &client,
                        serde_json::json!({
                            "phase": "error",
                            "root": config.root.display().to_string(),
                            "message": format!("Workspace folder indexing failed: {}", e)
                        }),
                    )
                    .await;
                    client
                        .log_message(
                            MessageType::ERROR,
                            format!("Workspace folder indexing failed: {}", e),
                        )
                        .await;
                    continue;
                }
                if indexing_token.is_cancelled() {
                    return;
                }

                if index_vendor {
                    preload_vendor_entrypoints(
                        index.clone(),
                        &config.root,
                        &indexing_options.exclude_paths,
                        php_version,
                        &vendor_autoload_cache,
                        &vendor_file_lru,
                    )
                    .await;
                }
            }
        });
    }

    async fn shutdown(&self) -> Result<()> {
        tracing::info!("php-lsp: shutdown");
        Ok(())
    }

    // --- Document Synchronization ---

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let uri_str = uri.as_str().to_string();
        let text = &params.text_document.text;
        let version = params.text_document.version;

        tracing::debug!("didOpen: {}", uri_str);
        self.log_trace(&format!("didOpen: {}", uri_str)).await;
        self.document_versions.insert(uri_str.clone(), version);
        self.cancel_debounced_diagnostics(&uri_str).await;
        self.cancel_analyzer_run(&uri_str).await;

        let mut parser = FileParser::new();
        parser.parse_full(text);

        // Update index with symbols from this file
        let excluded = if let Some(path) = uri_to_path(&uri_str) {
            self.path_is_excluded_by_config(&path).await
        } else {
            false
        };
        if !excluded {
            if let Some(tree) = parser.tree() {
                let file_symbols = extract_file_symbols(tree, text, &uri_str);
                let references = collect_symbol_references_in_file(tree, text, &file_symbols);
                let sym_count = file_symbols.symbols.len();
                self.index
                    .update_file_with_references(&uri_str, file_symbols, references);
                self.log_trace(&format!("Indexed {} symbols from {}", sym_count, uri_str))
                    .await;
            }
        } else {
            self.index.remove_file(&uri_str);
        }

        self.open_files.insert(uri_str, parser);

        self.publish_diagnostics(&uri).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let uri_str = uri.as_str().to_string();
        let version = params.text_document.version;

        tracing::debug!("didChange: {} version {}", uri_str, version);
        if !self.accept_document_version(&uri_str, version) {
            return;
        }
        self.cancel_analyzer_run(&uri_str).await;

        let excluded = if let Some(path) = uri_to_path(&uri_str) {
            self.path_is_excluded_by_config(&path).await
        } else {
            false
        };

        if let Some(mut parser) = self.open_files.get_mut(&uri_str) {
            for change in &params.content_changes {
                if let Some(range) = change.range {
                    parser.apply_edit(
                        range.start.line,
                        range.start.character,
                        range.end.line,
                        range.end.character,
                        &change.text,
                    );
                } else {
                    // Full content replacement
                    parser.parse_full(&change.text);
                }
            }

            // Update index with new symbols
            if excluded {
                self.index.remove_file(&uri_str);
            } else if let Some(tree) = parser.tree() {
                let source = parser.source();
                let file_symbols = extract_file_symbols(tree, &source, &uri_str);
                let references = collect_symbol_references_in_file(tree, &source, &file_symbols);
                self.index
                    .update_file_with_references(&uri_str, file_symbols, references);
            }
        }

        self.schedule_fast_diagnostics(uri, version).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        let uri_str = uri.as_str().to_string();
        tracing::debug!("didClose: {}", uri_str);
        self.open_files.remove(&uri_str);
        self.document_versions.remove(&uri_str);
        self.cancel_debounced_diagnostics(&uri_str).await;
        self.cancel_analyzer_run(&uri_str).await;
        self.semantic_tokens_cache.lock().await.remove(&uri_str);
        // Clear diagnostics for closed file
        self.client.publish_diagnostics(uri, vec![], None).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        tracing::debug!("didSave: {}", params.text_document.uri.as_str());
        self.cancel_debounced_diagnostics(params.text_document.uri.as_str())
            .await;
        self.publish_diagnostics(&params.text_document.uri).await;
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        tracing::debug!("didChangeWatchedFiles: {} change(s)", params.changes.len());

        let mut config_changed = false;
        for event in params.changes {
            if uri_is_project_config_file(&event.uri) {
                config_changed = true;
                continue;
            }

            match event.typ {
                FileChangeType::DELETED => self.remove_php_file(&event.uri).await,
                FileChangeType::CREATED | FileChangeType::CHANGED => {
                    self.reindex_php_file(&event.uri).await
                }
                _ => {}
            }
        }

        if config_changed {
            self.reload_effective_configuration().await;
        }
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        tracing::debug!("didChangeConfiguration");

        *self.client_settings.lock().await = params.settings.clone();
        self.reload_effective_configuration().await;
    }

    async fn will_create_files(&self, _params: CreateFilesParams) -> Result<Option<WorkspaceEdit>> {
        Ok(None)
    }

    async fn did_create_files(&self, params: CreateFilesParams) {
        tracing::debug!("didCreateFiles: {} file(s)", params.files.len());

        for file in params.files {
            if let Ok(uri) = file.uri.parse::<Uri>() {
                self.reindex_php_file(&uri).await;
            }
        }
    }

    async fn will_rename_files(&self, _params: RenameFilesParams) -> Result<Option<WorkspaceEdit>> {
        Ok(None)
    }

    async fn did_rename_files(&self, params: RenameFilesParams) {
        tracing::debug!("didRenameFiles: {} file(s)", params.files.len());

        for file in params.files {
            let old_uri = file.old_uri.parse::<Uri>();
            let new_uri = file.new_uri.parse::<Uri>();
            if let (Ok(old_uri), Ok(new_uri)) = (old_uri, new_uri) {
                self.rename_php_file(&old_uri, &new_uri).await;
            }
        }
    }

    async fn will_delete_files(&self, _params: DeleteFilesParams) -> Result<Option<WorkspaceEdit>> {
        Ok(None)
    }

    async fn did_delete_files(&self, params: DeleteFilesParams) {
        tracing::debug!("didDeleteFiles: {} file(s)", params.files.len());

        for file in params.files {
            if let Ok(uri) = file.uri.parse::<Uri>() {
                self.remove_php_file(&uri).await;
            }
        }
    }

    // --- Language Features ---

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let uri_str = params.text_document.uri.as_str().to_string();
        tracing::debug!("formatting: {}", uri_str);

        let source = {
            let parser = match self.open_files.get(&uri_str) {
                Some(parser) => parser,
                None => return Ok(None),
            };
            parser.source()
        };

        let config = self.formatting_config.lock().await.clone();
        if config.command_template().is_none() {
            return Ok(None);
        }

        let workspace_root = self.workspace_root_for_uri(&uri_str).await;
        let source_for_formatter = source.clone();
        let formatted = tokio::task::spawn_blocking(move || {
            run_external_formatter(source_for_formatter, config, workspace_root)
        })
        .await
        .map_err(|err| {
            tracing::error!("Formatter task failed: {}", err);
            tower_lsp::jsonrpc::Error::internal_error()
        })?;

        let formatted = match formatted {
            Ok(Some(formatted)) => formatted,
            Ok(None) => return Ok(Some(vec![])),
            Err(message) => {
                self.client
                    .log_message(
                        MessageType::WARNING,
                        format!("php-lsp formatter failed: {}", message),
                    )
                    .await;
                return Ok(Some(vec![]));
            }
        };

        Ok(Some(vec![TextEdit {
            range: full_document_range(&source),
            new_text: formatted,
        }]))
    }

    async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let uri_str = params.text_document.uri.as_str().to_string();
        tracing::debug!("rangeFormatting: {}", uri_str);

        let source = {
            let parser = match self.open_files.get(&uri_str) {
                Some(parser) => parser,
                None => return Ok(None),
            };
            parser.source()
        };

        let Some(fragment) = text_at_lsp_range(&source, params.range) else {
            return Ok(Some(vec![]));
        };
        if fragment.is_empty() {
            return Ok(Some(vec![]));
        }

        let config = self.formatting_config.lock().await.clone();
        if config.command_template().is_none() {
            return Ok(None);
        }

        let (formatter_input, was_wrapped) = range_formatter_input(fragment);
        let workspace_root = self.workspace_root_for_uri(&uri_str).await;
        let formatted = tokio::task::spawn_blocking(move || {
            run_external_formatter(formatter_input, config, workspace_root)
        })
        .await
        .map_err(|err| {
            tracing::error!("Range formatter task failed: {}", err);
            tower_lsp::jsonrpc::Error::internal_error()
        })?;

        let formatted = match formatted {
            Ok(Some(formatted)) => strip_range_formatter_wrapper(formatted, was_wrapped),
            Ok(None) => return Ok(Some(vec![])),
            Err(message) => {
                self.client
                    .log_message(
                        MessageType::WARNING,
                        format!("php-lsp range formatter failed: {}", message),
                    )
                    .await;
                return Ok(Some(vec![]));
            }
        };

        if formatted == fragment {
            return Ok(Some(vec![]));
        }

        Ok(Some(vec![TextEdit {
            range: params.range,
            new_text: formatted,
        }]))
    }

    async fn on_type_formatting(
        &self,
        params: DocumentOnTypeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let uri_str = params
            .text_document_position
            .text_document
            .uri
            .as_str()
            .to_string();
        let position = params.text_document_position.position;
        tracing::debug!(
            "onTypeFormatting: {}:{}:{} trigger={:?}",
            uri_str,
            position.line,
            position.character,
            params.ch
        );

        if !matches!(params.ch.as_str(), "\n" | ";" | "}") {
            return Ok(Some(vec![]));
        }

        let source = {
            let parser = match self.open_files.get(&uri_str) {
                Some(parser) => parser,
                None => return Ok(None),
            };
            parser.source()
        };

        let Some(current_line) = formatting_source_line(&source, position.line) else {
            return Ok(Some(vec![]));
        };
        if params.ch == "}"
            && !current_line
                .trim_start_matches([' ', '\t'])
                .starts_with('}')
        {
            return Ok(Some(vec![]));
        }

        Ok(Some(
            on_type_indent_edit(&source, position.line, &params.options)
                .into_iter()
                .collect(),
        ))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let uri_str = uri.as_str().to_string();
        let pos = params.text_document_position_params.position;
        tracing::debug!("hover: {}:{}:{}", uri_str, pos.line, pos.character);

        // Extract symbol-at-position and local variable hover info inside a block so DashMap guard is dropped.
        let (sym_at_pos, var_hover_info) = {
            let parser = match self.open_files.get(&uri_str) {
                Some(p) => p,
                None => return Ok(None),
            };

            let tree = match parser.tree() {
                Some(t) => t,
                None => return Ok(None),
            };

            let source = parser.source();
            let byte_col = utf16_col_to_byte(&source, pos.line, pos.character);

            // Get file symbols for name resolution
            let file_symbols = self
                .index
                .file_symbols
                .get(&uri_str)
                .map(|entry| entry.value().clone())
                .unwrap_or_default();

            // Build a cross-file type resolver for method chain resolution
            let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
                self.resolve_member_type(class_fqn, member_name)
            };

            // Find symbol at cursor position (with resolver for chains)
            let sym_at_pos = match symbol_at_position_with_resolver(
                tree,
                &source,
                pos.line,
                byte_col,
                &file_symbols,
                Some(&resolver),
            ) {
                Some(s) => s,
                None => return Ok(None),
            };
            let var_hover_info = if sym_at_pos.ref_kind == RefKind::Variable {
                variable_hover_info_at_position(tree, &source, &file_symbols, pos.line, byte_col)
            } else {
                None
            };

            (sym_at_pos, var_hover_info)
        };

        // Look up symbol in index (with lazy vendor fallback)
        let symbol_info = match sym_at_pos.ref_kind {
            RefKind::Variable => None, // Variables are local, handled by gotoDefinition.
            _ => {
                let info = self
                    .resolve_fqn_lazy_with_fallback(&sym_at_pos.fqn, sym_at_pos.ref_kind)
                    .await;
                // For constructor refs, fall back to the class if __construct is
                // not explicitly defined.
                if info.is_none() && sym_at_pos.ref_kind == RefKind::Constructor {
                    if let Some(class_fqn) = sym_at_pos.fqn.strip_suffix("::__construct") {
                        self.resolve_fqn_lazy_with_fallback(class_fqn, RefKind::ClassName)
                            .await
                    } else {
                        None
                    }
                } else {
                    info
                }
            }
        };

        let virtual_member = if symbol_info.is_none() {
            phpdoc_virtual_member_for_symbol(&self.index, &sym_at_pos)
        } else {
            None
        };

        let result = if let Some(sym) = symbol_info {
            // Build hover content
            let mut content = String::new();

            // Symbol kind label
            let kind_label = match sym.kind {
                php_lsp_types::PhpSymbolKind::Class => "class",
                php_lsp_types::PhpSymbolKind::Interface => "interface",
                php_lsp_types::PhpSymbolKind::Trait => "trait",
                php_lsp_types::PhpSymbolKind::Enum => "enum",
                php_lsp_types::PhpSymbolKind::Function => "function",
                php_lsp_types::PhpSymbolKind::Method => "method",
                php_lsp_types::PhpSymbolKind::Property => "property",
                php_lsp_types::PhpSymbolKind::ClassConstant => "const",
                php_lsp_types::PhpSymbolKind::GlobalConstant => "const",
                php_lsp_types::PhpSymbolKind::EnumCase => "case",
                php_lsp_types::PhpSymbolKind::Namespace => "namespace",
            };

            // PHP code block with signature
            content.push_str("```php\n");
            if let Some(ref sig) = sym.signature {
                // Function/method signature
                content.push_str(kind_label);
                content.push(' ');
                content.push_str(&sym.fqn);
                content.push('(');
                for (i, param) in sig.params.iter().enumerate() {
                    if i > 0 {
                        content.push_str(", ");
                    }
                    if let Some(ref t) = param.type_info {
                        content.push_str(&t.to_string());
                        content.push(' ');
                    }
                    if param.is_variadic {
                        content.push_str("...");
                    }
                    if param.is_by_ref {
                        content.push('&');
                    }
                    content.push('$');
                    content.push_str(&param.name);
                    if let Some(ref def) = param.default_value {
                        content.push_str(" = ");
                        content.push_str(def);
                    }
                }
                content.push(')');
                if let Some(ref ret) = sig.return_type {
                    content.push_str(": ");
                    content.push_str(&ret.to_string());
                }
            } else {
                content.push_str(kind_label);
                content.push(' ');
                content.push_str(&sym.fqn);
            }
            content.push_str("\n```\n");

            // PHPDoc summary
            if let Some(ref doc) = sym.doc_comment {
                let phpdoc = parse_phpdoc(doc);
                if let Some(ref summary) = phpdoc.summary {
                    content.push_str("\n---\n\n");
                    content.push_str(summary);
                    content.push('\n');
                }

                // @param descriptions
                if !phpdoc.params.is_empty() {
                    content.push_str("\n**Parameters:**\n\n");
                    for p in &phpdoc.params {
                        content.push_str("- `$");
                        content.push_str(&p.name);
                        content.push('`');
                        if let Some(ref t) = p.type_info {
                            content.push_str(" — `");
                            content.push_str(&t.to_string());
                            content.push('`');
                        }
                        if let Some(ref desc) = p.description {
                            content.push_str(" — ");
                            content.push_str(desc);
                        }
                        content.push('\n');
                    }
                }

                // @return
                if let Some(ref ret) = phpdoc.return_type {
                    content.push_str("\n**Returns:** `");
                    content.push_str(&ret.to_string());
                    content.push_str("`\n");
                }

                for section in phpdoc_extra_markdown_sections(&phpdoc) {
                    content.push('\n');
                    content.push_str(&section);
                    content.push('\n');
                }

                // @deprecated
                if let Some(ref dep) = phpdoc.deprecated {
                    content.push_str("\n⚠️ **Deprecated**");
                    if !dep.is_empty() {
                        content.push_str(": ");
                        content.push_str(dep);
                    }
                    content.push('\n');
                }
            }

            let range = Range {
                start: Position::new(sym_at_pos.range.0, sym_at_pos.range.1),
                end: Position::new(sym_at_pos.range.2, sym_at_pos.range.3),
            };

            Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: content,
                }),
                range: Some(range),
            })
        } else if let Some(virtual_member) = virtual_member {
            let range = Range {
                start: Position::new(sym_at_pos.range.0, sym_at_pos.range.1),
                end: Position::new(sym_at_pos.range.2, sym_at_pos.range.3),
            };
            Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: phpdoc_virtual_member_markdown(&virtual_member),
                }),
                range: Some(range),
            })
        } else if let Some(var_info) = var_hover_info {
            let mut content = String::new();
            content.push_str("```php\n");
            if let Some(ref t) = var_info.type_display {
                content.push_str(t);
                content.push(' ');
                content.push_str(&var_info.variable_name);
            } else if let Some(ref fqn) = var_info.resolved_type_fqn {
                content.push_str(fqn);
                content.push(' ');
                content.push_str(&var_info.variable_name);
            } else {
                content.push_str("variable ");
                content.push_str(&var_info.variable_name);
            }
            content.push_str("\n```\n");

            if let Some(ref doc) = var_info.phpdoc_comment {
                let phpdoc = parse_phpdoc(doc);
                if let Some(ref summary) = phpdoc.summary {
                    content.push_str("\n---\n\n");
                    content.push_str(summary);
                    content.push('\n');
                }
                if let Some(ref var_type) = phpdoc.var_type {
                    content.push_str("\n**@var** `");
                    content.push_str(&var_type.to_string());
                    content.push_str("`\n");
                }
            }

            let range = Range {
                start: Position::new(sym_at_pos.range.0, sym_at_pos.range.1),
                end: Position::new(sym_at_pos.range.2, sym_at_pos.range.3),
            };
            Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: content,
                }),
                range: Some(range),
            })
        } else {
            None
        };

        Ok(result)
    }

    async fn goto_declaration(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .clone();
        let pos = params.text_document_position_params.position;
        tracing::debug!(
            "gotoDeclaration: {}:{}:{}",
            uri.as_str(),
            pos.line,
            pos.character
        );

        if let Some(import_declaration) = self.import_declaration_at_position(&uri, pos) {
            return Ok(Some(import_declaration));
        }

        self.goto_definition(params).await
    }

    async fn goto_type_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let uri_str = uri.as_str().to_string();
        let pos = params.text_document_position_params.position;
        tracing::debug!(
            "gotoTypeDefinition: {}:{}:{}",
            uri_str,
            pos.line,
            pos.character
        );

        let (sym_at_pos, variable_type_fqn, file_symbols) = {
            let parser = match self.open_files.get(&uri_str) {
                Some(parser) => parser,
                None => return Ok(None),
            };
            let tree = match parser.tree() {
                Some(tree) => tree,
                None => return Ok(None),
            };
            let source = parser.source();
            let byte_col = utf16_col_to_byte(&source, pos.line, pos.character);
            let file_symbols = self
                .index
                .file_symbols
                .get(&uri_str)
                .map(|entry| entry.value().clone())
                .unwrap_or_else(|| extract_file_symbols(tree, &source, &uri_str));

            let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
                self.resolve_member_type(class_fqn, member_name)
            };

            let sym_at_pos = symbol_at_position_with_resolver(
                tree,
                &source,
                pos.line,
                byte_col,
                &file_symbols,
                Some(&resolver),
            );
            let variable_type_fqn = if let Some(sym) = &sym_at_pos {
                if sym.ref_kind == RefKind::Variable {
                    variable_hover_info_at_position(
                        tree,
                        &source,
                        &file_symbols,
                        pos.line,
                        byte_col,
                    )
                    .and_then(|info| info.resolved_type_fqn)
                    .or_else(|| {
                        infer_variable_type_at_position(
                            tree,
                            &source,
                            &file_symbols,
                            pos.line,
                            byte_col,
                            &sym.name,
                        )
                    })
                } else {
                    None
                }
            } else {
                None
            };

            (sym_at_pos, variable_type_fqn, file_symbols)
        };

        if let Some(type_fqn) = variable_type_fqn {
            return Ok(self
                .location_for_type_fqn(&type_fqn)
                .await
                .map(GotoDefinitionResponse::Scalar));
        }

        let Some(sym_at_pos) = sym_at_pos else {
            return Ok(None);
        };

        if matches!(
            sym_at_pos.ref_kind,
            RefKind::ClassName | RefKind::Constructor
        ) {
            let type_fqn = import_target_fqn(&sym_at_pos);
            return Ok(self
                .location_for_type_fqn(type_fqn)
                .await
                .map(GotoDefinitionResponse::Scalar));
        }

        let symbol_info = self
            .resolve_fqn_lazy_with_fallback(&sym_at_pos.fqn, sym_at_pos.ref_kind)
            .await;

        let Some(symbol_info) = symbol_info else {
            return Ok(None);
        };
        let Some(type_fqn) = self.type_definition_fqn_for_symbol(&symbol_info, &file_symbols)
        else {
            return Ok(None);
        };

        Ok(self
            .location_for_type_fqn(&type_fqn)
            .await
            .map(GotoDefinitionResponse::Scalar))
    }

    async fn goto_implementation(
        &self,
        params: GotoImplementationParams,
    ) -> Result<Option<GotoImplementationResponse>> {
        let uri_str = params
            .text_document_position_params
            .text_document
            .uri
            .as_str()
            .to_string();
        let pos = params.text_document_position_params.position;
        tracing::debug!(
            "gotoImplementation: {}:{}:{}",
            uri_str,
            pos.line,
            pos.character
        );

        let (candidate, local_candidate) = {
            let parser = match self.open_files.get(&uri_str) {
                Some(parser) => parser,
                None => return Ok(None),
            };
            let tree = match parser.tree() {
                Some(tree) => tree,
                None => return Ok(None),
            };
            let source = parser.source();
            let byte_col = utf16_col_to_byte(&source, pos.line, pos.character);
            let file_symbols = self
                .index
                .file_symbols
                .get(&uri_str)
                .map(|entry| entry.value().clone())
                .unwrap_or_else(|| extract_file_symbols(tree, &source, &uri_str));
            let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
                self.resolve_member_type(class_fqn, member_name)
            };
            let Some(sym_at_pos) = symbol_at_position_with_resolver(
                tree,
                &source,
                pos.line,
                byte_col,
                &file_symbols,
                Some(&resolver),
            ) else {
                return Ok(None);
            };

            let candidate = match sym_at_pos.ref_kind {
                RefKind::ClassName => Some((
                    sym_at_pos.fqn.clone(),
                    php_lsp_types::PhpSymbolKind::Class,
                    RefKind::ClassName,
                )),
                RefKind::Constructor => {
                    let class_fqn = sym_at_pos
                        .fqn
                        .strip_suffix("::__construct")
                        .unwrap_or(&sym_at_pos.fqn)
                        .to_string();
                    Some((
                        class_fqn,
                        php_lsp_types::PhpSymbolKind::Class,
                        RefKind::ClassName,
                    ))
                }
                RefKind::MethodCall => Some((
                    sym_at_pos.fqn.clone(),
                    php_lsp_types::PhpSymbolKind::Method,
                    RefKind::MethodCall,
                )),
                _ => None,
            };

            let local_candidate = candidate.as_ref().and_then(|(fqn, kind, _)| {
                file_symbols
                    .symbols
                    .iter()
                    .find(|sym| sym.fqn == *fqn && sym.kind == *kind)
                    .cloned()
            });
            (candidate, local_candidate)
        };

        let Some((target_fqn, _, ref_kind)) = candidate else {
            return Ok(None);
        };
        let target = self
            .resolve_fqn_lazy_with_fallback(&target_fqn, ref_kind)
            .await
            .or_else(|| local_candidate.map(Arc::new));
        let Some(target) = target else {
            return Ok(None);
        };

        let locations = match target.kind {
            php_lsp_types::PhpSymbolKind::Class
            | php_lsp_types::PhpSymbolKind::Interface
            | php_lsp_types::PhpSymbolKind::Trait
            | php_lsp_types::PhpSymbolKind::Enum => {
                implementation_locations_for_type(&self.index, &target)
            }
            php_lsp_types::PhpSymbolKind::Method => {
                implementation_locations_for_method(&self.index, &target)
            }
            _ => Vec::new(),
        };

        if locations.is_empty() {
            Ok(None)
        } else {
            Ok(Some(GotoImplementationResponse::Array(locations)))
        }
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let uri_str = uri.as_str().to_string();
        let pos = params.text_document_position_params.position;
        tracing::debug!("gotoDefinition: {}:{}:{}", uri_str, pos.line, pos.character);

        // Extract symbol-at-position inside a block so DashMap guard is dropped
        let (sym_at_pos, local_var_def, this_class_def) = {
            let parser = match self.open_files.get(&uri_str) {
                Some(p) => p,
                None => return Ok(None),
            };

            let tree = match parser.tree() {
                Some(t) => t,
                None => return Ok(None),
            };

            let source = parser.source();
            let byte_col = utf16_col_to_byte(&source, pos.line, pos.character);

            let file_symbols = self
                .index
                .file_symbols
                .get(&uri_str)
                .map(|entry| entry.value().clone())
                .unwrap_or_default();

            // Build a cross-file type resolver that uses the workspace index
            let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
                self.resolve_member_type(class_fqn, member_name)
            };

            let local_var_def = variable_definition_at_position(tree, &source, pos.line, byte_col)
                .map(|d| range_byte_to_utf16(&source, d));
            let sym = symbol_at_position_with_resolver(
                tree,
                &source,
                pos.line,
                byte_col,
                &file_symbols,
                Some(&resolver),
            );
            let this_class_def = sym.as_ref().and_then(|sym| {
                if sym.ref_kind == RefKind::Variable && sym.name == "$this" {
                    current_class_symbol_at_range(
                        &file_symbols,
                        (pos.line, byte_col, pos.line, byte_col),
                    )
                    .map(|class_sym| (class_sym.uri.clone(), class_sym.selection_range))
                } else {
                    None
                }
            });
            (sym, local_var_def, this_class_def)
        };

        if let Some((target_uri, def)) = this_class_def {
            let range = Range {
                start: Position::new(def.0, def.1),
                end: Position::new(def.2, def.3),
            };
            return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                uri: target_uri.parse::<Uri>().unwrap_or_else(|_| uri.clone()),
                range,
            })));
        }

        // Local variable definition (same file/scope).
        if let Some(def) = local_var_def {
            let range = Range {
                start: Position::new(def.0, def.1),
                end: Position::new(def.2, def.3),
            };
            return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                uri,
                range,
            })));
        }

        let sym_at_pos = match sym_at_pos {
            Some(s) => {
                tracing::debug!(
                    "goto_definition: sym_at_pos fqn='{}', name='{}', ref_kind={:?}",
                    s.fqn,
                    s.name,
                    s.ref_kind
                );
                s
            }
            None => {
                tracing::debug!("goto_definition: no symbol at position");
                return Ok(None);
            }
        };

        // Look up symbol in index (with lazy vendor fallback)
        let symbol_info = self
            .resolve_fqn_lazy_with_fallback(&sym_at_pos.fqn, sym_at_pos.ref_kind)
            .await;

        // For constructor refs (`new ClassName()`), fall back to the class
        // declaration when `__construct` is not explicitly defined.
        let symbol_info = if symbol_info.is_none() && sym_at_pos.ref_kind == RefKind::Constructor {
            if let Some(class_fqn) = sym_at_pos.fqn.strip_suffix("::__construct") {
                self.resolve_fqn_lazy_with_fallback(class_fqn, RefKind::ClassName)
                    .await
            } else {
                None
            }
        } else {
            symbol_info
        };

        let result = if let Some(sym) = symbol_info {
            // Convert URI string to lsp_types::Uri
            if let Ok(target_uri) = sym.uri.parse::<Uri>() {
                let range = Range {
                    start: Position::new(sym.selection_range.0, sym.selection_range.1),
                    end: Position::new(sym.selection_range.2, sym.selection_range.3),
                };
                Some(GotoDefinitionResponse::Scalar(Location {
                    uri: target_uri,
                    range,
                }))
            } else {
                None
            }
        } else if let Some(virtual_member) =
            phpdoc_virtual_member_for_symbol(&self.index, &sym_at_pos)
        {
            self.phpdoc_virtual_member_location(&virtual_member)
                .await
                .map(GotoDefinitionResponse::Scalar)
        } else {
            None
        };

        // Fallback: when a member call on `$this->prop` fails because the declared
        // property type doesn't have that member, try resolving from the actual
        // assignment (e.g., `$this->em = $this->createStub(...)` → Stub type).
        let result = if result.is_none()
            && (sym_at_pos.ref_kind == RefKind::MethodCall
                || sym_at_pos.ref_kind == RefKind::PropertyAccess)
        {
            tracing::debug!(
                "goto_definition: primary resolution failed, trying property assignment fallback for obj_expr={:?}",
                sym_at_pos.object_expr
            );
            if let Some(ref obj_expr) = sym_at_pos.object_expr {
                if let Some(prop_name) = obj_expr.strip_prefix("$this->") {
                    // Only handle simple property access (no chaining)
                    if !prop_name.contains("->") {
                        self.try_property_assignment_type_fallback(
                            &uri_str,
                            prop_name,
                            &sym_at_pos.name,
                        )
                        .await
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            result
        };

        Ok(result)
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let uri_str = params
            .text_document_position_params
            .text_document
            .uri
            .as_str()
            .to_string();
        let pos = params.text_document_position_params.position;

        let parser = match self.open_files.get(&uri_str) {
            Some(parser) => parser,
            None => return Ok(None),
        };
        let tree = match parser.tree() {
            Some(tree) => tree,
            None => return Ok(None),
        };
        let source = parser.source();
        let byte_col = utf16_col_to_byte(&source, pos.line, pos.character);
        let file_symbols = extract_file_symbols(tree, &source, &uri_str);
        let sym = match symbol_at_position(tree, &source, pos.line, byte_col, &file_symbols) {
            Some(sym) => sym,
            None => return Ok(None),
        };

        if sym.ref_kind == RefKind::Variable {
            let highlights: Vec<DocumentHighlight> =
                find_variable_references_at_position(tree, &source, pos.line, byte_col, true)
                    .into_iter()
                    .map(|reference| document_highlight_from_range(&source, reference.range, true))
                    .collect();
            return if highlights.is_empty() {
                Ok(None)
            } else {
                Ok(Some(highlights))
            };
        }

        let Some(kind) = php_symbol_kind_for_ref_kind(sym.ref_kind) else {
            return Ok(None);
        };
        let resolved = self.resolve_fqn_with_fallback(&sym.fqn, sym.ref_kind);
        let (target_fqn, target_kind) = if let Some(resolved) = resolved {
            (resolved.fqn.clone(), resolved.kind)
        } else {
            (sym.fqn.clone(), kind)
        };
        let read_write_capable = target_kind == php_lsp_types::PhpSymbolKind::Property;

        let highlights: Vec<DocumentHighlight> =
            find_references_in_file(tree, &source, &file_symbols, &target_fqn, target_kind, true)
                .into_iter()
                .map(|reference| {
                    document_highlight_from_range(&source, reference.range, read_write_capable)
                })
                .collect();

        if highlights.is_empty() {
            Ok(None)
        } else {
            Ok(Some(highlights))
        }
    }

    async fn selection_range(
        &self,
        params: SelectionRangeParams,
    ) -> Result<Option<Vec<SelectionRange>>> {
        let uri_str = params.text_document.uri.as_str().to_string();
        let parser = match self.open_files.get(&uri_str) {
            Some(parser) => parser,
            None => return Ok(None),
        };
        let tree = match parser.tree() {
            Some(tree) => tree,
            None => return Ok(None),
        };
        let source = parser.source();
        let root = tree.root_node();

        let mut results = Vec::with_capacity(params.positions.len());
        for position in params.positions {
            let byte_col = utf16_col_to_byte(&source, position.line, position.character);
            let point = tree_sitter::Point::new(position.line as usize, byte_col as usize);
            let mut node = match root.descendant_for_point_range(point, point) {
                Some(node) => node,
                None => continue,
            };

            while !node.is_named() {
                node = match node.parent() {
                    Some(parent) => parent,
                    None => break,
                };
            }

            let mut byte_ranges = Vec::new();
            let mut current = Some(node);
            while let Some(node) = current {
                if node.is_named() && node.kind() != "program" {
                    let start = node.start_position();
                    let end = node.end_position();
                    let range = (
                        start.row as u32,
                        start.column as u32,
                        end.row as u32,
                        end.column as u32,
                    );
                    if byte_ranges.last() != Some(&range) {
                        byte_ranges.push(range);
                    }
                }
                current = node.parent();
            }

            if let Some(selection_range) = selection_range_from_byte_ranges(&source, byte_ranges) {
                results.push(selection_range);
            }
        }

        if results.is_empty() {
            Ok(None)
        } else {
            Ok(Some(results))
        }
    }

    async fn linked_editing_range(
        &self,
        params: LinkedEditingRangeParams,
    ) -> Result<Option<LinkedEditingRanges>> {
        let uri_str = params
            .text_document_position_params
            .text_document
            .uri
            .as_str()
            .to_string();
        let position = params.text_document_position_params.position;

        let parser = match self.open_files.get(&uri_str) {
            Some(parser) => parser,
            None => return Ok(None),
        };
        let tree = match parser.tree() {
            Some(tree) => tree,
            None => return Ok(None),
        };
        let source = parser.source();
        let byte_col = utf16_col_to_byte(&source, position.line, position.character);
        let point = tree_sitter::Point::new(position.line as usize, byte_col as usize);
        let root = tree.root_node();
        let mut node = match root.descendant_for_point_range(point, point) {
            Some(node) => node,
            None => return Ok(None),
        };

        while !node.is_named() {
            node = match node.parent() {
                Some(parent) => parent,
                None => return Ok(None),
            };
        }

        let Some(byte_ranges) = linked_editing_ranges_for_namespace_or_use(&source, node) else {
            return Ok(None);
        };
        let ranges = byte_ranges
            .into_iter()
            .map(|range| {
                let range = range_byte_to_utf16(&source, range);
                Range {
                    start: Position::new(range.0, range.1),
                    end: Position::new(range.2, range.3),
                }
            })
            .collect();

        Ok(Some(LinkedEditingRanges {
            ranges,
            word_pattern: Some("[A-Za-z_][A-Za-z0-9_]*".to_string()),
        }))
    }

    async fn prepare_call_hierarchy(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> Result<Option<Vec<CallHierarchyItem>>> {
        let uri_str = params
            .text_document_position_params
            .text_document
            .uri
            .as_str()
            .to_string();
        let pos = params.text_document_position_params.position;

        let (candidate, local_candidate, containing_candidate, allow_containing_fallback) = {
            let parser = match self.open_files.get(&uri_str) {
                Some(parser) => parser,
                None => return Ok(None),
            };
            let tree = match parser.tree() {
                Some(tree) => tree,
                None => return Ok(None),
            };
            let source = parser.source();
            let byte_col = utf16_col_to_byte(&source, pos.line, pos.character);
            let file_symbols = self
                .index
                .file_symbols
                .get(&uri_str)
                .map(|entry| entry.value().clone())
                .unwrap_or_else(|| extract_file_symbols(tree, &source, &uri_str));
            let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
                self.resolve_member_type(class_fqn, member_name)
            };
            let sym_at_pos = symbol_at_position_with_resolver(
                tree,
                &source,
                pos.line,
                byte_col,
                &file_symbols,
                Some(&resolver),
            );
            let allow_containing_fallback = sym_at_pos
                .as_ref()
                .is_none_or(|sym| !is_call_hierarchy_ref_kind(sym.ref_kind));
            let candidate = sym_at_pos
                .as_ref()
                .filter(|sym| is_call_hierarchy_ref_kind(sym.ref_kind))
                .map(|sym| (sym.fqn.clone(), sym.ref_kind));
            let local_candidate = candidate.as_ref().and_then(|(fqn, _)| {
                file_symbols
                    .symbols
                    .iter()
                    .find(|sym| sym.fqn == *fqn && is_call_hierarchy_symbol_kind(sym.kind))
                    .cloned()
            });
            let point_range = (pos.line, byte_col, pos.line, byte_col);
            let containing_candidate =
                containing_callable_symbol(&file_symbols, point_range).cloned();
            (
                candidate,
                local_candidate,
                containing_candidate,
                allow_containing_fallback,
            )
        };

        let mut symbol = None;
        if let Some((fqn, ref_kind)) = candidate {
            symbol = self.resolve_fqn_lazy_with_fallback(&fqn, ref_kind).await;
            if symbol.is_none() && ref_kind == RefKind::Constructor {
                if let Some(class_fqn) = fqn.strip_suffix("::__construct") {
                    symbol = self
                        .resolve_fqn_lazy_with_fallback(class_fqn, RefKind::ClassName)
                        .await;
                }
            }
            if symbol.is_none() {
                symbol = local_candidate.map(Arc::new);
            }
        }

        if symbol.is_none() && allow_containing_fallback {
            symbol = containing_candidate.map(Arc::new);
        }

        let Some(symbol) = symbol else {
            return Ok(None);
        };
        if !is_call_hierarchy_symbol_kind(symbol.kind) {
            return Ok(None);
        }

        Ok(call_hierarchy_item_from_symbol(&symbol).map(|item| vec![item]))
    }

    async fn incoming_calls(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyIncomingCall>>> {
        let Some((target, target_kind)) =
            call_hierarchy_target_from_item(&self.index, &params.item)
        else {
            return Ok(None);
        };

        let mut calls_by_caller: HashMap<String, (php_lsp_types::SymbolInfo, Vec<Range>)> =
            HashMap::new();
        for entry in self.index.file_symbols.iter() {
            let file_uri = entry.key().clone();
            let file_symbols = entry.value().clone();

            if let Some(parser) = self.open_files.get(&file_uri) {
                if let Some(tree) = parser.tree() {
                    let source = parser.source();
                    incoming_call_hierarchy_for_file(
                        tree,
                        &source,
                        &file_symbols,
                        &target.fqn,
                        target_kind,
                        &mut calls_by_caller,
                    );
                }
                continue;
            }

            let Some(path) = uri_to_path(&file_uri) else {
                continue;
            };
            let Ok(source) =
                read_file_to_string_blocking(path, "callHierarchy/incoming read").await
            else {
                continue;
            };
            let mut parser = FileParser::new();
            parser.parse_full(&source);
            if let Some(tree) = parser.tree() {
                incoming_call_hierarchy_for_file(
                    tree,
                    &source,
                    &file_symbols,
                    &target.fqn,
                    target_kind,
                    &mut calls_by_caller,
                );
            }
        }

        let mut calls: Vec<_> = calls_by_caller
            .into_values()
            .filter_map(|(caller, ranges)| {
                Some(CallHierarchyIncomingCall {
                    from: call_hierarchy_item_from_symbol(&caller)?,
                    from_ranges: ranges,
                })
            })
            .collect();
        calls.sort_by(|left, right| left.from.name.cmp(&right.from.name));

        if calls.is_empty() {
            Ok(None)
        } else {
            Ok(Some(calls))
        }
    }

    async fn outgoing_calls(
        &self,
        params: CallHierarchyOutgoingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyOutgoingCall>>> {
        let Some((caller, _)) = call_hierarchy_target_from_item(&self.index, &params.item) else {
            return Ok(None);
        };
        if !is_call_hierarchy_symbol_kind(caller.kind) {
            return Ok(None);
        }

        let file_uri = caller.uri.clone();
        let file_symbols = self
            .index
            .file_symbols
            .get(&file_uri)
            .map(|entry| entry.value().clone())
            .unwrap_or_default();

        let calls = if let Some(parser) = self.open_files.get(&file_uri) {
            let Some(tree) = parser.tree() else {
                return Ok(None);
            };
            let source = parser.source();
            outgoing_call_hierarchy_for_tree(tree, &source, &file_symbols, &self.index, &caller)
        } else {
            let Some(path) = uri_to_path(&file_uri) else {
                return Ok(None);
            };
            let Ok(source) =
                read_file_to_string_blocking(path, "callHierarchy/outgoing read").await
            else {
                return Ok(None);
            };
            let mut parser = FileParser::new();
            parser.parse_full(&source);
            let Some(tree) = parser.tree() else {
                return Ok(None);
            };
            outgoing_call_hierarchy_for_tree(tree, &source, &file_symbols, &self.index, &caller)
        };

        if calls.is_empty() {
            Ok(None)
        } else {
            Ok(Some(calls))
        }
    }

    async fn prepare_type_hierarchy(
        &self,
        params: TypeHierarchyPrepareParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let uri_str = params
            .text_document_position_params
            .text_document
            .uri
            .as_str()
            .to_string();
        let pos = params.text_document_position_params.position;

        let (candidate, local_candidate, containing_class_fqn) = {
            let parser = match self.open_files.get(&uri_str) {
                Some(parser) => parser,
                None => return Ok(None),
            };
            let tree = match parser.tree() {
                Some(tree) => tree,
                None => return Ok(None),
            };
            let source = parser.source();
            let byte_col = utf16_col_to_byte(&source, pos.line, pos.character);
            let file_symbols = self
                .index
                .file_symbols
                .get(&uri_str)
                .map(|entry| entry.value().clone())
                .unwrap_or_else(|| extract_file_symbols(tree, &source, &uri_str));
            let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
                self.resolve_member_type(class_fqn, member_name)
            };
            let sym_at_pos = symbol_at_position_with_resolver(
                tree,
                &source,
                pos.line,
                byte_col,
                &file_symbols,
                Some(&resolver),
            );
            let candidate = sym_at_pos.as_ref().and_then(|sym| match sym.ref_kind {
                RefKind::ClassName => Some(sym.fqn.clone()),
                RefKind::Constructor => sym
                    .fqn
                    .strip_suffix("::__construct")
                    .map(str::to_string)
                    .or_else(|| Some(sym.fqn.clone())),
                _ => None,
            });
            let local_candidate = candidate.as_ref().and_then(|fqn| {
                file_symbols
                    .symbols
                    .iter()
                    .find(|sym| sym.fqn == *fqn && is_type_hierarchy_symbol_kind(sym.kind))
                    .cloned()
            });
            let point_range = (pos.line, byte_col, pos.line, byte_col);
            let containing_class_fqn = current_class_fqn_at_range(&file_symbols, point_range);
            (candidate, local_candidate, containing_class_fqn)
        };

        let mut symbol = None;
        if let Some(fqn) = candidate {
            symbol = self
                .resolve_fqn_lazy_with_fallback(&fqn, RefKind::ClassName)
                .await;
            if symbol.is_none() {
                symbol = local_candidate.map(Arc::new);
            }
        }

        if symbol.is_none() {
            if let Some(class_fqn) = containing_class_fqn {
                symbol = self
                    .resolve_fqn_lazy_with_fallback(&class_fqn, RefKind::ClassName)
                    .await;
            }
        }

        let Some(symbol) = symbol else {
            return Ok(None);
        };

        Ok(type_hierarchy_item_from_symbol(&symbol).map(|item| vec![item]))
    }

    async fn supertypes(
        &self,
        params: TypeHierarchySupertypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let Some(symbol) = type_hierarchy_symbol_from_item(&self.index, &params.item) else {
            return Ok(None);
        };

        let parent_fqns = direct_type_parent_fqns(&symbol);
        let mut parents = Vec::new();
        for parent_fqn in parent_fqns {
            self.lazy_index_class(&parent_fqn).await;
            if let Some(parent) = self
                .resolve_fqn_lazy_with_fallback(&parent_fqn, RefKind::ClassName)
                .await
            {
                if let Some(item) = type_hierarchy_item_from_symbol(&parent) {
                    parents.push(item);
                }
            }
        }
        parents.sort_by(|left, right| left.name.cmp(&right.name));

        if parents.is_empty() {
            Ok(None)
        } else {
            Ok(Some(parents))
        }
    }

    async fn subtypes(
        &self,
        params: TypeHierarchySubtypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let Some(symbol) = type_hierarchy_symbol_from_item(&self.index, &params.item) else {
            return Ok(None);
        };

        let subtypes: Vec<_> = direct_type_subtypes(&self.index, &symbol.fqn)
            .into_iter()
            .filter_map(|symbol| type_hierarchy_item_from_symbol(&symbol))
            .collect();

        if subtypes.is_empty() {
            Ok(None)
        } else {
            Ok(Some(subtypes))
        }
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri_str = params
            .text_document_position
            .text_document
            .uri
            .as_str()
            .to_string();
        let pos = params.text_document_position.position;
        let include_declaration = params.context.include_declaration;

        // Resolve symbol under cursor to get FQN
        let (target_fqn, target_kind) = {
            let parser = match self.open_files.get(&uri_str) {
                Some(p) => p,
                None => return Ok(None),
            };
            let tree = match parser.tree() {
                Some(t) => t,
                None => return Ok(None),
            };
            let source = parser.source();
            let byte_col = utf16_col_to_byte(&source, pos.line, pos.character);
            let file_symbols = extract_file_symbols(tree, &source, &uri_str);

            match symbol_at_position(tree, &source, pos.line, byte_col, &file_symbols) {
                Some(sym) => {
                    if sym.ref_kind == RefKind::Variable {
                        let refs = find_variable_references_at_position(
                            tree,
                            &source,
                            pos.line,
                            byte_col,
                            include_declaration,
                        );
                        if refs.is_empty() {
                            return Ok(None);
                        }
                        let uri = match uri_str.parse::<Uri>() {
                            Ok(u) => u,
                            Err(_) => return Ok(None),
                        };
                        let locations: Vec<Location> = refs
                            .into_iter()
                            .map(|r| {
                                let rng = range_byte_to_utf16(&source, r.range);
                                Location {
                                    uri: uri.clone(),
                                    range: Range {
                                        start: Position::new(rng.0, rng.1),
                                        end: Position::new(rng.2, rng.3),
                                    },
                                }
                            })
                            .collect();
                        return Ok(Some(locations));
                    }

                    let kind = match sym.ref_kind {
                        RefKind::ClassName | RefKind::Constructor => {
                            php_lsp_types::PhpSymbolKind::Class
                        }
                        RefKind::FunctionCall => php_lsp_types::PhpSymbolKind::Function,
                        RefKind::MethodCall => php_lsp_types::PhpSymbolKind::Method,
                        RefKind::PropertyAccess | RefKind::StaticPropertyAccess => {
                            php_lsp_types::PhpSymbolKind::Property
                        }
                        RefKind::ClassConstant => php_lsp_types::PhpSymbolKind::ClassConstant,
                        RefKind::GlobalConstant => php_lsp_types::PhpSymbolKind::GlobalConstant,
                        RefKind::Variable => return Ok(None),
                        RefKind::NamespaceName | RefKind::Unknown => return Ok(None),
                    };

                    // Try to canonicalize symbol via index lookup.
                    let resolved = self.resolve_fqn_with_fallback(&sym.fqn, sym.ref_kind);
                    if let Some(resolved) = resolved {
                        (resolved.fqn.clone(), resolved.kind)
                    } else {
                        (sym.fqn.clone(), kind)
                    }
                }
                None => return Ok(None),
            }
        };

        // Search all indexed files for references
        let mut locations = Vec::new();
        let indexed_files: Vec<_> = self
            .index
            .file_references
            .iter()
            .map(|entry| entry.key().clone())
            .collect();

        for (scanned_files, file_uri) in indexed_files.into_iter().enumerate() {
            cooperative_heavy_request_yield(scanned_files).await;

            for r in
                self.references_for_file(&file_uri, &target_fqn, target_kind, include_declaration)
            {
                if let Ok(uri) = file_uri.parse::<Uri>() {
                    locations.push(Location {
                        uri,
                        range: Range {
                            start: Position::new(r.range.0, r.range.1),
                            end: Position::new(r.range.2, r.range.3),
                        },
                    });
                }
            }
        }

        if locations.is_empty() {
            Ok(None)
        } else {
            Ok(Some(locations))
        }
    }

    async fn code_lens(&self, params: CodeLensParams) -> Result<Option<Vec<CodeLens>>> {
        let uri_str = params.text_document.uri.as_str().to_string();
        let document_uri = match uri_str.parse::<Uri>() {
            Ok(uri) => uri,
            Err(_) => return Ok(None),
        };

        let (file_symbols, source) = if let Some(parser) = self.open_files.get(&uri_str) {
            let Some(tree) = parser.tree() else {
                return Ok(None);
            };
            let source = parser.source();
            (extract_file_symbols(tree, &source, &uri_str), source)
        } else if let Some(file_symbols) = self.index.file_symbols.get(&uri_str) {
            let file_symbols = file_symbols.value().clone();
            let Some(path) = uri_to_path(&uri_str) else {
                return Ok(None);
            };
            let Ok(source) = read_file_to_string_blocking(path, "codeLens source read").await
            else {
                return Ok(None);
            };
            (file_symbols, source)
        } else {
            return Ok(None);
        };

        let mut lenses = Vec::new();
        for symbol in file_symbols
            .symbols
            .iter()
            .filter(|symbol| is_code_lens_symbol_kind(symbol.kind))
        {
            let locations = self.reference_locations_for_symbol(&symbol.fqn, symbol.kind, false);
            let range_tuple = range_byte_to_utf16(&source, symbol.selection_range);
            let start = Position::new(range_tuple.0, range_tuple.1);
            let end = if range_tuple.0 == range_tuple.2 {
                Position::new(range_tuple.2, range_tuple.3)
            } else {
                start
            };

            let arguments = match (
                serde_json::to_value(document_uri.clone()),
                serde_json::to_value(start),
                serde_json::to_value(&locations),
            ) {
                (Ok(uri), Ok(position), Ok(locations)) => Some(vec![uri, position, locations]),
                _ => None,
            };

            lenses.push(CodeLens {
                range: Range { start, end },
                command: Some(Command {
                    title: reference_count_title(locations.len()),
                    command: "editor.action.showReferences".to_string(),
                    arguments,
                }),
                data: Some(serde_json::json!({
                    "fqn": symbol.fqn,
                    "kind": call_hierarchy_kind_key(symbol.kind),
                    "references": locations.len(),
                })),
            });
        }

        if lenses.is_empty() {
            Ok(None)
        } else {
            Ok(Some(lenses))
        }
    }

    async fn folding_range(&self, params: FoldingRangeParams) -> Result<Option<Vec<FoldingRange>>> {
        let uri_str = params.text_document.uri.as_str().to_string();

        let ranges = if let Some(parser) = self.open_files.get(&uri_str) {
            let Some(tree) = parser.tree() else {
                return Ok(None);
            };
            folding_ranges(tree, &parser.source())
        } else {
            let Some(path) = uri_to_path(&uri_str) else {
                return Ok(None);
            };
            let Ok(source) = read_file_to_string_blocking(path, "foldingRange source read").await
            else {
                return Ok(None);
            };
            let mut parser = FileParser::new();
            parser.parse_full(&source);
            let Some(tree) = parser.tree() else {
                return Ok(None);
            };
            folding_ranges(tree, &source)
        };

        if ranges.is_empty() {
            Ok(None)
        } else {
            Ok(Some(ranges))
        }
    }

    async fn document_link(&self, params: DocumentLinkParams) -> Result<Option<Vec<DocumentLink>>> {
        let uri_str = params.text_document.uri.as_str().to_string();
        let Some(file_path) = uri_to_path(&uri_str) else {
            return Ok(None);
        };

        let links = if let Some(parser) = self.open_files.get(&uri_str) {
            let Some(tree) = parser.tree() else {
                return Ok(None);
            };
            document_links_for_source(&parser.source(), tree, &file_path)
        } else {
            let Ok(source) =
                read_file_to_string_blocking(file_path.clone(), "documentLink source read").await
            else {
                return Ok(None);
            };
            let mut parser = FileParser::new();
            parser.parse_full(&source);
            let Some(tree) = parser.tree() else {
                return Ok(None);
            };
            document_links_for_source(&source, tree, &file_path)
        };

        if links.is_empty() {
            Ok(None)
        } else {
            Ok(Some(links))
        }
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri_str = params
            .text_document_position
            .text_document
            .uri
            .as_str()
            .to_string();
        let pos = params.text_document_position.position;
        let new_name = &params.new_name;

        // Validate new name
        if new_name.is_empty() || new_name.contains(' ') || new_name.contains('\\') {
            return Err(tower_lsp::jsonrpc::Error::invalid_params(
                "Invalid new name",
            ));
        }

        let parser = match self.open_files.get(&uri_str) {
            Some(p) => p,
            None => return Ok(None),
        };
        let tree = match parser.tree() {
            Some(t) => t,
            None => return Ok(None),
        };
        let source = parser.source();
        let byte_col = utf16_col_to_byte(&source, pos.line, pos.character);
        let file_symbols = extract_file_symbols(tree, &source, &uri_str);

        let sym = match symbol_at_position(tree, &source, pos.line, byte_col, &file_symbols) {
            Some(s) => s,
            None => return Ok(None),
        };

        if sym.ref_kind == RefKind::Variable {
            if !is_renameable_variable(&sym.name) {
                return Err(tower_lsp::jsonrpc::Error::invalid_params(
                    "Cannot rename this variable",
                ));
            }
            let replacement = normalize_variable_new_name(new_name).ok_or_else(|| {
                tower_lsp::jsonrpc::Error::invalid_params("Invalid variable name")
            })?;
            let refs =
                find_variable_references_at_position(tree, &source, pos.line, byte_col, true);
            if refs.is_empty() {
                return Ok(None);
            }
            let uri = match uri_str.parse::<Uri>() {
                Ok(u) => u,
                Err(_) => return Ok(None),
            };
            let edits: Vec<TextEdit> = refs
                .into_iter()
                .map(|r| {
                    let rng = range_byte_to_utf16(&source, r.range);
                    TextEdit {
                        range: Range {
                            start: Position::new(rng.0, rng.1),
                            end: Position::new(rng.2, rng.3),
                        },
                        new_text: replacement.clone(),
                    }
                })
                .collect();
            let mut changes: std::collections::HashMap<Uri, Vec<TextEdit>> =
                std::collections::HashMap::new();
            changes.insert(uri, edits);
            return Ok(Some(WorkspaceEdit {
                changes: Some(changes),
                document_changes: None,
                change_annotations: None,
            }));
        }

        if sym.ref_kind == RefKind::Unknown || sym.ref_kind == RefKind::NamespaceName {
            return Ok(None);
        }

        let resolved_for_rename = self.resolve_fqn_with_fallback(&sym.fqn, sym.ref_kind);
        if resolved_for_rename.is_none()
            && phpdoc_virtual_member_for_symbol(&self.index, &sym).is_some()
        {
            return Err(tower_lsp::jsonrpc::Error::invalid_params(
                "Cannot rename PHPDoc virtual members",
            ));
        }

        // Resolve symbol under cursor
        let (target_fqn, target_kind, _old_name) = {
            let kind = match sym.ref_kind {
                RefKind::ClassName | RefKind::Constructor => php_lsp_types::PhpSymbolKind::Class,
                RefKind::FunctionCall => php_lsp_types::PhpSymbolKind::Function,
                RefKind::MethodCall => php_lsp_types::PhpSymbolKind::Method,
                RefKind::PropertyAccess | RefKind::StaticPropertyAccess => {
                    php_lsp_types::PhpSymbolKind::Property
                }
                RefKind::ClassConstant => php_lsp_types::PhpSymbolKind::ClassConstant,
                RefKind::GlobalConstant => php_lsp_types::PhpSymbolKind::GlobalConstant,
                _ => return Ok(None),
            };

            if let Some(resolved) = resolved_for_rename {
                (resolved.fqn.clone(), resolved.kind, sym.name.clone())
            } else {
                (sym.fqn.clone(), kind, sym.name.clone())
            }
        };

        let property_new_name = if target_kind == php_lsp_types::PhpSymbolKind::Property {
            Some(normalize_property_new_name(new_name).ok_or_else(|| {
                tower_lsp::jsonrpc::Error::invalid_params("Invalid property name")
            })?)
        } else {
            None
        };

        // Don't rename built-in symbols
        if let Some(sym) = self.index.resolve_fqn(&target_fqn) {
            if sym.modifiers.is_builtin {
                return Err(tower_lsp::jsonrpc::Error::invalid_params(
                    "Cannot rename built-in symbols",
                ));
            }
        }

        // Find all references (including declaration)
        let mut changes: std::collections::HashMap<Uri, Vec<TextEdit>> =
            std::collections::HashMap::new();
        let indexed_files: Vec<_> = self
            .index
            .file_references
            .iter()
            .map(|entry| entry.key().clone())
            .collect();

        for (scanned_files, file_uri) in indexed_files.into_iter().enumerate() {
            cooperative_heavy_request_yield(scanned_files).await;
            let refs = self.references_for_file(&file_uri, &target_fqn, target_kind, true);

            if !refs.is_empty() {
                if let Ok(uri) = file_uri.parse::<Uri>() {
                    let edits: Vec<TextEdit> = refs
                        .into_iter()
                        .map(|r| TextEdit {
                            range: Range {
                                start: Position::new(r.range.0, r.range.1),
                                end: Position::new(r.range.2, r.range.3),
                            },
                            new_text: if target_kind == php_lsp_types::PhpSymbolKind::Property
                                && r.starts_with_dollar
                            {
                                format!(
                                    "${}",
                                    property_new_name
                                        .as_deref()
                                        .unwrap_or(new_name.trim_start_matches('$'))
                                )
                            } else {
                                property_new_name.as_deref().unwrap_or(new_name).to_string()
                            },
                        })
                        .collect();
                    changes.entry(uri).or_default().extend(edits);
                }
            }
        }

        if changes.is_empty() {
            Ok(None)
        } else {
            Ok(Some(WorkspaceEdit {
                changes: Some(changes),
                document_changes: None,
                change_annotations: None,
            }))
        }
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let uri_str = params.text_document.uri.as_str().to_string();
        let pos = params.position;

        let parser = match self.open_files.get(&uri_str) {
            Some(p) => p,
            None => return Ok(None),
        };
        let tree = match parser.tree() {
            Some(t) => t,
            None => return Ok(None),
        };
        let source = parser.source();
        let byte_col = utf16_col_to_byte(&source, pos.line, pos.character);
        let file_symbols = extract_file_symbols(tree, &source, &uri_str);

        match symbol_at_position(tree, &source, pos.line, byte_col, &file_symbols) {
            Some(sym) => {
                // Variable rename support is local-scope only.
                if sym.ref_kind == RefKind::Variable {
                    if !is_renameable_variable(&sym.name) {
                        return Ok(None);
                    }
                    let rng = range_byte_to_utf16(&source, sym.range);
                    let range = Range {
                        start: Position::new(rng.0, rng.1),
                        end: Position::new(rng.2, rng.3),
                    };
                    return Ok(Some(PrepareRenameResponse::Range(range)));
                }
                if sym.ref_kind == RefKind::Unknown || sym.ref_kind == RefKind::NamespaceName {
                    return Ok(None);
                }

                // Don't rename built-in or PHPDoc virtual symbols
                let resolved = self.resolve_fqn_with_fallback(&sym.fqn, sym.ref_kind);
                if resolved.is_none()
                    && phpdoc_virtual_member_for_symbol(&self.index, &sym).is_some()
                {
                    return Ok(None);
                }
                if let Some(resolved) = resolved {
                    if resolved.modifiers.is_builtin {
                        return Ok(None);
                    }
                }

                let rng2 = range_byte_to_utf16(&source, sym.range);
                let range = Range {
                    start: Position::new(rng2.0, rng2.1),
                    end: Position::new(rng2.2, rng2.3),
                };

                Ok(Some(PrepareRenameResponse::Range(range)))
            }
            None => Ok(None),
        }
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri_str = params.text_document.uri.as_str().to_string();

        // Try open files first, then fall back to index
        let file_symbols = if let Some(parser) = self.open_files.get(&uri_str) {
            if let Some(tree) = parser.tree() {
                extract_file_symbols(tree, &parser.source(), &uri_str)
            } else {
                return Ok(None);
            }
        } else if let Some(fs) = self.index.file_symbols.get(&uri_str) {
            fs.value().clone()
        } else {
            return Ok(None);
        };

        // Build hierarchical DocumentSymbol tree
        let mut top_level: Vec<DocumentSymbol> = Vec::new();

        // Collect type-level symbols (classes, interfaces, traits, enums, functions, constants)
        // and member symbols (methods, properties, class constants, enum cases)
        let mut type_symbols: Vec<&php_lsp_types::SymbolInfo> = Vec::new();
        let mut member_symbols: Vec<&php_lsp_types::SymbolInfo> = Vec::new();
        let mut namespace_sym: Option<&php_lsp_types::SymbolInfo> = None;

        for sym in &file_symbols.symbols {
            match sym.kind {
                php_lsp_types::PhpSymbolKind::Class
                | php_lsp_types::PhpSymbolKind::Interface
                | php_lsp_types::PhpSymbolKind::Trait
                | php_lsp_types::PhpSymbolKind::Enum
                | php_lsp_types::PhpSymbolKind::Function
                | php_lsp_types::PhpSymbolKind::GlobalConstant => {
                    type_symbols.push(sym);
                }
                php_lsp_types::PhpSymbolKind::Method
                | php_lsp_types::PhpSymbolKind::Property
                | php_lsp_types::PhpSymbolKind::ClassConstant
                | php_lsp_types::PhpSymbolKind::EnumCase => {
                    member_symbols.push(sym);
                }
                php_lsp_types::PhpSymbolKind::Namespace => {
                    namespace_sym = Some(sym);
                }
            }
        }

        // Helper to convert SymbolInfo range to LSP Range
        let to_range = |r: (u32, u32, u32, u32)| -> Range {
            Range {
                start: Position::new(r.0, r.1),
                end: Position::new(r.2, r.3),
            }
        };

        // Build DocumentSymbol for a symbol with its children
        #[allow(deprecated)] // DocumentSymbol.deprecated field
        let make_doc_symbol =
            |sym: &php_lsp_types::SymbolInfo, children: Vec<DocumentSymbol>| -> DocumentSymbol {
                DocumentSymbol {
                    name: sym.name.clone(),
                    detail: sym.signature.as_ref().map(|sig| {
                        let params_str: Vec<String> = sig
                            .params
                            .iter()
                            .map(|p| {
                                let mut s = String::new();
                                if let Some(ref t) = p.type_info {
                                    s.push_str(&t.to_string());
                                    s.push(' ');
                                }
                                s.push('$');
                                s.push_str(&p.name);
                                s
                            })
                            .collect();
                        let mut detail = format!("({})", params_str.join(", "));
                        if let Some(ref ret) = sig.return_type {
                            detail.push_str(&format!(": {}", ret));
                        }
                        detail
                    }),
                    kind: php_kind_to_lsp(sym.kind),
                    tags: if sym.modifiers.is_deprecated {
                        Some(vec![SymbolTag::DEPRECATED])
                    } else {
                        None
                    },
                    deprecated: None,
                    range: to_range(sym.range),
                    selection_range: to_range(sym.selection_range),
                    children: if children.is_empty() {
                        None
                    } else {
                        Some(children)
                    },
                }
            };

        // Build type symbols with their children
        for type_sym in &type_symbols {
            let children: Vec<DocumentSymbol> = member_symbols
                .iter()
                .filter(|m| m.parent_fqn.as_deref() == Some(&type_sym.fqn))
                .map(|m| make_doc_symbol(m, vec![]))
                .collect();

            top_level.push(make_doc_symbol(type_sym, children));
        }

        // Wrap in namespace if present
        if let Some(ns) = namespace_sym {
            #[allow(deprecated)]
            let ns_symbol = DocumentSymbol {
                name: ns.name.clone(),
                detail: None,
                kind: SymbolKind::NAMESPACE,
                tags: None,
                deprecated: None,
                range: to_range(ns.range),
                selection_range: to_range(ns.selection_range),
                children: if top_level.is_empty() {
                    None
                } else {
                    Some(top_level)
                },
            };
            return Ok(Some(DocumentSymbolResponse::Nested(vec![ns_symbol])));
        }

        if top_level.is_empty() {
            Ok(None)
        } else {
            Ok(Some(DocumentSymbolResponse::Nested(top_level)))
        }
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        let uri_str = params.text_document.uri.as_str().to_string();
        let php_version = *self.php_version.lock().await;

        let hints = {
            let parser = match self.open_files.get(&uri_str) {
                Some(parser) => parser,
                None => return Ok(None),
            };
            let tree = match parser.tree() {
                Some(tree) => tree,
                None => return Ok(None),
            };
            let source = parser.source();
            let file_symbols = self
                .index
                .file_symbols
                .get(&uri_str)
                .map(|entry| entry.value().clone())
                .unwrap_or_else(|| extract_file_symbols(tree, &source, &uri_str));

            inlay_hints(
                tree,
                &source,
                &file_symbols,
                &self.index,
                params.range,
                php_version,
            )
        };

        if hints.is_empty() {
            Ok(None)
        } else {
            Ok(Some(hints))
        }
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri_str = params.text_document.uri.as_str().to_string();
        tracing::debug!("semanticTokens/full: {}", uri_str);

        let data = {
            let parser = match self.open_files.get(&uri_str) {
                Some(parser) => parser,
                None => return Ok(None),
            };
            match semantic_tokens_for_parser(&parser) {
                Some(data) => data,
                None => return Ok(None),
            }
        };
        let snapshot = self
            .semantic_tokens_cache
            .lock()
            .await
            .store(&uri_str, data);

        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: Some(snapshot.result_id),
            data: snapshot.data,
        })))
    }

    async fn semantic_tokens_full_delta(
        &self,
        params: SemanticTokensDeltaParams,
    ) -> Result<Option<SemanticTokensFullDeltaResult>> {
        let uri_str = params.text_document.uri.as_str().to_string();
        tracing::debug!(
            "semanticTokens/full/delta: {} previous={}",
            uri_str,
            params.previous_result_id
        );

        let data = {
            let parser = match self.open_files.get(&uri_str) {
                Some(parser) => parser,
                None => return Ok(None),
            };
            match semantic_tokens_for_parser(&parser) {
                Some(data) => data,
                None => return Ok(None),
            }
        };

        let mut cache = self.semantic_tokens_cache.lock().await;
        let previous = cache.previous_data(&uri_str, &params.previous_result_id);
        let snapshot = cache.store(&uri_str, data);

        let Some(previous) = previous else {
            return Ok(Some(SemanticTokensFullDeltaResult::Tokens(
                SemanticTokens {
                    result_id: Some(snapshot.result_id),
                    data: snapshot.data,
                },
            )));
        };

        Ok(Some(SemanticTokensFullDeltaResult::TokensDelta(
            SemanticTokensDelta {
                result_id: Some(snapshot.result_id),
                edits: semantic_tokens_delta_edits(&previous, &snapshot.data),
            },
        )))
    }

    async fn semantic_tokens_range(
        &self,
        params: SemanticTokensRangeParams,
    ) -> Result<Option<SemanticTokensRangeResult>> {
        let uri_str = params.text_document.uri.as_str().to_string();
        tracing::debug!("semanticTokens/range: {}", uri_str);

        let data = {
            let parser = match self.open_files.get(&uri_str) {
                Some(parser) => parser,
                None => return Ok(None),
            };
            match semantic_tokens_for_parser_range(&parser, params.range) {
                Some(data) => data,
                None => return Ok(None),
            }
        };

        Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens {
            result_id: None,
            data,
        })))
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<WorkspaceSymbolResponse>> {
        let query = &params.query;

        // Empty query returns nothing (avoid overwhelming results)
        if query.is_empty() {
            return Ok(Some(WorkspaceSymbolResponse::Flat(vec![])));
        }

        let candidates = workspace_symbol_candidates(&self.index, query);

        // Limit results to avoid overwhelming the client.
        let mut source_cache = HashMap::new();
        let mut symbols = Vec::new();
        for candidate in candidates.into_iter().take(200) {
            if let Some(symbol) =
                workspace_symbol_information(&candidate.symbol, &self.open_files, &mut source_cache)
                    .await
            {
                symbols.push(symbol);
            }
        }

        Ok(Some(WorkspaceSymbolResponse::Flat(symbols)))
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let wants_quickfix =
            code_action_kind_allowed(params.context.only.as_ref(), &CodeActionKind::QUICKFIX);
        let wants_organize_imports = code_action_kind_allowed(
            params.context.only.as_ref(),
            &CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
        );
        let wants_add_return_type = code_action_kind_allowed(
            params.context.only.as_ref(),
            &CodeActionKind::REFACTOR_REWRITE,
        );
        let wants_generate_members = code_action_kind_allowed(
            params.context.only.as_ref(),
            &CodeActionKind::REFACTOR_REWRITE,
        );
        let wants_implement_missing_methods =
            code_action_kind_allowed(params.context.only.as_ref(), &CodeActionKind::QUICKFIX);

        if !wants_quickfix
            && !wants_organize_imports
            && !wants_add_return_type
            && !wants_generate_members
            && !wants_implement_missing_methods
        {
            return Ok(Some(vec![]));
        }

        let uri = params.text_document.uri;
        let uri_str = uri.as_str().to_string();
        let php_version = *self.php_version.lock().await;
        let document_version = self.current_document_version(&uri_str);

        let (
            source,
            file_symbols,
            add_return_type_actions,
            generate_member_actions,
            implement_missing_methods_actions,
        ) = {
            let parser = match self.open_files.get(&uri_str) {
                Some(p) => p,
                None => return Ok(Some(vec![])),
            };
            let tree = match parser.tree() {
                Some(t) => t,
                None => return Ok(Some(vec![])),
            };
            let source = parser.source();
            let file_symbols = self
                .index
                .file_symbols
                .get(&uri_str)
                .map(|entry| entry.value().clone())
                .unwrap_or_else(|| extract_file_symbols(tree, &source, &uri_str));
            let add_return_type_actions = if wants_add_return_type {
                let range = lsp_range_to_byte_range(&source, params.range);
                find_missing_return_type_candidates(tree, &source, range)
                    .into_iter()
                    .filter_map(|candidate| {
                        build_add_return_type_action(
                            uri.clone(),
                            &candidate,
                            php_version,
                            params.range,
                            document_version,
                        )
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let generate_member_actions = if wants_generate_members {
                let range = lsp_range_to_byte_range(&source, params.range);
                let mut actions = Vec::new();
                let visibility_symbol = property_symbol_at_range(&file_symbols, range)
                    .or_else(|| member_symbol_at_range(&file_symbols, range));
                if let Some(symbol) = visibility_symbol {
                    actions.extend(build_change_visibility_actions(
                        uri.clone(),
                        symbol,
                        params.range,
                        document_version,
                    ));
                }
                if let Some(class_sym) = concrete_class_symbol_at_range(&file_symbols, range) {
                    if let Some(action) = build_generate_constructor_action(
                        uri.clone(),
                        &source,
                        &file_symbols,
                        class_sym,
                        params.range,
                        document_version,
                    ) {
                        actions.push(action);
                    }
                }
                if let Some(property) = property_symbol_at_range(&file_symbols, range) {
                    let parent_is_class =
                        property.parent_fqn.as_deref().is_some_and(|parent_fqn| {
                            file_symbols.symbols.iter().any(|sym| {
                                sym.fqn == parent_fqn
                                    && sym.kind == php_lsp_types::PhpSymbolKind::Class
                            })
                        });
                    if parent_is_class {
                        actions.extend(build_generate_accessor_actions(
                            uri.clone(),
                            &self.index,
                            property,
                            params.range,
                            document_version,
                        ));
                    }
                }
                let promote_property =
                    property_symbol_at_range(&file_symbols, range).or_else(|| {
                        property_for_constructor_param_at_range(&source, &file_symbols, range)
                    });
                if let Some(property) = promote_property {
                    if let Some(action) = build_promote_constructor_parameter_action(
                        uri.clone(),
                        &source,
                        &file_symbols,
                        property,
                        params.range,
                        document_version,
                    ) {
                        actions.push(action);
                    }
                }
                if let Some(symbol) = callable_symbol_at_range(&file_symbols, range) {
                    if let Some(action) = build_update_phpdoc_from_signature_action(
                        uri.clone(),
                        &source,
                        symbol,
                        params.range,
                        document_version,
                    ) {
                        actions.push(action);
                    }
                }
                actions
            } else {
                Vec::new()
            };
            let implement_missing_methods_actions = if wants_implement_missing_methods {
                let range = lsp_range_to_byte_range(&source, params.range);
                concrete_class_symbol_at_range(&file_symbols, range)
                    .and_then(|class_sym| {
                        let missing_methods =
                            missing_implementation_methods(&self.index, &file_symbols, class_sym);
                        build_implement_missing_methods_action(
                            uri.clone(),
                            class_sym,
                            &missing_methods,
                            params.range,
                            document_version,
                        )
                    })
                    .into_iter()
                    .collect()
            } else {
                Vec::new()
            };
            (
                source,
                file_symbols,
                add_return_type_actions,
                generate_member_actions,
                implement_missing_methods_actions,
            )
        };

        let mut actions = Vec::new();
        actions.extend(add_return_type_actions);
        actions.extend(generate_member_actions);
        actions.extend(implement_missing_methods_actions);

        if wants_organize_imports {
            if let Some(edit) = build_organize_imports_edit(uri.clone(), &source, &file_symbols) {
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: "Organize imports".to_string(),
                    kind: Some(CodeActionKind::SOURCE_ORGANIZE_IMPORTS),
                    diagnostics: None,
                    edit: Some(edit),
                    command: None,
                    is_preferred: Some(false),
                    disabled: None,
                    data: None,
                }));
            }
        }

        if !wants_quickfix {
            return Ok(Some(actions));
        }

        let diagnostics = if params.context.diagnostics.is_empty() {
            let parser = match self.open_files.get(&uri_str) {
                Some(p) => p,
                None => return Ok(Some(vec![])),
            };
            let diagnostics_mode = *self.diagnostics_mode.lock().await;
            let diagnostic_severity = *self.diagnostic_severity.lock().await;
            compute_diagnostics_with_config(
                &uri_str,
                &parser,
                &self.index,
                diagnostics_mode,
                diagnostic_severity,
                php_version,
            )
            .into_iter()
            .filter(|diag| range_overlaps(diag.range, params.range))
            .collect()
        } else {
            params.context.diagnostics
        };

        let mut quickfix_count = 0usize;

        for diagnostic in diagnostics {
            let Some((import_kind, unresolved_fqn)) =
                unknown_symbol_from_diagnostic(&diagnostic.message)
            else {
                continue;
            };
            let unresolved_short = short_name(&unresolved_fqn);

            let mut candidates: Vec<std::sync::Arc<php_lsp_types::SymbolInfo>> = match import_kind {
                ImportKind::Class => self
                    .index
                    .types
                    .iter()
                    .filter(|entry| {
                        let sym = entry.value();
                        !sym.modifiers.is_builtin
                            && (sym.name == unresolved_short
                                || short_name(&sym.fqn) == unresolved_short)
                    })
                    .map(|entry| entry.value().clone())
                    .collect(),
                ImportKind::Function => self
                    .index
                    .functions
                    .iter()
                    .filter(|entry| {
                        let sym = entry.value();
                        !sym.modifiers.is_builtin
                            && (sym.name == unresolved_short
                                || short_name(&sym.fqn) == unresolved_short)
                    })
                    .map(|entry| entry.value().clone())
                    .collect(),
                ImportKind::Constant => Vec::new(),
            };
            candidates.sort_by(|a, b| a.fqn.cmp(&b.fqn));
            candidates.dedup_by(|a, b| a.fqn == b.fqn);
            candidates.truncate(5);

            for candidate in candidates {
                let Some((edit, alias)) = build_add_import_edit(
                    uri.clone(),
                    &source,
                    &file_symbols,
                    &candidate.fqn,
                    import_kind,
                    diagnostic.range,
                ) else {
                    continue;
                };

                let title = if let Some(alias) = alias {
                    format!("Import {} as {}", candidate.fqn, alias)
                } else {
                    format!("Import {}", candidate.fqn)
                };

                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title,
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: Some(vec![diagnostic.clone()]),
                    edit: Some(edit),
                    command: None,
                    is_preferred: Some(quickfix_count == 0),
                    disabled: None,
                    data: None,
                }));
                quickfix_count += 1;
            }
        }

        Ok(Some(actions))
    }

    async fn code_action_resolve(&self, mut params: CodeAction) -> Result<CodeAction> {
        let Some(data_value) = params.data.clone() else {
            return Ok(params);
        };
        let Ok(data) = serde_json::from_value::<CodeActionData>(data_value) else {
            return Ok(params);
        };

        let CodeActionData {
            action_kind,
            uri,
            range: _requested_range,
            document_version,
            extra,
        } = data;

        match (action_kind, extra) {
            (
                CodeActionDataKind::AddReturnType,
                CodeActionDataExtra::AddReturnType {
                    hint,
                    insert_position,
                },
            ) => {
                if self.current_document_version(&uri) != document_version {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                }

                let Ok(uri_value) = uri.parse::<Uri>() else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };

                let source = match self.open_files.get(&uri) {
                    Some(parser) => parser.source(),
                    None => {
                        params.edit = Some(empty_workspace_edit());
                        return Ok(params);
                    }
                };

                params.edit = Some(add_return_type_edit(
                    uri_value,
                    &source,
                    &hint,
                    insert_position,
                ));
            }
            (
                CodeActionDataKind::ImplementMissingMethods,
                CodeActionDataExtra::ImplementMissingMethods { class_fqn },
            ) => {
                if self.current_document_version(&uri) != document_version {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                }

                let Ok(uri_value) = uri.parse::<Uri>() else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };

                let (source, file_symbols) = match self.open_files.get(&uri) {
                    Some(parser) => {
                        let source = parser.source();
                        let file_symbols = match parser.tree() {
                            Some(tree) => self
                                .index
                                .file_symbols
                                .get(&uri)
                                .map(|entry| entry.value().clone())
                                .unwrap_or_else(|| extract_file_symbols(tree, &source, &uri)),
                            None => {
                                params.edit = Some(empty_workspace_edit());
                                return Ok(params);
                            }
                        };
                        (source, file_symbols)
                    }
                    None => {
                        params.edit = Some(empty_workspace_edit());
                        return Ok(params);
                    }
                };

                let Some(class_sym) = file_symbols.symbols.iter().find(|sym| {
                    sym.fqn == class_fqn && sym.kind == php_lsp_types::PhpSymbolKind::Class
                }) else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };

                let php_version = *self.php_version.lock().await;
                let missing_methods =
                    missing_implementation_methods(&self.index, &file_symbols, class_sym);
                params.edit = implement_missing_methods_edit(
                    uri_value,
                    &source,
                    class_sym,
                    &missing_methods,
                    php_version,
                )
                .or_else(|| Some(empty_workspace_edit()));
            }
            (
                CodeActionDataKind::GenerateConstructor,
                CodeActionDataExtra::GenerateConstructor { class_fqn },
            ) => {
                if self.current_document_version(&uri) != document_version {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                }

                let Ok(uri_value) = uri.parse::<Uri>() else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };

                let (source, file_symbols) = match self.open_files.get(&uri) {
                    Some(parser) => {
                        let source = parser.source();
                        let file_symbols = match parser.tree() {
                            Some(tree) => self
                                .index
                                .file_symbols
                                .get(&uri)
                                .map(|entry| entry.value().clone())
                                .unwrap_or_else(|| extract_file_symbols(tree, &source, &uri)),
                            None => {
                                params.edit = Some(empty_workspace_edit());
                                return Ok(params);
                            }
                        };
                        (source, file_symbols)
                    }
                    None => {
                        params.edit = Some(empty_workspace_edit());
                        return Ok(params);
                    }
                };

                let Some(class_sym) = file_symbols.symbols.iter().find(|sym| {
                    sym.fqn == class_fqn && sym.kind == php_lsp_types::PhpSymbolKind::Class
                }) else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };

                let php_version = *self.php_version.lock().await;
                params.edit = generate_constructor_edit(
                    uri_value,
                    &source,
                    &file_symbols,
                    class_sym,
                    php_version,
                )
                .or_else(|| Some(empty_workspace_edit()));
            }
            (
                CodeActionDataKind::GenerateAccessor,
                CodeActionDataExtra::GenerateAccessor {
                    property_fqn,
                    accessor_kind,
                    method_name,
                },
            ) => {
                if self.current_document_version(&uri) != document_version {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                }

                let Ok(uri_value) = uri.parse::<Uri>() else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };

                let (source, file_symbols) = match self.open_files.get(&uri) {
                    Some(parser) => {
                        let source = parser.source();
                        let file_symbols = match parser.tree() {
                            Some(tree) => self
                                .index
                                .file_symbols
                                .get(&uri)
                                .map(|entry| entry.value().clone())
                                .unwrap_or_else(|| extract_file_symbols(tree, &source, &uri)),
                            None => {
                                params.edit = Some(empty_workspace_edit());
                                return Ok(params);
                            }
                        };
                        (source, file_symbols)
                    }
                    None => {
                        params.edit = Some(empty_workspace_edit());
                        return Ok(params);
                    }
                };

                let Some(property) = file_symbols.symbols.iter().find(|sym| {
                    sym.fqn == property_fqn && sym.kind == php_lsp_types::PhpSymbolKind::Property
                }) else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };

                let php_version = *self.php_version.lock().await;
                params.edit = generate_accessor_edit(
                    uri_value,
                    &source,
                    &file_symbols,
                    property,
                    accessor_kind,
                    &method_name,
                    php_version,
                )
                .or_else(|| Some(empty_workspace_edit()));
            }
            (
                CodeActionDataKind::ChangeVisibility,
                CodeActionDataExtra::ChangeVisibility {
                    symbol_fqn,
                    target_visibility,
                },
            ) => {
                if self.current_document_version(&uri) != document_version {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                }

                let Ok(uri_value) = uri.parse::<Uri>() else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };

                let (source, file_symbols) = match self.open_files.get(&uri) {
                    Some(parser) => {
                        let source = parser.source();
                        let file_symbols = match parser.tree() {
                            Some(tree) => self
                                .index
                                .file_symbols
                                .get(&uri)
                                .map(|entry| entry.value().clone())
                                .unwrap_or_else(|| extract_file_symbols(tree, &source, &uri)),
                            None => {
                                params.edit = Some(empty_workspace_edit());
                                return Ok(params);
                            }
                        };
                        (source, file_symbols)
                    }
                    None => {
                        params.edit = Some(empty_workspace_edit());
                        return Ok(params);
                    }
                };

                let Some(symbol) = file_symbols
                    .symbols
                    .iter()
                    .find(|sym| sym.fqn == symbol_fqn)
                else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };

                params.edit = change_visibility_edit(uri_value, &source, symbol, target_visibility)
                    .or_else(|| Some(empty_workspace_edit()));
            }
            (
                CodeActionDataKind::PromoteConstructorParameter,
                CodeActionDataExtra::PromoteConstructorParameter { property_fqn },
            ) => {
                if self.current_document_version(&uri) != document_version {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                }

                let Ok(uri_value) = uri.parse::<Uri>() else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };

                let (source, file_symbols) = match self.open_files.get(&uri) {
                    Some(parser) => {
                        let source = parser.source();
                        let file_symbols = match parser.tree() {
                            Some(tree) => self
                                .index
                                .file_symbols
                                .get(&uri)
                                .map(|entry| entry.value().clone())
                                .unwrap_or_else(|| extract_file_symbols(tree, &source, &uri)),
                            None => {
                                params.edit = Some(empty_workspace_edit());
                                return Ok(params);
                            }
                        };
                        (source, file_symbols)
                    }
                    None => {
                        params.edit = Some(empty_workspace_edit());
                        return Ok(params);
                    }
                };

                let Some(property) = file_symbols.symbols.iter().find(|sym| {
                    sym.fqn == property_fqn && sym.kind == php_lsp_types::PhpSymbolKind::Property
                }) else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };

                params.edit =
                    promote_constructor_parameter_edit(uri_value, &source, &file_symbols, property)
                        .or_else(|| Some(empty_workspace_edit()));
            }
            (
                CodeActionDataKind::UpdatePhpDoc,
                CodeActionDataExtra::UpdatePhpDoc { symbol_fqn },
            ) => {
                if self.current_document_version(&uri) != document_version {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                }

                let Ok(uri_value) = uri.parse::<Uri>() else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };

                let (source, file_symbols) = match self.open_files.get(&uri) {
                    Some(parser) => {
                        let source = parser.source();
                        let file_symbols = match parser.tree() {
                            Some(tree) => self
                                .index
                                .file_symbols
                                .get(&uri)
                                .map(|entry| entry.value().clone())
                                .unwrap_or_else(|| extract_file_symbols(tree, &source, &uri)),
                            None => {
                                params.edit = Some(empty_workspace_edit());
                                return Ok(params);
                            }
                        };
                        (source, file_symbols)
                    }
                    None => {
                        params.edit = Some(empty_workspace_edit());
                        return Ok(params);
                    }
                };

                let Some(symbol) = file_symbols.symbols.iter().find(|sym| {
                    sym.fqn == symbol_fqn
                        && matches!(
                            sym.kind,
                            php_lsp_types::PhpSymbolKind::Function
                                | php_lsp_types::PhpSymbolKind::Method
                        )
                }) else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };

                params.edit = update_phpdoc_from_signature_edit(uri_value, &source, symbol)
                    .or_else(|| Some(empty_workspace_edit()));
            }
            _ => {
                params.edit = Some(empty_workspace_edit());
            }
        }

        Ok(params)
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let uri_str = params
            .text_document_position_params
            .text_document
            .uri
            .as_str()
            .to_string();
        let pos = params.text_document_position_params.position;
        tracing::debug!("signatureHelp: {}:{}:{}", uri_str, pos.line, pos.character);

        let (sym_at_pos, active_parameter) = {
            let parser = match self.open_files.get(&uri_str) {
                Some(p) => p,
                None => return Ok(None),
            };
            let tree = match parser.tree() {
                Some(t) => t,
                None => return Ok(None),
            };
            let source = parser.source();
            let byte_col = utf16_col_to_byte(&source, pos.line, pos.character);
            let file_symbols = self
                .index
                .file_symbols
                .get(&uri_str)
                .map(|entry| entry.value().clone())
                .unwrap_or_else(|| extract_file_symbols(tree, &source, &uri_str));

            let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
                self.resolve_member_type(class_fqn, member_name)
            };

            let context = match signature_help_context_at_position(
                tree,
                &source,
                pos.line,
                byte_col,
                &file_symbols,
                Some(&resolver),
            ) {
                Some(context) => context,
                None => return Ok(None),
            };

            (context.symbol, context.active_parameter)
        };

        let symbol_info = self
            .resolve_fqn_lazy_with_fallback(&sym_at_pos.fqn, sym_at_pos.ref_kind)
            .await;

        let symbol_info = if symbol_info.is_none() && sym_at_pos.ref_kind == RefKind::Constructor {
            if let Some(class_fqn) = sym_at_pos.fqn.strip_suffix("::__construct") {
                self.resolve_fqn_lazy_with_fallback(class_fqn, RefKind::ClassName)
                    .await
            } else {
                None
            }
        } else {
            symbol_info
        };

        Ok(symbol_info.and_then(|sym| build_signature_help(&sym, active_parameter)))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri_str = params
            .text_document_position
            .text_document
            .uri
            .as_str()
            .to_string();
        let pos = params.text_document_position.position;
        tracing::debug!("completion: {}:{}:{}", uri_str, pos.line, pos.character);

        let (tree, source) = {
            let parser = match self.open_files.get(&uri_str) {
                Some(p) => p,
                None => return Ok(None),
            };
            let tree = match parser.tree() {
                Some(t) => t.clone(),
                None => return Ok(None),
            };
            (tree, parser.source())
        };
        let byte_col = utf16_col_to_byte(&source, pos.line, pos.character);
        let file_symbols = extract_file_symbols(&tree, &source, &uri_str);

        // Detect completion context
        let context = detect_context(&tree, &source, pos.line, byte_col, &file_symbols);
        let context = match context {
            php_lsp_completion::context::CompletionContext::MemberAccess {
                object_expr,
                class_fqn,
                member_prefix,
            } => php_lsp_completion::context::CompletionContext::MemberAccess {
                class_fqn: class_fqn.or_else(|| {
                    self.infer_completion_object_type(
                        &object_expr,
                        &tree,
                        &source,
                        &file_symbols,
                        pos.line,
                        byte_col,
                    )
                }),
                object_expr,
                member_prefix,
            },
            other => other,
        };

        if context == php_lsp_completion::context::CompletionContext::None {
            return Ok(None);
        }

        let completion_class_fqn =
            match &context {
                php_lsp_completion::context::CompletionContext::MemberAccess {
                    class_fqn: Some(class_fqn),
                    ..
                } => Some(class_fqn.clone()),
                php_lsp_completion::context::CompletionContext::StaticAccess {
                    class_fqn, ..
                } if !class_fqn.is_empty() => Some(class_fqn.clone()),
                _ => None,
            };

        if let Some(class_fqn) = completion_class_fqn {
            self.lazy_index_class_dependencies(&class_fqn).await;
        }

        // Get completion items from the provider
        let mut lsp_items = provide_completions(&context, &self.index, &file_symbols);
        if let php_lsp_completion::context::CompletionContext::Variable { prefix } = &context {
            add_local_variable_completion_items(
                &mut lsp_items,
                &tree,
                &source,
                pos.line,
                byte_col,
                prefix,
            );
        }

        let enable_auto_imports = matches!(
            context,
            php_lsp_completion::context::CompletionContext::Free { .. }
                | php_lsp_completion::context::CompletionContext::Namespace { .. }
        );

        // Convert lsp_types::CompletionItem to ls_types::CompletionItem
        // We need to map between the two different type systems
        let items: Vec<CompletionItem> = lsp_items
            .into_iter()
            .map(|mut item| {
                let kind = item.kind.map(lsp_completion_kind_to_ls);

                let tags = item.tags.map(|tags| {
                    tags.into_iter()
                        .filter_map(|t| {
                            if t == lsp_types::CompletionItemTag::DEPRECATED {
                                Some(CompletionItemTag::DEPRECATED)
                            } else {
                                None
                            }
                        })
                        .collect()
                });

                let auto_import_edit = if enable_auto_imports {
                    item.data
                        .as_ref()
                        .and_then(|data| data.as_str())
                        .and_then(|fqn| self.index.resolve_fqn(fqn))
                        .and_then(|sym| {
                            build_completion_auto_import_edit(&source, &file_symbols, &sym)
                        })
                } else {
                    None
                };
                let mut additional_text_edits: Vec<TextEdit> = item
                    .additional_text_edits
                    .take()
                    .unwrap_or_default()
                    .into_iter()
                    .map(lsp_text_edit_to_ls)
                    .collect();
                if let Some(edit) = auto_import_edit {
                    additional_text_edits.insert(0, edit);
                }
                let additional_text_edits =
                    (!additional_text_edits.is_empty()).then_some(additional_text_edits);

                CompletionItem {
                    label: item.label,
                    kind,
                    detail: item.detail,
                    sort_text: item.sort_text,
                    filter_text: item.filter_text,
                    insert_text: item.insert_text,
                    insert_text_format: item.insert_text_format.map(lsp_insert_text_format_to_ls),
                    additional_text_edits,
                    commit_characters: item.commit_characters,
                    tags,
                    data: item.data,
                    ..Default::default()
                }
            })
            .collect();

        if items.is_empty() {
            Ok(None)
        } else {
            Ok(Some(CompletionResponse::Array(items)))
        }
    }

    async fn completion_resolve(&self, mut item: CompletionItem) -> Result<CompletionItem> {
        let virtual_data =
            phpdoc_virtual_completion_data(&item).map(|(owner_fqn, member_kind, member_name)| {
                (
                    owner_fqn.to_string(),
                    member_kind.to_string(),
                    member_name.to_string(),
                )
            });
        if let Some((owner_fqn, member_kind, member_name)) = virtual_data {
            let kind = match member_kind.as_str() {
                "property" => PhpDocVirtualMemberKind::Property,
                "method" => PhpDocVirtualMemberKind::Method,
                _ => return Ok(item),
            };
            if let Some(member) = phpdoc_virtual_member(&self.index, &owner_fqn, &member_name, kind)
            {
                item.detail = Some(match member.kind {
                    PhpDocVirtualMemberKind::Property => {
                        let access = member
                            .access
                            .map(phpdoc_property_tag)
                            .unwrap_or("@property");
                        match &member.type_info {
                            Some(type_info) => format!("{} {}", access, type_info),
                            None => access.to_string(),
                        }
                    }
                    PhpDocVirtualMemberKind::Method => {
                        let mut detail = String::from("()");
                        if let Some(ref return_type) = member.return_type {
                            detail.push_str(": ");
                            detail.push_str(&return_type.to_string());
                        }
                        detail
                    }
                });
                item.documentation = Some(Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: phpdoc_virtual_member_markdown(&member),
                }));
            }
            return Ok(item);
        }

        // Try to resolve more details for the completion item
        // The FQN is stored in item.data
        if let Some(ref data) = item.data {
            if let Some(fqn) = data.as_str() {
                if let Some(sym) = self.resolve_fqn_lazy(fqn).await {
                    // Add full documentation
                    let mut doc_parts = Vec::new();

                    // Signature
                    if let Some(ref sig) = sym.signature {
                        let params_str: Vec<String> = sig
                            .params
                            .iter()
                            .map(|p| {
                                let mut s = String::new();
                                if let Some(ref t) = p.type_info {
                                    s.push_str(&t.to_string());
                                    s.push(' ');
                                }
                                if p.is_variadic {
                                    s.push_str("...");
                                }
                                if p.is_by_ref {
                                    s.push('&');
                                }
                                s.push('$');
                                s.push_str(&p.name);
                                if let Some(ref default) = p.default_value {
                                    s.push_str(" = ");
                                    s.push_str(default);
                                }
                                s
                            })
                            .collect();
                        let mut sig_str = format!("({})", params_str.join(", "));
                        if let Some(ref ret) = sig.return_type {
                            sig_str.push_str(&format!(": {}", ret));
                        }
                        item.detail = Some(sig_str);
                    }

                    // PHPDoc
                    if let Some(ref doc) = sym.doc_comment {
                        let phpdoc = parse_phpdoc(doc);
                        if let Some(ref summary) = phpdoc.summary {
                            doc_parts.push(summary.clone());
                        }

                        if phpdoc.deprecated.is_some() {
                            doc_parts.push("**@deprecated**".to_string());
                            if let Some(ref tags) = item.tags {
                                if !tags.contains(&CompletionItemTag::DEPRECATED) {
                                    let mut tags = tags.clone();
                                    tags.push(CompletionItemTag::DEPRECATED);
                                    item.tags = Some(tags);
                                }
                            } else {
                                item.tags = Some(vec![CompletionItemTag::DEPRECATED]);
                            }
                        }

                        // Param docs
                        if !phpdoc.params.is_empty() {
                            doc_parts.push(String::new());
                            for param in &phpdoc.params {
                                let type_str = param
                                    .type_info
                                    .as_ref()
                                    .map(|t| format!(" `{}`", t))
                                    .unwrap_or_default();
                                let desc = param
                                    .description
                                    .as_ref()
                                    .map(|d| format!(" — {}", d))
                                    .unwrap_or_default();
                                doc_parts
                                    .push(format!("@param{} `${}`{}", type_str, param.name, desc));
                            }
                        }

                        // Return type
                        if let Some(ref ret) = phpdoc.return_type {
                            doc_parts.push(format!("\n@return `{}`", ret));
                        }

                        let extra_sections = phpdoc_extra_markdown_sections(&phpdoc);
                        if !extra_sections.is_empty() {
                            doc_parts.push(String::new());
                            doc_parts.extend(extra_sections);
                        }
                    }

                    if !doc_parts.is_empty() {
                        item.documentation = Some(Documentation::MarkupContent(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: doc_parts.join("\n"),
                        }));
                    }
                }
            }
        }

        Ok(item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use php_lsp_types::*;

    fn make_symbol(
        name: &str,
        fqn: &str,
        kind: PhpSymbolKind,
        range: (u32, u32, u32, u32),
        parent_fqn: Option<&str>,
    ) -> SymbolInfo {
        SymbolInfo {
            name: name.to_string(),
            fqn: fqn.to_string(),
            kind,
            uri: "file:///test.php".to_string(),
            range,
            selection_range: range,
            visibility: Visibility::Public,
            modifiers: SymbolModifiers::default(),
            doc_comment: None,
            signature: None,
            parent_fqn: parent_fqn.map(|s| s.to_string()),
            extends: vec![],
            implements: vec![],
            traits: vec![],
        }
    }

    fn offset_at(source: &str, line: u32, col: u32) -> usize {
        let mut current_line = 0u32;
        let mut line_start = 0usize;
        for (idx, ch) in source.char_indices() {
            if current_line == line {
                return line_start + col as usize;
            }
            if ch == '\n' {
                current_line += 1;
                line_start = idx + 1;
            }
        }
        line_start + col as usize
    }

    #[test]
    fn test_infer_new_expression_type_from_parenthesized_expression() {
        let file_symbols = FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![UseStatement {
                fqn: "Symfony\\Component\\Form\\Guess\\TypeGuess".to_string(),
                alias: None,
                kind: UseKind::Class,
                range: (0, 0, 0, 0),
            }],
            symbols: vec![],
        };

        assert_eq!(
            infer_new_expression_type("(new \\ReflectionClass($v))", &file_symbols).as_deref(),
            Some("ReflectionClass")
        );
        assert_eq!(
            infer_new_expression_type("((new TypeGuess(Foo::class)))", &file_symbols).as_deref(),
            Some("Symfony\\Component\\Form\\Guess\\TypeGuess")
        );
    }

    #[test]
    fn test_phpdoc_extra_markdown_sections_include_virtual_members() {
        let phpdoc = parse_phpdoc(
            "/**\n * @property-read string $slug Service slug\n * @method User owner()\n * @throws \\RuntimeException\n */",
        );
        let sections = phpdoc_extra_markdown_sections(&phpdoc).join("\n");

        assert!(sections.contains("**Throws:**"));
        assert!(sections.contains("`\\RuntimeException`"));
        assert!(sections.contains("`@property-read string $slug` - Service slug"));
        assert!(sections.contains("`@method User owner()`"));
    }

    #[test]
    fn test_phpdoc_virtual_member_range_points_to_tag_name() {
        let source =
            "<?php\n/**\n * @property-read string $slug Service slug\n */\nclass Service {}\n";
        let doc_start = source.find("/**").expect("doc comment start");
        let doc_end = source.find("*/").expect("doc comment end") + 2;
        let doc_comment = &source[doc_start..doc_end];
        let mut owner = make_symbol(
            "Service",
            "App\\Service",
            PhpSymbolKind::Class,
            (4, 6, 4, 13),
            None,
        );
        owner.doc_comment = Some(doc_comment.to_string());
        let member = PhpDocVirtualMember {
            owner: Arc::new(owner),
            name: "slug".to_string(),
            kind: PhpDocVirtualMemberKind::Property,
            type_info: Some(TypeInfo::Simple("string".to_string())),
            access: Some(PhpDocPropertyAccess::ReadOnly),
            return_type: None,
            description: Some("Service slug".to_string()),
            is_static: false,
        };

        let range = phpdoc_virtual_member_range(source, doc_comment, doc_start, &member)
            .expect("virtual member range");
        let start = offset_at(source, range.0, range.1);
        let end = offset_at(source, range.2, range.3);

        assert_eq!(&source[start..end], "slug");
    }

    #[test]
    fn test_php_kind_to_lsp() {
        assert_eq!(php_kind_to_lsp(PhpSymbolKind::Class), SymbolKind::CLASS);
        assert_eq!(
            php_kind_to_lsp(PhpSymbolKind::Function),
            SymbolKind::FUNCTION
        );
        assert_eq!(php_kind_to_lsp(PhpSymbolKind::Method), SymbolKind::METHOD);
        assert_eq!(
            php_kind_to_lsp(PhpSymbolKind::Property),
            SymbolKind::PROPERTY
        );
        assert_eq!(
            php_kind_to_lsp(PhpSymbolKind::EnumCase),
            SymbolKind::ENUM_MEMBER
        );
        assert_eq!(
            php_kind_to_lsp(PhpSymbolKind::Namespace),
            SymbolKind::NAMESPACE
        );
    }

    #[test]
    fn test_document_symbol_hierarchy() {
        // Simulate file with namespace → class → methods
        let file_symbols = FileSymbols {
            namespace: Some("App\\Service".to_string()),
            use_statements: vec![],
            symbols: vec![
                make_symbol(
                    "App\\Service",
                    "App\\Service",
                    PhpSymbolKind::Namespace,
                    (0, 0, 20, 0),
                    None,
                ),
                make_symbol(
                    "UserService",
                    "App\\Service\\UserService",
                    PhpSymbolKind::Class,
                    (2, 0, 18, 1),
                    None,
                ),
                make_symbol(
                    "getUser",
                    "App\\Service\\UserService::getUser",
                    PhpSymbolKind::Method,
                    (4, 4, 8, 5),
                    Some("App\\Service\\UserService"),
                ),
                make_symbol(
                    "$name",
                    "App\\Service\\UserService::$name",
                    PhpSymbolKind::Property,
                    (3, 4, 3, 30),
                    Some("App\\Service\\UserService"),
                ),
            ],
        };

        // Index file
        let index = WorkspaceIndex::new();
        index.update_file("file:///test.php", file_symbols);

        // Retrieve and verify structure
        let fs = index.file_symbols.get("file:///test.php").unwrap();
        let symbols = &fs.symbols;

        // Should have 4 symbols total
        assert_eq!(symbols.len(), 4);

        // Verify the class has proper kind
        let class = symbols
            .iter()
            .find(|s| s.kind == PhpSymbolKind::Class)
            .unwrap();
        assert_eq!(class.name, "UserService");

        // Verify members belong to the class
        let members: Vec<_> = symbols
            .iter()
            .filter(|s| s.parent_fqn.as_deref() == Some("App\\Service\\UserService"))
            .collect();
        assert_eq!(members.len(), 2); // getUser + $name
    }

    #[test]
    fn test_workspace_symbol_search() {
        let index = WorkspaceIndex::new();
        let file_symbols = FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![
                make_symbol(
                    "FooController",
                    "App\\FooController",
                    PhpSymbolKind::Class,
                    (0, 0, 10, 0),
                    None,
                ),
                make_symbol(
                    "BarService",
                    "App\\BarService",
                    PhpSymbolKind::Class,
                    (12, 0, 20, 0),
                    None,
                ),
                make_symbol(
                    "helper_foo",
                    "App\\helper_foo",
                    PhpSymbolKind::Function,
                    (22, 0, 25, 0),
                    None,
                ),
            ],
        };
        index.update_file("file:///app.php", file_symbols);

        // Search for "foo" should find FooController + helper_foo
        let results = index.search("foo");
        assert_eq!(results.len(), 2);

        // Search for "Service" should find BarService
        let results = index.search("Service");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "BarService");

        // Search for "xyz" should find nothing
        let results = index.search("xyz");
        assert!(results.is_empty());
    }

    #[test]
    fn test_workspace_symbol_candidates_rank_filters_and_members() {
        let index = WorkspaceIndex::new();
        let file_symbols = FileSymbols {
            namespace: Some("App\\Service".to_string()),
            use_statements: vec![],
            symbols: vec![
                make_symbol(
                    "UserService",
                    "App\\Service\\UserService",
                    PhpSymbolKind::Class,
                    (0, 0, 10, 0),
                    None,
                ),
                make_symbol(
                    "buildUser",
                    "App\\Service\\UserService::buildUser",
                    PhpSymbolKind::Method,
                    (2, 4, 4, 5),
                    Some("App\\Service\\UserService"),
                ),
                make_symbol(
                    "UserServiceFactory",
                    "App\\Factory\\UserServiceFactory",
                    PhpSymbolKind::Class,
                    (20, 0, 25, 0),
                    None,
                ),
            ],
        };
        index.update_file("file:///app.php", file_symbols);

        let candidates = workspace_symbol_candidates(&index, "usrsvc");
        let names: Vec<_> = candidates
            .iter()
            .map(|candidate| candidate.symbol.name.as_str())
            .collect();
        assert!(
            names.starts_with(&["UserService"]),
            "fuzzy query should rank the closest type first, got: {:?}",
            names
        );

        let method_candidates = workspace_symbol_candidates(&index, "method:build");
        assert_eq!(method_candidates.len(), 1);
        assert_eq!(method_candidates[0].symbol.name, "buildUser");
        assert_eq!(method_candidates[0].symbol.kind, PhpSymbolKind::Method);

        let class_candidates = workspace_symbol_candidates(&index, "class:build");
        assert!(
            class_candidates.is_empty(),
            "kind filter should exclude method-only matches"
        );
    }

    #[test]
    fn test_workspace_symbol_lsp_range_converts_byte_columns_to_utf16() {
        let source = "<?php\n$привет = 1; class Demo {}\n";
        let range = workspace_symbol_lsp_range(Some(source), (1, 19, 1, 24));

        assert_eq!(range.start, Position::new(1, 13));
        assert_eq!(range.end, Position::new(1, 18));
    }

    #[test]
    fn test_workspace_reindex_keeps_vendor_and_stub_symbols() {
        let index = WorkspaceIndex::new();
        index.update_file(
            "file:///tmp/project/src/Foo.php",
            FileSymbols {
                namespace: Some("App".to_string()),
                use_statements: vec![],
                symbols: vec![make_symbol(
                    "Foo",
                    "App\\Foo",
                    PhpSymbolKind::Class,
                    (0, 0, 1, 0),
                    None,
                )],
            },
        );
        index.update_file(
            "file:///tmp/project/vendor/acme/pkg/Bar.php",
            FileSymbols {
                namespace: Some("Vendor\\Pkg".to_string()),
                use_statements: vec![],
                symbols: vec![make_symbol(
                    "Bar",
                    "Vendor\\Pkg\\Bar",
                    PhpSymbolKind::Class,
                    (0, 0, 1, 0),
                    None,
                )],
            },
        );
        index.update_file(
            "phpstub://Core/Core.php",
            FileSymbols {
                namespace: None,
                use_statements: vec![],
                symbols: vec![make_symbol(
                    "stdClass",
                    "stdClass",
                    PhpSymbolKind::Class,
                    (0, 0, 1, 0),
                    None,
                )],
            },
        );

        let removed = remove_indexed_file_symbols(&index, &[PathBuf::from("/tmp/project")]);

        assert_eq!(removed, 1);
        assert!(index.resolve_fqn("App\\Foo").is_none());
        assert!(index.resolve_fqn("Vendor\\Pkg\\Bar").is_some());
        assert!(index.resolve_fqn("stdClass").is_some());
    }

    #[test]
    fn test_workspace_index_reads_non_utf8_php_lossily() {
        let tmp =
            std::env::temp_dir().join(format!("php-lsp-non-utf8-index-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let file = tmp.join("Legacy.php");
        std::fs::write(
            &file,
            b"<?php\nclass Legacy {\n    public const VALUE = \"\xff\";\n}\n",
        )
        .unwrap();

        let parsed = parse_workspace_file_for_index(file);

        assert!(
            parsed.error.is_none(),
            "got parse error: {:?}",
            parsed.error
        );
        assert!(parsed
            .file_symbols
            .as_ref()
            .is_some_and(|symbols| symbols.symbols.iter().any(|sym| sym.fqn == "Legacy")));

        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[test]
    fn test_workspace_index_parallel_updates_are_safe() {
        let index = Arc::new(WorkspaceIndex::new());
        let mut handles = Vec::new();

        for i in 0..32 {
            let index = index.clone();
            handles.push(std::thread::spawn(move || {
                let uri = format!("file:///tmp/project/src/Foo{}.php", i);
                let fqn = format!("App\\Foo{}", i);
                index.update_file(
                    &uri,
                    FileSymbols {
                        namespace: Some("App".to_string()),
                        use_statements: vec![],
                        symbols: vec![make_symbol(
                            &format!("Foo{}", i),
                            &fqn,
                            PhpSymbolKind::Class,
                            (0, 0, 1, 0),
                            None,
                        )],
                    },
                );
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        for i in 0..32 {
            assert!(index.resolve_fqn(&format!("App\\Foo{}", i)).is_some());
        }
    }

    #[test]
    fn test_document_version_ordering_accepts_only_newer_versions() {
        assert!(document_version_is_newer(None, 1));
        assert!(document_version_is_newer(Some(1), 2));
        assert!(!document_version_is_newer(Some(2), 2));
        assert!(!document_version_is_newer(Some(3), 2));
    }

    #[test]
    fn test_cache_configs_use_separate_namespaces() {
        let root = Path::new("/tmp/project");
        let workspace_config =
            workspace_index_cache_config(Some(root), PhpVersion::DEFAULT, &[], &[], &[], None);
        let stubs_config = stubs_index_cache_config(
            Path::new("/tmp/project/stubs"),
            PhpVersion::DEFAULT,
            &["Core".to_string()],
        );
        let vendor_config = vendor_index_cache_config(root, PhpVersion::DEFAULT, &[]);

        assert_eq!(workspace_config.namespace, CacheNamespace::Workspace);
        assert_eq!(stubs_config.namespace, CacheNamespace::Stubs);
        assert_eq!(vendor_config.namespace, CacheNamespace::Vendor);
        assert_ne!(workspace_config.config_hash(), stubs_config.config_hash());
        assert_ne!(workspace_config.config_hash(), vendor_config.config_hash());
    }

    #[test]
    fn test_vendor_file_lru_evicts_old_index_entries() {
        let index = WorkspaceIndex::new();
        let uri1 = "file:///tmp/project/vendor/acme/pkg/One.php";
        let uri2 = "file:///tmp/project/vendor/acme/pkg/Two.php";
        index.update_file(
            uri1,
            FileSymbols {
                namespace: Some("Vendor\\Pkg".to_string()),
                use_statements: vec![],
                symbols: vec![make_symbol(
                    "One",
                    "Vendor\\Pkg\\One",
                    PhpSymbolKind::Class,
                    (0, 0, 1, 0),
                    None,
                )],
            },
        );
        index.update_file(
            uri2,
            FileSymbols {
                namespace: Some("Vendor\\Pkg".to_string()),
                use_statements: vec![],
                symbols: vec![make_symbol(
                    "Two",
                    "Vendor\\Pkg\\Two",
                    PhpSymbolKind::Class,
                    (0, 0, 1, 0),
                    None,
                )],
            },
        );

        let mut lru = VendorFileLru::with_capacity(1);
        assert!(lru.touch(uri1.to_string()).is_empty());
        let evicted = lru.touch(uri2.to_string());
        for uri in evicted {
            index.remove_file(&uri);
        }

        assert!(index.resolve_fqn("Vendor\\Pkg\\One").is_none());
        assert!(index.resolve_fqn("Vendor\\Pkg\\Two").is_some());
    }

    #[test]
    fn test_vendor_autoload_map_parses_psr4_and_files() {
        let tmp = std::env::temp_dir().join(format!(
            "php-lsp-vendor-autoload-map-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let vendor_dir = tmp.join("vendor");
        let composer_dir = vendor_dir.join("composer");
        std::fs::create_dir_all(&composer_dir).unwrap();

        let installed_json = serde_json::json!({
            "packages": [
                {
                    "name": "acme/library",
                    "install-path": "../acme/library",
                    "autoload": {
                        "psr-4": {
                            "Acme\\Library\\": ["src/", "generated/"]
                        },
                        "files": ["bootstrap.php"]
                    }
                }
            ]
        });
        std::fs::write(
            composer_dir.join("installed.json"),
            serde_json::to_string(&installed_json).unwrap(),
        )
        .unwrap();

        let map = parse_vendor_autoload_map(&vendor_dir).unwrap();
        let paths = resolve_vendor_paths_from_map("Acme\\Library\\Http\\Client", &map).unwrap();

        assert_eq!(paths.len(), 2);
        assert!(
            paths
                .iter()
                .any(|path| path.to_string_lossy().ends_with("src/Http/Client.php")),
            "Expected src PSR-4 path, got: {:?}",
            paths
        );
        assert!(
            map.files
                .iter()
                .any(|path| path.to_string_lossy().ends_with("bootstrap.php")),
            "Expected autoload file path, got: {:?}",
            map.files
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_compute_diagnostics_reports_duplicate_workspace_symbols() {
        let uri1 = "file:///one.php";
        let uri2 = "file:///two.php";
        let code1 = "<?php\nnamespace App;\nclass Duplicate {}\n";
        let code2 = "<?php\nnamespace App;\nclass Duplicate {}\n";

        let mut parser1 = FileParser::new();
        parser1.parse_full(code1);
        let mut parser2 = FileParser::new();
        parser2.parse_full(code2);

        let index = WorkspaceIndex::new();
        let symbols1 = extract_file_symbols(parser1.tree().unwrap(), code1, uri1);
        let symbols2 = extract_file_symbols(parser2.tree().unwrap(), code2, uri2);
        index.update_file(uri1, symbols1);
        index.update_file(uri2, symbols2);

        let diagnostics = compute_diagnostics(
            uri1,
            &parser1,
            &index,
            DiagnosticsMode::BasicSemantic,
            PhpVersion::DEFAULT,
        );

        assert!(
            diagnostics
                .iter()
                .any(|diag| diag.message == "Duplicate symbol: App\\Duplicate"),
            "Expected duplicate workspace symbol diagnostic, got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn test_compute_diagnostics_reports_member_access_errors() {
        let uri = "file:///members.php";
        let code = r#"<?php
namespace App;

class Service {
    public string $name;
    public static string $count;
    protected object $request;
    private function hidden(): void {}
    public static function stat(): void {}
    public function inst(): void {}
    public function fluent(): static { return $this; }
    public function request(): void {}
    public const OK = 'ok';
}

class Demo {
    public function run(Service $service): void {
        $service->missing();
        echo $service->missingProp;
        echo Service::MISSING;
        $service->stat();
        Service::inst();
        $service->fluent();
        $service->request();
        echo $service->count;
        echo Service::$name;
        $service->hidden();
    }
}
"#;

        let mut parser = FileParser::new();
        parser.parse_full(code);

        let index = WorkspaceIndex::new();
        let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
        index.update_file(uri, symbols);

        let diagnostics = compute_diagnostics(
            uri,
            &parser,
            &index,
            DiagnosticsMode::BasicSemantic,
            PhpVersion::DEFAULT,
        );
        let messages: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect();

        for expected in [
            "Unknown method: App\\Service::missing",
            "Unknown property: App\\Service::$missingProp",
            "Unknown class constant: App\\Service::MISSING",
            "Static method called as instance method: App\\Service::stat",
            "Instance method called statically: App\\Service::inst",
            "Static property accessed as instance property: App\\Service::$count",
            "Instance property accessed statically: App\\Service::$name",
            "Private member is not accessible here: App\\Service::hidden",
        ] {
            assert!(
                messages.contains(&expected),
                "Expected `{}` in diagnostics, got: {:?}",
                expected,
                messages
            );
        }

        assert!(
            !messages.contains(&"Static method called as instance method: App\\Service::fluent"),
            "Method returning `static` must not be treated as a static method: {:?}",
            messages
        );
        assert!(
            !messages.contains(&"Protected member is not accessible here: App\\Service::$request"),
            "Method calls must not resolve to same-named properties: {:?}",
            messages
        );
    }

    #[test]
    fn test_compute_diagnostics_skips_members_on_unindexed_imported_types() {
        let uri = "file:///external-client.php";
        let code = r#"<?php
namespace App;

use Vendor\Package\Client;

class Demo {
    public function run(Client $client): void {
        $client->send();
    }
}
"#;

        let mut parser = FileParser::new();
        parser.parse_full(code);

        let index = WorkspaceIndex::new();
        let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
        index.update_file(uri, symbols);

        let diagnostics = compute_diagnostics(
            uri,
            &parser,
            &index,
            DiagnosticsMode::BasicSemantic,
            PhpVersion::DEFAULT,
        );
        let messages: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect();

        assert!(
            !messages.contains(&"Unknown method: Vendor\\Package\\Client::send"),
            "Unindexed imported types should not get guessed member diagnostics: {:?}",
            messages
        );
    }

    #[test]
    fn test_compute_diagnostics_allows_framework_heavy_dynamic_patterns() {
        let uri = "file:///framework-heavy.php";
        let code = r#"<?php
namespace Symfony\Bundle\FrameworkBundle\Controller;
abstract class AbstractController {}

namespace Illuminate\Database\Eloquent;
class Model {}
class Builder {}

namespace App\Models;
class User extends \Illuminate\Database\Eloquent\Model {}

namespace App\Controller;

use App\Models\User;
use Symfony\Bundle\FrameworkBundle\Controller\AbstractController;

final class DashboardController extends AbstractController
{
    public function index(User $user): void
    {
        $this->render('dashboard.html.twig');
        $this->json(['ok' => true]);
        $this->redirectToRoute('dashboard');

        echo $user->email;
        User::whereEmail('demo@example.com')->firstOrFail();
    }
}
"#;

        let mut parser = FileParser::new();
        parser.parse_full(code);

        let index = WorkspaceIndex::new();
        let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
        index.update_file(uri, symbols);

        let diagnostics = compute_diagnostics(
            uri,
            &parser,
            &index,
            DiagnosticsMode::BasicSemantic,
            PhpVersion::DEFAULT,
        );
        let messages: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect();

        for unexpected in [
            "Unknown method: App\\Controller\\DashboardController::render",
            "Unknown method: App\\Controller\\DashboardController::json",
            "Unknown method: App\\Controller\\DashboardController::redirectToRoute",
            "Unknown property: App\\Models\\User::$email",
            "Unknown method: App\\Models\\User::whereEmail",
        ] {
            assert!(
                !messages.iter().any(|message| message.contains(unexpected)),
                "Did not expect `{}` in diagnostics, got: {:?}",
                unexpected,
                messages
            );
        }
    }

    #[test]
    fn test_compute_diagnostics_allows_promoted_properties_on_self_typed_parameter() {
        let uri = "file:///promoted-self-defaults.php";
        let code = r#"<?php
namespace App\Diagnostics;

final class PromotedSelfDefaults
{
    public function __construct(
        public ?string $objectManager = null,
        public ?array $mapping = null,
    ) {
    }

    public function withDefaults(self $defaults): static
    {
        $clone = clone $this;
        $clone->objectManager ??= $defaults->objectManager;
        $clone->mapping ??= $defaults->mapping ?? [];

        return $clone;
    }
}
"#;

        let mut parser = FileParser::new();
        parser.parse_full(code);

        let index = WorkspaceIndex::new();
        let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
        index.update_file(uri, symbols);

        let diagnostics = compute_diagnostics(
            uri,
            &parser,
            &index,
            DiagnosticsMode::BasicSemantic,
            PhpVersion::DEFAULT,
        );
        let messages: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect();

        for unexpected in [
            "Unknown property: App\\Diagnostics\\PromotedSelfDefaults::$objectManager",
            "Unknown property: App\\Diagnostics\\PromotedSelfDefaults::$mapping",
            "Unknown property: self::$objectManager",
            "Unknown property: self::$mapping",
        ] {
            assert!(
                !messages.contains(&unexpected),
                "Did not expect `{}` in diagnostics, got: {:?}",
                unexpected,
                messages
            );
        }
    }

    #[test]
    fn test_compute_diagnostics_applies_category_severity_controls() {
        let uri = "file:///severity-controls.php";
        let code = r#"<?php
namespace App;

class Service {}

function run(Service $service): void
{
    $unused = 1;
    $service->missing();
    new MissingClass();
}
"#;

        let mut parser = FileParser::new();
        parser.parse_full(code);

        let index = WorkspaceIndex::new();
        let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
        index.update_file(uri, symbols);

        let mut severity = DiagnosticSeverityConfig::default();
        severity.set(DiagnosticCategory::Members, DiagnosticLevel(None));
        severity.set(
            DiagnosticCategory::UnknownSymbols,
            DiagnosticLevel(Some(DiagnosticSeverity::INFORMATION)),
        );
        severity.set(
            DiagnosticCategory::Unused,
            DiagnosticLevel(Some(DiagnosticSeverity::HINT)),
        );

        let diagnostics = compute_diagnostics_with_config(
            uri,
            &parser,
            &index,
            DiagnosticsMode::BasicSemantic,
            severity,
            PhpVersion::DEFAULT,
        );

        assert!(
            !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == "Unknown method: App\\Service::missing"),
            "Member category is off, got diagnostics: {:?}",
            diagnostics
        );

        let unknown_class = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.message == "Unknown class: App\\MissingClass")
            .expect("Expected unknown class diagnostic");
        assert_eq!(
            unknown_class.severity,
            Some(DiagnosticSeverity::INFORMATION)
        );
        assert_eq!(
            unknown_class.code,
            Some(NumberOrString::String("php-lsp.unknownClass".to_string()))
        );

        let unused_variable = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.message == "Unused variable: $unused")
            .expect("Expected unused variable diagnostic");
        assert_eq!(unused_variable.severity, Some(DiagnosticSeverity::HINT));
        assert_eq!(
            unused_variable.code,
            Some(NumberOrString::String("php-lsp.unusedVariable".to_string()))
        );
    }

    #[test]
    fn test_compute_diagnostics_allows_magic_class_and_late_bound_self_calls() {
        let uri = "file:///phpunit-patterns.php";
        let code = r#"<?php
namespace App;

class Foo {}

class Base {
    protected function once(): void {}
    protected static function createStub(string $type): object { return new Foo(); }
    public static function callback(callable $callback): bool { return true; }
}

class Demo extends Base {
    public function run(): void {
        echo Foo::class;
        self::once();
        self::callback(static fn (): bool => true);
        $this->createStub(Foo::class);
    }
}
"#;

        let mut parser = FileParser::new();
        parser.parse_full(code);

        let index = WorkspaceIndex::new();
        let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
        index.update_file(uri, symbols);

        let diagnostics = compute_diagnostics(
            uri,
            &parser,
            &index,
            DiagnosticsMode::BasicSemantic,
            PhpVersion::DEFAULT,
        );
        let messages: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect();

        for unexpected in [
            "Unknown class constant: App\\Foo::class",
            "Instance method called statically: App\\Base::once",
            "Static method called as instance method: App\\Base::createStub",
        ] {
            assert!(
                !messages.contains(&unexpected),
                "Did not expect `{}` in diagnostics, got: {:?}",
                unexpected,
                messages
            );
        }
    }

    #[test]
    fn test_compute_diagnostics_allows_phpunit_stub_api_on_typed_properties() {
        let uri = "file:///phpunit-stub-api.php";
        let code = r#"<?php
namespace PHPUnit\Framework;
class TestCase {}

namespace Symfony\Component\Console\Tester;
class CommandTester {}

namespace App\Tests\Command;

use PHPUnit\Framework\TestCase;
use Symfony\Component\Console\Tester\CommandTester;

class UserRepository {}

final class ChangeUserPasswordCommandTest extends TestCase
{
    private UserRepository $userRepo;
    private CommandTester $commandTester;

    protected function setUp(): void
    {
        $this->userRepo = $this->createStub(UserRepository::class);
        $this->commandTester = new CommandTester();
    }

    public function testUserNotFoundByEmail(): void
    {
        $this->userRepo->method('findOneBy')->willReturn(null);
        self::assertSame(1, 1);
    }
}
"#;

        let mut parser = FileParser::new();
        parser.parse_full(code);

        let index = WorkspaceIndex::new();
        let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
        index.update_file(uri, symbols);

        let diagnostics = compute_diagnostics(
            uri,
            &parser,
            &index,
            DiagnosticsMode::BasicSemantic,
            PhpVersion::DEFAULT,
        );
        let messages: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect();

        for unexpected in [
            "Unknown method: App\\Tests\\Command\\ChangeUserPasswordCommandTest::createStub",
            "Unknown method: App\\Tests\\Command\\UserRepository::method",
            "Unknown method: App\\Tests\\Command\\ChangeUserPasswordCommandTest::assertSame",
            "Property assignment type mismatch for App\\Tests\\Command\\ChangeUserPasswordCommandTest::$commandTester",
        ] {
            assert!(
                !messages.iter().any(|message| message.contains(unexpected)),
                "Did not expect `{}` in diagnostics, got: {:?}",
                unexpected,
                messages
            );
        }
    }

    #[test]
    fn test_compute_diagnostics_allows_trait_member_visibility_and_stdclass_properties() {
        let uri = "file:///trait-members.php";
        let code = r#"<?php
namespace App\Tests;

enum TimerType: string {
    case Test = 'test';
}

trait HelperTestTrait {
    protected int $count;
    protected function protectedHelper(): void {}
    private function privateHelper(): void {}
}

final class HelperConsumerTest {
    use HelperTestTrait;

    public function run(\stdClass $payload, object $response, TimerType $type): void {
        $this->count = 1;
        $this->protectedHelper();
        $this->privateHelper();
        echo $payload->PortMessages;
        echo $response->getContent();
        echo $response->headers;
        echo $type->name;
        echo $type->value;
    }
}
"#;

        let mut parser = FileParser::new();
        parser.parse_full(code);

        let index = WorkspaceIndex::new();
        let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
        index.update_file(uri, symbols);

        let diagnostics = compute_diagnostics(
            uri,
            &parser,
            &index,
            DiagnosticsMode::BasicSemantic,
            PhpVersion::DEFAULT,
        );
        let messages: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect();

        for unexpected in [
            "Protected member is not accessible here: App\\Tests\\HelperTestTrait::$count",
            "Protected member is not accessible here: App\\Tests\\HelperTestTrait::protectedHelper",
            "Private member is not accessible here: App\\Tests\\HelperTestTrait::privateHelper",
            "Unknown property: stdClass::$PortMessages",
            "Unknown method: object::getContent",
            "Unknown property: object::$headers",
            "Unknown property: App\\Tests\\TimerType::$name",
            "Unknown property: App\\Tests\\TimerType::$value",
        ] {
            assert!(
                !messages.iter().any(|message| {
                    message.contains(unexpected)
                        || (unexpected.contains("object::getContent")
                            && message.ends_with("object::getContent"))
                        || (unexpected.contains("object::$headers")
                            && message.ends_with("object::$headers"))
                }),
                "Did not expect `{}` in diagnostics, got: {:?}",
                unexpected,
                messages
            );
        }
    }

    #[test]
    fn test_compute_diagnostics_skips_anonymous_class_body_member_checks() {
        let uri = "file:///anonymous-class.php";
        let code = r#"<?php
namespace App\Tests;

final class Factory
{
    public function make(): object
    {
        return new class('demo') {
            private string $name;

            public function __construct(string $name)
            {
                $this->name = $name;
            }

            public function getName(): string
            {
                return $this->name;
            }

            public function getDate(): ?\DateTime
            {
                return null;
            }
        };
    }
}
"#;

        let mut parser = FileParser::new();
        parser.parse_full(code);

        let index = WorkspaceIndex::new();
        let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
        index.update_file(uri, symbols);

        let diagnostics = compute_diagnostics(
            uri,
            &parser,
            &index,
            DiagnosticsMode::BasicSemantic,
            PhpVersion::DEFAULT,
        );
        let messages: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect();

        for unexpected in [
            "Unknown property: App\\Tests\\Factory::$name",
            "Return type mismatch in App\\Tests\\Factory::make: expected object, got null",
        ] {
            assert!(
                !messages.iter().any(|message| message.contains(unexpected)),
                "Did not expect `{}` in diagnostics, got: {:?}",
                unexpected,
                messages
            );
        }
    }

    #[test]
    fn test_compute_diagnostics_skips_member_type_checks_above_node_budget() {
        let uri = "file:///large-member-heavy.php";
        let mut code = String::from(
            r#"<?php
namespace App;

class Service {}

function configure(Service $service): void
{
"#,
        );
        for index in 0..=MEMBER_TYPE_DIAGNOSTIC_NODE_LIMIT {
            code.push_str(&format!("    $service->missing{}();\n", index));
        }
        code.push_str("}\n");

        let mut parser = FileParser::new();
        parser.parse_full(&code);

        let index = WorkspaceIndex::new();
        let symbols = extract_file_symbols(parser.tree().unwrap(), &code, uri);
        index.update_file(uri, symbols);

        let diagnostics = compute_diagnostics(
            uri,
            &parser,
            &index,
            DiagnosticsMode::BasicSemantic,
            PhpVersion::DEFAULT,
        );
        let messages: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect();

        assert!(
            !messages
                .iter()
                .any(|message| message.contains("Unknown method: App\\Service::missing")),
            "Member diagnostics should be skipped above budget, got: {:?}",
            messages
        );
    }

    #[test]
    fn test_compute_diagnostics_allows_phpunit_helpers_in_framework_tests_and_test_traits() {
        let deps_uri = "file:///phpunit-deps.php";
        let deps_code = r#"<?php
namespace PHPUnit\Framework;
class TestCase {}

namespace Symfony\Bundle\FrameworkBundle\Test;
class WebTestCase extends \PHPUnit\Framework\TestCase {}
"#;

        let test_uri = "file:///framework-test.php";
        let test_code = r#"<?php
namespace App\Tests\Controller;

use Symfony\Bundle\FrameworkBundle\Test\WebTestCase;

final class FlowTest extends WebTestCase
{
    protected function setUp(): void
    {
        parent::setUp();
    }

    protected function tearDown(): void
    {
        parent::tearDown();
    }

    public function run(): void
    {
        self::assertSame(1, 1);
        $this->anything();
        $this->stringContains('needle');
    }
}
"#;

        let trait_uri = "file:///outbound-test-trait.php";
        let trait_code = r#"<?php
namespace App\Tests\Soap\Outbound;

trait OutboundTestTrait
{
    protected function helper(): void
    {
        $this->createStub(\stdClass::class);
    }
}
"#;

        let mut deps_parser = FileParser::new();
        deps_parser.parse_full(deps_code);
        let mut test_parser = FileParser::new();
        test_parser.parse_full(test_code);
        let mut trait_parser = FileParser::new();
        trait_parser.parse_full(trait_code);

        let index = WorkspaceIndex::new();
        index.update_file(
            deps_uri,
            extract_file_symbols(deps_parser.tree().unwrap(), deps_code, deps_uri),
        );
        index.update_file(
            test_uri,
            extract_file_symbols(test_parser.tree().unwrap(), test_code, test_uri),
        );
        index.update_file(
            trait_uri,
            extract_file_symbols(trait_parser.tree().unwrap(), trait_code, trait_uri),
        );

        let test_diagnostics = compute_diagnostics(
            test_uri,
            &test_parser,
            &index,
            DiagnosticsMode::BasicSemantic,
            PhpVersion::DEFAULT,
        );
        let trait_diagnostics = compute_diagnostics(
            trait_uri,
            &trait_parser,
            &index,
            DiagnosticsMode::BasicSemantic,
            PhpVersion::DEFAULT,
        );
        let messages: Vec<_> = test_diagnostics
            .iter()
            .chain(trait_diagnostics.iter())
            .map(|diagnostic| diagnostic.message.as_str())
            .collect();

        for unexpected in [
            "Unknown method: App\\Tests\\Controller\\FlowTest::assertSame",
            "Unknown method: App\\Tests\\Controller\\FlowTest::anything",
            "Unknown method: App\\Tests\\Controller\\FlowTest::stringContains",
            "Unknown method: parent::setUp",
            "Unknown method: parent::tearDown",
            "Unknown method: App\\Tests\\Soap\\Outbound\\OutboundTestTrait::createStub",
        ] {
            assert!(
                !messages.iter().any(|message| message.contains(unexpected)),
                "Did not expect `{}` in diagnostics, got: {:?}",
                unexpected,
                messages
            );
        }
    }

    #[test]
    fn test_compute_diagnostics_reports_basic_type_mismatches() {
        let uri = "file:///types.php";
        let code = r#"<?php
namespace App;

function takesInt(int $value): void {}

function returnsInt(): int {
    return "bad";
}

class Box {
    public int $count;

    public function set(string $name): void {}
}

function run(Box $box): void {
    takesInt("bad");
    $box->set(123);
    $box->count = "bad";
}
"#;

        let mut parser = FileParser::new();
        parser.parse_full(code);

        let index = WorkspaceIndex::new();
        let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
        index.update_file(uri, symbols);

        let diagnostics = compute_diagnostics(
            uri,
            &parser,
            &index,
            DiagnosticsMode::BasicSemantic,
            PhpVersion::DEFAULT,
        );
        let messages: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect();

        for expected in [
            "Type mismatch for App\\takesInt argument $value: expected int, got string",
            "Return type mismatch in App\\returnsInt: expected int, got string",
            "Type mismatch for App\\Box::set argument $name: expected string, got int",
            "Property assignment type mismatch for App\\Box::$count: expected int, got string",
        ] {
            assert!(
                messages.contains(&expected),
                "Expected `{}` in diagnostics, got: {:?}",
                expected,
                messages
            );
        }
    }

    #[test]
    fn test_compute_diagnostics_skips_uncertain_ternary_return_type() {
        let uri = "file:///ternary-return.php";
        let code = r#"<?php
namespace App;

class RemoteFileService {}

class Controller {
    private RemoteFileService $primaryFileService;
    private RemoteFileService $secondaryFileService;

    private function getService(string $name): RemoteFileService {
        return 'primary' === $name ? $this->primaryFileService : $this->secondaryFileService;
    }
}
"#;

        let mut parser = FileParser::new();
        parser.parse_full(code);

        let index = WorkspaceIndex::new();
        let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
        index.update_file(uri, symbols);

        let diagnostics = compute_diagnostics(
            uri,
            &parser,
            &index,
            DiagnosticsMode::BasicSemantic,
            PhpVersion::DEFAULT,
        );
        let messages: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect();

        assert!(
            !messages.iter().any(|message| {
                message.contains("Return type mismatch in App\\Controller::getService")
            }),
            "Uncertain ternary return should not be inferred from its condition, got: {:?}",
            messages
        );
    }

    #[test]
    fn test_compute_diagnostics_reports_override_and_php_version_errors() {
        let uri = "file:///override.php";
        let code = r#"<?php
namespace App;

class Base {
    public function value(int $id): string {
        return "";
    }
}

class Child extends Base {
    public function value(string $id): int {
        return 1;
    }
}

function nullableUnion(): string|null {
    return null;
}
"#;

        let mut parser = FileParser::new();
        parser.parse_full(code);

        let index = WorkspaceIndex::new();
        let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
        index.update_file(uri, symbols);

        let diagnostics = compute_diagnostics(
            uri,
            &parser,
            &index,
            DiagnosticsMode::BasicSemantic,
            PhpVersion { major: 7, minor: 4 },
        );
        let messages: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect();

        for expected in [
            "Incompatible override signature: App\\Child::value differs from App\\Base::value",
            "Type is not supported by PHP 7.4: string|null",
        ] {
            assert!(
                messages.contains(&expected),
                "Expected `{}` in diagnostics, got: {:?}",
                expected,
                messages
            );
        }
    }

    #[test]
    fn test_compute_diagnostics_allows_named_arguments() {
        let uri = "file:///named-args.php";
        let code = r#"<?php
namespace Symfony\Component\Validator\Constraints;

class NotBlank {
    public function __construct(?array $options = null, ?string $message = null) {}
}

class Length {
    public function __construct(?array $options = null, ?int $min = null, ?int $max = null, ?string $minMessage = null, ?string $maxMessage = null) {}
}

namespace App;

use Symfony\Component\Validator\Constraints\Length;
use Symfony\Component\Validator\Constraints\NotBlank;

function run(): void {
    new NotBlank(message: 'Required');
    new Length(max: 255, maxMessage: 'Too long');
}
"#;

        let mut parser = FileParser::new();
        parser.parse_full(code);

        let index = WorkspaceIndex::new();
        let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
        index.update_file(uri, symbols);

        let diagnostics = compute_diagnostics(
            uri,
            &parser,
            &index,
            DiagnosticsMode::BasicSemantic,
            PhpVersion::DEFAULT,
        );
        let messages: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect();

        assert!(
            !messages
                .iter()
                .any(|message| message.contains("Type mismatch")),
            "Named arguments should be matched by parameter name, got: {:?}",
            messages
        );
    }

    #[test]
    fn test_compute_diagnostics_allows_enum_builtin_methods_and_parent_constructor() {
        let uri = "file:///enum-parent.php";
        let code = r#"<?php
namespace App;

enum TimerType: string {
    case Tccp = 'tccp';
}

class BaseCommand {
    public function __construct(?string $name = null) {}
}

class SendCommand extends BaseCommand {
    public function __construct(private TimerType $timerType) {
        parent::__construct();
    }

    public function run(): void {
        TimerType::cases();
        TimerType::tryFrom('tccp');
    }
}
"#;

        let mut parser = FileParser::new();
        parser.parse_full(code);

        let index = WorkspaceIndex::new();
        let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
        index.update_file(uri, symbols);

        let diagnostics = compute_diagnostics(
            uri,
            &parser,
            &index,
            DiagnosticsMode::BasicSemantic,
            PhpVersion::DEFAULT,
        );
        let messages: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect();

        for unexpected in [
            "Unknown method: App\\TimerType::cases",
            "Unknown method: App\\TimerType::tryFrom",
            "Unknown method: parent::__construct",
            "Incompatible override signature: App\\SendCommand::__construct differs from App\\BaseCommand::__construct",
        ] {
            assert!(
                !messages.contains(&unexpected),
                "Did not expect `{}` in diagnostics, got: {:?}",
                unexpected,
                messages
            );
        }
    }

    #[test]
    fn test_compute_diagnostics_allows_alias_and_mixed_override_signatures() {
        let scheduler_uri = "file:///scheduler-overrides.php";
        let scheduler_code = r#"<?php
namespace Symfony\Component\Scheduler;

class Schedule {}

interface ScheduleProviderInterface {
    public function getSchedule(): Schedule;
}
"#;

        let voter_uri = "file:///voter-overrides.php";
        let voter_code = r#"<?php
namespace Symfony\Component\Security\Core\Authorization\Voter;

abstract class Voter {
    protected function supports(string $attribute, mixed $subject): bool {
        echo $attribute;
        echo $subject;
        return true;
    }
}
"#;

        let app_uri = "file:///app-overrides.php";
        let app_code = r#"<?php
namespace App;

use Symfony\Component\Scheduler\Schedule as SymfonySchedule;
use Symfony\Component\Scheduler\ScheduleProviderInterface;
use Symfony\Component\Security\Core\Authorization\Voter\Voter;

class Schedule implements ScheduleProviderInterface {
    public function getSchedule(): SymfonySchedule {
        return new SymfonySchedule();
    }
}

class UserVoter extends Voter {
    protected function supports(string $attribute, $subject): bool {
        echo $attribute;
        echo $subject;
        return true;
    }
}
"#;

        let mut scheduler_parser = FileParser::new();
        scheduler_parser.parse_full(scheduler_code);
        let mut voter_parser = FileParser::new();
        voter_parser.parse_full(voter_code);
        let mut app_parser = FileParser::new();
        app_parser.parse_full(app_code);

        let index = WorkspaceIndex::new();
        index.update_file(
            scheduler_uri,
            extract_file_symbols(
                scheduler_parser.tree().unwrap(),
                scheduler_code,
                scheduler_uri,
            ),
        );
        index.update_file(
            voter_uri,
            extract_file_symbols(voter_parser.tree().unwrap(), voter_code, voter_uri),
        );
        index.update_file(
            app_uri,
            extract_file_symbols(app_parser.tree().unwrap(), app_code, app_uri),
        );

        let diagnostics = compute_diagnostics(
            app_uri,
            &app_parser,
            &index,
            DiagnosticsMode::BasicSemantic,
            PhpVersion::DEFAULT,
        );
        let messages: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect();

        for unexpected in [
            "Incompatible override signature: App\\Schedule::getSchedule differs from Symfony\\Component\\Scheduler\\ScheduleProviderInterface::getSchedule",
            "Incompatible override signature: App\\UserVoter::supports differs from Symfony\\Component\\Security\\Core\\Authorization\\Voter\\Voter::supports",
        ] {
            assert!(
                !messages.contains(&unexpected),
                "Did not expect `{}` in diagnostics, got: {:?}",
                unexpected,
                messages
            );
        }
    }

    #[test]
    fn test_formatting_provider_none_disables_stale_command() {
        let config =
            FormattingConfig::from_options(Some("none"), Some("vendor/bin/php-cs-fixer"), None);
        assert!(config.command_template().is_none());

        let custom =
            FormattingConfig::from_options(Some("custom"), Some("vendor/bin/fmt {file}"), None);
        assert_eq!(
            custom.command_template().as_deref(),
            Some("vendor/bin/fmt {file}")
        );
    }

    #[test]
    fn test_parse_phpstan_json_diagnostics_maps_messages() {
        let file_path = PathBuf::from("/tmp/php-lsp-phpstan/src/Foo.php");
        let output = serde_json::json!({
            "totals": { "errors": 0, "file_errors": 1 },
            "files": {
                (file_path.to_string_lossy().to_string()): {
                    "errors": 1,
                    "messages": [
                        {
                            "message": "Call to an undefined method App\\Foo::missing().",
                            "line": 7,
                            "identifier": "method.notFound",
                            "tip": "Check the object type."
                        }
                    ]
                }
            },
            "errors": []
        })
        .to_string();

        let diagnostics = parse_phpstan_json_diagnostics(&output, &file_path).unwrap();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].range.start.line, 6);
        assert_eq!(diagnostics[0].source.as_deref(), Some("phpstan"));
        assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            diagnostics[0].code,
            Some(NumberOrString::String("method.notFound".to_string()))
        );
        assert!(
            diagnostics[0]
                .message
                .contains("Call to an undefined method App\\Foo::missing()."),
            "unexpected message: {}",
            diagnostics[0].message
        );
        assert!(
            diagnostics[0].message.contains("Check the object type."),
            "tip should be appended to diagnostic message"
        );
    }

    #[tokio::test]
    async fn test_run_phpstan_for_file_accepts_nonzero_json_output() {
        if cfg!(windows) {
            return;
        }

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let tmp = std::env::temp_dir().join(format!(
            "php-lsp-phpstan-test-{}-{}",
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let file_path = tmp.join("Subject.php");
        std::fs::write(&file_path, "<?php\nclass Subject {}\n").unwrap();

        let output = serde_json::json!({
            "totals": { "errors": 0, "file_errors": 1 },
            "files": {
                (file_path.to_string_lossy().to_string()): {
                    "errors": 1,
                    "messages": [
                        {
                            "message": "PHPStan reported a test error.",
                            "line": 2,
                            "identifier": "test.identifier"
                        }
                    ]
                }
            },
            "errors": []
        })
        .to_string();

        let script_path = tmp.join("phpstan-fake.sh");
        std::fs::write(
            &script_path,
            format!("#!/bin/sh\ncat <<'JSON'\n{}\nJSON\nexit 1\n", output),
        )
        .unwrap();

        let config = PhpStanConfig {
            enabled: true,
            command: format!(
                "sh {} {{file}}",
                shell_escape(&script_path.to_string_lossy())
            ),
            timeout_ms: 5_000,
            memory_limit: None,
        };
        let diagnostics = run_phpstan_for_file(config, file_path, Some(tmp.clone()), None)
            .await
            .unwrap();

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].source.as_deref(), Some("phpstan"));
        assert_eq!(diagnostics[0].message, "PHPStan reported a test error.");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_run_shell_command_with_timeout_respects_cancellation() {
        if cfg!(windows) {
            return;
        }

        let token = OperationCancellationToken::new();
        let cancel_token = token.clone();
        let run = tokio::spawn(async move {
            run_shell_command_with_timeout("Test", "sleep 5", None, 10_000, Some(token)).await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel_token.cancel();

        let error = run.await.unwrap().unwrap_err();
        assert_eq!(error, "Test command cancelled");
    }

    #[tokio::test]
    async fn test_external_analyzers_timeout_without_hanging() {
        if cfg!(windows) {
            return;
        }

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let tmp = std::env::temp_dir().join(format!(
            "php-lsp-analyzer-timeout-test-{}-{}",
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let file_path = tmp.join("Subject.php");
        std::fs::write(&file_path, "<?php\nclass Subject {}\n").unwrap();
        let script_path = tmp.join("slow-analyzer.sh");
        std::fs::write(&script_path, "#!/bin/sh\nsleep 5\n").unwrap();
        let command = format!(
            "sh {} {{file}}",
            shell_escape(&script_path.to_string_lossy())
        );

        let phpstan = tokio::time::timeout(
            Duration::from_secs(1),
            run_phpstan_for_file(
                PhpStanConfig {
                    enabled: true,
                    command: command.clone(),
                    timeout_ms: 50,
                    memory_limit: None,
                },
                file_path.clone(),
                Some(tmp.clone()),
                None,
            ),
        )
        .await
        .expect("PHPStan timeout path should not hang")
        .unwrap_err();
        assert!(
            phpstan.contains("PHPStan command timed out after 50ms"),
            "unexpected PHPStan timeout error: {}",
            phpstan
        );

        let psalm = tokio::time::timeout(
            Duration::from_secs(1),
            run_psalm_for_file(
                PsalmConfig {
                    enabled: true,
                    command,
                    timeout_ms: 50,
                },
                file_path,
                Some(tmp.clone()),
                None,
            ),
        )
        .await
        .expect("Psalm timeout path should not hang")
        .unwrap_err();
        assert!(
            psalm.contains("Psalm command timed out after 50ms"),
            "unexpected Psalm timeout error: {}",
            psalm
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_external_analyzers_malformed_json_without_hanging() {
        if cfg!(windows) {
            return;
        }

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let tmp = std::env::temp_dir().join(format!(
            "php-lsp-analyzer-malformed-json-test-{}-{}",
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let file_path = tmp.join("Subject.php");
        std::fs::write(&file_path, "<?php\nclass Subject {}\n").unwrap();
        let script_path = tmp.join("malformed-analyzer.sh");
        std::fs::write(&script_path, "#!/bin/sh\nprintf '{not-json'\nexit 0\n").unwrap();
        let command = format!(
            "sh {} {{file}}",
            shell_escape(&script_path.to_string_lossy())
        );

        let phpstan = tokio::time::timeout(
            Duration::from_secs(1),
            run_phpstan_for_file(
                PhpStanConfig {
                    enabled: true,
                    command: command.clone(),
                    timeout_ms: 5_000,
                    memory_limit: None,
                },
                file_path.clone(),
                Some(tmp.clone()),
                None,
            ),
        )
        .await
        .expect("PHPStan malformed JSON path should not hang")
        .unwrap_err();
        assert!(
            phpstan.contains("invalid PHPStan JSON"),
            "unexpected PHPStan malformed JSON error: {}",
            phpstan
        );

        let psalm = tokio::time::timeout(
            Duration::from_secs(1),
            run_psalm_for_file(
                PsalmConfig {
                    enabled: true,
                    command,
                    timeout_ms: 5_000,
                },
                file_path,
                Some(tmp.clone()),
                None,
            ),
        )
        .await
        .expect("Psalm malformed JSON path should not hang")
        .unwrap_err();
        assert!(
            psalm.contains("invalid Psalm JSON"),
            "unexpected Psalm malformed JSON error: {}",
            psalm
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_parse_psalm_json_diagnostics_maps_issues() {
        let file_path = PathBuf::from("/tmp/php-lsp-psalm/src/Foo.php");
        let output = serde_json::json!([
            {
                "severity": "error",
                "line_from": 4,
                "line_to": 4,
                "type": "UndefinedMethod",
                "message": "Method App\\Foo::missing does not exist",
                "file_name": file_path.to_string_lossy().to_string(),
                "file_path": file_path.to_string_lossy().to_string(),
                "column_from": 12,
                "column_to": 19
            }
        ])
        .to_string();

        let diagnostics = parse_psalm_json_diagnostics(&output, &file_path).unwrap();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].range.start.line, 3);
        assert_eq!(diagnostics[0].range.start.character, 11);
        assert_eq!(diagnostics[0].range.end.character, 18);
        assert_eq!(diagnostics[0].source.as_deref(), Some("psalm"));
        assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            diagnostics[0].code,
            Some(NumberOrString::String("UndefinedMethod".to_string()))
        );
        assert_eq!(
            diagnostics[0].message,
            "Method App\\Foo::missing does not exist"
        );
    }

    #[tokio::test]
    async fn test_run_psalm_for_file_accepts_nonzero_json_output() {
        if cfg!(windows) {
            return;
        }

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let tmp = std::env::temp_dir().join(format!(
            "php-lsp-psalm-test-{}-{}",
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let file_path = tmp.join("Subject.php");
        std::fs::write(&file_path, "<?php\nclass Subject {}\n").unwrap();

        let output = serde_json::json!([
            {
                "severity": "info",
                "line_from": 2,
                "line_to": 2,
                "type": "PossiblyUnusedMethod",
                "message": "Psalm reported a test issue.",
                "file_path": file_path.to_string_lossy().to_string()
            }
        ])
        .to_string();

        let script_path = tmp.join("psalm-fake.sh");
        std::fs::write(
            &script_path,
            format!("#!/bin/sh\ncat <<'JSON'\n{}\nJSON\nexit 1\n", output),
        )
        .unwrap();

        let config = PsalmConfig {
            enabled: true,
            command: format!(
                "sh {} {{file}}",
                shell_escape(&script_path.to_string_lossy())
            ),
            timeout_ms: 5_000,
        };
        let diagnostics = run_psalm_for_file(config, file_path, Some(tmp.clone()), None)
            .await
            .unwrap();

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].source.as_deref(), Some("psalm"));
        assert_eq!(
            diagnostics[0].severity,
            Some(DiagnosticSeverity::INFORMATION)
        );
        assert_eq!(diagnostics[0].message, "Psalm reported a test issue.");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_uri_to_path_and_back() {
        let path = PathBuf::from("/home/user/project/src/Foo.php");
        let uri = path_to_uri(&path);
        assert_eq!(uri, "file:///home/user/project/src/Foo.php");

        let back = uri_to_path(&uri).unwrap();
        assert_eq!(back, path);
    }

    #[test]
    fn test_path_is_excluded_matches_relative_directory() {
        let root = PathBuf::from("/project");
        let exclude_paths = normalize_config_paths(vec!["var/cache".to_string()]);

        assert!(path_is_excluded(
            Path::new("/project/var/cache/Generated.php"),
            &root,
            &exclude_paths
        ));
        assert!(!path_is_excluded(
            Path::new("/project/src/Service.php"),
            &root,
            &exclude_paths
        ));
    }

    #[test]
    fn test_collect_php_files_uses_include_paths_and_excludes() {
        let tmp = std::env::temp_dir().join(format!(
            "php-lsp-include-exclude-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src = tmp.join("src");
        let extra = tmp.join("extra");
        let generated = extra.join("generated");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&generated).unwrap();
        std::fs::write(src.join("App.php"), "<?php class App {}").unwrap();
        std::fs::write(extra.join("Helper.php"), "<?php function helper() {}").unwrap();
        std::fs::write(generated.join("Generated.php"), "<?php class Generated {}").unwrap();

        let include_paths = vec![PathBuf::from("src"), PathBuf::from("extra")];
        let exclude_paths = normalize_config_paths(vec!["extra/generated".to_string()]);
        let mut files = collect_php_files(&include_paths, &tmp, &exclude_paths);
        files.sort();

        assert_eq!(files, vec![extra.join("Helper.php"), src.join("App.php")]);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_resolve_vendor_paths() {
        // Create temp dir with fake vendor/composer/installed.json
        let tmp = std::env::temp_dir().join("php-lsp-test-vendor");
        let vendor_dir = tmp.join("vendor");
        let composer_dir = vendor_dir.join("composer");
        std::fs::create_dir_all(&composer_dir).unwrap();

        let installed_json = serde_json::json!({
            "packages": [
                {
                    "name": "acme/library",
                    "install-path": "../acme/library",
                    "autoload": {
                        "psr-4": {
                            "Acme\\Library\\": "src/"
                        }
                    }
                }
            ]
        });

        std::fs::write(
            composer_dir.join("installed.json"),
            serde_json::to_string(&installed_json).unwrap(),
        )
        .unwrap();

        // Test resolving a FQN
        let paths = resolve_vendor_paths("Acme\\Library\\Http\\Client", &vendor_dir);
        assert!(paths.is_some());
        let paths = paths.unwrap();
        assert_eq!(paths.len(), 1);
        // The path should resolve to vendor/composer/../acme/library/src/Http/Client.php
        let expected_end = "src/Http/Client.php";
        assert!(
            paths[0].to_string_lossy().ends_with(expected_end),
            "Expected path to end with {}, got: {}",
            expected_end,
            paths[0].display()
        );

        // Test FQN that doesn't match any prefix
        let no_match = resolve_vendor_paths("Other\\Namespace\\Foo", &vendor_dir);
        // Should return Some(empty vec) or None — no paths match
        assert!(no_match.is_none() || no_match.unwrap().is_empty());

        // Cleanup
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
