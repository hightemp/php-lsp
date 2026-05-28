//! LSP server implementation and `LanguageServer` wiring.
//!
//! This module connects LSP request handlers to parser, index, completion,
//! framework, template, analyzer, and formatter helpers. Keep feature-specific
//! pure logic in helper functions/modules when possible.
//!
//! Position convention:
//! - incoming LSP positions/ranges are UTF-16;
//! - parser/tree-sitter symbol ranges are byte columns;
//! - convert incoming positions to byte offsets before parser queries;
//! - convert byte-backed ranges before returning them through LSP.

use crate::config::{
    global_config_candidates, load_toml_settings, merge_json_objects, normalize_client_settings,
    PROJECT_CONFIG_FILE_NAME,
};
use crate::template::{
    is_blade_template_language_id, is_blade_template_uri, is_twig_template_language_id,
    is_twig_template_uri, preprocess_blade_template, preprocess_twig_template, TemplateDocument,
    TemplateKind, TemplateVariableType,
};
use crate::util::lsp_text::{lsp_position_to_byte, text_at_lsp_range};
use crate::util::uri::{path_to_uri, uri_to_path};
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
    infer_property_type_from_assignments, infer_variable_hover_info_at_node_with_resolvers,
    infer_variable_type_at_position_with_resolvers,
    infer_variable_type_info_at_position_with_resolvers, iterable_value_type_info,
    local_variable_names_at_position, resolve_class_name_pub, symbol_at_position,
    symbol_at_position_with_resolvers, variable_definition_at_position, CallableParamTypeResolver,
    CallableParameterContext, MemberTypeResolver, RefKind, SymbolAtPosition,
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
use std::cell::RefCell;
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

#[path = "indexing/mod.rs"]
mod indexing;
#[path = "lsp/mod.rs"]
mod lsp;
use indexing::cache::*;
use indexing::stubs::*;
use indexing::vendor::*;
pub(crate) use lsp::code_action::*;

struct PhpLspIndexingStatusNotification;

const DID_CHANGE_DIAGNOSTICS_DEBOUNCE_MS: u64 = 180;
const HEAVY_REQUEST_YIELD_INTERVAL: usize = 32;
const FILE_IO_SLOW_WARNING_MS: u64 = 100;
const FILE_IO_TIMEOUT_MS: u64 = 15_000;
const DIAGNOSTIC_PHASE_SLOW_WARNING_MS: u64 = 500;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RequestTypeCacheKey {
    uri: String,
    document_version: Option<i32>,
    range: (u32, u32, u32, u32),
    context: &'static str,
    expected_context: String,
}

#[derive(Debug)]
struct RequestTypeCache {
    uri: String,
    document_version: Option<i32>,
    string_values: RefCell<HashMap<RequestTypeCacheKey, Option<String>>>,
    type_info_values: RefCell<HashMap<RequestTypeCacheKey, Option<php_lsp_types::TypeInfo>>>,
    inferred_expr_values: RefCell<HashMap<RequestTypeCacheKey, Option<InferredExprType>>>,
    symbol_values: RefCell<HashMap<RequestTypeCacheKey, Option<SymbolAtPosition>>>,
    local_inlay_values: RefCell<HashMap<RequestTypeCacheKey, Option<LocalVariableInlayType>>>,
}

impl RequestTypeCache {
    fn new(uri: impl Into<String>, document_version: Option<i32>) -> Self {
        Self {
            uri: uri.into(),
            document_version,
            string_values: RefCell::new(HashMap::new()),
            type_info_values: RefCell::new(HashMap::new()),
            inferred_expr_values: RefCell::new(HashMap::new()),
            symbol_values: RefCell::new(HashMap::new()),
            local_inlay_values: RefCell::new(HashMap::new()),
        }
    }

    fn key(
        &self,
        range: (u32, u32, u32, u32),
        context: &'static str,
        expected_context: impl Into<String>,
    ) -> RequestTypeCacheKey {
        RequestTypeCacheKey {
            uri: self.uri.clone(),
            document_version: self.document_version,
            range,
            context,
            expected_context: expected_context.into(),
        }
    }

    fn cached_string(
        &self,
        range: (u32, u32, u32, u32),
        context: &'static str,
        expected_context: impl Into<String>,
        compute: impl FnOnce() -> Option<String>,
    ) -> Option<String> {
        let key = self.key(range, context, expected_context);
        if let Some(value) = self.string_values.borrow().get(&key).cloned() {
            return value;
        }

        let value = compute();
        self.string_values.borrow_mut().insert(key, value.clone());
        value
    }

    fn cached_type_info(
        &self,
        range: (u32, u32, u32, u32),
        context: &'static str,
        expected_context: impl Into<String>,
        compute: impl FnOnce() -> Option<php_lsp_types::TypeInfo>,
    ) -> Option<php_lsp_types::TypeInfo> {
        let key = self.key(range, context, expected_context);
        if let Some(value) = self.type_info_values.borrow().get(&key).cloned() {
            return value;
        }

        let value = compute();
        self.type_info_values
            .borrow_mut()
            .insert(key, value.clone());
        value
    }

    fn cached_inferred_expr(
        &self,
        range: (u32, u32, u32, u32),
        context: &'static str,
        expected_context: impl Into<String>,
        compute: impl FnOnce() -> Option<InferredExprType>,
    ) -> Option<InferredExprType> {
        let key = self.key(range, context, expected_context);
        if let Some(value) = self.inferred_expr_values.borrow().get(&key).cloned() {
            return value;
        }

        let value = compute();
        self.inferred_expr_values
            .borrow_mut()
            .insert(key, value.clone());
        value
    }

    fn cached_symbol(
        &self,
        line: u32,
        byte_col: u32,
        context: &'static str,
        expected_context: impl Into<String>,
        compute: impl FnOnce() -> Option<SymbolAtPosition>,
    ) -> Option<SymbolAtPosition> {
        let key = self.key((line, byte_col, line, byte_col), context, expected_context);
        if let Some(value) = self.symbol_values.borrow().get(&key).cloned() {
            return value;
        }

        let value = compute();
        self.symbol_values.borrow_mut().insert(key, value.clone());
        value
    }

    fn cached_local_inlay(
        &self,
        range: (u32, u32, u32, u32),
        context: &'static str,
        expected_context: impl Into<String>,
        compute: impl FnOnce() -> Option<LocalVariableInlayType>,
    ) -> Option<LocalVariableInlayType> {
        let key = self.key(range, context, expected_context);
        if let Some(value) = self.local_inlay_values.borrow().get(&key).cloned() {
            return value;
        }

        let value = compute();
        self.local_inlay_values
            .borrow_mut()
            .insert(key, value.clone());
        value
    }
}

struct CompletionInferenceContext<'a> {
    tree: &'a tree_sitter::Tree,
    source_uri: &'a str,
    source: &'a str,
    file_symbols: &'a php_lsp_types::FileSymbols,
    type_cache: &'a RequestTypeCache,
    line: u32,
    byte_col: u32,
}
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
pub(crate) struct PhpVersion {
    major: u16,
    minor: u16,
}

impl PhpVersion {
    pub(crate) const DEFAULT: Self = Self { major: 8, minor: 2 };

    pub(crate) fn parse(raw: &str) -> Option<Self> {
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
            provider: "auto".to_string(),
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
        let mut provider = provider.unwrap_or("auto").trim().to_ascii_lowercase();
        let command = command
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        if provider.is_empty() {
            provider = if command.is_some() {
                "custom".to_string()
            } else {
                "auto".to_string()
            };
        }
        Self {
            provider,
            command,
            timeout_ms: timeout_ms.unwrap_or(30_000).max(1_000),
        }
    }

    fn resolve_for_workspace(&self, workspace_root: Option<&Path>) -> Self {
        if self.provider != "auto" {
            return self.clone();
        }

        let Some(workspace_root) = workspace_root else {
            return self.clone();
        };
        let Some(tool) = detect_project_formatter_tool(workspace_root) else {
            return self.clone();
        };

        Self {
            provider: tool.provider().to_string(),
            command: Some(tool.command_template().to_string()),
            timeout_ms: self.timeout_ms,
        }
    }

    fn command_template(&self) -> Option<String> {
        match self.provider.as_str() {
            "auto" | "none" => None,
            "custom" => self.command.clone(),
            "pint" => self
                .command
                .clone()
                .or_else(|| Some("vendor/bin/pint --quiet {file}".to_string())),
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
struct AnalyzerCodeActionConfig {
    enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum DiagnosticsMode {
    Off,
    SyntaxOnly,
    #[default]
    BasicSemantic,
}

impl DiagnosticsMode {
    pub(crate) fn parse(raw: &str) -> Option<Self> {
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
pub(crate) struct DiagnosticSeverityConfig {
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
    pub(crate) fn parse(value: &serde_json::Value) -> Option<Self> {
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
pub(crate) struct WorkspaceRootConfig {
    pub(crate) root: PathBuf,
    pub(crate) namespace_map: Option<NamespaceMap>,
}

const VENDOR_FILE_LRU_CAPACITY: usize = 512;
pub(crate) const VENDOR_PRELOAD_ENTRYPOINT_LIMIT: usize = 16;
const MAX_INDEXING_PARSE_CONCURRENCY: usize = 8;

#[derive(Debug, Clone)]
pub(crate) struct VendorPsr4Mapping {
    prefix: String,
    directories: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct VendorAutoloadMap {
    psr4: Vec<VendorPsr4Mapping>,
    pub(crate) files: Vec<PathBuf>,
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

pub(crate) fn normalize_config_paths(paths: Vec<String>) -> Vec<PathBuf> {
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
    /// Open Blade-like template documents backed by virtual PHP parsers.
    template_documents: Arc<DashMap<String, TemplateDocument>>,
    /// Latest LSP document version observed for each open document.
    document_versions: Arc<DashMap<String, i32>>,
    /// Per-document debounce tasks for fast diagnostics after didChange.
    diagnostic_debounce_tasks: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
    /// Per-document external analyzer runs that can be cancelled by newer document events.
    analyzer_runs: Arc<Mutex<HashMap<String, OperationCancellationToken>>>,
    /// Per-document external formatter runs that can be cancelled by newer document events.
    formatter_runs: Arc<Mutex<HashMap<String, OperationCancellationToken>>>,
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
    /// Opt-in code actions for external analyzer diagnostics.
    analyzer_code_actions: Mutex<AnalyzerCodeActionConfig>,
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
            template_documents: Arc::new(DashMap::new()),
            document_versions: Arc::new(DashMap::new()),
            diagnostic_debounce_tasks: Arc::new(Mutex::new(HashMap::new())),
            analyzer_runs: Arc::new(Mutex::new(HashMap::new())),
            formatter_runs: Arc::new(Mutex::new(HashMap::new())),
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
            analyzer_code_actions: Mutex::new(AnalyzerCodeActionConfig::default()),
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

    fn template_document(&self, uri_str: &str) -> Option<TemplateDocument> {
        self.template_documents
            .get(uri_str)
            .map(|document| document.value().clone())
    }

    fn open_template_document(
        &self,
        uri_str: &str,
        text: &str,
        kind: TemplateKind,
        twig_variable_types: &[TemplateVariableType],
    ) -> FileParser {
        let template = match kind {
            TemplateKind::Blade => preprocess_blade_template(text),
            TemplateKind::Twig => preprocess_twig_template(text, twig_variable_types),
        };
        let mut parser = FileParser::new();
        parser.parse_full(template.virtual_source());
        self.template_documents
            .insert(uri_str.to_string(), template);
        parser
    }

    async fn twig_variable_types_for_template(&self, uri_str: &str) -> Vec<TemplateVariableType> {
        let Some(root) = self.workspace_root_for_uri(uri_str).await else {
            return Vec::new();
        };
        let Some(template_name) = twig_template_name_for_uri(uri_str, &root) else {
            return Vec::new();
        };

        let mut variables = HashMap::<String, String>::new();

        for entry in self.open_files.iter() {
            let source_uri = entry.key();
            if source_uri == uri_str || !source_uri.ends_with(".php") {
                continue;
            }
            let source = entry.value().source();
            let file_symbols = self
                .index
                .file_symbols
                .get(source_uri.as_str())
                .map(|symbols| symbols.value().clone())
                .or_else(|| {
                    entry
                        .value()
                        .tree()
                        .map(|tree| extract_file_symbols(tree, &source, source_uri.as_str()))
                })
                .unwrap_or_default();
            collect_twig_render_context_types(
                &template_name,
                &source,
                &file_symbols,
                &mut variables,
            );
        }

        for path in collect_twig_context_php_files(&root, 2048) {
            let source_uri = path_to_uri(&path);
            if self.open_files.contains_key(&source_uri) {
                continue;
            }
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            let mut parser = FileParser::new();
            parser.parse_full(&source);
            let file_symbols = parser
                .tree()
                .map(|tree| extract_file_symbols(tree, &source, &source_uri))
                .unwrap_or_default();
            collect_twig_render_context_types(
                &template_name,
                &source,
                &file_symbols,
                &mut variables,
            );
        }

        let mut result: Vec<_> = variables
            .into_iter()
            .map(|(name, type_text)| TemplateVariableType { name, type_text })
            .collect();
        result.sort_by(|left, right| left.name.cmp(&right.name));
        result
    }

    async fn twig_template_location(&self, uri_str: &str, key: &str) -> Option<Location> {
        let root = self.workspace_root_for_uri(uri_str).await?;
        let path = twig_template_path_for_key(&root, key)?;
        Some(Location {
            uri: path_to_uri(&path).parse::<Uri>().ok()?,
            range: Range {
                start: Position::new(0, 0),
                end: Position::new(0, 0),
            },
        })
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

    async fn start_formatter_run(&self, uri_str: &str) -> OperationCancellationToken {
        let token = OperationCancellationToken::new();
        if let Some(previous) = self
            .formatter_runs
            .lock()
            .await
            .insert(uri_str.to_string(), token.clone())
        {
            previous.cancel();
        }
        token
    }

    async fn finish_formatter_run(&self, uri_str: &str, token: &OperationCancellationToken) {
        let mut runs = self.formatter_runs.lock().await;
        if runs
            .get(uri_str)
            .is_some_and(|current| current.is_same(token))
        {
            runs.remove(uri_str);
        }
    }

    async fn cancel_formatter_run(&self, uri_str: &str) {
        if let Some(token) = self.formatter_runs.lock().await.remove(uri_str) {
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
        let template_documents = self.template_documents.clone();
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

            let template_document = template_documents
                .get(&task_uri_str)
                .map(|template| template.value().clone());
            let effective_diagnostics_mode = if template_document.is_some() {
                DiagnosticsMode::SyntaxOnly
            } else {
                diagnostics_mode
            };
            let mut diagnostics = compute_open_file_diagnostics(
                &task_uri_str,
                &open_files,
                &index,
                effective_diagnostics_mode,
                diagnostic_severity,
                php_version,
                Some(version),
            );
            if let Some(template) = template_document {
                diagnostics = template.map_diagnostics_to_original(diagnostics);
                diagnostics.clear();
            }

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
                let provider = formatting_provider.map(str::to_string).unwrap_or_else(|| {
                    if formatting_command.is_some() {
                        "custom".to_string()
                    } else {
                        current.provider.clone()
                    }
                });
                let command = if formatting_command.is_some() {
                    formatting_command
                } else if formatting_provider.is_some() && provider != current.provider {
                    None
                } else {
                    current.command.as_deref()
                };
                FormattingConfig::from_options(
                    Some(&provider),
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

        if let Some(enabled) = settings_bool(
            settings,
            "analyzerCodeActionsEnabled",
            &["analyzerCodeActions", "enabled"],
        ) {
            let mut analyzer_code_actions = self.analyzer_code_actions.lock().await;
            let next_config = AnalyzerCodeActionConfig { enabled };
            if *analyzer_code_actions != next_config {
                *analyzer_code_actions = next_config;
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
            if message.contains("failed") || message.starts_with("Ignored executable") {
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
        let template_documents = self.template_documents.clone();
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
                    let template_document = template_documents
                        .get(&uri_str)
                        .map(|template| template.value().clone());
                    let effective_diagnostics_mode = if template_document.is_some() {
                        DiagnosticsMode::SyntaxOnly
                    } else {
                        diagnostics_mode
                    };
                    let mut diags = compute_diagnostics_with_config_for_version(
                        &uri_str,
                        &entry,
                        &reindex_index,
                        effective_diagnostics_mode,
                        diagnostic_severity,
                        php_version,
                        version,
                    );
                    if let Some(template) = template_document {
                        diags = template.map_diagnostics_to_original(diags);
                        diags.clear();
                    }
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

    async fn current_workspace_roots(&self) -> Vec<PathBuf> {
        let mut roots = self.workspace_roots.lock().await.clone();
        if roots.is_empty() {
            if let Some(root) = self.workspace_root.lock().await.clone() {
                roots.push(root);
            }
        }
        if roots.is_empty() {
            roots.extend(
                self.workspace_configs
                    .lock()
                    .await
                    .iter()
                    .map(|config| config.root.clone()),
            );
        }
        roots
    }

    async fn invalidate_composer_metadata(&self, path: &Path, reindex_workspace: bool) {
        self.vendor_autoload_cache.lock().await.clear();
        let evicted = self.vendor_file_lru.lock().await.clear();
        for uri in evicted {
            self.index.remove_file(&uri);
        }

        let roots = self.current_workspace_roots().await;
        let removed_vendor_files = remove_indexed_vendor_symbols(&self.index, &roots);
        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "php-lsp: Composer metadata changed at {}; cleared vendor metadata cache and {} indexed vendor file(s)",
                    path.display(),
                    removed_vendor_files
                ),
            )
            .await;

        if reindex_workspace {
            self.reindex_workspaces().await;
        } else {
            self.republish_open_diagnostics().await;
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
        source_uri: Option<&str>,
        source: Option<&str>,
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
            .or_else(|| {
                framework_virtual_member_type_fqn(
                    &self.index,
                    class_fqn,
                    member_name,
                    source_uri,
                    Some(file_symbols),
                    source,
                )
            })
    }

    fn resolve_completion_member_type_cached(
        &self,
        class_fqn: &str,
        member_name: &str,
        file_symbols: &php_lsp_types::FileSymbols,
        source_uri: Option<&str>,
        source: Option<&str>,
        type_cache: &RequestTypeCache,
    ) -> Option<String> {
        type_cache.cached_string(
            (0, 0, 0, 0),
            "completion-member-type",
            format!("{class_fqn}::{member_name}"),
            || {
                self.resolve_completion_member_type(
                    class_fqn,
                    member_name,
                    file_symbols,
                    source_uri,
                    source,
                )
            },
        )
    }

    fn resolve_completion_member_call_type(
        &self,
        class_fqn: &str,
        member_name: &str,
        member_text: &str,
        file_symbols: &php_lsp_types::FileSymbols,
        type_cache: &RequestTypeCache,
    ) -> Option<String> {
        type_cache.cached_string(
            (0, 0, 0, 0),
            "completion-member-call-type",
            format!("{class_fqn}::{member_name}:{member_text}"),
            || {
                let symbol = self
                    .index
                    .resolve_member(&format!("{}::{}", class_fqn, member_name))
                    .or_else(|| {
                        let member_fqn = format!("{}::{}", class_fqn, member_name);
                        file_symbols.symbols.iter().find_map(|sym| {
                            (sym.fqn == member_fqn
                                || (sym.parent_fqn.as_deref() == Some(class_fqn)
                                    && sym.name == member_name))
                                .then(|| Arc::new(sym.clone()))
                        })
                    })?;
                let signature = symbol.signature.as_ref()?;
                let return_type = signature.return_type.as_ref()?;
                let arguments = completion_call_arguments_by_param(
                    member_text,
                    signature,
                    file_symbols,
                    &self.index,
                );
                let template_names: HashSet<String> = symbol
                    .templates
                    .iter()
                    .map(|template| template.name.clone())
                    .collect();
                let substitutions =
                    call_site_template_substitutions(&arguments, signature, &template_names);
                let resolved = resolve_call_site_type_info(
                    return_type,
                    &arguments,
                    &template_names,
                    &substitutions,
                );
                type_info_fqn_from_index(&self.index, class_fqn, &symbol.uri, &resolved)
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn infer_completion_object_type(
        &self,
        object_expr: &str,
        tree: &tree_sitter::Tree,
        source_uri: &str,
        source: &str,
        file_symbols: &php_lsp_types::FileSymbols,
        line: u32,
        byte_col: u32,
        type_cache: &RequestTypeCache,
    ) -> Option<String> {
        type_cache.cached_string(
            (line, byte_col, line, byte_col),
            "completion-object-type",
            object_expr,
            || {
                let object_expr = object_expr.trim();
                if let Some(class_fqn) = infer_new_expression_type(object_expr, file_symbols) {
                    return Some(class_fqn);
                }
                if let Some(class_fqn) = infer_static_call_expression_type(
                    object_expr,
                    file_symbols,
                    |class_fqn, method_name| {
                        self.resolve_completion_member_type_cached(
                            class_fqn,
                            method_name,
                            file_symbols,
                            Some(source_uri),
                            Some(source),
                            type_cache,
                        )
                    },
                ) {
                    return Some(class_fqn);
                }

                if object_expr.contains("->") || object_expr.contains("?->") {
                    return self.infer_completion_member_chain_type(
                        object_expr,
                        tree,
                        source_uri,
                        source,
                        file_symbols,
                        line,
                        byte_col,
                        type_cache,
                    );
                }

                if object_expr == "$this" {
                    current_class_fqn_at_range(file_symbols, (line, byte_col, line, byte_col))
                        .or_else(|| current_class_fqn(file_symbols))
                } else if object_expr.starts_with('$') {
                    self.infer_completion_variable_type(
                        tree,
                        source_uri,
                        source,
                        file_symbols,
                        line,
                        byte_col,
                        object_expr,
                        type_cache,
                    )
                } else {
                    None
                }
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn infer_completion_variable_type(
        &self,
        tree: &tree_sitter::Tree,
        source_uri: &str,
        source: &str,
        file_symbols: &php_lsp_types::FileSymbols,
        line: u32,
        byte_col: u32,
        var_name: &str,
        type_cache: &RequestTypeCache,
    ) -> Option<String> {
        type_cache.cached_string(
            (line, byte_col, line, byte_col),
            "completion-variable-type",
            var_name,
            || {
                let resolve_member_type = |class_fqn: &str, member_name: &str| {
                    self.resolve_completion_member_type_cached(
                        class_fqn,
                        member_name,
                        file_symbols,
                        Some(source_uri),
                        Some(source),
                        type_cache,
                    )
                };
                let callable_param_resolver = |ctx: CallableParameterContext<'_>| {
                    resolve_callable_parameter_type_from_index(&self.index, file_symbols, ctx)
                };
                infer_variable_type_at_position_with_resolvers(
                    tree,
                    source,
                    file_symbols,
                    line,
                    byte_col,
                    var_name,
                    Some(&resolve_member_type),
                    Some(&callable_param_resolver),
                )
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn infer_completion_member_chain_type(
        &self,
        object_expr: &str,
        tree: &tree_sitter::Tree,
        source_uri: &str,
        source: &str,
        file_symbols: &php_lsp_types::FileSymbols,
        line: u32,
        byte_col: u32,
        type_cache: &RequestTypeCache,
    ) -> Option<String> {
        type_cache.cached_string(
            (line, byte_col, line, byte_col),
            "completion-member-chain-type",
            object_expr,
            || {
                let normalized = object_expr.replace("?->", "->");
                let mut parts = normalized.split("->");
                let base_expr = parts.next()?.trim();
                let mut class_fqn = if base_expr == "$this" {
                    current_class_fqn_at_range(file_symbols, (line, byte_col, line, byte_col))
                        .or_else(|| current_class_fqn(file_symbols))?
                } else if base_expr.starts_with('$') {
                    self.infer_completion_variable_type(
                        tree,
                        source_uri,
                        source,
                        file_symbols,
                        line,
                        byte_col,
                        base_expr,
                        type_cache,
                    )?
                } else {
                    infer_new_expression_type(base_expr, file_symbols).or_else(|| {
                        infer_static_call_expression_type(
                            base_expr,
                            file_symbols,
                            |class_fqn, method_name| {
                                self.resolve_completion_member_type_cached(
                                    class_fqn,
                                    method_name,
                                    file_symbols,
                                    Some(source_uri),
                                    Some(source),
                                    type_cache,
                                )
                            },
                        )
                    })?
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
                    class_fqn = if is_method_call {
                        self.resolve_completion_member_call_type(
                            &class_fqn,
                            &lookup_name,
                            member,
                            file_symbols,
                            type_cache,
                        )
                        .or_else(|| {
                            self.resolve_completion_member_type_cached(
                                &class_fqn,
                                &lookup_name,
                                file_symbols,
                                Some(source_uri),
                                Some(source),
                                type_cache,
                            )
                        })?
                    } else {
                        self.resolve_completion_member_type_cached(
                            &class_fqn,
                            &lookup_name,
                            file_symbols,
                            Some(source_uri),
                            Some(source),
                            type_cache,
                        )?
                    };
                }

                Some(class_fqn)
            },
        )
    }

    fn infer_completion_type_info(
        &self,
        ctx: &CompletionInferenceContext<'_>,
        expr: &str,
    ) -> Option<php_lsp_types::TypeInfo> {
        ctx.type_cache.cached_type_info(
            (ctx.line, ctx.byte_col, ctx.line, ctx.byte_col),
            "completion-type-info",
            expr,
            || {
                let resolve_member_type = |class_fqn: &str, member_name: &str| {
                    self.resolve_completion_member_type_cached(
                        class_fqn,
                        member_name,
                        ctx.file_symbols,
                        Some(ctx.source_uri),
                        Some(ctx.source),
                        ctx.type_cache,
                    )
                };
                let callable_param_resolver = |callable_ctx: CallableParameterContext<'_>| {
                    resolve_callable_parameter_type_from_index(
                        &self.index,
                        ctx.file_symbols,
                        callable_ctx,
                    )
                };
                infer_variable_type_info_at_position_with_resolvers(
                    ctx.tree,
                    ctx.source,
                    ctx.file_symbols,
                    ctx.line,
                    ctx.byte_col,
                    expr,
                    Some(&resolve_member_type),
                    Some(&callable_param_resolver),
                )
            },
        )
    }

    fn shape_key_completion_items(
        &self,
        ctx: &CompletionInferenceContext<'_>,
        array_expr: &str,
        key_prefix: &str,
    ) -> Vec<lsp_types::CompletionItem> {
        let Some(type_info) = self.infer_completion_type_info(ctx, array_expr) else {
            return Vec::new();
        };

        shape_completion_items_from_type_info(&type_info, ShapeCompletionKind::ArrayKey, key_prefix)
    }

    fn add_object_shape_completion_items(
        &self,
        items: &mut Vec<lsp_types::CompletionItem>,
        ctx: &CompletionInferenceContext<'_>,
        object_expr: &str,
        member_prefix: &str,
    ) {
        let Some(type_info) = self.infer_completion_type_info(ctx, object_expr) else {
            return;
        };

        let mut seen: HashSet<String> = items.iter().map(|item| item.label.clone()).collect();
        for item in shape_completion_items_from_type_info(
            &type_info,
            ShapeCompletionKind::ObjectProperty,
            member_prefix,
        ) {
            if seen.insert(item.label.clone()) {
                items.push(item);
            }
        }
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

    async fn framework_virtual_member_location(
        &self,
        member: &crate::framework::VirtualMember,
    ) -> Option<Location> {
        let (uri, range) = member.sources.iter().find_map(|source| match source {
            crate::framework::VirtualMemberSource::SourceRange { uri, range } => {
                Some((uri.clone(), *range))
            }
            crate::framework::VirtualMemberSource::Synthetic { .. } => None,
        })?;
        let source = self
            .source_for_uri(&uri, "framework virtual member source read")
            .await?;
        let utf16_range = range_byte_to_utf16(&source, range);
        Some(Location {
            uri: uri.parse::<Uri>().ok()?,
            range: Range {
                start: Position::new(utf16_range.0, utf16_range.1),
                end: Position::new(utf16_range.2, utf16_range.3),
            },
        })
    }

    fn framework_string_key_items(
        &self,
        workspace_root: Option<&Path>,
        namespace_map: Option<&NamespaceMap>,
        uri_str: &str,
        file_symbols: &php_lsp_types::FileSymbols,
        source: &str,
        context: &FrameworkStringKeyAtPosition,
    ) -> Vec<lsp_types::CompletionItem> {
        let Some(workspace_root) = workspace_root else {
            return Vec::new();
        };
        let framework_ctx = crate::framework::FrameworkProviderContext::new(&self.index)
            .with_workspace(Some(workspace_root), namespace_map)
            .with_source_uri(Some(uri_str))
            .with_file(Some(file_symbols), Some(source))
            .with_relevant_files(&[]);
        let registry = crate::framework::default_framework_provider_registry();
        let query = crate::framework::FrameworkStringKeyQuery {
            domain: context.domain.to_string(),
            prefix: context.prefix.clone(),
        };

        registry
            .string_keys(&framework_ctx, &query)
            .into_iter()
            .map(|key| framework_string_key_completion_item(&key, &context.prefix))
            .collect()
    }

    async fn framework_string_key_location(
        &self,
        uri_str: &str,
        file_symbols: &php_lsp_types::FileSymbols,
        source: &str,
        context: &FrameworkStringKeyAtPosition,
    ) -> Option<Location> {
        let workspace_root = self.workspace_root_for_uri(uri_str).await?;
        let namespace_map = self.namespace_map.lock().await.clone();
        let framework_ctx = crate::framework::FrameworkProviderContext::new(&self.index)
            .with_workspace(Some(workspace_root.as_path()), namespace_map.as_ref())
            .with_source_uri(Some(uri_str))
            .with_file(Some(file_symbols), Some(source))
            .with_relevant_files(&[]);
        let registry = crate::framework::default_framework_provider_registry();
        let query = crate::framework::FrameworkStringKeyQuery {
            domain: context.domain.to_string(),
            prefix: context.key.clone(),
        };

        registry
            .string_keys(&framework_ctx, &query)
            .into_iter()
            .find(|key| key.key == context.key)
            .and_then(|key| framework_string_key_source_location(&key))
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
        let template_document = self.template_document(&uri_str);
        let version = self.current_document_version(&uri_str);
        let diagnostics_mode = *self.diagnostics_mode.lock().await;
        let should_preresolve_dependencies = template_document.is_none()
            && diagnostics_mode == DiagnosticsMode::BasicSemantic
            && *self.index_vendor.lock().await;

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
        let effective_diagnostics_mode = if template_document.is_some() {
            DiagnosticsMode::SyntaxOnly
        } else {
            diagnostics_mode
        };
        let mut diagnostics = compute_open_file_diagnostics(
            &uri_str,
            &self.open_files,
            &self.index,
            effective_diagnostics_mode,
            diagnostic_severity,
            php_version,
            version,
        );
        if let Some(template) = &template_document {
            diagnostics = template.map_diagnostics_to_original(diagnostics);
            diagnostics.clear();
        } else if should_preresolve_dependencies {
            diagnostics = self
                .filter_lazy_resolved_symbol_diagnostics(diagnostics)
                .await;
        }

        let has_syntax_errors = diagnostics.iter().any(|diagnostic| {
            diagnostic.source.as_deref() == Some("php-lsp")
                && diagnostic.severity == Some(DiagnosticSeverity::ERROR)
        });
        if template_document.is_none()
            && diagnostics_mode == DiagnosticsMode::BasicSemantic
            && !has_syntax_errors
        {
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

    async fn filter_lazy_resolved_symbol_diagnostics(
        &self,
        diagnostics: Vec<Diagnostic>,
    ) -> Vec<Diagnostic> {
        let mut filtered = Vec::with_capacity(diagnostics.len());

        for diagnostic in diagnostics {
            if diagnostic.source.as_deref() == Some("php-lsp") {
                if let Some(fqn) = lazy_resolvable_diagnostic_fqn(&diagnostic.message) {
                    if self.resolve_fqn_lazy(&fqn).await.is_some() {
                        continue;
                    }
                }
            }
            filtered.push(diagnostic);
        }

        filtered
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
        if is_blade_template_uri(&uri_str) {
            self.index.remove_file(&uri_str);
            self.semantic_tokens_cache.lock().await.remove(&uri_str);
            if self.template_documents.contains_key(&uri_str) {
                self.publish_diagnostics(uri).await;
            }
            return;
        }

        if let Some(path) = uri_to_path(&uri_str) {
            let roots = self.current_workspace_roots().await;
            if path_is_under_vendor_roots(&path, &roots)
                && !self.index.file_symbols.contains_key(&uri_str)
            {
                return;
            }
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
        self.template_documents.remove(&uri_str);
        self.document_versions.remove(&uri_str);
        self.cancel_debounced_diagnostics(&uri_str).await;
        self.cancel_analyzer_run(&uri_str).await;
        self.cancel_formatter_run(&uri_str).await;
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
        let moved_template = self
            .template_documents
            .remove(&old_uri_str)
            .map(|(_, template)| template);
        let moved_version = self
            .document_versions
            .remove(&old_uri_str)
            .map(|(_, version)| version);
        self.cancel_debounced_diagnostics(&old_uri_str).await;
        self.cancel_analyzer_run(&old_uri_str).await;
        self.cancel_analyzer_run(new_uri.as_str()).await;
        self.cancel_formatter_run(&old_uri_str).await;
        self.cancel_formatter_run(new_uri.as_str()).await;
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

        if is_blade_template_uri(new_uri.as_str()) {
            let new_uri_str = new_uri.as_str().to_string();
            if let Some(parser) = moved_parser {
                self.open_files.insert(new_uri_str.clone(), parser);
            }
            if let Some(template) = moved_template {
                self.template_documents
                    .insert(new_uri_str.clone(), template);
            }
            if let Some(version) = moved_version {
                self.document_versions.insert(new_uri_str.clone(), version);
            }
            self.index.remove_file(&new_uri_str);
            self.semantic_tokens_cache.lock().await.remove(&new_uri_str);
            self.publish_diagnostics(new_uri).await;
            return;
        }

        if moved_template.is_some() {
            self.reindex_php_file(new_uri).await;
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

fn map_goto_definition_response_for_template(
    current_uri: &str,
    template: &TemplateDocument,
    response: GotoDefinitionResponse,
) -> GotoDefinitionResponse {
    match response {
        GotoDefinitionResponse::Scalar(location) => GotoDefinitionResponse::Scalar(
            map_location_for_template(current_uri, template, location),
        ),
        GotoDefinitionResponse::Array(locations) => GotoDefinitionResponse::Array(
            locations
                .into_iter()
                .map(|location| map_location_for_template(current_uri, template, location))
                .collect(),
        ),
        GotoDefinitionResponse::Link(links) => GotoDefinitionResponse::Link(
            links
                .into_iter()
                .map(|mut link| {
                    if link.target_uri.as_str() == current_uri {
                        if let Some(range) =
                            template.map_virtual_range_to_original(link.target_range)
                        {
                            link.target_range = range;
                        }
                        if let Some(range) =
                            template.map_virtual_range_to_original(link.target_selection_range)
                        {
                            link.target_selection_range = range;
                        }
                    }
                    link
                })
                .collect(),
        ),
    }
}

fn map_location_for_template(
    current_uri: &str,
    template: &TemplateDocument,
    mut location: Location,
) -> Location {
    if location.uri.as_str() == current_uri {
        if let Some(range) = template.map_virtual_range_to_original(location.range) {
            location.range = range;
        }
    }
    location
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetectedFormatterTool {
    Pint,
    PhpCsFixer,
    PhpCbf,
}

impl DetectedFormatterTool {
    fn provider(self) -> &'static str {
        match self {
            Self::Pint => "pint",
            Self::PhpCsFixer => "php-cs-fixer",
            Self::PhpCbf => "phpcbf",
        }
    }

    fn command_template(self) -> &'static str {
        match self {
            Self::Pint => "vendor/bin/pint --quiet {file}",
            Self::PhpCsFixer => "vendor/bin/php-cs-fixer fix --using-cache=no --quiet {file}",
            Self::PhpCbf => "vendor/bin/phpcbf {file}",
        }
    }
}

fn detect_project_formatter_tool(workspace_root: &Path) -> Option<DetectedFormatterTool> {
    let composer_json = find_composer_json(workspace_root)?;
    let content = std::fs::read_to_string(composer_json).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;

    if composer_declares_package(&value, "laravel/pint") {
        return Some(DetectedFormatterTool::Pint);
    }
    if composer_declares_package(&value, "friendsofphp/php-cs-fixer") {
        return Some(DetectedFormatterTool::PhpCsFixer);
    }
    if composer_declares_package(&value, "squizlabs/php_codesniffer") {
        return Some(DetectedFormatterTool::PhpCbf);
    }

    None
}

fn composer_declares_package(value: &serde_json::Value, package: &str) -> bool {
    ["require-dev", "require"].iter().any(|section| {
        value
            .get(section)
            .and_then(|section| section.as_object())
            .is_some_and(|packages| packages.contains_key(package))
    })
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

async fn run_external_formatter(
    source: String,
    config: FormattingConfig,
    workspace_root: Option<PathBuf>,
    cancellation: Option<OperationCancellationToken>,
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
    let output = run_shell_command_with_timeout(
        "Formatter",
        &command,
        workspace_root.as_deref(),
        config.timeout_ms,
        cancellation,
    )
    .await
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

#[allow(clippy::too_many_arguments)]
fn inlay_hints(
    uri_str: &str,
    document_version: Option<i32>,
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
    let type_cache = RequestTypeCache::new(uri_str, document_version);
    let ctx = InlayHintContext {
        tree,
        source,
        file_symbols,
        index,
        type_cache: &type_cache,
        utf16_index: &utf16_index,
        requested_range: byte_range,
    };

    collect_call_argument_inlay_hints(&ctx, tree.root_node(), &mut hints);
    collect_local_variable_type_inlay_hints(&ctx, tree.root_node(), &mut hints);
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
    type_cache: &'a RequestTypeCache,
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
            | "nullsafe_member_call_expression"
            | "scoped_call_expression"
            | "object_creation_expression"
    ) {
        if let Some(callable) = resolve_callable_for_inlay_hint(ctx, node) {
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
    ctx: &InlayHintContext<'_>,
    node: tree_sitter::Node,
) -> Option<Arc<php_lsp_types::SymbolInfo>> {
    let name_node = call_target_name_node(node)?;
    let (_, sym) = resolve_reference_symbol_at_node_cached(
        ctx.tree,
        ctx.source,
        name_node,
        ctx.file_symbols,
        ctx.index,
        ctx.type_cache,
    )?;
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
        "member_call_expression" | "nullsafe_member_call_expression" | "scoped_call_expression" => {
            member_reference_name_node(node)
        }
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

fn collect_local_variable_type_inlay_hints(
    ctx: &InlayHintContext<'_>,
    node: tree_sitter::Node,
    hints: &mut Vec<InlayHint>,
) {
    let mut seen = HashSet::new();
    collect_local_variable_type_inlay_hints_inner(ctx, node, hints, &mut seen);
}

fn collect_local_variable_type_inlay_hints_inner(
    ctx: &InlayHintContext<'_>,
    node: tree_sitter::Node,
    hints: &mut Vec<InlayHint>,
    seen: &mut HashSet<(u32, u32, String)>,
) {
    match node.kind() {
        "expression_statement" => {
            add_assignment_variable_type_inlay_hint(ctx, node, hints, seen);
        }
        "foreach_statement" => {
            add_foreach_variable_type_inlay_hint(ctx, node, hints, seen);
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_local_variable_type_inlay_hints_inner(ctx, child, hints, seen);
    }
}

fn add_assignment_variable_type_inlay_hint(
    ctx: &InlayHintContext<'_>,
    statement: tree_sitter::Node,
    hints: &mut Vec<InlayHint>,
    seen: &mut HashSet<(u32, u32, String)>,
) {
    let Some(expr) = statement.named_child(0) else {
        return;
    };
    if expr.kind() != "assignment_expression" {
        return;
    }
    let Some(left) = expr.child_by_field_name("left") else {
        return;
    };
    let Some(right) = expr.child_by_field_name("right") else {
        return;
    };
    if left.kind() != "variable_name"
        || !is_plain_assignment_expression(left, right, ctx.source)
        || !byte_ranges_overlap(node_range_node(left), ctx.requested_range)
    {
        return;
    }

    add_local_variable_type_inlay_hint(ctx, left, right.end_byte(), Some(right), hints, seen);
}

fn add_foreach_variable_type_inlay_hint(
    ctx: &InlayHintContext<'_>,
    statement: tree_sitter::Node,
    hints: &mut Vec<InlayHint>,
    seen: &mut HashSet<(u32, u32, String)>,
) {
    let Some(value_node) = foreach_value_variable_node_for_inlay(statement, ctx.source) else {
        return;
    };
    if !byte_ranges_overlap(node_range_node(value_node), ctx.requested_range) {
        return;
    }

    add_local_variable_type_inlay_hint(ctx, value_node, value_node.end_byte(), None, hints, seen);
}

#[derive(Debug, Clone)]
struct LocalVariableInlayType {
    display: String,
    target_fqn: Option<String>,
}

#[derive(Debug, Clone)]
struct LocalVariableHoverData {
    variable_name: String,
    type_hint: Option<LocalVariableInlayType>,
    phpdoc_comment: Option<String>,
}

fn add_local_variable_type_inlay_hint(
    ctx: &InlayHintContext<'_>,
    variable_node: tree_sitter::Node,
    usage_start: usize,
    rhs_node: Option<tree_sitter::Node>,
    hints: &mut Vec<InlayHint>,
    seen: &mut HashSet<(u32, u32, String)>,
) {
    let Some(variable_name) = variable_text_for_node(ctx.source, variable_node) else {
        return;
    };
    let Some(type_hint) =
        local_variable_inlay_type(ctx, variable_node, usage_start, &variable_name, rhs_node)
    else {
        return;
    };

    let end = variable_node.end_position();
    let position = Position::new(
        end.row as u32,
        ctx.utf16_index
            .byte_col_to_utf16(end.row as u32, end.column as u32),
    );
    let label_text = format!(": {}", type_hint.display);
    if !seen.insert((position.line, position.character, label_text)) {
        return;
    }

    hints.push(InlayHint {
        position,
        label: local_variable_inlay_label(ctx, &type_hint),
        kind: Some(InlayHintKind::TYPE),
        text_edits: None,
        tooltip: Some(InlayHintTooltip::String(local_variable_inlay_tooltip(
            &type_hint,
        ))),
        padding_left: Some(false),
        padding_right: Some(true),
        data: None,
    });
}

fn local_variable_inlay_type(
    ctx: &InlayHintContext<'_>,
    variable_node: tree_sitter::Node,
    usage_start: usize,
    variable_name: &str,
    rhs_node: Option<tree_sitter::Node>,
) -> Option<LocalVariableInlayType> {
    ctx.type_cache.cached_local_inlay(
        node_range_node(variable_node),
        "local-variable-inlay",
        format!("{variable_name}:{usage_start}"),
        || {
            let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
                ctx.type_cache.cached_string(
                    (0, 0, 0, 0),
                    "member-type",
                    format!("{class_fqn}::{member_name}"),
                    || resolve_member_type_from_index(ctx.index, class_fqn, member_name),
                )
            };
            let callable_param_resolver = |callable_ctx: CallableParameterContext<'_>| {
                resolve_callable_parameter_type_from_index(
                    ctx.index,
                    ctx.file_symbols,
                    callable_ctx,
                )
            };
            let parser_info = infer_variable_hover_info_at_node_with_resolvers(
                variable_node,
                ctx.source,
                ctx.file_symbols,
                usage_start,
                variable_name,
                Some(&resolver),
                Some(&callable_param_resolver),
            );
            let allow_scalar =
                enclosing_foreach_statement_for_variable(ctx.source, variable_node).is_some();

            if let Some(type_hint) = parser_info.as_ref().and_then(|info| {
                info.phpdoc_comment.as_ref().and_then(|_| {
                    local_variable_type_from_hover_info(info, ctx.file_symbols, allow_scalar)
                })
            }) {
                return Some(type_hint);
            }

            if let Some(type_hint) =
                rhs_node.and_then(|rhs| local_variable_inlay_type_from_expression(ctx, rhs))
            {
                return Some(type_hint);
            }

            if let Some(type_hint) = foreach_variable_inlay_type_from_index(ctx, variable_node) {
                return Some(type_hint);
            }

            parser_info.as_ref().and_then(|info| {
                local_variable_type_from_hover_info(info, ctx.file_symbols, allow_scalar)
            })
        },
    )
}

fn local_variable_inlay_type_from_expression(
    ctx: &InlayHintContext<'_>,
    expression: tree_sitter::Node,
) -> Option<LocalVariableInlayType> {
    let expression = normalized_expression_node(expression);
    match expression.kind() {
        "object_creation_expression" => {
            local_variable_inlay_type_from_new_expression(ctx, expression)
        }
        "function_call_expression"
        | "member_call_expression"
        | "nullsafe_member_call_expression"
        | "scoped_call_expression" => {
            local_variable_inlay_type_from_call_expression(ctx, expression)
        }
        "cast_expression" => local_variable_inlay_type_from_cast_expression(ctx, expression),
        "conditional_expression" => {
            local_variable_inlay_type_from_conditional_expression(ctx, expression)
        }
        "variable_name" => local_variable_inlay_type_from_variable_expression(ctx, expression),
        _ => None,
    }
}

fn local_variable_inlay_type_from_conditional_expression(
    ctx: &InlayHintContext<'_>,
    expression: tree_sitter::Node,
) -> Option<LocalVariableInlayType> {
    let type_info = conditional_expression_type_info(ctx.source, expression)?;
    local_variable_inlay_type_from_type_info(ctx, "", "", &type_info, true)
}

fn conditional_expression_type_info(
    source: &str,
    expression: tree_sitter::Node,
) -> Option<php_lsp_types::TypeInfo> {
    let text = node_text(source, expression);
    let question = find_top_level_conditional_question(text)?;
    let colon = find_top_level_needle(text, question + 1, text.len(), ":")?;
    let if_type = scalar_literal_type_info_from_text(&text[question + 1..colon])?;
    let else_type = scalar_literal_type_info_from_text(&text[colon + 1..])?;
    merge_conditional_branch_type_infos(if_type, else_type)
}

fn find_top_level_conditional_question(text: &str) -> Option<usize> {
    split_top_level_text_scan(text, |idx, ch, nested| {
        (ch == '?' && !nested && !text[idx..].starts_with("?->")).then_some(idx)
    })
}

fn scalar_literal_type_info_from_text(text: &str) -> Option<php_lsp_types::TypeInfo> {
    let text = text.trim();
    let lower = text.to_ascii_lowercase();
    if text.starts_with(['\'', '"']) {
        return Some(php_lsp_types::TypeInfo::Simple("string".to_string()));
    }
    if lower == "true" || lower == "false" {
        return Some(php_lsp_types::TypeInfo::Simple("bool".to_string()));
    }
    if lower == "null" {
        return Some(php_lsp_types::TypeInfo::LiteralNull);
    }

    let numeric = lower.trim_start_matches(['+', '-']);
    if numeric.parse::<i64>().is_ok() {
        return Some(php_lsp_types::TypeInfo::Simple("int".to_string()));
    }
    if numeric.parse::<f64>().is_ok() && numeric.contains('.') {
        return Some(php_lsp_types::TypeInfo::Simple("float".to_string()));
    }

    None
}

fn merge_conditional_branch_type_infos(
    left: php_lsp_types::TypeInfo,
    right: php_lsp_types::TypeInfo,
) -> Option<php_lsp_types::TypeInfo> {
    match (left, right) {
        (php_lsp_types::TypeInfo::LiteralNull, php_lsp_types::TypeInfo::LiteralNull) => None,
        (php_lsp_types::TypeInfo::LiteralNull, other)
        | (other, php_lsp_types::TypeInfo::LiteralNull) => {
            Some(php_lsp_types::TypeInfo::Nullable(Box::new(other)))
        }
        (left, right) if left == right => Some(left),
        (left, right) => Some(php_lsp_types::TypeInfo::Union(vec![left, right])),
    }
}

#[derive(Debug, Clone)]
struct IndexedExpressionTypeInfo {
    type_info: php_lsp_types::TypeInfo,
    owner_fqn: String,
    uri: String,
}

fn foreach_variable_inlay_type_from_index(
    ctx: &InlayHintContext<'_>,
    variable_node: tree_sitter::Node,
) -> Option<LocalVariableInlayType> {
    let foreach_stmt = enclosing_foreach_statement_for_variable(ctx.source, variable_node)?;
    let iterable_node = foreach_iterable_node_for_inlay(foreach_stmt)?;
    let iterable_type = indexed_expression_type_info(ctx, iterable_node)?;
    let value_type = iterable_value_type_info(&iterable_type.type_info, None)?;

    local_variable_inlay_type_from_type_info(
        ctx,
        &iterable_type.owner_fqn,
        &iterable_type.uri,
        &value_type,
        true,
    )
}

fn enclosing_foreach_statement_for_variable<'tree>(
    source: &str,
    variable_node: tree_sitter::Node<'tree>,
) -> Option<tree_sitter::Node<'tree>> {
    let variable_name = variable_text_for_node(source, variable_node)?;
    let mut current = variable_node;

    loop {
        if current.kind() == "foreach_statement" {
            let value_node = foreach_value_variable_node_for_inlay(current, source)?;
            if variable_text_for_node(source, value_node).as_deref() == Some(&variable_name)
                && variable_node.start_byte() >= current.start_byte()
                && variable_node.end_byte() <= current.end_byte()
            {
                return Some(current);
            }
        }
        current = current.parent()?;
    }
}

fn foreach_iterable_node_for_inlay(statement: tree_sitter::Node) -> Option<tree_sitter::Node> {
    (statement.kind() == "foreach_statement")
        .then(|| statement.named_child(0))
        .flatten()
}

fn indexed_expression_type_info(
    ctx: &InlayHintContext<'_>,
    expression: tree_sitter::Node,
) -> Option<IndexedExpressionTypeInfo> {
    let expression = normalized_expression_node(expression);
    match expression.kind() {
        "function_call_expression"
        | "member_call_expression"
        | "nullsafe_member_call_expression"
        | "scoped_call_expression" => indexed_call_expression_type_info(ctx, expression),
        _ => None,
    }
}

fn indexed_call_expression_type_info(
    ctx: &InlayHintContext<'_>,
    expression: tree_sitter::Node,
) -> Option<IndexedExpressionTypeInfo> {
    if let Some(type_info) = doctrine_repository_call_type_info(ctx, expression) {
        return Some(type_info);
    }

    let name_node = call_target_name_node(expression)?;
    let Some((sym_at_pos, symbol)) = resolve_reference_symbol_at_node_cached(
        ctx.tree,
        ctx.source,
        name_node,
        ctx.file_symbols,
        ctx.index,
        ctx.type_cache,
    ) else {
        return server_member_call_expression_type_info(ctx, expression);
    };
    if !matches!(
        symbol.kind,
        php_lsp_types::PhpSymbolKind::Function | php_lsp_types::PhpSymbolKind::Method
    ) {
        return None;
    }

    let return_type = symbol_effective_return_type(&symbol)?;
    let owner_fqn = sym_at_pos
        .fqn
        .rsplit_once("::")
        .map(|(owner, _)| owner.to_string())
        .or_else(|| symbol.parent_fqn.clone())
        .unwrap_or_default();
    let type_info = resolve_call_site_return_type(ctx, expression, &symbol, &return_type);
    let type_info =
        doctrine_collection_getter_return_type_info(ctx, &symbol, &owner_fqn, &type_info)
            .unwrap_or(type_info);

    Some(IndexedExpressionTypeInfo {
        type_info,
        owner_fqn,
        uri: symbol.uri.clone(),
    })
}

fn server_member_call_expression_type_info(
    ctx: &InlayHintContext<'_>,
    expression: tree_sitter::Node,
) -> Option<IndexedExpressionTypeInfo> {
    let expression = normalized_expression_node(expression);
    let (receiver_fqn, symbol) = server_member_call_symbol(ctx, expression)?;

    let return_type = symbol_effective_return_type(&symbol)?;
    let owner_fqn = symbol.parent_fqn.as_deref().unwrap_or(&receiver_fqn);
    let type_info = resolve_call_site_return_type(ctx, expression, &symbol, &return_type);
    Some(IndexedExpressionTypeInfo {
        type_info,
        owner_fqn: owner_fqn.to_string(),
        uri: symbol.uri.clone(),
    })
}

fn server_member_call_symbol(
    ctx: &InlayHintContext<'_>,
    expression: tree_sitter::Node,
) -> Option<(String, Arc<php_lsp_types::SymbolInfo>)> {
    let expression = normalized_expression_node(expression);
    if !matches!(
        expression.kind(),
        "member_call_expression" | "nullsafe_member_call_expression"
    ) {
        return None;
    }

    let object = expression.child_by_field_name("object")?;
    let name = expression.child_by_field_name("name")?;
    let method_name = node_text(ctx.source, name).trim();
    let receiver_type = server_expression_type_info(ctx, object)?;
    let receiver_fqn = type_info_fqn_from_index(
        ctx.index,
        &receiver_type.owner_fqn,
        &receiver_type.uri,
        &receiver_type.type_info,
    )?;
    let method_fqn = format!("{receiver_fqn}::{method_name}");
    let symbol = ctx.index.resolve_fqn(&method_fqn)?;
    (symbol.kind == php_lsp_types::PhpSymbolKind::Method).then_some((receiver_fqn, symbol))
}

fn server_member_symbol_at_position(
    ctx: &InlayHintContext<'_>,
    line: u32,
    byte_col: u32,
) -> Option<SymbolAtPosition> {
    let point = tree_sitter::Point::new(line as usize, byte_col as usize);
    let mut node = ctx
        .tree
        .root_node()
        .descendant_for_point_range(point, point)?;
    while !node.is_named() {
        node = node.parent()?;
    }

    let point_range = (line, byte_col, line, byte_col);
    let mut current = Some(node);
    while let Some(candidate) = current {
        if matches!(
            candidate.kind(),
            "member_call_expression" | "nullsafe_member_call_expression"
        ) {
            let name_node = member_reference_name_node(candidate)?;
            if byte_range_contains(node_range_node(name_node), point_range) {
                let method_name = node_text(ctx.source, name_node).trim().to_string();
                let (_, symbol) = server_member_call_symbol(ctx, candidate)?;
                return Some(SymbolAtPosition {
                    fqn: symbol.fqn.clone(),
                    name: method_name,
                    ref_kind: RefKind::MethodCall,
                    object_expr: candidate
                        .child_by_field_name("object")
                        .map(|object| node_text(ctx.source, object).trim().to_string()),
                    range: node_range_node(name_node),
                });
            }
        }
        current = candidate.parent();
    }

    None
}

fn server_expression_type_info(
    ctx: &InlayHintContext<'_>,
    expression: tree_sitter::Node,
) -> Option<IndexedExpressionTypeInfo> {
    let expression = normalized_expression_node(expression);
    match expression.kind() {
        "object_creation_expression" => {
            let class_node = object_creation_class_node(expression)?;
            let class_name = node_text(ctx.source, class_node).trim();
            let fqn = resolve_class_name_pub(class_name, ctx.file_symbols)
                .trim_start_matches('\\')
                .to_string();
            if fqn.is_empty() {
                return None;
            }
            let uri = ctx
                .index
                .resolve_fqn(&fqn)
                .map(|symbol| symbol.uri.clone())
                .unwrap_or_default();
            Some(IndexedExpressionTypeInfo {
                type_info: php_lsp_types::TypeInfo::Simple(fqn.clone()),
                owner_fqn: fqn,
                uri,
            })
        }
        "variable_name" => server_variable_type_info(ctx, expression),
        "function_call_expression"
        | "member_call_expression"
        | "nullsafe_member_call_expression"
        | "scoped_call_expression" => indexed_call_expression_type_info(ctx, expression),
        _ => None,
    }
}

fn server_variable_type_info(
    ctx: &InlayHintContext<'_>,
    variable_node: tree_sitter::Node,
) -> Option<IndexedExpressionTypeInfo> {
    if let Some(foreach_stmt) = enclosing_foreach_statement_for_variable(ctx.source, variable_node)
    {
        let iterable_node = foreach_iterable_node_for_inlay(foreach_stmt)?;
        let iterable_type = indexed_expression_type_info(ctx, iterable_node)?;
        let value_type = iterable_value_type_info(&iterable_type.type_info, None)?;
        return Some(IndexedExpressionTypeInfo {
            type_info: value_type,
            owner_fqn: iterable_type.owner_fqn,
            uri: iterable_type.uri,
        });
    }

    call_site_variable_phpdoc_type(ctx, variable_node).map(|type_info| IndexedExpressionTypeInfo {
        type_info,
        owner_fqn: current_class_fqn(ctx.file_symbols).unwrap_or_default(),
        uri: String::new(),
    })
}

fn doctrine_repository_call_type_info(
    ctx: &InlayHintContext<'_>,
    expression: tree_sitter::Node,
) -> Option<IndexedExpressionTypeInfo> {
    let expression = normalized_expression_node(expression);
    if !matches!(
        expression.kind(),
        "member_call_expression" | "nullsafe_member_call_expression"
    ) {
        return None;
    }

    let object = expression.child_by_field_name("object")?;
    let name = expression.child_by_field_name("name")?;
    let method_name = node_text(ctx.source, name).trim();
    let entity_fqn = doctrine_get_repository_entity_fqn(ctx, object)?;

    if let Some(repository_fqn) = doctrine_repository_class_for_entity(ctx, &entity_fqn) {
        let method_fqn = format!("{repository_fqn}::{method_name}");
        if let Some(symbol) = ctx.index.resolve_fqn(&method_fqn) {
            if symbol.kind == php_lsp_types::PhpSymbolKind::Method {
                let return_type = symbol_effective_return_type(&symbol)?;
                let owner_fqn = symbol.parent_fqn.as_deref().unwrap_or(&repository_fqn);
                let type_info =
                    resolve_call_site_return_type(ctx, expression, &symbol, &return_type);
                return Some(IndexedExpressionTypeInfo {
                    type_info,
                    owner_fqn: owner_fqn.to_string(),
                    uri: symbol.uri.clone(),
                });
            }
        }
    }

    let type_info = doctrine_standard_repository_method_return_type(method_name, &entity_fqn)?;
    let uri = ctx
        .index
        .resolve_fqn(&entity_fqn)
        .map(|symbol| symbol.uri.clone())
        .unwrap_or_default();

    Some(IndexedExpressionTypeInfo {
        type_info,
        owner_fqn: entity_fqn,
        uri,
    })
}

fn doctrine_get_repository_entity_fqn(
    ctx: &InlayHintContext<'_>,
    object: tree_sitter::Node,
) -> Option<String> {
    let object = normalized_expression_node(object);
    if !matches!(
        object.kind(),
        "member_call_expression" | "nullsafe_member_call_expression"
    ) {
        return None;
    }

    let name = object.child_by_field_name("name")?;
    if node_text(ctx.source, name).trim() != "getRepository" {
        return None;
    }

    let first_arg = call_arguments(object, ctx.source).into_iter().next()?;
    let raw = node_text(ctx.source, first_arg.value_node);
    class_string_fqn_from_expression_text(raw, ctx.file_symbols, ctx.index)
}

fn doctrine_repository_class_for_entity(
    ctx: &InlayHintContext<'_>,
    entity_fqn: &str,
) -> Option<String> {
    let normalized_entity = entity_fqn.trim_start_matches('\\');
    ctx.type_cache.cached_string(
        (0, 0, 0, 0),
        "doctrine-repository-class",
        normalized_entity,
        || {
            doctrine_repository_class_from_template_binding(ctx.index, normalized_entity).or_else(
                || {
                    doctrine_repository_class_from_entity_attribute(
                        ctx.index,
                        ctx.file_symbols,
                        normalized_entity,
                    )
                },
            )
        },
    )
}

fn doctrine_repository_class_from_template_binding(
    index: &WorkspaceIndex,
    entity_fqn: &str,
) -> Option<String> {
    index.types.iter().find_map(|entry| {
        let symbol = entry.value();
        if !matches!(symbol.kind, php_lsp_types::PhpSymbolKind::Class) {
            return None;
        }

        symbol.template_bindings.iter().find_map(|binding| {
            if binding.kind != php_lsp_types::TemplateBindingKind::Extends
                || !is_doctrine_repository_base(&binding.target)
            {
                return None;
            }

            let bound_entity = binding.args.first().and_then(type_info_simple_fqn)?;
            fqn_eq(&bound_entity, entity_fqn).then(|| symbol.fqn.clone())
        })
    })
}

fn doctrine_repository_class_from_entity_attribute(
    index: &WorkspaceIndex,
    current_file_symbols: &php_lsp_types::FileSymbols,
    entity_fqn: &str,
) -> Option<String> {
    let entity = index.resolve_fqn(entity_fqn)?;
    let path = uri_to_path(&entity.uri)?;
    let source = std::fs::read_to_string(path).ok()?;
    let declaration_line = entity.range.0 as usize;
    let start_line = declaration_line.saturating_sub(32);
    let attribute_text = source
        .lines()
        .skip(start_line)
        .take(declaration_line.saturating_sub(start_line) + 1)
        .collect::<Vec<_>>()
        .join("\n");
    let repository_name = doctrine_repository_class_name_from_attribute_text(&attribute_text)?;

    let entity_file_symbols = index.file_symbols.get(&entity.uri);
    let file_symbols = entity_file_symbols
        .as_ref()
        .map(|symbols| symbols.value())
        .unwrap_or(current_file_symbols);
    let resolved = resolve_class_name_pub(&repository_name, file_symbols)
        .trim_start_matches('\\')
        .to_string();

    (!resolved.is_empty() && index.resolve_fqn(&resolved).is_some()).then_some(resolved)
}

fn doctrine_repository_class_name_from_attribute_text(text: &str) -> Option<String> {
    doctrine_class_name_argument_from_attribute_text(text, "repositoryClass")
}

fn doctrine_standard_repository_method_return_type(
    method_name: &str,
    entity_fqn: &str,
) -> Option<php_lsp_types::TypeInfo> {
    let entity = php_lsp_types::TypeInfo::Simple(entity_fqn.to_string());
    if matches!(method_name, "find" | "findOneBy") || method_name.starts_with("findOneBy") {
        return Some(php_lsp_types::TypeInfo::Nullable(Box::new(entity)));
    }

    if matches!(method_name, "findAll" | "findBy") || method_name.starts_with("findBy") {
        return Some(php_lsp_types::TypeInfo::Generic {
            base: "list".to_string(),
            args: vec![entity],
        });
    }

    if method_name == "count" || method_name.starts_with("countBy") {
        return Some(php_lsp_types::TypeInfo::Simple("int".to_string()));
    }

    None
}

fn is_doctrine_repository_base(fqn: &str) -> bool {
    matches!(
        fqn.trim_start_matches('\\'),
        "Doctrine\\ORM\\EntityRepository"
            | "Doctrine\\Bundle\\DoctrineBundle\\Repository\\ServiceEntityRepository"
            | "Doctrine\\Persistence\\ObjectRepository"
    )
}

fn type_info_simple_fqn(type_info: &php_lsp_types::TypeInfo) -> Option<String> {
    match type_info {
        php_lsp_types::TypeInfo::Simple(name) => Some(name.trim_start_matches('\\').to_string()),
        php_lsp_types::TypeInfo::Nullable(inner) => type_info_simple_fqn(inner),
        _ => None,
    }
}

fn fqn_eq(left: &str, right: &str) -> bool {
    left.trim_start_matches('\\') == right.trim_start_matches('\\')
}

fn doctrine_collection_getter_return_type_info(
    ctx: &InlayHintContext<'_>,
    method: &php_lsp_types::SymbolInfo,
    owner_fqn: &str,
    return_type: &php_lsp_types::TypeInfo,
) -> Option<php_lsp_types::TypeInfo> {
    let collection_base = collection_base_type_name(return_type)?;
    let path = uri_to_path(&method.uri)?;
    let source = std::fs::read_to_string(path).ok()?;
    let property_name = returned_this_property_name_from_method_source(&source, method)
        .or_else(|| property_name_from_getter(&method.name))?;
    let target_fqn = doctrine_collection_target_entity_for_property(
        ctx.index,
        ctx.file_symbols,
        &method.uri,
        owner_fqn,
        &property_name,
        method.range.0 as usize,
        &source,
    )?;

    Some(php_lsp_types::TypeInfo::Generic {
        base: collection_base,
        args: vec![
            php_lsp_types::TypeInfo::Simple("int".to_string()),
            php_lsp_types::TypeInfo::Simple(target_fqn),
        ],
    })
}

fn collection_base_type_name(type_info: &php_lsp_types::TypeInfo) -> Option<String> {
    match type_info {
        php_lsp_types::TypeInfo::Simple(name) if is_collection_type_name(name) => {
            Some(name.clone())
        }
        php_lsp_types::TypeInfo::Nullable(inner) => collection_base_type_name(inner),
        _ => None,
    }
}

fn is_collection_type_name(name: &str) -> bool {
    let lower = name.trim_start_matches('\\').to_ascii_lowercase();
    lower == "collection"
        || lower.ends_with("\\collection")
        || lower == "doctrine\\common\\collections\\collection"
}

fn returned_this_property_name_from_method_source(
    source: &str,
    method: &php_lsp_types::SymbolInfo,
) -> Option<String> {
    let start = method.range.0 as usize;
    let end = method.range.2 as usize;
    let method_source = source
        .lines()
        .skip(start)
        .take(end.saturating_sub(start) + 1)
        .collect::<Vec<_>>()
        .join("\n");
    let marker = "return $this->";
    let after_marker = method_source
        .find(marker)
        .map(|idx| &method_source[idx + marker.len()..])?;
    let end = after_marker
        .char_indices()
        .find_map(|(idx, ch)| (!(ch.is_ascii_alphanumeric() || ch == '_')).then_some(idx))
        .unwrap_or(after_marker.len());
    let property = after_marker[..end].trim();
    (!property.is_empty()).then(|| property.to_string())
}

fn property_name_from_getter(method_name: &str) -> Option<String> {
    let rest = method_name.strip_prefix("get")?;
    let mut chars = rest.chars();
    let first = chars.next()?;
    let mut property = first.to_ascii_lowercase().to_string();
    property.push_str(chars.as_str());
    Some(property)
}

#[allow(clippy::too_many_arguments)]
fn doctrine_collection_target_entity_for_property(
    index: &WorkspaceIndex,
    current_file_symbols: &php_lsp_types::FileSymbols,
    uri: &str,
    owner_fqn: &str,
    property_name: &str,
    before_line: usize,
    source: &str,
) -> Option<String> {
    let owner = index.resolve_fqn(owner_fqn)?;
    if owner.uri != uri {
        return None;
    }

    let property_pattern = format!("${property_name}");
    let lines: Vec<&str> = source.lines().collect();
    let search_end = before_line.min(lines.len().saturating_sub(1));
    for line_index in 0..=search_end {
        let line = lines[line_index];
        if !line.contains(&property_pattern) || !line.contains("Collection") {
            continue;
        }

        let start_line = line_index.saturating_sub(32);
        let metadata = lines[start_line..=line_index].join("\n");
        let Some(target_name) = doctrine_target_entity_class_name_from_attribute_text(&metadata)
        else {
            continue;
        };

        let owner_file_symbols = index.file_symbols.get(uri);
        let file_symbols = owner_file_symbols
            .as_ref()
            .map(|symbols| symbols.value())
            .unwrap_or(current_file_symbols);
        let resolved = resolve_class_name_pub(&target_name, file_symbols)
            .trim_start_matches('\\')
            .to_string();
        if !resolved.is_empty()
            && (index.resolve_fqn(&resolved).is_some()
                || file_symbols
                    .symbols
                    .iter()
                    .any(|symbol| symbol.fqn == resolved))
        {
            return Some(resolved);
        }
    }

    None
}

fn doctrine_target_entity_class_name_from_attribute_text(text: &str) -> Option<String> {
    doctrine_class_name_argument_from_attribute_text(text, "targetEntity")
}

fn doctrine_class_name_argument_from_attribute_text(text: &str, argument: &str) -> Option<String> {
    let marker_start = text.find(argument)?;
    let after_marker = &text[marker_start + argument.len()..];
    let separator = after_marker
        .char_indices()
        .find_map(|(idx, ch)| matches!(ch, ':' | '=').then_some(idx))?;
    let after_separator = after_marker[separator + 1..].trim_start();
    let mut end = 0usize;
    for (idx, ch) in after_separator.char_indices() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '\\') {
            end = idx + ch.len_utf8();
        } else {
            break;
        }
    }

    let class_name = after_separator[..end].trim().trim_start_matches('\\');
    if class_name.is_empty() || !after_separator[end..].trim_start().starts_with("::class") {
        return None;
    }

    Some(class_name.to_string())
}

fn local_variable_inlay_type_from_new_expression(
    ctx: &InlayHintContext<'_>,
    expression: tree_sitter::Node,
) -> Option<LocalVariableInlayType> {
    let class_node = object_creation_class_node(expression)?;
    let class_name = node_text(ctx.source, class_node).trim();
    let fqn = resolve_class_name_pub(class_name, ctx.file_symbols)
        .trim_start_matches('\\')
        .to_string();
    if fqn.is_empty() {
        return None;
    }

    Some(LocalVariableInlayType {
        display: shorten_inlay_type_display(&fqn, ctx.file_symbols),
        target_fqn: Some(fqn),
    })
}

fn local_variable_inlay_type_from_call_expression(
    ctx: &InlayHintContext<'_>,
    expression: tree_sitter::Node,
) -> Option<LocalVariableInlayType> {
    let info = indexed_call_expression_type_info(ctx, expression)?;
    local_variable_inlay_type_from_type_info(ctx, &info.owner_fqn, &info.uri, &info.type_info, true)
}

fn completion_call_arguments_by_param(
    member_text: &str,
    signature: &php_lsp_types::Signature,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
) -> HashMap<String, php_lsp_types::TypeInfo> {
    let mut arguments = HashMap::new();
    let Some(args_text) = call_arguments_text(member_text) else {
        return arguments;
    };

    for (arg_index, raw_arg) in split_top_level_argument_texts(args_text)
        .into_iter()
        .enumerate()
    {
        let (name, value) = split_named_argument_text(raw_arg);
        let Some(param) = signature_param_for_call_arg(signature, arg_index, name) else {
            continue;
        };
        let Some(type_info) = call_site_argument_type_from_text(value, file_symbols, index) else {
            continue;
        };
        arguments.insert(param.name.trim_start_matches('$').to_string(), type_info);
    }

    arguments
}

fn call_arguments_text(member_text: &str) -> Option<&str> {
    let open = member_text.find('(')?;
    let close = matching_paren_in_text(member_text, open)?;
    Some(member_text[open + 1..close].trim())
}

fn matching_paren_in_text(text: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for (idx, ch) in text[open..].char_indices() {
        let idx = open + idx;
        if let Some(quote_ch) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote_ch {
                quote = None;
            }
            continue;
        }

        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }

        match ch {
            '(' => depth += 1,
            ')' => {
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

fn split_top_level_argument_texts(args_text: &str) -> Vec<&str> {
    split_top_level_text(args_text, ',')
}

fn split_named_argument_text(arg_text: &str) -> (Option<&str>, &str) {
    let arg_text = arg_text.trim();
    let Some(colon) = find_named_argument_colon(arg_text) else {
        return (None, arg_text);
    };
    let name = arg_text[..colon].trim();
    let value = arg_text[colon + 1..].trim();
    if name.is_empty() || value.is_empty() {
        (None, arg_text)
    } else {
        (Some(name), value)
    }
}

fn find_named_argument_colon(arg_text: &str) -> Option<usize> {
    split_top_level_text_scan(arg_text, |idx, ch, nested| {
        if ch != ':' || nested {
            return None;
        }
        let prev = arg_text[..idx].chars().next_back();
        let next = arg_text[idx + ch.len_utf8()..].chars().next();
        (prev != Some(':') && next != Some(':')).then_some(idx)
    })
}

fn call_site_argument_type_from_text(
    raw: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
) -> Option<php_lsp_types::TypeInfo> {
    let raw = raw.trim();
    let lower = raw.to_ascii_lowercase();

    if let Some(class_fqn) = class_string_fqn_from_expression_text(raw, file_symbols, index) {
        return Some(php_lsp_types::TypeInfo::ClassString(Some(Box::new(
            php_lsp_types::TypeInfo::Simple(class_fqn),
        ))));
    }

    if let Some(value) = unquote_php_string_literal(raw) {
        let resolved = resolve_class_name_pub(&value, file_symbols)
            .trim_start_matches('\\')
            .to_string();
        if index.resolve_fqn(&resolved).is_some()
            || file_symbols
                .symbols
                .iter()
                .any(|symbol| symbol.fqn == resolved)
        {
            return Some(php_lsp_types::TypeInfo::ClassString(Some(Box::new(
                php_lsp_types::TypeInfo::Simple(resolved),
            ))));
        }
        return Some(php_lsp_types::TypeInfo::LiteralString(raw.to_string()));
    }

    if lower == "true" {
        return Some(php_lsp_types::TypeInfo::LiteralBool(true));
    }
    if lower == "false" {
        return Some(php_lsp_types::TypeInfo::LiteralBool(false));
    }
    if lower == "null" {
        return Some(php_lsp_types::TypeInfo::LiteralNull);
    }

    let numeric = lower.trim_start_matches(['+', '-']);
    if numeric.parse::<i64>().is_ok() {
        return Some(php_lsp_types::TypeInfo::LiteralInt(raw.to_string()));
    }
    if numeric.parse::<f64>().is_ok() && numeric.contains('.') {
        return Some(php_lsp_types::TypeInfo::LiteralFloat(raw.to_string()));
    }

    None
}

fn unquote_php_string_literal(raw: &str) -> Option<String> {
    if raw.len() < 2 {
        return None;
    }
    let quote = raw.as_bytes()[0] as char;
    if !matches!(quote, '\'' | '"') || !raw.ends_with(quote) {
        return None;
    }
    Some(raw[1..raw.len() - 1].replace("\\\\", "\\"))
}

fn split_top_level_text(text: &str, delimiter: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut paren_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for (idx, ch) in text.char_indices() {
        if let Some(quote_ch) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote_ch {
                quote = None;
            }
            continue;
        }

        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }

        let nested = paren_depth > 0 || angle_depth > 0 || bracket_depth > 0 || brace_depth > 0;
        if ch == delimiter && !nested {
            let part = text[start..idx].trim();
            if !part.is_empty() {
                parts.push(part);
            }
            start = idx + ch.len_utf8();
            continue;
        }

        match ch {
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '<' => angle_depth += 1,
            '>' => angle_depth = angle_depth.saturating_sub(1),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }
    }

    let part = text[start..].trim();
    if !part.is_empty() {
        parts.push(part);
    }
    parts
}

fn split_top_level_text_scan<T>(
    text: &str,
    mut f: impl FnMut(usize, char, bool) -> Option<T>,
) -> Option<T> {
    let mut paren_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for (idx, ch) in text.char_indices() {
        if let Some(quote_ch) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote_ch {
                quote = None;
            }
            continue;
        }

        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }

        let nested = paren_depth > 0 || angle_depth > 0 || bracket_depth > 0 || brace_depth > 0;
        if let Some(value) = f(idx, ch, nested) {
            return Some(value);
        }

        match ch {
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '<' => angle_depth += 1,
            '>' => angle_depth = angle_depth.saturating_sub(1),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }
    }

    None
}

fn resolve_call_site_return_type(
    ctx: &InlayHintContext<'_>,
    call_node: tree_sitter::Node,
    symbol: &php_lsp_types::SymbolInfo,
    return_type: &php_lsp_types::TypeInfo,
) -> php_lsp_types::TypeInfo {
    let Some(signature) = symbol.signature.as_ref() else {
        return return_type.clone();
    };

    let arguments = call_site_arguments_by_param(ctx, call_node, signature);
    let template_names: HashSet<String> = symbol
        .templates
        .iter()
        .map(|template| template.name.clone())
        .collect();
    let substitutions = call_site_template_substitutions(&arguments, signature, &template_names);
    resolve_call_site_type_info(return_type, &arguments, &template_names, &substitutions)
}

fn symbol_effective_return_type(
    symbol: &php_lsp_types::SymbolInfo,
) -> Option<php_lsp_types::TypeInfo> {
    let native = symbol
        .signature
        .as_ref()
        .and_then(|signature| signature.return_type.as_ref());
    let phpdoc = symbol
        .doc_comment
        .as_deref()
        .and_then(|doc| parse_phpdoc(doc).return_type);

    match (native, phpdoc) {
        (Some(native), Some(phpdoc))
            if type_info_specificity_score(&phpdoc) > type_info_specificity_score(native) =>
        {
            Some(phpdoc)
        }
        (Some(native), _) => Some(native.clone()),
        (None, Some(phpdoc)) => Some(phpdoc),
        (None, None) => None,
    }
}

fn type_info_specificity_score(type_info: &php_lsp_types::TypeInfo) -> usize {
    match type_info {
        php_lsp_types::TypeInfo::Mixed
        | php_lsp_types::TypeInfo::Void
        | php_lsp_types::TypeInfo::Never
        | php_lsp_types::TypeInfo::LiteralNull => 0,
        php_lsp_types::TypeInfo::Simple(name) => {
            if is_builtin_type_name(name) {
                1
            } else {
                3
            }
        }
        php_lsp_types::TypeInfo::Self_
        | php_lsp_types::TypeInfo::Static_
        | php_lsp_types::TypeInfo::Parent_ => 3,
        php_lsp_types::TypeInfo::Nullable(inner) => type_info_specificity_score(inner),
        php_lsp_types::TypeInfo::Union(types) | php_lsp_types::TypeInfo::Intersection(types) => {
            types.iter().map(type_info_specificity_score).sum()
        }
        php_lsp_types::TypeInfo::Generic { args, .. } => {
            4 + args.iter().map(type_info_specificity_score).sum::<usize>()
        }
        php_lsp_types::TypeInfo::ArrayShape(items) => {
            5 + items
                .iter()
                .map(|item| type_info_specificity_score(&item.value))
                .sum::<usize>()
        }
        php_lsp_types::TypeInfo::ObjectShape(items) => {
            5 + items
                .iter()
                .map(|item| type_info_specificity_score(&item.value))
                .sum::<usize>()
        }
        php_lsp_types::TypeInfo::Callable {
            params,
            return_type,
        } => {
            3 + params
                .iter()
                .map(type_info_specificity_score)
                .sum::<usize>()
                + return_type
                    .as_ref()
                    .map(|return_type| type_info_specificity_score(return_type))
                    .unwrap_or_default()
        }
        php_lsp_types::TypeInfo::ClassString(inner) => {
            3 + inner
                .as_ref()
                .map(|inner| type_info_specificity_score(inner))
                .unwrap_or_default()
        }
        php_lsp_types::TypeInfo::LiteralString(_)
        | php_lsp_types::TypeInfo::LiteralInt(_)
        | php_lsp_types::TypeInfo::LiteralFloat(_)
        | php_lsp_types::TypeInfo::LiteralBool(_) => 2,
        php_lsp_types::TypeInfo::Conditional {
            if_type, else_type, ..
        } => 3 + type_info_specificity_score(if_type) + type_info_specificity_score(else_type),
    }
}

fn call_site_arguments_by_param(
    ctx: &InlayHintContext<'_>,
    call_node: tree_sitter::Node,
    signature: &php_lsp_types::Signature,
) -> HashMap<String, php_lsp_types::TypeInfo> {
    let mut arguments = HashMap::new();
    for (arg_index, arg) in call_arguments(call_node, ctx.source)
        .into_iter()
        .enumerate()
    {
        let Some(param) = signature_param_for_call_arg(signature, arg_index, arg.name.as_deref())
        else {
            continue;
        };
        let Some(type_info) = call_site_argument_type(ctx, arg.value_node) else {
            continue;
        };
        arguments.insert(param.name.trim_start_matches('$').to_string(), type_info);
    }
    arguments
}

fn call_site_argument_type(
    ctx: &InlayHintContext<'_>,
    node: tree_sitter::Node,
) -> Option<php_lsp_types::TypeInfo> {
    let node = normalized_expression_node(node);
    ctx.type_cache.cached_type_info(
        node_range_node(node),
        "call-site-argument-type",
        node.kind(),
        || call_site_argument_type_uncached(ctx, node),
    )
}

fn call_site_argument_type_uncached(
    ctx: &InlayHintContext<'_>,
    node: tree_sitter::Node,
) -> Option<php_lsp_types::TypeInfo> {
    let raw = node_text(ctx.source, node).trim();
    let lower = raw.to_ascii_lowercase();

    if let Some(class_fqn) = class_string_fqn_from_expression_text(raw, ctx.file_symbols, ctx.index)
    {
        return Some(php_lsp_types::TypeInfo::ClassString(Some(Box::new(
            php_lsp_types::TypeInfo::Simple(class_fqn),
        ))));
    }

    if let Some(value) = unquote_php_string_literal(raw) {
        let resolved = resolve_class_name_pub(&value, ctx.file_symbols)
            .trim_start_matches('\\')
            .to_string();
        if ctx.index.resolve_fqn(&resolved).is_some()
            || ctx
                .file_symbols
                .symbols
                .iter()
                .any(|symbol| symbol.fqn == resolved)
        {
            return Some(php_lsp_types::TypeInfo::ClassString(Some(Box::new(
                php_lsp_types::TypeInfo::Simple(resolved),
            ))));
        }
        return Some(php_lsp_types::TypeInfo::LiteralString(raw.to_string()));
    }
    if node.kind().contains("string") {
        return Some(php_lsp_types::TypeInfo::LiteralString(raw.to_string()));
    }
    if lower == "true" {
        return Some(php_lsp_types::TypeInfo::LiteralBool(true));
    }
    if lower == "false" {
        return Some(php_lsp_types::TypeInfo::LiteralBool(false));
    }
    if lower == "null" {
        return Some(php_lsp_types::TypeInfo::LiteralNull);
    }

    let numeric = lower.trim_start_matches(['+', '-']);
    if numeric.parse::<i64>().is_ok() {
        return Some(php_lsp_types::TypeInfo::LiteralInt(raw.to_string()));
    }
    if numeric.parse::<f64>().is_ok() && numeric.contains('.') {
        return Some(php_lsp_types::TypeInfo::LiteralFloat(raw.to_string()));
    }

    if node.kind() == "object_creation_expression" {
        let class_node = object_creation_class_node(node)?;
        let class_name = node_text(ctx.source, class_node).trim();
        let fqn = resolve_class_name_pub(class_name, ctx.file_symbols)
            .trim_start_matches('\\')
            .to_string();
        if !fqn.is_empty() {
            return Some(php_lsp_types::TypeInfo::Simple(fqn));
        }
    }

    if node.kind() == "variable_name" {
        return call_site_variable_phpdoc_type(ctx, node);
    }

    None
}

fn class_string_fqn_from_expression_text(
    raw: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
) -> Option<String> {
    let class_name = raw.trim().strip_suffix("::class")?.trim();
    if class_name.is_empty() {
        return None;
    }

    let fqn = resolve_class_name_pub(class_name, file_symbols)
        .trim_start_matches('\\')
        .to_string();
    (index.resolve_fqn(&fqn).is_some()
        || file_symbols.symbols.iter().any(|symbol| symbol.fqn == fqn))
    .then_some(fqn)
}

fn call_site_variable_phpdoc_type(
    ctx: &InlayHintContext<'_>,
    node: tree_sitter::Node,
) -> Option<php_lsp_types::TypeInfo> {
    let variable_name = variable_text_for_node(ctx.source, node)?;
    let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
        ctx.type_cache.cached_string(
            (0, 0, 0, 0),
            "member-type",
            format!("{class_fqn}::{member_name}"),
            || resolve_member_type_from_index(ctx.index, class_fqn, member_name),
        )
    };
    let callable_param_resolver = |callable_ctx: CallableParameterContext<'_>| {
        resolve_callable_parameter_type_from_index(ctx.index, ctx.file_symbols, callable_ctx)
    };
    let info = infer_variable_hover_info_at_node_with_resolvers(
        node,
        ctx.source,
        ctx.file_symbols,
        node.start_byte(),
        &variable_name,
        Some(&resolver),
        Some(&callable_param_resolver),
    )?;
    let phpdoc = parse_phpdoc(info.phpdoc_comment.as_deref()?);
    phpdoc
        .var_type
        .map(|type_info| resolve_call_site_type_names(&type_info, ctx.file_symbols))
}

fn resolve_callable_parameter_type_from_index(
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    ctx: CallableParameterContext<'_>,
) -> Option<php_lsp_types::TypeInfo> {
    let symbol = index.resolve_fqn(ctx.target_fqn)?;
    let signature = symbol.signature.as_ref()?;
    let callable_param =
        signature_param_for_call_arg(signature, ctx.argument_index, ctx.argument_name)?;
    let expected = callable_param.type_info.as_ref()?;
    let template_names = callable_template_names_from_index(index, &symbol, ctx.target_fqn);
    let mut substitutions = receiver_template_substitutions_from_index(index, file_symbols, &ctx);

    for arg in ctx.argument_types {
        if arg.argument_index == ctx.argument_index {
            continue;
        }
        let Some(param) = signature_param_for_call_arg(
            signature,
            arg.argument_index,
            arg.argument_name.as_deref(),
        ) else {
            continue;
        };
        let Some(param_type) = param.type_info.as_ref() else {
            continue;
        };
        bind_template_type_info(
            param_type,
            &arg.type_info,
            &template_names,
            &mut substitutions,
        );
    }

    let expected = substitute_call_site_type_info(expected, &substitutions);
    callable_param_type_from_type_info(&expected, ctx.parameter_index)
}

fn callable_template_names_from_index(
    index: &WorkspaceIndex,
    symbol: &php_lsp_types::SymbolInfo,
    target_fqn: &str,
) -> HashSet<String> {
    let mut names = symbol
        .templates
        .iter()
        .map(|template| template.name.clone())
        .collect::<HashSet<_>>();
    if let Some((class_fqn, _)) = target_fqn.rsplit_once("::") {
        if let Some(class_symbol) = index.resolve_fqn(class_fqn) {
            names.extend(
                class_symbol
                    .templates
                    .iter()
                    .map(|template| template.name.clone()),
            );
        }
    }
    names
}

fn receiver_template_substitutions_from_index(
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    ctx: &CallableParameterContext<'_>,
) -> HashMap<String, php_lsp_types::TypeInfo> {
    let mut substitutions = HashMap::new();
    let Some((class_fqn, _)) = ctx.target_fqn.rsplit_once("::") else {
        return substitutions;
    };
    let Some(php_lsp_types::TypeInfo::Generic { base, args }) = ctx.receiver_type else {
        return substitutions;
    };
    let resolved_base = resolve_class_name_pub(base, file_symbols)
        .trim_start_matches('\\')
        .to_string();
    if resolved_base != class_fqn.trim_start_matches('\\') {
        return substitutions;
    }
    let Some(class_symbol) = index.resolve_fqn(class_fqn) else {
        return substitutions;
    };
    for (template, arg) in class_symbol.templates.iter().zip(args.iter()) {
        substitutions.insert(template.name.clone(), arg.clone());
    }
    substitutions
}

fn callable_param_type_from_type_info(
    type_info: &php_lsp_types::TypeInfo,
    parameter_index: usize,
) -> Option<php_lsp_types::TypeInfo> {
    match type_info {
        php_lsp_types::TypeInfo::Callable { params, .. } => params.get(parameter_index).cloned(),
        php_lsp_types::TypeInfo::Nullable(inner) => {
            callable_param_type_from_type_info(inner, parameter_index)
        }
        php_lsp_types::TypeInfo::Union(types) | php_lsp_types::TypeInfo::Intersection(types) => {
            types.iter().find_map(|type_info| {
                callable_param_type_from_type_info(type_info, parameter_index)
            })
        }
        _ => None,
    }
}

fn call_site_template_substitutions(
    arguments: &HashMap<String, php_lsp_types::TypeInfo>,
    signature: &php_lsp_types::Signature,
    template_names: &HashSet<String>,
) -> HashMap<String, php_lsp_types::TypeInfo> {
    let mut substitutions = HashMap::new();
    for param in &signature.params {
        let Some(param_type) = param.type_info.as_ref() else {
            continue;
        };
        let Some(arg_type) = arguments.get(param.name.trim_start_matches('$')) else {
            continue;
        };
        bind_template_type_info(param_type, arg_type, template_names, &mut substitutions);
    }
    substitutions
}

fn resolve_call_site_type_info(
    type_info: &php_lsp_types::TypeInfo,
    arguments: &HashMap<String, php_lsp_types::TypeInfo>,
    template_names: &HashSet<String>,
    substitutions: &HashMap<String, php_lsp_types::TypeInfo>,
) -> php_lsp_types::TypeInfo {
    let substituted = substitute_call_site_type_info(type_info, substitutions);
    match substituted {
        php_lsp_types::TypeInfo::Conditional {
            subject,
            target,
            if_type,
            else_type,
        } => {
            let subject_key = subject.trim().trim_start_matches('$');
            let Some(actual) = arguments.get(subject_key) else {
                return conditional_union_fallback(*if_type, *else_type);
            };
            let mut branch_substitutions = substitutions.clone();
            if type_pattern_matches_actual(
                &target,
                actual,
                template_names,
                &mut branch_substitutions,
            ) {
                substitute_call_site_type_info(&if_type, &branch_substitutions)
            } else {
                substitute_call_site_type_info(&else_type, &branch_substitutions)
            }
        }
        other => other,
    }
}

fn conditional_union_fallback(
    if_type: php_lsp_types::TypeInfo,
    else_type: php_lsp_types::TypeInfo,
) -> php_lsp_types::TypeInfo {
    if if_type == else_type {
        if_type
    } else {
        php_lsp_types::TypeInfo::Union(vec![if_type, else_type])
    }
}

fn substitute_call_site_type_info(
    type_info: &php_lsp_types::TypeInfo,
    substitutions: &HashMap<String, php_lsp_types::TypeInfo>,
) -> php_lsp_types::TypeInfo {
    match type_info {
        php_lsp_types::TypeInfo::Simple(name) => substitutions
            .get(name)
            .cloned()
            .unwrap_or_else(|| php_lsp_types::TypeInfo::Simple(name.clone())),
        php_lsp_types::TypeInfo::Generic { base, args } => php_lsp_types::TypeInfo::Generic {
            base: base.clone(),
            args: args
                .iter()
                .map(|arg| substitute_call_site_type_info(arg, substitutions))
                .collect(),
        },
        php_lsp_types::TypeInfo::ArrayShape(items) => php_lsp_types::TypeInfo::ArrayShape(
            items
                .iter()
                .map(|item| php_lsp_types::ArrayShapeItem {
                    key: item.key.clone(),
                    optional: item.optional,
                    value: substitute_call_site_type_info(&item.value, substitutions),
                })
                .collect(),
        ),
        php_lsp_types::TypeInfo::ObjectShape(items) => php_lsp_types::TypeInfo::ObjectShape(
            items
                .iter()
                .map(|item| php_lsp_types::ArrayShapeItem {
                    key: item.key.clone(),
                    optional: item.optional,
                    value: substitute_call_site_type_info(&item.value, substitutions),
                })
                .collect(),
        ),
        php_lsp_types::TypeInfo::Callable {
            params,
            return_type,
        } => php_lsp_types::TypeInfo::Callable {
            params: params
                .iter()
                .map(|param| substitute_call_site_type_info(param, substitutions))
                .collect(),
            return_type: return_type.as_ref().map(|return_type| {
                Box::new(substitute_call_site_type_info(return_type, substitutions))
            }),
        },
        php_lsp_types::TypeInfo::ClassString(Some(inner)) => {
            php_lsp_types::TypeInfo::ClassString(Some(Box::new(substitute_call_site_type_info(
                inner,
                substitutions,
            ))))
        }
        php_lsp_types::TypeInfo::Conditional {
            subject,
            target,
            if_type,
            else_type,
        } => php_lsp_types::TypeInfo::Conditional {
            subject: subject.clone(),
            target: Box::new(substitute_call_site_type_info(target, substitutions)),
            if_type: Box::new(substitute_call_site_type_info(if_type, substitutions)),
            else_type: Box::new(substitute_call_site_type_info(else_type, substitutions)),
        },
        php_lsp_types::TypeInfo::Union(types) => php_lsp_types::TypeInfo::Union(
            types
                .iter()
                .map(|type_info| substitute_call_site_type_info(type_info, substitutions))
                .collect(),
        ),
        php_lsp_types::TypeInfo::Intersection(types) => php_lsp_types::TypeInfo::Intersection(
            types
                .iter()
                .map(|type_info| substitute_call_site_type_info(type_info, substitutions))
                .collect(),
        ),
        php_lsp_types::TypeInfo::Nullable(inner) => php_lsp_types::TypeInfo::Nullable(Box::new(
            substitute_call_site_type_info(inner, substitutions),
        )),
        php_lsp_types::TypeInfo::ClassString(None)
        | php_lsp_types::TypeInfo::LiteralString(_)
        | php_lsp_types::TypeInfo::LiteralInt(_)
        | php_lsp_types::TypeInfo::LiteralFloat(_)
        | php_lsp_types::TypeInfo::LiteralBool(_)
        | php_lsp_types::TypeInfo::LiteralNull
        | php_lsp_types::TypeInfo::Void
        | php_lsp_types::TypeInfo::Never
        | php_lsp_types::TypeInfo::Mixed
        | php_lsp_types::TypeInfo::Self_
        | php_lsp_types::TypeInfo::Static_
        | php_lsp_types::TypeInfo::Parent_ => type_info.clone(),
    }
}

fn bind_template_type_info(
    pattern: &php_lsp_types::TypeInfo,
    actual: &php_lsp_types::TypeInfo,
    template_names: &HashSet<String>,
    substitutions: &mut HashMap<String, php_lsp_types::TypeInfo>,
) {
    match (pattern, actual) {
        (php_lsp_types::TypeInfo::Simple(name), actual) if template_names.contains(name) => {
            substitutions
                .entry(name.clone())
                .or_insert_with(|| actual.clone());
        }
        (
            php_lsp_types::TypeInfo::ClassString(Some(pattern_inner)),
            php_lsp_types::TypeInfo::ClassString(Some(actual_inner)),
        ) => bind_template_type_info(pattern_inner, actual_inner, template_names, substitutions),
        (
            php_lsp_types::TypeInfo::Generic {
                base: pattern_base,
                args: pattern_args,
            },
            php_lsp_types::TypeInfo::Generic {
                base: actual_base,
                args: actual_args,
            },
        ) if pattern_base.eq_ignore_ascii_case(actual_base) => {
            for (pattern_arg, actual_arg) in pattern_args.iter().zip(actual_args.iter()) {
                bind_template_type_info(pattern_arg, actual_arg, template_names, substitutions);
            }
        }
        (php_lsp_types::TypeInfo::Nullable(pattern_inner), actual) => {
            bind_template_type_info(pattern_inner, actual, template_names, substitutions);
        }
        (php_lsp_types::TypeInfo::Union(patterns), actual)
        | (php_lsp_types::TypeInfo::Intersection(patterns), actual) => {
            for pattern in patterns {
                bind_template_type_info(pattern, actual, template_names, substitutions);
            }
        }
        _ => {}
    }
}

fn type_pattern_matches_actual(
    pattern: &php_lsp_types::TypeInfo,
    actual: &php_lsp_types::TypeInfo,
    template_names: &HashSet<String>,
    substitutions: &mut HashMap<String, php_lsp_types::TypeInfo>,
) -> bool {
    match (pattern, actual) {
        (php_lsp_types::TypeInfo::Mixed, _) => true,
        (php_lsp_types::TypeInfo::Simple(name), actual) if template_names.contains(name) => {
            substitutions
                .entry(name.clone())
                .or_insert_with(|| actual.clone());
            true
        }
        (php_lsp_types::TypeInfo::Simple(expected), php_lsp_types::TypeInfo::Simple(actual)) => {
            same_type_name(expected, actual)
        }
        (
            php_lsp_types::TypeInfo::ClassString(Some(pattern_inner)),
            php_lsp_types::TypeInfo::ClassString(Some(actual_inner)),
        ) => {
            type_pattern_matches_actual(pattern_inner, actual_inner, template_names, substitutions)
        }
        (php_lsp_types::TypeInfo::ClassString(None), php_lsp_types::TypeInfo::ClassString(_)) => {
            true
        }
        (
            php_lsp_types::TypeInfo::Generic {
                base: expected_base,
                args: expected_args,
            },
            php_lsp_types::TypeInfo::Generic {
                base: actual_base,
                args: actual_args,
            },
        ) if same_type_name(expected_base, actual_base)
            && expected_args.len() == actual_args.len() =>
        {
            expected_args
                .iter()
                .zip(actual_args.iter())
                .all(|(expected_arg, actual_arg)| {
                    type_pattern_matches_actual(
                        expected_arg,
                        actual_arg,
                        template_names,
                        substitutions,
                    )
                })
        }
        (php_lsp_types::TypeInfo::Union(types), actual) => types.iter().any(|type_info| {
            let mut branch_substitutions = substitutions.clone();
            let matches = type_pattern_matches_actual(
                type_info,
                actual,
                template_names,
                &mut branch_substitutions,
            );
            if matches {
                *substitutions = branch_substitutions;
            }
            matches
        }),
        (php_lsp_types::TypeInfo::Intersection(types), actual) => types.iter().all(|type_info| {
            type_pattern_matches_actual(type_info, actual, template_names, substitutions)
        }),
        (php_lsp_types::TypeInfo::Nullable(_), php_lsp_types::TypeInfo::LiteralNull) => true,
        (php_lsp_types::TypeInfo::Nullable(inner), actual) => {
            type_pattern_matches_actual(inner, actual, template_names, substitutions)
        }
        (
            php_lsp_types::TypeInfo::LiteralString(expected),
            php_lsp_types::TypeInfo::LiteralString(actual),
        )
        | (
            php_lsp_types::TypeInfo::LiteralInt(expected),
            php_lsp_types::TypeInfo::LiteralInt(actual),
        )
        | (
            php_lsp_types::TypeInfo::LiteralFloat(expected),
            php_lsp_types::TypeInfo::LiteralFloat(actual),
        ) => expected == actual,
        (
            php_lsp_types::TypeInfo::LiteralBool(expected),
            php_lsp_types::TypeInfo::LiteralBool(actual),
        ) => expected == actual,
        (php_lsp_types::TypeInfo::LiteralNull, php_lsp_types::TypeInfo::LiteralNull) => true,
        _ => false,
    }
}

fn resolve_call_site_type_names(
    type_info: &php_lsp_types::TypeInfo,
    file_symbols: &php_lsp_types::FileSymbols,
) -> php_lsp_types::TypeInfo {
    match type_info {
        php_lsp_types::TypeInfo::Simple(name) if is_builtin_type_name(name) => {
            php_lsp_types::TypeInfo::Simple(name.clone())
        }
        php_lsp_types::TypeInfo::Simple(name) => php_lsp_types::TypeInfo::Simple(
            resolve_class_name_pub(name, file_symbols)
                .trim_start_matches('\\')
                .to_string(),
        ),
        php_lsp_types::TypeInfo::Generic { base, args } => php_lsp_types::TypeInfo::Generic {
            base: if is_builtin_type_name(base) {
                base.clone()
            } else {
                resolve_class_name_pub(base, file_symbols)
                    .trim_start_matches('\\')
                    .to_string()
            },
            args: args
                .iter()
                .map(|arg| resolve_call_site_type_names(arg, file_symbols))
                .collect(),
        },
        php_lsp_types::TypeInfo::ClassString(Some(inner)) => php_lsp_types::TypeInfo::ClassString(
            Some(Box::new(resolve_call_site_type_names(inner, file_symbols))),
        ),
        php_lsp_types::TypeInfo::Conditional {
            subject,
            target,
            if_type,
            else_type,
        } => php_lsp_types::TypeInfo::Conditional {
            subject: subject.clone(),
            target: Box::new(resolve_call_site_type_names(target, file_symbols)),
            if_type: Box::new(resolve_call_site_type_names(if_type, file_symbols)),
            else_type: Box::new(resolve_call_site_type_names(else_type, file_symbols)),
        },
        php_lsp_types::TypeInfo::Union(types) => php_lsp_types::TypeInfo::Union(
            types
                .iter()
                .map(|type_info| resolve_call_site_type_names(type_info, file_symbols))
                .collect(),
        ),
        php_lsp_types::TypeInfo::Intersection(types) => php_lsp_types::TypeInfo::Intersection(
            types
                .iter()
                .map(|type_info| resolve_call_site_type_names(type_info, file_symbols))
                .collect(),
        ),
        php_lsp_types::TypeInfo::Nullable(inner) => php_lsp_types::TypeInfo::Nullable(Box::new(
            resolve_call_site_type_names(inner, file_symbols),
        )),
        php_lsp_types::TypeInfo::ArrayShape(_)
        | php_lsp_types::TypeInfo::ObjectShape(_)
        | php_lsp_types::TypeInfo::Callable { .. }
        | php_lsp_types::TypeInfo::ClassString(None)
        | php_lsp_types::TypeInfo::LiteralString(_)
        | php_lsp_types::TypeInfo::LiteralInt(_)
        | php_lsp_types::TypeInfo::LiteralFloat(_)
        | php_lsp_types::TypeInfo::LiteralBool(_)
        | php_lsp_types::TypeInfo::LiteralNull
        | php_lsp_types::TypeInfo::Void
        | php_lsp_types::TypeInfo::Never
        | php_lsp_types::TypeInfo::Mixed
        | php_lsp_types::TypeInfo::Self_
        | php_lsp_types::TypeInfo::Static_
        | php_lsp_types::TypeInfo::Parent_ => type_info.clone(),
    }
}

fn same_type_name(left: &str, right: &str) -> bool {
    left.trim_start_matches('\\')
        .eq_ignore_ascii_case(right.trim_start_matches('\\'))
}

fn local_variable_inlay_type_from_cast_expression(
    ctx: &InlayHintContext<'_>,
    expression: tree_sitter::Node,
) -> Option<LocalVariableInlayType> {
    let cast_type = expression.child_by_field_name("type")?;
    let display = local_variable_cast_type_display(node_text(ctx.source, cast_type))?;
    Some(LocalVariableInlayType {
        display,
        target_fqn: None,
    })
}

fn local_variable_cast_type_display(raw_type: &str) -> Option<String> {
    let normalized = raw_type
        .trim()
        .trim_matches(|ch| ch == '(' || ch == ')')
        .to_ascii_lowercase();
    let display = match normalized.as_str() {
        "array" => "array",
        "binary" | "string" => "string",
        "bool" | "boolean" => "bool",
        "double" | "float" | "real" => "float",
        "int" | "integer" => "int",
        "object" => "object",
        _ => return None,
    };
    Some(display.to_string())
}

fn local_variable_inlay_type_from_variable_expression(
    ctx: &InlayHintContext<'_>,
    expression: tree_sitter::Node,
) -> Option<LocalVariableInlayType> {
    let variable_name = variable_text_for_node(ctx.source, expression)?;
    let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
        ctx.type_cache.cached_string(
            (0, 0, 0, 0),
            "member-type",
            format!("{class_fqn}::{member_name}"),
            || resolve_member_type_from_index(ctx.index, class_fqn, member_name),
        )
    };
    let callable_param_resolver = |callable_ctx: CallableParameterContext<'_>| {
        resolve_callable_parameter_type_from_index(ctx.index, ctx.file_symbols, callable_ctx)
    };
    let info = infer_variable_hover_info_at_node_with_resolvers(
        expression,
        ctx.source,
        ctx.file_symbols,
        expression.start_byte(),
        &variable_name,
        Some(&resolver),
        Some(&callable_param_resolver),
    )?;

    local_variable_type_from_hover_info(&info, ctx.file_symbols, false)
}

fn is_plain_assignment_expression(
    left: tree_sitter::Node,
    right: tree_sitter::Node,
    source: &str,
) -> bool {
    left.end_byte() <= right.start_byte()
        && source
            .get(left.end_byte()..right.start_byte())
            .is_some_and(|between| between.trim() == "=")
}

fn foreach_value_variable_node_for_inlay<'tree>(
    statement: tree_sitter::Node<'tree>,
    source: &str,
) -> Option<tree_sitter::Node<'tree>> {
    let value_expr = match statement.named_child(1)? {
        pair if pair.kind() == "pair" => {
            let count = pair.named_child_count();
            pair.named_child(count.saturating_sub(1))?
        }
        value => value,
    };
    variable_node_in_foreach_part_for_inlay(value_expr, source)
}

fn variable_node_in_foreach_part_for_inlay<'tree>(
    node: tree_sitter::Node<'tree>,
    source: &str,
) -> Option<tree_sitter::Node<'tree>> {
    if node.kind() == "variable_name" && node_text(source, node).starts_with('$') {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = variable_node_in_foreach_part_for_inlay(child, source) {
            return Some(found);
        }
    }
    None
}

fn local_variable_hover_data(
    ctx: &InlayHintContext<'_>,
    variable_node: tree_sitter::Node,
) -> Option<LocalVariableHoverData> {
    let variable_name = variable_text_for_node(ctx.source, variable_node)?;
    let usage_start = variable_node.start_byte();
    let current_rhs = current_assignment_rhs_for_variable(variable_node, ctx.source);
    let parser_usage_start = current_rhs
        .as_ref()
        .map(|rhs| rhs.end_byte())
        .unwrap_or(usage_start);
    let rhs_node = current_rhs.or_else(|| {
        let scope = local_variable_scope_node(variable_node);
        latest_assignment_rhs_before_usage(scope, &variable_name, usage_start, ctx.source)
            .map(|(_, rhs)| rhs)
    });

    let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
        ctx.type_cache.cached_string(
            (0, 0, 0, 0),
            "member-type",
            format!("{class_fqn}::{member_name}"),
            || resolve_member_type_from_index(ctx.index, class_fqn, member_name),
        )
    };
    let callable_param_resolver = |callable_ctx: CallableParameterContext<'_>| {
        resolve_callable_parameter_type_from_index(ctx.index, ctx.file_symbols, callable_ctx)
    };
    let parser_info = infer_variable_hover_info_at_node_with_resolvers(
        variable_node,
        ctx.source,
        ctx.file_symbols,
        parser_usage_start,
        &variable_name,
        Some(&resolver),
        Some(&callable_param_resolver),
    );
    let type_hint = parser_info
        .as_ref()
        .and_then(|info| {
            info.phpdoc_comment
                .as_ref()
                .and_then(|_| local_variable_type_from_hover_info(info, ctx.file_symbols, true))
        })
        .or_else(|| rhs_node.and_then(|rhs| local_variable_inlay_type_from_expression(ctx, rhs)))
        .or_else(|| foreach_variable_inlay_type_from_index(ctx, variable_node))
        .or_else(|| {
            parser_info
                .as_ref()
                .and_then(|info| local_variable_type_from_hover_info(info, ctx.file_symbols, true))
        });
    let phpdoc_comment = parser_info.and_then(|info| info.phpdoc_comment);

    if type_hint.is_none() && phpdoc_comment.is_none() {
        return None;
    }

    Some(LocalVariableHoverData {
        variable_name,
        type_hint,
        phpdoc_comment,
    })
}

fn current_assignment_rhs_for_variable<'tree>(
    variable_node: tree_sitter::Node<'tree>,
    source: &str,
) -> Option<tree_sitter::Node<'tree>> {
    let assignment = variable_node.parent()?;
    if assignment.kind() != "assignment_expression" {
        return None;
    }
    let left = assignment.child_by_field_name("left")?;
    let right = assignment.child_by_field_name("right")?;
    (left.id() == variable_node.id() && is_plain_assignment_expression(left, right, source))
        .then_some(right)
}

fn latest_assignment_rhs_before_usage<'tree>(
    node: tree_sitter::Node<'tree>,
    variable_name: &str,
    usage_start: usize,
    source: &str,
) -> Option<(usize, tree_sitter::Node<'tree>)> {
    let mut best = None;
    collect_latest_assignment_rhs_before_usage(
        node,
        variable_name,
        usage_start,
        source,
        &mut best,
        true,
    );
    best
}

fn collect_latest_assignment_rhs_before_usage<'tree>(
    node: tree_sitter::Node<'tree>,
    variable_name: &str,
    usage_start: usize,
    source: &str,
    best: &mut Option<(usize, tree_sitter::Node<'tree>)>,
    is_scope_root: bool,
) {
    if node.start_byte() > usage_start {
        return;
    }
    if !is_scope_root && is_variable_inference_scope_boundary_for_hover(node) {
        return;
    }

    if let Some(rhs) = assignment_rhs_for_variable_node(node, variable_name, source)
        .filter(|rhs| rhs.end_byte() <= usage_start)
    {
        let candidate = (node.start_byte(), rhs);
        if best
            .as_ref()
            .is_none_or(|(best_start, _)| candidate.0 >= *best_start)
        {
            *best = Some(candidate);
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.start_byte() > usage_start {
            break;
        }
        collect_latest_assignment_rhs_before_usage(
            child,
            variable_name,
            usage_start,
            source,
            best,
            false,
        );
    }
}

fn assignment_rhs_for_variable_node<'tree>(
    node: tree_sitter::Node<'tree>,
    variable_name: &str,
    source: &str,
) -> Option<tree_sitter::Node<'tree>> {
    if node.kind() != "assignment_expression" {
        return None;
    }
    let left = node.child_by_field_name("left")?;
    let right = node.child_by_field_name("right")?;
    if left.kind() != "variable_name"
        || variable_text_for_node(source, left).as_deref() != Some(variable_name)
        || !is_plain_assignment_expression(left, right, source)
    {
        return None;
    }
    Some(right)
}

fn local_variable_scope_node(mut node: tree_sitter::Node) -> tree_sitter::Node {
    loop {
        if matches!(
            node.kind(),
            "method_declaration" | "function_definition" | "anonymous_function"
        ) {
            return node;
        }
        let Some(parent) = node.parent() else {
            return node;
        };
        node = parent;
    }
}

fn is_variable_inference_scope_boundary_for_hover(node: tree_sitter::Node) -> bool {
    matches!(
        node.kind(),
        "method_declaration"
            | "function_definition"
            | "arrow_function"
            | "anonymous_function"
            | "anonymous_function_creation_expression"
            | "class_declaration"
            | "interface_declaration"
            | "trait_declaration"
            | "enum_declaration"
    )
}

fn local_variable_type_from_hover_info(
    info: &php_lsp_parser::resolve::VariableHoverInfo,
    file_symbols: &php_lsp_types::FileSymbols,
    allow_scalar: bool,
) -> Option<LocalVariableInlayType> {
    let display = info
        .type_display
        .as_deref()
        .or(info.resolved_type_fqn.as_deref())?
        .trim();
    if display.is_empty() || (!allow_scalar && !is_useful_local_variable_type_hint(display)) {
        return None;
    }

    let target_fqn = info.resolved_type_fqn.as_ref().and_then(|fqn| {
        type_display_has_single_object_target(display).then(|| {
            fqn.trim_start_matches('\\')
                .trim_start_matches('?')
                .to_string()
        })
    });

    Some(LocalVariableInlayType {
        display: shorten_inlay_type_display(display, file_symbols),
        target_fqn,
    })
}

fn local_variable_inlay_type_from_type_info(
    ctx: &InlayHintContext<'_>,
    owner_fqn: &str,
    uri: &str,
    type_info: &php_lsp_types::TypeInfo,
    allow_scalar: bool,
) -> Option<LocalVariableInlayType> {
    if !is_explicit_local_variable_type_hint(type_info) {
        return None;
    }

    let display =
        local_variable_type_info_display(ctx.index, owner_fqn, uri, type_info, ctx.file_symbols);
    if display.trim().is_empty()
        || (!allow_scalar && !is_useful_local_variable_type_hint(display.as_str()))
    {
        return None;
    }

    Some(LocalVariableInlayType {
        display,
        target_fqn: single_inlay_target_fqn_from_type_info(ctx.index, owner_fqn, uri, type_info),
    })
}

fn local_variable_type_info_display(
    index: &WorkspaceIndex,
    owner_fqn: &str,
    uri: &str,
    type_info: &php_lsp_types::TypeInfo,
    file_symbols: &php_lsp_types::FileSymbols,
) -> String {
    match type_info {
        php_lsp_types::TypeInfo::Simple(name) => {
            local_variable_simple_type_display(index, owner_fqn, uri, name, file_symbols)
        }
        php_lsp_types::TypeInfo::Generic { base, args } => {
            let base =
                local_variable_simple_type_display(index, owner_fqn, uri, base, file_symbols);
            let args = args
                .iter()
                .map(|arg| {
                    local_variable_type_info_display(index, owner_fqn, uri, arg, file_symbols)
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("{base}<{args}>")
        }
        php_lsp_types::TypeInfo::Union(types) => types
            .iter()
            .map(|type_info| {
                local_variable_type_info_display(index, owner_fqn, uri, type_info, file_symbols)
            })
            .collect::<Vec<_>>()
            .join("|"),
        php_lsp_types::TypeInfo::Intersection(types) => types
            .iter()
            .map(|type_info| {
                local_variable_type_info_display(index, owner_fqn, uri, type_info, file_symbols)
            })
            .collect::<Vec<_>>()
            .join("&"),
        php_lsp_types::TypeInfo::Nullable(inner) => {
            format!(
                "?{}",
                local_variable_type_info_display(index, owner_fqn, uri, inner, file_symbols)
            )
        }
        php_lsp_types::TypeInfo::Conditional {
            if_type, else_type, ..
        } => [if_type.as_ref(), else_type.as_ref()]
            .into_iter()
            .map(|type_info| {
                local_variable_type_info_display(index, owner_fqn, uri, type_info, file_symbols)
            })
            .collect::<Vec<_>>()
            .join("|"),
        php_lsp_types::TypeInfo::Self_ | php_lsp_types::TypeInfo::Static_ => {
            shorten_inlay_type_display(owner_fqn, file_symbols)
        }
        php_lsp_types::TypeInfo::Parent_ => "parent".to_string(),
        php_lsp_types::TypeInfo::ArrayShape(_)
        | php_lsp_types::TypeInfo::ObjectShape(_)
        | php_lsp_types::TypeInfo::Callable { .. }
        | php_lsp_types::TypeInfo::ClassString(_)
        | php_lsp_types::TypeInfo::LiteralString(_)
        | php_lsp_types::TypeInfo::LiteralInt(_)
        | php_lsp_types::TypeInfo::LiteralFloat(_)
        | php_lsp_types::TypeInfo::LiteralBool(_)
        | php_lsp_types::TypeInfo::LiteralNull
        | php_lsp_types::TypeInfo::Void
        | php_lsp_types::TypeInfo::Never
        | php_lsp_types::TypeInfo::Mixed => type_info.to_string(),
    }
}

fn local_variable_simple_type_display(
    index: &WorkspaceIndex,
    owner_fqn: &str,
    uri: &str,
    name: &str,
    file_symbols: &php_lsp_types::FileSymbols,
) -> String {
    let name = name.trim();
    let lower = name.trim_start_matches('\\').to_ascii_lowercase();
    if matches!(lower.as_str(), "self" | "static") && !owner_fqn.is_empty() {
        return shorten_inlay_type_display(owner_fqn, file_symbols);
    }
    if lower == "parent" {
        return "parent".to_string();
    }
    if is_builtin_type_name(name) {
        return name.trim_start_matches('\\').to_string();
    }

    simple_type_fqn_from_owner_or_index(index, owner_fqn, uri, name)
        .map(|fqn| shorten_inlay_type_display(&fqn, file_symbols))
        .unwrap_or_else(|| shorten_inlay_type_display(name, file_symbols))
}

fn single_inlay_target_fqn_from_type_info(
    index: &WorkspaceIndex,
    owner_fqn: &str,
    uri: &str,
    type_info: &php_lsp_types::TypeInfo,
) -> Option<String> {
    match type_info {
        php_lsp_types::TypeInfo::Simple(name) => {
            let lower = name.trim_start_matches('\\').to_ascii_lowercase();
            if matches!(lower.as_str(), "self" | "static") && !owner_fqn.is_empty() {
                return Some(owner_fqn.trim_start_matches('\\').to_string());
            }
            if lower == "parent" || is_builtin_type_name(name) {
                return None;
            }
            simple_type_fqn_from_owner_or_index(index, owner_fqn, uri, name)
        }
        php_lsp_types::TypeInfo::Nullable(inner) => {
            single_inlay_target_fqn_from_type_info(index, owner_fqn, uri, inner)
        }
        php_lsp_types::TypeInfo::Self_ | php_lsp_types::TypeInfo::Static_
            if !owner_fqn.is_empty() =>
        {
            Some(owner_fqn.trim_start_matches('\\').to_string())
        }
        _ => None,
    }
}

fn simple_type_fqn_from_owner_or_index(
    index: &WorkspaceIndex,
    owner_fqn: &str,
    uri: &str,
    type_name: &str,
) -> Option<String> {
    let type_name = type_name.trim();
    if type_name.is_empty()
        || type_name.starts_with('\\')
        || type_name.contains('\\')
        || is_builtin_type_name(type_name)
    {
        return simple_type_fqn_from_index(index, uri, type_name);
    }

    if let Some((owner_namespace, _)) = owner_fqn.rsplit_once('\\') {
        let candidate = format!("{owner_namespace}\\{type_name}");
        if index.resolve_fqn(&candidate).is_some() {
            return Some(candidate);
        }
    }

    simple_type_fqn_from_index(index, uri, type_name)
}

fn is_explicit_local_variable_type_hint(type_info: &php_lsp_types::TypeInfo) -> bool {
    match type_info {
        php_lsp_types::TypeInfo::Void
        | php_lsp_types::TypeInfo::Never
        | php_lsp_types::TypeInfo::Mixed
        | php_lsp_types::TypeInfo::LiteralNull => false,
        php_lsp_types::TypeInfo::Simple(name) => {
            let lower = name.trim_start_matches('\\').to_ascii_lowercase();
            !matches!(lower.as_str(), "mixed" | "void" | "never" | "null")
        }
        php_lsp_types::TypeInfo::Nullable(inner) => is_explicit_local_variable_type_hint(inner),
        php_lsp_types::TypeInfo::Union(types) | php_lsp_types::TypeInfo::Intersection(types) => {
            types.iter().any(is_explicit_local_variable_type_hint)
        }
        _ => true,
    }
}

fn type_display_has_single_object_target(display: &str) -> bool {
    let display = display.trim().trim_start_matches('?');
    !display.is_empty()
        && !display.contains(['<', '>', '{', '}', '|', '&', '(', ')', ',', ' '])
        && !is_scalar_local_variable_type_hint(display)
}

fn local_variable_inlay_label(
    ctx: &InlayHintContext<'_>,
    type_hint: &LocalVariableInlayType,
) -> InlayHintLabel {
    if let Some(location) = type_hint
        .target_fqn
        .as_deref()
        .and_then(|fqn| location_for_inlay_type_fqn(ctx.index, fqn))
    {
        let mut parts = vec![InlayHintLabelPart {
            value: ": ".to_string(),
            ..Default::default()
        }];
        let clickable_value = if let Some(rest) = type_hint.display.strip_prefix('?') {
            parts.push(InlayHintLabelPart {
                value: "?".to_string(),
                ..Default::default()
            });
            rest.to_string()
        } else {
            type_hint.display.clone()
        };

        parts.push(InlayHintLabelPart {
            value: clickable_value,
            tooltip: type_hint
                .target_fqn
                .as_ref()
                .map(|fqn| InlayHintLabelPartTooltip::String(fqn.clone())),
            location: Some(location),
            command: None,
        });

        return InlayHintLabel::LabelParts(parts);
    }

    InlayHintLabel::String(format!(": {}", type_hint.display))
}

fn local_variable_inlay_tooltip(type_hint: &LocalVariableInlayType) -> String {
    let type_text = type_hint
        .target_fqn
        .as_deref()
        .unwrap_or(type_hint.display.as_str());
    format!("Inferred local variable type: {type_text}")
}

fn local_variable_type_markdown(
    index: &WorkspaceIndex,
    type_hint: &LocalVariableInlayType,
) -> String {
    let Some(target_fqn) = type_hint.target_fqn.as_deref() else {
        return markdown_code_span(&type_hint.display);
    };
    let Some(symbol) = index.resolve_fqn(target_fqn.trim_start_matches('\\')) else {
        return markdown_code_span(&type_hint.display);
    };
    let destination = markdown_file_location_destination(&symbol);
    if let Some(rest) = type_hint.display.strip_prefix('?') {
        return format!("?[{}](<{}>)", markdown_code_span(rest), destination);
    }
    format!(
        "[{}](<{}>)",
        markdown_code_span(&type_hint.display),
        destination
    )
}

fn magic_property_hover_markdown(
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    sym_at_pos: &SymbolAtPosition,
) -> Option<String> {
    if sym_at_pos.ref_kind != RefKind::PropertyAccess {
        return None;
    }
    let (class_fqn, member_name) = sym_at_pos.fqn.rsplit_once("::")?;
    let getter_fqn = format!("{class_fqn}::__get");
    let getter = index.resolve_fqn(&getter_fqn)?;
    if getter.kind != php_lsp_types::PhpSymbolKind::Method {
        return None;
    }
    let return_type = symbol_effective_return_type(&getter)?;
    if !is_explicit_local_variable_type_hint(&return_type) {
        return None;
    }

    let owner_fqn = getter.parent_fqn.as_deref().unwrap_or(class_fqn);
    let display =
        local_variable_type_info_display(index, owner_fqn, &getter.uri, &return_type, file_symbols);
    if display.trim().is_empty() {
        return None;
    }

    let type_hint = LocalVariableInlayType {
        display: display.clone(),
        target_fqn: single_inlay_target_fqn_from_type_info(
            index,
            owner_fqn,
            &getter.uri,
            &return_type,
        ),
    };

    let mut content = String::new();
    content.push_str("```php\n");
    content.push_str("property ");
    content.push_str(class_fqn);
    content.push_str("::");
    content.push_str(member_name);
    content.push_str(": ");
    content.push_str(&display);
    content.push_str("\n```\n");
    content.push_str("\n**Type:** ");
    content.push_str(&local_variable_type_markdown(index, &type_hint));
    content.push('\n');
    Some(content)
}

fn markdown_file_location_destination(symbol: &php_lsp_types::SymbolInfo) -> String {
    let line = symbol.selection_range.0.saturating_add(1);
    format!("{}#L{}", symbol.uri, line)
}

fn markdown_code_span(text: &str) -> String {
    if text.contains('`') {
        format!("`` {} ``", text)
    } else {
        format!("`{}`", text)
    }
}

fn location_for_inlay_type_fqn(index: &WorkspaceIndex, fqn: &str) -> Option<Location> {
    let symbol = index.resolve_fqn(fqn.trim_start_matches('\\'))?;
    if !matches!(
        symbol.kind,
        php_lsp_types::PhpSymbolKind::Class
            | php_lsp_types::PhpSymbolKind::Interface
            | php_lsp_types::PhpSymbolKind::Trait
            | php_lsp_types::PhpSymbolKind::Enum
    ) {
        return None;
    }
    Some(Location::new(
        symbol.uri.parse::<Uri>().ok()?,
        range_from_tuple(symbol.selection_range),
    ))
}

fn shorten_inlay_type_display(display: &str, file_symbols: &php_lsp_types::FileSymbols) -> String {
    if !display.contains('\\')
        || display.contains(['<', '>', '{', '}', '|', '&', '?', '(', ')', ',', ' '])
    {
        return display.to_string();
    }

    if let Some(use_stmt) = file_symbols
        .use_statements
        .iter()
        .find(|use_stmt| use_stmt.kind == php_lsp_types::UseKind::Class && use_stmt.fqn == display)
    {
        return use_stmt
            .alias
            .clone()
            .unwrap_or_else(|| display.rsplit('\\').next().unwrap_or(display).to_string());
    }

    if let Some(namespace) = file_symbols.namespace.as_deref() {
        if let Some(rest) = display
            .strip_prefix(namespace)
            .and_then(|rest| rest.strip_prefix('\\'))
        {
            return rest.to_string();
        }
    }

    display.rsplit('\\').next().unwrap_or(display).to_string()
}

fn is_useful_local_variable_type_hint(display: &str) -> bool {
    let display = display.trim();
    if display.is_empty() {
        return false;
    }

    if display.contains('<') || display.contains('{') || display.contains('\\') {
        return true;
    }
    if display.contains('|') {
        return display.split('|').any(is_useful_local_variable_type_hint);
    }
    if display.contains('&') {
        return display.split('&').any(is_useful_local_variable_type_hint);
    }

    !is_scalar_local_variable_type_hint(display.trim_start_matches('?'))
}

fn is_scalar_local_variable_type_hint(display: &str) -> bool {
    matches!(
        display
            .trim_start_matches('\\')
            .to_ascii_lowercase()
            .as_str(),
        "array"
            | "bool"
            | "boolean"
            | "callable"
            | "false"
            | "float"
            | "int"
            | "integer"
            | "iterable"
            | "mixed"
            | "never"
            | "null"
            | "object"
            | "resource"
            | "scalar"
            | "string"
            | "true"
            | "void"
    )
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
    document_version: Option<i32>,
) -> Vec<Diagnostic> {
    if let Some(parser) = open_files.get(uri_str) {
        compute_source_diagnostics_on_dedicated_stack(
            uri_str.to_string(),
            parser.source(),
            index.clone(),
            diagnostics_mode,
            diagnostic_severity,
            php_version,
            document_version,
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
    document_version: Option<i32>,
) -> Vec<Diagnostic> {
    let thread_name = format!("php-lsp-diagnostics:{uri_str}");
    let handle = match std::thread::Builder::new()
        .name(thread_name)
        .stack_size(DIAGNOSTIC_THREAD_STACK_SIZE)
        .spawn(move || {
            let mut parser = FileParser::new();
            parser.parse_full(&source);
            compute_diagnostics_with_config_for_version(
                &uri_str,
                &parser,
                &index,
                diagnostics_mode,
                diagnostic_severity,
                php_version,
                document_version,
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

pub(crate) fn compute_diagnostics_with_config(
    uri_str: &str,
    parser: &FileParser,
    index: &WorkspaceIndex,
    diagnostics_mode: DiagnosticsMode,
    diagnostic_severity: DiagnosticSeverityConfig,
    php_version: PhpVersion,
) -> Vec<Diagnostic> {
    compute_diagnostics_with_config_for_version(
        uri_str,
        parser,
        index,
        diagnostics_mode,
        diagnostic_severity,
        php_version,
        None,
    )
}

fn compute_diagnostics_with_config_for_version(
    uri_str: &str,
    parser: &FileParser,
    index: &WorkspaceIndex,
    diagnostics_mode: DiagnosticsMode,
    diagnostic_severity: DiagnosticSeverityConfig,
    php_version: PhpVersion,
    document_version: Option<i32>,
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
    let type_cache = RequestTypeCache::new(uri_str, document_version);
    let framework_cache = crate::framework::FrameworkProviderCache::default();

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
            member_access_diagnostics(
                uri_str,
                tree,
                &source,
                &file_symbols,
                index,
                &utf16_index,
                &type_cache,
                &framework_cache,
            ),
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
            type_compatibility_diagnostics(
                tree,
                &source,
                &file_symbols,
                index,
                &utf16_index,
                &type_cache,
            ),
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

#[allow(clippy::too_many_arguments)]
fn member_access_diagnostics(
    uri_str: &str,
    tree: &tree_sitter::Tree,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
    type_cache: &RequestTypeCache,
    framework_cache: &crate::framework::FrameworkProviderCache,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    walk_member_access_diagnostics(
        tree,
        tree.root_node(),
        uri_str,
        source,
        file_symbols,
        index,
        utf16_index,
        type_cache,
        framework_cache,
        &mut diagnostics,
    );
    diagnostics
}

#[allow(clippy::too_many_arguments)]
fn walk_member_access_diagnostics(
    tree: &tree_sitter::Tree,
    node: tree_sitter::Node,
    uri_str: &str,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
    type_cache: &RequestTypeCache,
    framework_cache: &crate::framework::FrameworkProviderCache,
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
            uri_str,
            source,
            file_symbols,
            index,
            utf16_index,
            type_cache,
            framework_cache,
            diagnostics,
        );
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_member_access_diagnostics(
            tree,
            child,
            uri_str,
            source,
            file_symbols,
            index,
            utf16_index,
            type_cache,
            framework_cache,
            diagnostics,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn check_member_access_node(
    tree: &tree_sitter::Tree,
    node: tree_sitter::Node,
    uri_str: &str,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
    type_cache: &RequestTypeCache,
    framework_cache: &crate::framework::FrameworkProviderCache,
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
        type_cache.cached_string(
            (0, 0, 0, 0),
            "member-type",
            format!("{class_fqn}::{member_name}"),
            || resolve_member_type_from_index(index, class_fqn, member_name),
        )
    };
    let callable_param_resolver = |ctx: CallableParameterContext<'_>| {
        resolve_callable_parameter_type_from_index(index, file_symbols, ctx)
    };
    let Some(sym_at_pos) = symbol_at_position_with_request_cache(
        type_cache,
        tree,
        source,
        pos.row as u32,
        pos.column as u32,
        file_symbols,
        "diagnostic-member-access",
        Some(&member_type_resolver),
        Some(&callable_param_resolver),
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
            || is_phpunit_test_double_api_call(
                tree,
                source,
                file_symbols,
                index,
                type_cache,
                &sym_at_pos,
            )
            || is_missing_parent_constructor_call(&sym_at_pos)
            || is_enum_builtin_method_call(index, &sym_at_pos)
            || is_dynamic_member_access(
                index,
                file_symbols,
                uri_str,
                source,
                &sym_at_pos,
                framework_cache,
            )
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
    type_cache: &RequestTypeCache,
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
            .or_else(|| {
                type_cache.cached_string(
                    (0, 0, 0, 0),
                    "member-type",
                    format!("{class_fqn}::{member_name}"),
                    || resolve_member_type_from_index(index, class_fqn, member_name),
                )
            })
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
    uri_str: &str,
    source: &str,
    sym_at_pos: &SymbolAtPosition,
    framework_cache: &crate::framework::FrameworkProviderCache,
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

    let framework_query = crate::framework::VirtualMemberQuery::from_ref_kind(
        class_fqn,
        member_name,
        sym_at_pos.ref_kind,
    );
    let framework_ctx = crate::framework::FrameworkProviderContext::new(index)
        .with_source_uri(Some(uri_str))
        .with_workspace(None, None)
        .with_file(Some(file_symbols), Some(source))
        .with_relevant_files(&[]);
    let framework_registry = crate::framework::default_framework_provider_registry();

    if sym_at_pos.ref_kind == RefKind::MethodCall {
        return framework_query.is_some_and(|query| {
            framework_cache.has_virtual_member(&framework_registry, &framework_ctx, &query)
        }) || is_unindexed_imported_type(index, file_symbols, class_fqn);
    }

    if sym_at_pos.ref_kind != RefKind::PropertyAccess {
        return false;
    }

    if fqn_matches(class_fqn, "stdClass")
        || class_has_magic_get(index, class_fqn)
        || is_phpunit_test_double_type(class_fqn)
    {
        return true;
    }

    if framework_query.is_some_and(|query| {
        framework_cache.has_virtual_member(&framework_registry, &framework_ctx, &query)
    }) {
        return true;
    }

    let bare_member_name = member_name.strip_prefix('$').unwrap_or(member_name);
    matches!(bare_member_name, "name" | "value")
        && index
            .types
            .get(class_fqn.trim_start_matches('\\'))
            .is_some_and(|sym| sym.kind == php_lsp_types::PhpSymbolKind::Enum)
}

fn class_has_magic_get(index: &WorkspaceIndex, class_fqn: &str) -> bool {
    index
        .resolve_fqn(&format!("{}::__get", class_fqn.trim_start_matches('\\')))
        .is_some_and(|symbol| symbol.kind == php_lsp_types::PhpSymbolKind::Method)
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

pub(crate) fn lazy_resolvable_diagnostic_fqn(message: &str) -> Option<String> {
    for prefix in [
        "Unresolved use statement: ",
        "Unknown class: ",
        "Unknown method: ",
        "Unknown class constant: ",
    ] {
        if let Some(fqn) = message.strip_prefix(prefix) {
            let fqn = fqn.trim();
            if fqn.contains('\\') || fqn.contains("::") {
                return Some(fqn.to_string());
            }
        }
    }

    None
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
    type_cache: &RequestTypeCache,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    walk_type_compatibility_diagnostics(
        tree,
        tree.root_node(),
        source,
        file_symbols,
        index,
        utf16_index,
        type_cache,
        &mut diagnostics,
    );
    diagnostics
}

#[allow(clippy::too_many_arguments)]
fn walk_type_compatibility_diagnostics(
    tree: &tree_sitter::Tree,
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
    type_cache: &RequestTypeCache,
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
            type_cache,
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
                type_cache,
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
            type_cache,
            diagnostics,
        ),
        "return_statement" => check_return_type_compatibility(
            node,
            source,
            file_symbols,
            index,
            utf16_index,
            type_cache,
            diagnostics,
        ),
        "assignment_expression" => check_property_assignment_type_compatibility(
            tree,
            node,
            source,
            file_symbols,
            index,
            utf16_index,
            type_cache,
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
            type_cache,
            diagnostics,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn check_function_call_type_compatibility(
    tree: &tree_sitter::Tree,
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
    type_cache: &RequestTypeCache,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(name_node) = node
        .child_by_field_name("function")
        .or_else(|| node.named_child(0))
    else {
        return;
    };
    let Some((_, sym)) = resolve_reference_symbol_at_node_cached(
        tree,
        source,
        name_node,
        file_symbols,
        index,
        type_cache,
    ) else {
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
            type_cache,
            diagnostics,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn check_member_call_type_compatibility(
    tree: &tree_sitter::Tree,
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
    type_cache: &RequestTypeCache,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(name_node) = member_reference_name_node(node) else {
        return;
    };
    let Some((_, sym)) = resolve_reference_symbol_at_node_cached(
        tree,
        source,
        name_node,
        file_symbols,
        index,
        type_cache,
    ) else {
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
            type_cache,
            diagnostics,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn check_constructor_type_compatibility(
    tree: &tree_sitter::Tree,
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
    type_cache: &RequestTypeCache,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(name_node) = object_creation_class_node(node) else {
        return;
    };
    let Some((_, sym)) = resolve_reference_symbol_at_node_cached(
        tree,
        source,
        name_node,
        file_symbols,
        index,
        type_cache,
    ) else {
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
            type_cache,
            diagnostics,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn check_call_argument_types(
    call_node: tree_sitter::Node,
    callable: &php_lsp_types::SymbolInfo,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
    type_cache: &RequestTypeCache,
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
        let Some(actual) =
            infer_expression_type_cached(arg.value_node, source, file_symbols, type_cache)
        else {
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
    type_cache: &RequestTypeCache,
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
    let Some(actual) = infer_expression_type_cached(expr_node, source, file_symbols, type_cache)
    else {
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

#[allow(clippy::too_many_arguments)]
fn check_property_assignment_type_compatibility(
    tree: &tree_sitter::Tree,
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
    type_cache: &RequestTypeCache,
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
    let Some((_, property)) = resolve_reference_symbol_at_node_cached(
        tree,
        source,
        name_node,
        file_symbols,
        index,
        type_cache,
    ) else {
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
    let Some(actual) = infer_expression_type_cached(right_node, source, file_symbols, type_cache)
    else {
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
    let callable_param_resolver =
        |ctx: CallableParameterContext<'_>| -> Option<php_lsp_types::TypeInfo> {
            resolve_callable_parameter_type_from_index(index, file_symbols, ctx)
        };
    let sym_at_pos = symbol_at_position_with_resolvers(
        tree,
        source,
        pos.row as u32,
        pos.column as u32,
        file_symbols,
        Some(&member_type_resolver),
        Some(&callable_param_resolver),
    )?;
    let resolved = resolve_symbol_at_position_from_index(index, &sym_at_pos)?;
    Some((sym_at_pos, resolved))
}

#[allow(clippy::too_many_arguments)]
fn symbol_at_position_with_request_cache(
    type_cache: &RequestTypeCache,
    tree: &tree_sitter::Tree,
    source: &str,
    line: u32,
    byte_col: u32,
    file_symbols: &php_lsp_types::FileSymbols,
    expected_context: &'static str,
    member_type_resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
) -> Option<SymbolAtPosition> {
    type_cache.cached_symbol(
        line,
        byte_col,
        "symbol-at-position",
        expected_context,
        || {
            symbol_at_position_with_resolvers(
                tree,
                source,
                line,
                byte_col,
                file_symbols,
                member_type_resolver,
                callable_resolver,
            )
        },
    )
}

fn resolve_reference_symbol_at_node_cached(
    tree: &tree_sitter::Tree,
    source: &str,
    node: tree_sitter::Node,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    type_cache: &RequestTypeCache,
) -> Option<(SymbolAtPosition, Arc<php_lsp_types::SymbolInfo>)> {
    let pos = node.start_position();
    let member_type_resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
        type_cache.cached_string(
            (0, 0, 0, 0),
            "member-type",
            format!("{class_fqn}::{member_name}"),
            || resolve_member_type_from_index(index, class_fqn, member_name),
        )
    };
    let callable_param_resolver =
        |ctx: CallableParameterContext<'_>| -> Option<php_lsp_types::TypeInfo> {
            resolve_callable_parameter_type_from_index(index, file_symbols, ctx)
        };
    let sym_at_pos = symbol_at_position_with_request_cache(
        type_cache,
        tree,
        source,
        pos.row as u32,
        pos.column as u32,
        file_symbols,
        "reference-symbol",
        Some(&member_type_resolver),
        Some(&callable_param_resolver),
    )?;
    let resolved = resolve_symbol_at_position_from_index(index, &sym_at_pos)?;
    Some((sym_at_pos, resolved))
}

fn resolve_symbol_at_position_from_index(
    index: &WorkspaceIndex,
    sym_at_pos: &SymbolAtPosition,
) -> Option<Arc<php_lsp_types::SymbolInfo>> {
    if let Some(symbol) = index.resolve_fqn(&sym_at_pos.fqn) {
        return Some(symbol);
    }

    if matches!(
        sym_at_pos.ref_kind,
        RefKind::FunctionCall | RefKind::GlobalConstant
    ) {
        if let Some((_, short_name)) = sym_at_pos.fqn.rsplit_once('\\') {
            return index.resolve_fqn(short_name);
        }
    }

    None
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

fn infer_expression_type_cached(
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    type_cache: &RequestTypeCache,
) -> Option<InferredExprType> {
    let normalized = normalized_expression_node(node);
    type_cache.cached_inferred_expr(
        node_range_node(normalized),
        "diagnostic-expression-type",
        normalized.kind(),
        || infer_expression_type(normalized, source, file_symbols),
    )
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
    if raw.starts_with('"') || raw.starts_with('\'') {
        return Some(InferredExprType {
            display: "string".to_string(),
            comparable: raw.to_string(),
            range,
        });
    }
    if kind.contains("string") {
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
        return Some(InferredExprType {
            display: "int".to_string(),
            comparable: raw.to_string(),
            range,
        });
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
        php_lsp_types::TypeInfo::Conditional {
            if_type, else_type, ..
        } => {
            type_info_accepts_inferred_type(if_type, actual, file_symbols, index)
                || type_info_accepts_inferred_type(else_type, actual, file_symbols, index)
        }
        php_lsp_types::TypeInfo::Intersection(_) => true,
        php_lsp_types::TypeInfo::Simple(name) => {
            simple_type_accepts_inferred_type(name, actual, file_symbols, index)
        }
        php_lsp_types::TypeInfo::Generic { base, .. } => {
            simple_type_accepts_inferred_type(base, actual, file_symbols, index)
        }
        php_lsp_types::TypeInfo::ArrayShape(_) => actual.comparable == "array",
        php_lsp_types::TypeInfo::ObjectShape(_) => actual.comparable == "object",
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
    let actual_display_lower = actual.display.trim_start_matches('\\').to_ascii_lowercase();

    match expected_lower.as_str() {
        "mixed" => true,
        "string" => {
            actual_lower == "string"
                || actual_display_lower == "string"
                || inferred_string_literal_inner(&actual.comparable).is_some()
        }
        "non-empty-string" => {
            actual_lower == "non-empty-string"
                || inferred_string_literal_inner(&actual.comparable)
                    .is_some_and(|inner| !inner.is_empty())
        }
        "literal-string" => {
            actual_lower == "literal-string"
                || inferred_string_literal_inner(&actual.comparable).is_some()
        }
        "int" => actual_lower == "int" || actual_lower.parse::<i64>().is_ok(),
        "positive-int" => actual_lower.parse::<i64>().is_ok_and(|value| value > 0),
        "negative-int" => actual_lower.parse::<i64>().is_ok_and(|value| value < 0),
        "non-negative-int" => actual_lower.parse::<i64>().is_ok_and(|value| value >= 0),
        "non-positive-int" => actual_lower.parse::<i64>().is_ok_and(|value| value <= 0),
        "non-zero-int" => actual_lower.parse::<i64>().is_ok_and(|value| value != 0),
        "float" => {
            actual_lower == "float" || actual_lower == "int" || actual_lower.parse::<i64>().is_ok()
        }
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

fn inferred_string_literal_inner(raw: &str) -> Option<&str> {
    let raw = raw.trim();
    let quote = raw.as_bytes().first().copied()?;
    if quote != b'\'' && quote != b'"' {
        return None;
    }
    if raw.as_bytes().last().copied() != Some(quote) || raw.len() < 2 {
        return None;
    }
    raw.get(1..raw.len() - 1)
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
                    && !is_phpdoc_virtual_method_symbol(sym, class_sym)
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

fn is_phpdoc_virtual_method_symbol(
    method: &php_lsp_types::SymbolInfo,
    owner: &php_lsp_types::SymbolInfo,
) -> bool {
    method.kind == php_lsp_types::PhpSymbolKind::Method
        && method.parent_fqn.as_deref() == Some(owner.fqn.as_str())
        && method.doc_comment.as_deref() == owner.doc_comment.as_deref()
        && method
            .doc_comment
            .as_deref()
            .is_some_and(|doc| doc.contains("@method"))
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
    index: &WorkspaceIndex,
) -> bool {
    match (child_type, parent_type) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(child_type), Some(parent_type)) => {
            type_info_is_mixed(child_type)
                || type_info_is_owner_template(parent_type, parent_owner_fqn, index)
                || normalized_type_info_for_override(
                    child_type,
                    child_file_symbols,
                    child_owner_fqn,
                ) == normalized_type_info_for_override(
                    parent_type,
                    parent_file_symbols,
                    parent_owner_fqn,
                )
                || type_info_refines_native(parent_type, child_type)
                || type_info_refines_native(child_type, parent_type)
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
    if type_info_refines_native(parent_type, child_type)
        || type_info_refines_native(child_type, parent_type)
    {
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

fn type_info_is_owner_template(
    type_info: &php_lsp_types::TypeInfo,
    owner_fqn: Option<&str>,
    index: &WorkspaceIndex,
) -> bool {
    let Some(owner_fqn) = owner_fqn else {
        return false;
    };
    let php_lsp_types::TypeInfo::Simple(name) = type_info else {
        return false;
    };

    index
        .types
        .get(owner_fqn.trim_start_matches('\\'))
        .is_some_and(|owner| {
            owner
                .templates
                .iter()
                .any(|template| template.name == *name)
        })
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
        php_lsp_types::TypeInfo::Conditional {
            if_type, else_type, ..
        } => format!(
            "conditional({}|{})",
            normalized_type_info_for_override(if_type, file_symbols, owner_fqn),
            normalized_type_info_for_override(else_type, file_symbols, owner_fqn)
        ),
        php_lsp_types::TypeInfo::ArrayShape(_)
        | php_lsp_types::TypeInfo::ObjectShape(_)
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

#[derive(Debug, Clone)]
struct FrameworkStringKeyAtPosition {
    domain: &'static str,
    prefix: String,
    key: String,
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

fn framework_virtual_member_for_symbol(
    index: &WorkspaceIndex,
    sym: &SymbolAtPosition,
    source_uri: Option<&str>,
    file_symbols: Option<&php_lsp_types::FileSymbols>,
    source: Option<&str>,
) -> Option<crate::framework::VirtualMember> {
    let (class_fqn, member_name) = sym.fqn.rsplit_once("::")?;
    let query =
        crate::framework::VirtualMemberQuery::from_ref_kind(class_fqn, member_name, sym.ref_kind)?;
    let ctx = crate::framework::FrameworkProviderContext::new(index)
        .with_source_uri(source_uri)
        .with_file(file_symbols, source)
        .with_relevant_files(&[]);
    let registry = crate::framework::default_framework_provider_registry();
    let cache = crate::framework::FrameworkProviderCache::default();
    cache
        .virtual_members(&registry, &ctx, &query)
        .into_iter()
        .next()
}

fn framework_virtual_member_candidates(
    index: &WorkspaceIndex,
    class_fqn: &str,
    source_uri: Option<&str>,
    file_symbols: Option<&php_lsp_types::FileSymbols>,
    source: Option<&str>,
    kind: Option<crate::framework::VirtualMemberKind>,
) -> Vec<crate::framework::VirtualMember> {
    let ctx = crate::framework::FrameworkProviderContext::new(index)
        .with_source_uri(source_uri)
        .with_file(file_symbols, source)
        .with_relevant_files(&[]);
    let registry = crate::framework::default_framework_provider_registry();
    registry.virtual_member_candidates(&ctx, class_fqn, kind)
}

fn framework_virtual_member_type_fqn(
    index: &WorkspaceIndex,
    class_fqn: &str,
    member_name: &str,
    source_uri: Option<&str>,
    file_symbols: Option<&php_lsp_types::FileSymbols>,
    source: Option<&str>,
) -> Option<String> {
    let kind = if member_name.starts_with('$') {
        crate::framework::VirtualMemberKind::Property
    } else {
        crate::framework::VirtualMemberKind::Method
    };
    let query = crate::framework::VirtualMemberQuery {
        owner_fqn: class_fqn.to_string(),
        member_name: member_name.to_string(),
        kind,
    };
    let ctx = crate::framework::FrameworkProviderContext::new(index)
        .with_source_uri(source_uri)
        .with_file(file_symbols, source)
        .with_relevant_files(&[]);
    let registry = crate::framework::default_framework_provider_registry();
    let cache = crate::framework::FrameworkProviderCache::default();
    let member = cache
        .virtual_members(&registry, &ctx, &query)
        .into_iter()
        .next()?;
    let type_info = member.type_info.as_ref()?;
    let uri = file_symbols
        .and_then(|symbols| symbols.symbols.first())
        .map(|symbol| symbol.uri.as_str())
        .or(source_uri)
        .unwrap_or("");
    type_info_fqn_from_index(index, class_fqn, uri, type_info)
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

fn framework_virtual_completion_data(item: &CompletionItem) -> Option<(&str, &str, &str)> {
    let data = item.data.as_ref()?;
    if data.get("kind")?.as_str()? != "framework-virtual-member" {
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

fn framework_virtual_member_detail(member: &crate::framework::VirtualMember) -> String {
    match member.kind {
        crate::framework::VirtualMemberKind::Property
        | crate::framework::VirtualMemberKind::StaticProperty => {
            let access = member
                .access
                .map(phpdoc_property_tag)
                .unwrap_or("@property");
            match member.type_info.as_ref() {
                Some(type_info) => format!("{} {}", access, type_info),
                None => access.to_string(),
            }
        }
        crate::framework::VirtualMemberKind::Method => member
            .type_info
            .as_ref()
            .map(|type_info| format!("(): {}", type_info))
            .unwrap_or_else(|| "()".to_string()),
        crate::framework::VirtualMemberKind::ClassConstant => "class constant".to_string(),
    }
}

fn framework_virtual_member_markdown(member: &crate::framework::VirtualMember) -> String {
    let mut content = String::new();
    content.push_str("```php\n");
    match member.kind {
        crate::framework::VirtualMemberKind::Property
        | crate::framework::VirtualMemberKind::StaticProperty => {
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
            content.push_str(member.name.trim_start_matches('$'));
        }
        crate::framework::VirtualMemberKind::Method => {
            content.push_str("@method ");
            if let Some(ref type_info) = member.type_info {
                content.push_str(&type_info.to_string());
                content.push(' ');
            }
            content.push_str(&member.name);
            content.push_str("()");
        }
        crate::framework::VirtualMemberKind::ClassConstant => {
            content.push_str("const ");
            content.push_str(&member.name);
        }
    }
    content.push_str("\n```\n");
    if let Some(ref detail) = member.detail {
        content.push_str("\n---\n\n");
        content.push_str(detail);
        content.push('\n');
    }
    content
}

fn framework_virtual_completion_item(
    member: &crate::framework::VirtualMember,
    member_prefix: &str,
) -> lsp_types::CompletionItem {
    let label = member.name.trim_start_matches('$').to_string();
    let rank = if label.starts_with(member_prefix) {
        "0"
    } else if label
        .to_ascii_lowercase()
        .contains(&member_prefix.to_ascii_lowercase())
    {
        "1"
    } else {
        "2"
    };

    lsp_types::CompletionItem {
        label: label.clone(),
        kind: Some(match member.kind {
            crate::framework::VirtualMemberKind::Method => lsp_types::CompletionItemKind::METHOD,
            crate::framework::VirtualMemberKind::Property
            | crate::framework::VirtualMemberKind::StaticProperty => {
                lsp_types::CompletionItemKind::PROPERTY
            }
            crate::framework::VirtualMemberKind::ClassConstant => {
                lsp_types::CompletionItemKind::CONSTANT
            }
        }),
        detail: Some(framework_virtual_member_detail(member)),
        documentation: Some(lsp_types::Documentation::MarkupContent(
            lsp_types::MarkupContent {
                kind: lsp_types::MarkupKind::Markdown,
                value: framework_virtual_member_markdown(member),
            },
        )),
        sort_text: Some(format!("2_{}_{}", rank, label.to_ascii_lowercase())),
        filter_text: Some(format!("{} {}", label, member.fqn)),
        data: Some(serde_json::json!({
            "kind": "framework-virtual-member",
            "ownerFqn": member.owner_fqn.as_str(),
            "memberKind": match member.kind {
                crate::framework::VirtualMemberKind::Method => "method",
                crate::framework::VirtualMemberKind::Property
                    | crate::framework::VirtualMemberKind::StaticProperty => "property",
                crate::framework::VirtualMemberKind::ClassConstant => "constant",
            },
            "memberName": member.name.as_str(),
        })),
        commit_characters: Some(match member.kind {
            crate::framework::VirtualMemberKind::Method => vec!["(".to_string()],
            _ => vec![";".to_string(), ",".to_string()],
        }),
        ..Default::default()
    }
}

fn framework_string_key_completion_item(
    key: &crate::framework::FrameworkStringKey,
    prefix: &str,
) -> lsp_types::CompletionItem {
    let insert_text = key
        .key
        .strip_prefix(prefix)
        .unwrap_or(key.key.as_str())
        .to_string();
    lsp_types::CompletionItem {
        label: key.key.clone(),
        kind: Some(lsp_types::CompletionItemKind::VALUE),
        detail: key.detail.clone(),
        insert_text: Some(insert_text),
        sort_text: Some(format!("1_{}", key.key.to_ascii_lowercase())),
        filter_text: Some(key.key.clone()),
        data: Some(serde_json::json!({
            "kind": "framework-string-key",
            "domain": key.provider_ids.first().copied().unwrap_or("framework"),
            "key": key.key.as_str(),
        })),
        ..Default::default()
    }
}

fn framework_string_key_completion_item_to_ls(
    mut item: lsp_types::CompletionItem,
) -> CompletionItem {
    CompletionItem {
        label: item.label,
        kind: item.kind.map(lsp_completion_kind_to_ls),
        detail: item.detail,
        sort_text: item.sort_text,
        filter_text: item.filter_text,
        insert_text: item.insert_text,
        insert_text_format: item.insert_text_format.map(lsp_insert_text_format_to_ls),
        commit_characters: item.commit_characters,
        tags: item.tags.take().map(|tags| {
            tags.into_iter()
                .filter_map(|tag| {
                    if tag == lsp_types::CompletionItemTag::DEPRECATED {
                        Some(CompletionItemTag::DEPRECATED)
                    } else {
                        None
                    }
                })
                .collect()
        }),
        data: item.data,
        ..Default::default()
    }
}

fn framework_string_key_source_location(
    key: &crate::framework::FrameworkStringKey,
) -> Option<Location> {
    let (uri, range) = key.sources.iter().find_map(|source| match source {
        crate::framework::VirtualMemberSource::SourceRange { uri, range } => {
            Some((uri.clone(), *range))
        }
        crate::framework::VirtualMemberSource::Synthetic { .. } => None,
    })?;
    Some(Location {
        uri: uri.parse::<Uri>().ok()?,
        range: Range {
            start: Position::new(range.0, range.1),
            end: Position::new(range.2, range.3),
        },
    })
}

fn framework_string_key_context_at_position(
    source: &str,
    line: u32,
    byte_col: u32,
) -> Option<FrameworkStringKeyAtPosition> {
    let offset = byte_offset_for_line_col(source, line, byte_col)?;
    let bounds = string_literal_bounds_at_offset(source, offset)?;
    let domain = framework_string_key_domain_before_string(source, bounds.quote_start)?;
    let prefix = source.get(bounds.content_start..offset)?.to_string();
    let key = source
        .get(bounds.content_start..bounds.content_end)
        .unwrap_or(prefix.as_str())
        .to_string();
    Some(FrameworkStringKeyAtPosition {
        domain,
        prefix,
        key,
    })
}

#[derive(Debug, Clone, Copy)]
struct StringLiteralBounds {
    quote_start: usize,
    content_start: usize,
    content_end: usize,
}

fn string_literal_bounds_at_offset(source: &str, offset: usize) -> Option<StringLiteralBounds> {
    let mut quote: Option<(char, usize)> = None;
    let mut escaped = false;
    for (idx, ch) in source.char_indices() {
        if idx >= offset {
            break;
        }
        if let Some((active_quote, _)) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active_quote {
                quote = None;
            }
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some((ch, idx));
        }
    }

    let (quote_char, quote_start) = quote?;
    let content_start = quote_start + quote_char.len_utf8();
    if offset < content_start {
        return None;
    }
    let content_end = find_unescaped_quote(source, offset, quote_char)
        .unwrap_or_else(|| line_end_offset(source, offset));
    Some(StringLiteralBounds {
        quote_start,
        content_start,
        content_end,
    })
}

fn find_unescaped_quote(source: &str, start: usize, quote: char) -> Option<usize> {
    let mut escaped = false;
    for (relative, ch) in source.get(start..)?.char_indices() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == quote {
            return Some(start + relative);
        } else if ch == '\n' {
            return None;
        }
    }
    None
}

fn framework_string_key_domain_before_string(
    source: &str,
    quote_start: usize,
) -> Option<&'static str> {
    let open_paren = previous_non_ws_char(source, quote_start)?;
    if source.as_bytes().get(open_paren).copied()? != b'(' {
        return None;
    }

    let name_end = previous_non_ws_char(source, open_paren)?;
    let name_start = scan_identifier_start(source, name_end + 1);
    let raw_name = source.get(name_start..=name_end)?.trim_start_matches('\\');
    let before_name = source.get(..name_start)?.trim_end();

    match raw_name {
        "config" => Some("config"),
        "route" => Some("route"),
        "view" => Some("view"),
        "render" | "renderView" => Some("twig"),
        "__" | "trans" | "trans_choice" => Some("translation"),
        "name" if before_name.ends_with("->") => Some("route"),
        "get" if before_name.ends_with("Lang::") => Some("translation"),
        "make" if before_name.ends_with("View::") => Some("view"),
        _ => None,
    }
}

fn previous_non_ws_char(source: &str, before: usize) -> Option<usize> {
    source
        .get(..before)?
        .char_indices()
        .rev()
        .find_map(|(idx, ch)| (!ch.is_whitespace()).then_some(idx))
}

fn scan_identifier_start(source: &str, end_exclusive: usize) -> usize {
    let mut start = end_exclusive;
    for (idx, ch) in source
        .get(..end_exclusive)
        .unwrap_or("")
        .char_indices()
        .rev()
    {
        if ch.is_alphanumeric() || ch == '_' || ch == '\\' {
            start = idx;
        } else {
            break;
        }
    }
    start
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShapeCompletionKind {
    ArrayKey,
    ObjectProperty,
}

fn shape_completion_items_from_type_info(
    type_info: &php_lsp_types::TypeInfo,
    kind: ShapeCompletionKind,
    prefix: &str,
) -> Vec<lsp_types::CompletionItem> {
    let mut shape_items = Vec::new();
    let mut seen = HashSet::new();
    collect_shape_completion_items(type_info, kind, &mut seen, &mut shape_items);

    let prefix_lower = prefix.to_ascii_lowercase();
    let mut completion_items = shape_items
        .into_iter()
        .filter(|item| {
            item.key
                .as_deref()
                .is_some_and(|key| key.to_ascii_lowercase().starts_with(&prefix_lower))
        })
        .filter_map(|item| {
            let key = item.key?;
            let detail = match kind {
                ShapeCompletionKind::ArrayKey => {
                    if item.optional {
                        format!("optional array shape key: {}", item.value)
                    } else {
                        format!("array shape key: {}", item.value)
                    }
                }
                ShapeCompletionKind::ObjectProperty => {
                    if item.optional {
                        format!("optional object shape property: {}", item.value)
                    } else {
                        format!("object shape property: {}", item.value)
                    }
                }
            };
            Some(lsp_types::CompletionItem {
                label: key.clone(),
                kind: Some(match kind {
                    ShapeCompletionKind::ArrayKey => lsp_types::CompletionItemKind::FIELD,
                    ShapeCompletionKind::ObjectProperty => lsp_types::CompletionItemKind::PROPERTY,
                }),
                detail: Some(detail),
                sort_text: Some(format!(
                    "01_{}_{}",
                    completion_prefix_rank_for_text(&key, prefix),
                    key.to_ascii_lowercase()
                )),
                filter_text: Some(key.clone()),
                insert_text: Some(key),
                commit_characters: Some(match kind {
                    ShapeCompletionKind::ArrayKey => vec!["'".to_string(), "\"".to_string()],
                    ShapeCompletionKind::ObjectProperty => {
                        vec!["(".to_string(), ";".to_string(), ",".to_string()]
                    }
                }),
                ..Default::default()
            })
        })
        .collect::<Vec<_>>();
    completion_items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text).then(a.label.cmp(&b.label)));
    completion_items
}

fn collect_shape_completion_items(
    type_info: &php_lsp_types::TypeInfo,
    kind: ShapeCompletionKind,
    seen: &mut HashSet<String>,
    out: &mut Vec<php_lsp_types::ArrayShapeItem>,
) {
    match type_info {
        php_lsp_types::TypeInfo::Nullable(inner) => {
            collect_shape_completion_items(inner, kind, seen, out);
        }
        php_lsp_types::TypeInfo::Union(types) | php_lsp_types::TypeInfo::Intersection(types) => {
            for ty in types {
                collect_shape_completion_items(ty, kind, seen, out);
            }
        }
        php_lsp_types::TypeInfo::Conditional {
            if_type, else_type, ..
        } => {
            collect_shape_completion_items(if_type, kind, seen, out);
            collect_shape_completion_items(else_type, kind, seen, out);
        }
        php_lsp_types::TypeInfo::ArrayShape(items) if kind == ShapeCompletionKind::ArrayKey => {
            collect_named_shape_items(items, seen, out);
        }
        php_lsp_types::TypeInfo::ObjectShape(items)
            if kind == ShapeCompletionKind::ObjectProperty =>
        {
            collect_named_shape_items(items, seen, out);
        }
        _ => {}
    }
}

fn collect_named_shape_items(
    items: &[php_lsp_types::ArrayShapeItem],
    seen: &mut HashSet<String>,
    out: &mut Vec<php_lsp_types::ArrayShapeItem>,
) {
    for item in items {
        let Some(key) = item.key.as_ref() else {
            continue;
        };
        if seen.insert(normalize_shape_key_text(key)) {
            out.push(item.clone());
        }
    }
}

fn completion_prefix_rank_for_text(label: &str, prefix: &str) -> u8 {
    if prefix.is_empty() {
        return 0;
    }
    let label = label.to_ascii_lowercase();
    let prefix = prefix.to_ascii_lowercase();
    if label == prefix {
        0
    } else if label.starts_with(&prefix) {
        1
    } else {
        2
    }
}

fn normalize_shape_key_text(key: &str) -> String {
    key.trim()
        .trim_end_matches('?')
        .trim_matches(|ch| ch == '\'' || ch == '"')
        .to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShapeDefinitionKind {
    Array,
    Object,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShapePathSegment {
    key: String,
    kind: ShapeDefinitionKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShapeDefinitionAccess {
    root_var: String,
    segments: Vec<ShapePathSegment>,
}

fn shape_definition_at_position(
    source: &str,
    line: u32,
    byte_col: u32,
) -> Option<(u32, u32, u32, u32)> {
    let usage_byte = line_col_to_byte_offset(source, line, byte_col)?;
    let access = array_shape_key_access_at_position(source, line, byte_col)
        .or_else(|| object_shape_property_access_at_position(source, line, byte_col))?;
    phpdoc_shape_key_range_before_usage(source, &access, usage_byte)
        .or_else(|| literal_array_shape_key_range_before_usage(source, &access, usage_byte))
        .map(|(start, end)| byte_offsets_to_range(source, start, end))
}

fn line_col_to_byte_offset(source: &str, line: u32, byte_col: u32) -> Option<usize> {
    let mut offset = 0usize;
    for (idx, row) in source.split_inclusive('\n').enumerate() {
        let row_without_newline = row.trim_end_matches('\n');
        if idx == line as usize {
            return Some(offset + (byte_col as usize).min(row_without_newline.len()));
        }
        offset += row.len();
    }
    (line as usize == source.lines().count()).then_some(source.len())
}

fn line_bounds_at(source: &str, line: u32) -> Option<(usize, usize)> {
    let mut offset = 0usize;
    for (idx, row) in source.split_inclusive('\n').enumerate() {
        let end = offset + row.trim_end_matches('\n').len();
        if idx == line as usize {
            return Some((offset, end));
        }
        offset += row.len();
    }
    None
}

fn array_shape_key_access_at_position(
    source: &str,
    line: u32,
    byte_col: u32,
) -> Option<ShapeDefinitionAccess> {
    let (line_start, line_end) = line_bounds_at(source, line)?;
    let offset = line_start + byte_col as usize;
    if offset > line_end {
        return None;
    }
    let line_text = &source[line_start..line_end];
    let rel = offset.saturating_sub(line_start);
    let quote_start = line_text[..rel].rfind(['\'', '"'])?;
    let quote = line_text.as_bytes().get(quote_start).copied()? as char;
    let before_quote = &line_text[..quote_start];
    let bracket = before_quote.rfind('[')?;
    if !before_quote[bracket + 1..].trim().is_empty() {
        return None;
    }

    let key_start = quote_start + quote.len_utf8();
    let key_end = line_text[key_start..]
        .find(quote)
        .map(|idx| key_start + idx)
        .unwrap_or(rel);
    if rel < key_start || rel > key_end {
        return None;
    }
    let key = normalize_shape_key_text(&line_text[key_start..key_end]);
    if key.is_empty() {
        return None;
    }

    let array_expr = extract_shape_base_expr(&line_text[..bracket])?;
    let (root_var, mut segments) = shape_array_expr_segments(&array_expr)
        .unwrap_or_else(|| (normalize_shape_root_var(&array_expr), Vec::new()));
    if !root_var.starts_with('$') {
        return None;
    }
    segments.push(ShapePathSegment {
        key,
        kind: ShapeDefinitionKind::Array,
    });
    Some(ShapeDefinitionAccess { root_var, segments })
}

fn object_shape_property_access_at_position(
    source: &str,
    line: u32,
    byte_col: u32,
) -> Option<ShapeDefinitionAccess> {
    let (line_start, line_end) = line_bounds_at(source, line)?;
    let offset = line_start + byte_col as usize;
    if offset > line_end {
        return None;
    }
    let line_text = &source[line_start..line_end];
    let rel = offset.saturating_sub(line_start);
    let (name_start, name_end) = identifier_bounds_at(line_text, rel)?;
    let name = &line_text[name_start..name_end];
    if name.is_empty() {
        return None;
    }

    let before_name = line_text[..name_start].trim_end();
    let (object_text, arrow_len) = if let Some(object_text) = before_name.strip_suffix("?->") {
        (object_text, 3)
    } else if let Some(object_text) = before_name.strip_suffix("->") {
        (object_text, 2)
    } else {
        return None;
    };
    if arrow_len == 0 {
        return None;
    }

    let object_expr = extract_shape_base_expr(object_text)?;
    let (root_var, mut segments) = shape_array_expr_segments(&object_expr)
        .unwrap_or_else(|| (normalize_shape_root_var(&object_expr), Vec::new()));
    if !root_var.starts_with('$') {
        return None;
    }
    segments.push(ShapePathSegment {
        key: name.to_string(),
        kind: ShapeDefinitionKind::Object,
    });
    Some(ShapeDefinitionAccess { root_var, segments })
}

fn identifier_bounds_at(text: &str, offset: usize) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    if offset > bytes.len() {
        return None;
    }
    let mut start = offset.min(bytes.len());
    while start > 0 {
        let ch = bytes[start - 1] as char;
        if ch.is_ascii_alphanumeric() || ch == '_' {
            start -= 1;
        } else {
            break;
        }
    }
    let mut end = offset.min(bytes.len());
    while end < bytes.len() {
        let ch = bytes[end] as char;
        if ch.is_ascii_alphanumeric() || ch == '_' {
            end += 1;
        } else {
            break;
        }
    }
    (start < end).then_some((start, end))
}

fn extract_shape_base_expr(text: &str) -> Option<String> {
    let trimmed = text.trim_end();
    let mut start = trimmed.len();
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    for (idx, ch) in trimmed.char_indices().rev() {
        match ch {
            ')' => {
                paren_depth += 1;
                start = idx;
                continue;
            }
            '(' if paren_depth > 0 => {
                paren_depth -= 1;
                start = idx;
                continue;
            }
            ']' => {
                bracket_depth += 1;
                start = idx;
                continue;
            }
            '[' if bracket_depth > 0 => {
                bracket_depth -= 1;
                start = idx;
                continue;
            }
            _ if paren_depth > 0 || bracket_depth > 0 => {
                start = idx;
                continue;
            }
            _ => {}
        }

        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '$' | '\\' | '-' | '>' | '?') {
            start = idx;
        } else {
            break;
        }
    }

    let expr = trimmed[start..].trim();
    (!expr.is_empty()).then(|| expr.to_string())
}

fn shape_array_expr_segments(expr: &str) -> Option<(String, Vec<ShapePathSegment>)> {
    let expr = expr.trim();
    let bracket = expr.find('[')?;
    let root_var = normalize_shape_root_var(expr[..bracket].trim());
    if !root_var.starts_with('$') {
        return None;
    }

    let mut segments = Vec::new();
    let mut idx = bracket;
    while idx < expr.len() {
        while idx < expr.len() && expr.as_bytes()[idx].is_ascii_whitespace() {
            idx += 1;
        }
        if idx >= expr.len() || expr.as_bytes()[idx] != b'[' {
            break;
        }
        let close = find_matching_pair(expr, idx, '[', ']').unwrap_or(expr.len());
        let key_text = expr[idx + 1..close].trim();
        let key = normalize_shape_key_text(key_text);
        if !key.is_empty() {
            segments.push(ShapePathSegment {
                key,
                kind: ShapeDefinitionKind::Array,
            });
        }
        if close >= expr.len() {
            break;
        }
        idx = close + 1;
    }

    Some((root_var, segments))
}

fn normalize_shape_root_var(expr: &str) -> String {
    let expr = expr.trim();
    if expr.starts_with('$') {
        expr.to_string()
    } else {
        format!("${expr}")
    }
}

fn phpdoc_shape_key_range_before_usage(
    source: &str,
    access: &ShapeDefinitionAccess,
    usage_byte: usize,
) -> Option<(usize, usize)> {
    let mut search_end = usage_byte.min(source.len());
    while let Some(open) = source[..search_end].rfind("/**") {
        let Some(close_rel) = source[open..].find("*/") else {
            break;
        };
        let close = open + close_rel + 2;
        if close > usage_byte {
            search_end = open;
            continue;
        }
        let comment = &source[open..close];
        if comment.contains("@var") && comment.contains(&access.root_var) {
            if let Some(range) = find_shape_path_range_in_text(comment, open, &access.segments) {
                return Some(range);
            }
        }
        search_end = open;
    }

    None
}

fn literal_array_shape_key_range_before_usage(
    source: &str,
    access: &ShapeDefinitionAccess,
    usage_byte: usize,
) -> Option<(usize, usize)> {
    if access
        .segments
        .iter()
        .any(|segment| segment.kind != ShapeDefinitionKind::Array)
    {
        return None;
    }

    let mut search_end = usage_byte.min(source.len());
    while let Some(var_pos) = source[..search_end].rfind(&access.root_var) {
        if let Some(array_start) = assignment_array_literal_start(source, var_pos, &access.root_var)
        {
            if let Some(range) =
                find_literal_array_path_range(source, array_start, &access.segments)
            {
                return Some(range);
            }
        }
        search_end = var_pos;
    }

    None
}

fn assignment_array_literal_start(source: &str, var_pos: usize, var_name: &str) -> Option<usize> {
    let mut idx = var_pos + var_name.len();
    idx = skip_ascii_whitespace(source, idx);
    if source.as_bytes().get(idx).copied()? != b'='
        || source.as_bytes().get(idx + 1).copied() == Some(b'=')
    {
        return None;
    }
    idx = skip_ascii_whitespace(source, idx + 1);
    if source[idx..].starts_with('[') || source[idx..].starts_with("array(") {
        Some(idx)
    } else {
        None
    }
}

fn skip_ascii_whitespace(source: &str, mut idx: usize) -> usize {
    while idx < source.len() && source.as_bytes()[idx].is_ascii_whitespace() {
        idx += 1;
    }
    idx
}

fn find_literal_array_path_range(
    source: &str,
    array_start: usize,
    segments: &[ShapePathSegment],
) -> Option<(usize, usize)> {
    let (body_start, body_end) = literal_array_body_range(source, array_start)?;
    find_literal_array_key_range_in_body(source, body_start, body_end, segments)
}

fn literal_array_body_range(source: &str, array_start: usize) -> Option<(usize, usize)> {
    if source[array_start..].starts_with('[') {
        let close = find_matching_pair(source, array_start, '[', ']')?;
        return Some((array_start + 1, close));
    }
    if source[array_start..].starts_with("array(") {
        let open = array_start + "array".len();
        let close = find_matching_pair(source, open, '(', ')')?;
        return Some((open + 1, close));
    }
    None
}

fn find_literal_array_key_range_in_body(
    source: &str,
    body_start: usize,
    body_end: usize,
    segments: &[ShapePathSegment],
) -> Option<(usize, usize)> {
    let segment = segments.first()?;
    for (item_start, item_end) in split_top_level_ranges(source, body_start, body_end, ',') {
        let arrow = find_top_level_needle(source, item_start, item_end, "=>")?;
        let (key, key_start, key_end) = shape_key_from_raw_range(source, item_start, arrow)?;
        if key != segment.key {
            continue;
        }
        if segments.len() == 1 {
            return Some((key_start, key_end));
        }
        let value_start = skip_ascii_whitespace(source, arrow + 2);
        if let Some(range) = find_literal_array_path_range(source, value_start, &segments[1..]) {
            return Some(range);
        }
    }

    None
}

fn find_shape_path_range_in_text(
    text: &str,
    text_abs_start: usize,
    segments: &[ShapePathSegment],
) -> Option<(usize, usize)> {
    let segment = segments.first()?;
    let prefix = match segment.kind {
        ShapeDefinitionKind::Array => "array{",
        ShapeDefinitionKind::Object => "object{",
    };
    let mut search_start = 0usize;
    while let Some(prefix_rel) = text[search_start..].find(prefix) {
        let shape_start = search_start + prefix_rel;
        let open = shape_start + prefix.len() - 1;
        let Some(close) = find_matching_pair(text, open, '{', '}') else {
            search_start = shape_start + prefix.len();
            continue;
        };
        if let Some(range) =
            find_shape_key_range_in_body(text, text_abs_start, open + 1, close, segments)
        {
            return Some(range);
        }
        search_start = close + 1;
    }

    None
}

fn find_shape_key_range_in_body(
    text: &str,
    text_abs_start: usize,
    body_start: usize,
    body_end: usize,
    segments: &[ShapePathSegment],
) -> Option<(usize, usize)> {
    let segment = segments.first()?;
    for (item_start, item_end) in split_top_level_ranges(text, body_start, body_end, ',') {
        let Some(colon) = find_top_level_char_in_range(text, item_start, item_end, ':') else {
            continue;
        };
        let (key, key_start, key_end) = shape_key_from_raw_range(text, item_start, colon)?;
        if key != segment.key {
            continue;
        }
        if segments.len() == 1 {
            return Some((text_abs_start + key_start, text_abs_start + key_end));
        }
        if let Some(range) = find_shape_path_range_in_text(
            &text[colon + 1..item_end],
            text_abs_start + colon + 1,
            &segments[1..],
        ) {
            return Some(range);
        }
    }

    None
}

fn shape_key_from_raw_range(
    text: &str,
    start: usize,
    end: usize,
) -> Option<(String, usize, usize)> {
    let mut key_start = start;
    let mut key_end = end;
    while key_start < key_end && text.as_bytes()[key_start].is_ascii_whitespace() {
        key_start += 1;
    }
    while key_end > key_start && text.as_bytes()[key_end - 1].is_ascii_whitespace() {
        key_end -= 1;
    }
    if key_end > key_start && text.as_bytes()[key_end - 1] == b'?' {
        key_end -= 1;
        while key_end > key_start && text.as_bytes()[key_end - 1].is_ascii_whitespace() {
            key_end -= 1;
        }
    }
    if key_end > key_start + 1 {
        let first = text.as_bytes()[key_start];
        let last = text.as_bytes()[key_end - 1];
        if (first == b'\'' && last == b'\'') || (first == b'"' && last == b'"') {
            key_start += 1;
            key_end -= 1;
        }
    }
    let key = normalize_shape_key_text(&text[key_start..key_end]);
    (!key.is_empty()).then_some((key, key_start, key_end))
}

fn split_top_level_ranges(
    text: &str,
    start: usize,
    end: usize,
    delimiter: char,
) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut item_start = start;
    let mut paren_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for (rel, ch) in text[start..end].char_indices() {
        let idx = start + rel;
        if let Some(quote_ch) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote_ch {
                quote = None;
            }
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }

        match ch {
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '<' => angle_depth += 1,
            '>' => angle_depth = angle_depth.saturating_sub(1),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        if ch == delimiter
            && paren_depth == 0
            && angle_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
        {
            ranges.push((item_start, idx));
            item_start = idx + ch.len_utf8();
        }
    }
    ranges.push((item_start, end));
    ranges
}

fn find_top_level_char_in_range(
    text: &str,
    start: usize,
    end: usize,
    needle: char,
) -> Option<usize> {
    let mut paren_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (rel, ch) in text[start..end].char_indices() {
        let idx = start + rel;
        if let Some(quote_ch) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote_ch {
                quote = None;
            }
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }
        if ch == needle
            && paren_depth == 0
            && angle_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
        {
            return Some(idx);
        }
        match ch {
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '<' => angle_depth += 1,
            '>' => angle_depth = angle_depth.saturating_sub(1),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }
    }
    None
}

fn find_top_level_needle(text: &str, start: usize, end: usize, needle: &str) -> Option<usize> {
    let mut paren_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (rel, ch) in text[start..end].char_indices() {
        let idx = start + rel;
        if let Some(quote_ch) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote_ch {
                quote = None;
            }
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }
        if paren_depth == 0
            && angle_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && text[idx..end].starts_with(needle)
        {
            return Some(idx);
        }
        match ch {
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '<' => angle_depth += 1,
            '>' => angle_depth = angle_depth.saturating_sub(1),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }
    }
    None
}

fn find_matching_pair(text: &str, open: usize, open_ch: char, close_ch: char) -> Option<usize> {
    if !text[open..].starts_with(open_ch) {
        return None;
    }
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut depth = 0usize;
    for (rel, ch) in text[open..].char_indices() {
        let idx = open + rel;
        if let Some(quote_ch) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote_ch {
                quote = None;
            }
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }
        if ch == open_ch {
            depth += 1;
        } else if ch == close_ch {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some(idx);
            }
        }
    }
    None
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

fn infer_static_call_expression_type<F>(
    expr: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    mut resolver: F,
) -> Option<String>
where
    F: FnMut(&str, &str) -> Option<String>,
{
    let expr = trim_balanced_outer_parens(expr.trim());
    let (class_expr, after_scope) = expr.split_once("::")?;
    let class_name = class_expr.trim();
    if class_name.is_empty() || matches!(class_name, "self" | "static" | "parent") {
        return None;
    }

    let method_name_end = after_scope
        .char_indices()
        .find_map(|(idx, ch)| (!ch.is_alphanumeric() && ch != '_').then_some(idx))
        .unwrap_or(after_scope.len());
    let method_name = after_scope[..method_name_end].trim();
    if method_name.is_empty() || after_scope[method_name_end..].trim_start().chars().next()? != '('
    {
        return None;
    }

    let class_fqn = resolve_class_name_pub(class_name, file_symbols)
        .trim_start_matches('\\')
        .to_string();
    resolver(&class_fqn, method_name)
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
    if class_fqn.is_empty() {
        return resolve_function_return_type_from_index(index, member_name);
    }

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

fn resolve_function_return_type_from_index(
    index: &WorkspaceIndex,
    function_fqn: &str,
) -> Option<String> {
    let sym = index.resolve_fqn(function_fqn).or_else(|| {
        function_fqn
            .rsplit_once('\\')
            .and_then(|(_, short_name)| index.resolve_fqn(short_name))
    })?;
    if sym.kind != php_lsp_types::PhpSymbolKind::Function {
        return None;
    }
    symbol_return_type_fqn(index, "", &sym)
}

fn symbol_return_type_fqn(
    index: &WorkspaceIndex,
    owner_fqn: &str,
    sym: &php_lsp_types::SymbolInfo,
) -> Option<String> {
    let ret = symbol_effective_return_type(sym)?;
    tracing::debug!("resolve_member_type: {} -> return type '{}'", sym.fqn, ret);

    type_info_fqn_from_index(index, owner_fqn, &sym.uri, &ret)
}

fn type_info_fqn_from_index(
    index: &WorkspaceIndex,
    owner_fqn: &str,
    uri: &str,
    type_info: &php_lsp_types::TypeInfo,
) -> Option<String> {
    match type_info {
        php_lsp_types::TypeInfo::Simple(name) => {
            if owner_fqn.is_empty() {
                let raw = name.trim().trim_start_matches('\\');
                if !raw.is_empty() && index.resolve_fqn(raw).is_some() {
                    return Some(raw.to_string());
                }
            }
            simple_type_fqn_from_owner_or_index(index, owner_fqn, uri, name)
        }
        php_lsp_types::TypeInfo::Nullable(inner) => {
            type_info_fqn_from_index(index, owner_fqn, uri, inner)
        }
        php_lsp_types::TypeInfo::Self_ | php_lsp_types::TypeInfo::Static_ => {
            Some(owner_fqn.to_string())
        }
        php_lsp_types::TypeInfo::Generic { base, .. } if !is_builtin_type_name(base) => {
            simple_type_fqn_from_owner_or_index(index, owner_fqn, uri, base)
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

pub(crate) fn path_is_excluded(path: &Path, root: &Path, exclude_paths: &[PathBuf]) -> bool {
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

pub(crate) fn workspace_index_directories(
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
pub(crate) fn collect_php_files(
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

fn uri_is_php_file(uri: &Uri) -> bool {
    if let Some(path) = uri_to_path(uri.as_str()) {
        return path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("php"));
    }

    uri.as_str().to_ascii_lowercase().ends_with(".php")
}

fn template_kind_for_document(uri_str: &str, language_id: &str) -> Option<TemplateKind> {
    if is_blade_template_uri(uri_str) || is_blade_template_language_id(language_id) {
        return Some(TemplateKind::Blade);
    }
    if is_twig_template_uri(uri_str) || is_twig_template_language_id(language_id) {
        return Some(TemplateKind::Twig);
    }
    None
}

fn twig_template_name_for_uri(uri_str: &str, root: &Path) -> Option<String> {
    let path = uri_to_path(uri_str)?;
    for base in [root.join("templates"), root.join("resources/views")] {
        if let Ok(relative) = path.strip_prefix(&base) {
            return normalize_twig_template_name(relative);
        }
    }

    path.file_name()
        .and_then(|file| file.to_str())
        .filter(|file| file.ends_with(".twig"))
        .map(str::to_string)
}

fn twig_template_path_for_key(root: &Path, key: &str) -> Option<PathBuf> {
    let normalized = normalize_twig_key(key);
    if normalized.is_empty()
        || normalized.starts_with('/')
        || normalized
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return None;
    }

    for base in [root.join("templates"), root.join("resources/views")] {
        let path = base.join(&normalized);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn normalize_twig_template_name(path: &Path) -> Option<String> {
    let parts: Vec<String> = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect();
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn collect_twig_context_php_files(root: &Path, limit: usize) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for base in [root.join("src"), root.join("app"), root.join("tests")] {
        collect_twig_context_php_files_recursive(&base, limit, &mut files);
        if files.len() >= limit {
            break;
        }
    }
    files.sort();
    files
}

fn collect_twig_context_php_files_recursive(root: &Path, limit: usize, files: &mut Vec<PathBuf>) {
    if files.len() >= limit || !root.is_dir() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        if files.len() >= limit {
            return;
        }
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.')
                || matches!(name.as_ref(), "vendor" | "node_modules" | "target" | "var")
            {
                continue;
            }
            collect_twig_context_php_files_recursive(&path, limit, files);
        } else if path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("php"))
        {
            files.push(path);
        }
    }
}

fn collect_twig_render_context_types(
    template_name: &str,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    variables: &mut HashMap<String, String>,
) {
    let mut offset = 0usize;
    while let Some((_, name_end, open_paren)) = next_twig_render_call(source, offset) {
        let Some(close_paren) = find_matching_delimiter(source, open_paren, '(', ')') else {
            offset = name_end;
            continue;
        };
        let args = split_top_level_spans(
            source.get(open_paren + 1..close_paren).unwrap_or(""),
            open_paren + 1,
        );
        if args.len() >= 2 {
            let template_arg = trim_source_range(source, args[0].0, args[0].1);
            let context_arg = trim_source_range(source, args[1].0, args[1].1);
            if php_string_literal_value_at_range(source, template_arg.0, template_arg.1)
                .is_some_and(|name| normalize_twig_key(&name) == normalize_twig_key(template_name))
            {
                collect_twig_context_array_types(source, context_arg, file_symbols, variables);
            }
        }
        offset = close_paren + 1;
    }
}

fn next_twig_render_call(source: &str, from: usize) -> Option<(usize, usize, usize)> {
    let mut offset = from;
    while offset < source.len() {
        let byte = *source.as_bytes().get(offset)?;
        if !is_ident_byte(byte) {
            offset += 1;
            continue;
        }

        let start = offset;
        offset += 1;
        while offset < source.len() && is_ident_byte(source.as_bytes()[offset]) {
            offset += 1;
        }
        let name = source.get(start..offset)?;
        if matches!(name, "render" | "renderView") {
            let open = skip_ascii_ws_server(source, offset);
            if source.as_bytes().get(open) == Some(&b'(') {
                return Some((start, offset, open));
            }
        }
    }
    None
}

fn collect_twig_context_array_types(
    source: &str,
    range: (usize, usize),
    file_symbols: &php_lsp_types::FileSymbols,
    variables: &mut HashMap<String, String>,
) {
    let (start, end) = range;
    let Some((inner_start, inner_end)) = php_array_inner_range(source, start, end) else {
        return;
    };
    let spans = split_top_level_spans(
        source.get(inner_start..inner_end).unwrap_or(""),
        inner_start,
    );
    for span in spans {
        let Some(arrow) = find_top_level_double_arrow(source, span.0, span.1) else {
            continue;
        };
        let key_range = trim_source_range(source, span.0, arrow);
        let value_range = trim_source_range(source, arrow + 2, span.1);
        let Some(name) = php_string_literal_value_at_range(source, key_range.0, key_range.1) else {
            continue;
        };
        if !is_template_variable_name(&name) || variables.contains_key(&name) {
            continue;
        }
        if let Some(type_text) = infer_twig_context_value_type(source, value_range, file_symbols) {
            variables.insert(name, type_text);
        }
    }
}

fn php_array_inner_range(source: &str, start: usize, end: usize) -> Option<(usize, usize)> {
    let (start, end) = trim_source_range(source, start, end);
    if source.as_bytes().get(start) == Some(&b'[') {
        let close = find_matching_delimiter(source, start, '[', ']')?;
        if close <= end {
            return Some((start + 1, close));
        }
    }
    if source.get(start..end)?.starts_with("array") {
        let open = skip_ascii_ws_server(source, start + "array".len());
        if source.as_bytes().get(open) == Some(&b'(') {
            let close = find_matching_delimiter(source, open, '(', ')')?;
            if close <= end {
                return Some((open + 1, close));
            }
        }
    }
    None
}

fn infer_twig_context_value_type(
    source: &str,
    range: (usize, usize),
    file_symbols: &php_lsp_types::FileSymbols,
) -> Option<String> {
    let (start, end) = trim_source_range(source, range.0, range.1);
    let value = source.get(start..end)?.trim();
    if value.starts_with('[') || value.starts_with("array") {
        if let Some(class_name) = first_new_class_name(value) {
            return Some(format!(
                "array<int, {}>",
                resolve_twig_context_class_name(file_symbols, class_name)
            ));
        }
    }

    first_new_class_name(value)
        .map(|class_name| resolve_twig_context_class_name(file_symbols, class_name))
}

fn first_new_class_name(value: &str) -> Option<&str> {
    let mut offset = 0usize;
    while let Some(relative) = value[offset..].find("new") {
        let start = offset + relative;
        let before_ok = start == 0
            || value
                .as_bytes()
                .get(start - 1)
                .map(|byte| !is_ident_byte(*byte))
                .unwrap_or(true);
        let after_new = start + "new".len();
        let after_ok = value
            .as_bytes()
            .get(after_new)
            .is_some_and(u8::is_ascii_whitespace);
        if before_ok && after_ok {
            let class_start = skip_ascii_ws_server(value, after_new);
            let class_end = scan_php_class_name_end(value, class_start);
            if class_end > class_start {
                return value.get(class_start..class_end);
            }
        }
        offset = after_new;
    }
    None
}

fn resolve_twig_context_class_name(
    file_symbols: &php_lsp_types::FileSymbols,
    raw_name: &str,
) -> String {
    let raw_name = raw_name.trim_start_matches('\\');
    if raw_name.contains('\\') {
        return raw_name.to_string();
    }

    for use_statement in &file_symbols.use_statements {
        if use_statement.kind != php_lsp_types::UseKind::Class {
            continue;
        }
        let alias = use_statement.alias.as_deref().unwrap_or_else(|| {
            use_statement
                .fqn
                .rsplit('\\')
                .next()
                .unwrap_or(use_statement.fqn.as_str())
        });
        if alias == raw_name {
            return use_statement.fqn.clone();
        }
    }

    file_symbols
        .namespace
        .as_ref()
        .map(|namespace| format!("{namespace}\\{raw_name}"))
        .unwrap_or_else(|| raw_name.to_string())
}

fn find_top_level_double_arrow(source: &str, start: usize, end: usize) -> Option<usize> {
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut offset = start;

    while offset < end {
        let ch = source[offset..end].chars().next()?;
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active_quote {
                quote = None;
            }
            offset += ch.len_utf8();
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
            '=' if paren_depth == 0
                && bracket_depth == 0
                && brace_depth == 0
                && source[offset..end].starts_with("=>") =>
            {
                return Some(offset);
            }
            _ => {}
        }
        offset += ch.len_utf8();
    }
    None
}

fn php_string_literal_value_at_range(source: &str, start: usize, end: usize) -> Option<String> {
    let text = source.get(start..end)?.trim();
    unquote_php_string_literal(text)
}

fn trim_source_range(source: &str, mut start: usize, mut end: usize) -> (usize, usize) {
    while start < end
        && source
            .as_bytes()
            .get(start)
            .is_some_and(u8::is_ascii_whitespace)
    {
        start += 1;
    }
    while end > start
        && source
            .as_bytes()
            .get(end - 1)
            .is_some_and(u8::is_ascii_whitespace)
    {
        end -= 1;
    }
    (start, end)
}

fn skip_ascii_ws_server(source: &str, mut offset: usize) -> usize {
    while offset < source.len()
        && source
            .as_bytes()
            .get(offset)
            .is_some_and(u8::is_ascii_whitespace)
    {
        offset += 1;
    }
    offset
}

fn scan_php_class_name_end(source: &str, start: usize) -> usize {
    let mut end = start;
    while end < source.len() {
        let byte = source.as_bytes()[end];
        if is_ident_byte(byte) || byte == b'\\' {
            end += 1;
        } else {
            break;
        }
    }
    end
}

fn normalize_twig_key(key: &str) -> String {
    key.trim_start_matches('/').replace('\\', "/")
}

fn is_template_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
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

fn project_command_trust_setting(settings: &serde_json::Value) -> Option<bool> {
    settings_bool(
        settings,
        "allowProjectCommands",
        &["security", "allowProjectCommands"],
    )
}

fn project_commands_are_trusted(
    trusted_settings: &serde_json::Value,
    client_settings: &serde_json::Value,
) -> bool {
    project_command_trust_setting(client_settings)
        .or_else(|| project_command_trust_setting(trusted_settings))
        .unwrap_or(false)
}

fn remove_section_key(
    settings: &mut serde_json::Value,
    section: &str,
    key: &str,
) -> Option<serde_json::Value> {
    settings
        .get_mut(section)
        .and_then(|section| section.as_object_mut())
        .and_then(|section| section.remove(key))
}

fn nested_bool(settings: &serde_json::Value, section: &str, key: &str) -> Option<bool> {
    settings
        .get(section)
        .and_then(|section| section.get(key))
        .and_then(|value| value.as_bool())
}

fn nested_string<'a>(settings: &'a serde_json::Value, section: &str, key: &str) -> Option<&'a str> {
    settings
        .get(section)
        .and_then(|section| section.get(key))
        .and_then(|value| value.as_str())
}

fn untrusted_project_formatter_provider_executes(provider: &str) -> bool {
    !matches!(
        provider.trim().to_ascii_lowercase().as_str(),
        "auto" | "none" | "custom"
    )
}

fn sanitize_project_settings_for_command_trust(
    settings: &mut serde_json::Value,
    path: &Path,
    allow_project_commands: bool,
) -> Option<String> {
    if let Some(object) = settings.as_object_mut() {
        // Project configs cannot opt themselves into executable command trust.
        object.remove("allowProjectCommands");
    }

    if allow_project_commands {
        return None;
    }

    let mut blocked = Vec::new();

    if remove_section_key(settings, "formatting", "command").is_some() {
        blocked.push("formatting.command");
    }
    if nested_string(settings, "formatting", "provider")
        .is_some_and(untrusted_project_formatter_provider_executes)
    {
        remove_section_key(settings, "formatting", "provider");
        blocked.push("formatting.provider");
    }

    if nested_bool(settings, "phpstan", "enabled") == Some(true) {
        remove_section_key(settings, "phpstan", "enabled");
        blocked.push("phpstan.enabled");
    }
    if remove_section_key(settings, "phpstan", "command").is_some() {
        blocked.push("phpstan.command");
    }

    if nested_bool(settings, "psalm", "enabled") == Some(true) {
        remove_section_key(settings, "psalm", "enabled");
        blocked.push("psalm.enabled");
    }
    if remove_section_key(settings, "psalm", "command").is_some() {
        blocked.push("psalm.command");
    }

    if blocked.is_empty() {
        return None;
    }

    Some(format!(
        "Ignored executable project config settings from {}: {}. Set phpLsp.allowProjectCommands=true in VS Code or allowProjectCommands=true in global php-lsp config to trust workspace commands.",
        path.display(),
        blocked.join(", ")
    ))
}

pub(crate) fn load_effective_configuration_settings(
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

    let client_settings = normalize_client_settings(client_settings);
    let allow_project_commands = project_commands_are_trusted(&effective, &client_settings);

    for root in workspace_roots {
        for path in project_config_candidates(root) {
            if !path.exists() {
                continue;
            }
            match load_toml_settings(&path) {
                Ok(mut settings) => {
                    if let Some(message) = sanitize_project_settings_for_command_trust(
                        &mut settings,
                        &path,
                        allow_project_commands,
                    ) {
                        messages.push(message);
                    }
                    merge_json_objects(&mut effective, &settings);
                    messages.push(format!("Loaded project config: {}", path.display()));
                    break;
                }
                Err(message) => messages.push(message),
            }
        }
    }

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposerMetadataChange {
    ProjectAutoload,
    VendorAutoload,
}

fn composer_metadata_change_for_path(path: &Path) -> Option<ComposerMetadataChange> {
    let file_name = path.file_name()?.to_str()?;
    if file_name == "composer.json" {
        return Some(ComposerMetadataChange::ProjectAutoload);
    }
    if file_name == "composer.lock" {
        return Some(ComposerMetadataChange::VendorAutoload);
    }

    let parent = path.parent()?;
    let parent_name = parent.file_name()?.to_str()?;
    if parent_name != "composer" {
        return None;
    }
    let grandparent_name = parent.parent()?.file_name()?.to_str()?;
    if grandparent_name != "vendor" {
        return None;
    }

    let is_vendor_metadata = file_name == "installed.json"
        || file_name == "installed.php"
        || (file_name.starts_with("autoload_") && file_name.ends_with(".php"));
    is_vendor_metadata.then_some(ComposerMetadataChange::VendorAutoload)
}

fn uri_composer_metadata_change(uri: &Uri) -> Option<(PathBuf, ComposerMetadataChange)> {
    let path = uri_to_path(uri.as_str())?;
    let change = composer_metadata_change_for_path(&path)?;
    Some((path, change))
}

pub(crate) fn discover_workspace_root_config(
    root: &Path,
    composer_enabled: bool,
) -> WorkspaceRootConfig {
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

pub(crate) fn parse_vendor_autoload_map(vendor_dir: &Path) -> Option<VendorAutoloadMap> {
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

pub(crate) fn resolve_vendor_paths_from_map(
    fqn: &str,
    map: &VendorAutoloadMap,
) -> Option<Vec<PathBuf>> {
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
        self.lsp_initialize(params).await
    }

    async fn initialized(&self, _params: InitializedParams) {
        self.lsp_initialized(_params).await
    }

    async fn did_change_workspace_folders(&self, params: DidChangeWorkspaceFoldersParams) {
        self.lsp_did_change_workspace_folders(params).await
    }

    async fn shutdown(&self) -> Result<()> {
        self.lsp_shutdown().await
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.lsp_did_open(params).await
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        self.lsp_did_change(params).await
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.lsp_did_close(params).await
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        self.lsp_did_save(params).await
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        self.lsp_did_change_watched_files(params).await
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        self.lsp_did_change_configuration(params).await
    }

    async fn will_create_files(&self, _params: CreateFilesParams) -> Result<Option<WorkspaceEdit>> {
        self.lsp_will_create_files(_params).await
    }

    async fn did_create_files(&self, params: CreateFilesParams) {
        self.lsp_did_create_files(params).await
    }

    async fn will_rename_files(&self, _params: RenameFilesParams) -> Result<Option<WorkspaceEdit>> {
        self.lsp_will_rename_files(_params).await
    }

    async fn did_rename_files(&self, params: RenameFilesParams) {
        self.lsp_did_rename_files(params).await
    }

    async fn will_delete_files(&self, _params: DeleteFilesParams) -> Result<Option<WorkspaceEdit>> {
        self.lsp_will_delete_files(_params).await
    }

    async fn did_delete_files(&self, params: DeleteFilesParams) {
        self.lsp_did_delete_files(params).await
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        self.lsp_formatting(params).await
    }

    async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        self.lsp_range_formatting(params).await
    }

    async fn on_type_formatting(
        &self,
        params: DocumentOnTypeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        self.lsp_on_type_formatting(params).await
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        self.lsp_hover(params).await
    }

    async fn goto_declaration(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        self.lsp_goto_declaration(params).await
    }

    async fn goto_type_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        self.lsp_goto_type_definition(params).await
    }

    async fn goto_implementation(
        &self,
        params: GotoImplementationParams,
    ) -> Result<Option<GotoImplementationResponse>> {
        self.lsp_goto_implementation(params).await
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        self.lsp_goto_definition(params).await
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        self.lsp_document_highlight(params).await
    }

    async fn selection_range(
        &self,
        params: SelectionRangeParams,
    ) -> Result<Option<Vec<SelectionRange>>> {
        self.lsp_selection_range(params).await
    }

    async fn linked_editing_range(
        &self,
        params: LinkedEditingRangeParams,
    ) -> Result<Option<LinkedEditingRanges>> {
        self.lsp_linked_editing_range(params).await
    }

    async fn prepare_call_hierarchy(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> Result<Option<Vec<CallHierarchyItem>>> {
        self.lsp_prepare_call_hierarchy(params).await
    }

    async fn incoming_calls(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyIncomingCall>>> {
        self.lsp_incoming_calls(params).await
    }

    async fn outgoing_calls(
        &self,
        params: CallHierarchyOutgoingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyOutgoingCall>>> {
        self.lsp_outgoing_calls(params).await
    }

    async fn prepare_type_hierarchy(
        &self,
        params: TypeHierarchyPrepareParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        self.lsp_prepare_type_hierarchy(params).await
    }

    async fn supertypes(
        &self,
        params: TypeHierarchySupertypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        self.lsp_supertypes(params).await
    }

    async fn subtypes(
        &self,
        params: TypeHierarchySubtypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        self.lsp_subtypes(params).await
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        self.lsp_references(params).await
    }

    async fn code_lens(&self, params: CodeLensParams) -> Result<Option<Vec<CodeLens>>> {
        self.lsp_code_lens(params).await
    }

    async fn folding_range(&self, params: FoldingRangeParams) -> Result<Option<Vec<FoldingRange>>> {
        self.lsp_folding_range(params).await
    }

    async fn document_link(&self, params: DocumentLinkParams) -> Result<Option<Vec<DocumentLink>>> {
        self.lsp_document_link(params).await
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        self.lsp_rename(params).await
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        self.lsp_prepare_rename(params).await
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        self.lsp_document_symbol(params).await
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        self.lsp_inlay_hint(params).await
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        self.lsp_semantic_tokens_full(params).await
    }

    async fn semantic_tokens_full_delta(
        &self,
        params: SemanticTokensDeltaParams,
    ) -> Result<Option<SemanticTokensFullDeltaResult>> {
        self.lsp_semantic_tokens_full_delta(params).await
    }

    async fn semantic_tokens_range(
        &self,
        params: SemanticTokensRangeParams,
    ) -> Result<Option<SemanticTokensRangeResult>> {
        self.lsp_semantic_tokens_range(params).await
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<WorkspaceSymbolResponse>> {
        self.lsp_symbol(params).await
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        self.lsp_code_action(params).await
    }

    async fn code_action_resolve(&self, params: CodeAction) -> Result<CodeAction> {
        self.lsp_code_action_resolve(params).await
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        self.lsp_signature_help(params).await
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        self.lsp_completion(params).await
    }

    async fn completion_resolve(&self, item: CompletionItem) -> Result<CompletionItem> {
        self.lsp_completion_resolve(item).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use php_lsp_types::*;
    use std::cell::Cell;

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
            templates: vec![],
            template_bindings: vec![],
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

    fn parse_and_index_php_file(index: &WorkspaceIndex, uri: &str, code: &str) -> FileParser {
        let mut parser = FileParser::new();
        parser.parse_full(code);
        let symbols = extract_file_symbols(parser.tree().unwrap(), code, uri);
        index.update_file(uri, symbols);
        parser
    }

    fn diagnostic_messages(diagnostics: &[Diagnostic]) -> Vec<String> {
        diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.clone())
            .collect()
    }

    fn assert_no_diagnostic_containing(messages: &[String], unexpected: &str) {
        assert!(
            !messages.iter().any(|message| message.contains(unexpected)),
            "Did not expect `{}` in diagnostics, got: {:?}",
            unexpected,
            messages
        );
    }

    #[test]
    fn test_request_type_cache_reuses_same_expression_context() {
        let cache = RequestTypeCache::new("file:///test.php", Some(7));
        let calls = Cell::new(0usize);

        let first = cache.cached_type_info((3, 4, 3, 10), "completion-type-info", "$user", || {
            calls.set(calls.get() + 1);
            Some(TypeInfo::Simple("App\\User".to_string()))
        });
        let second = cache.cached_type_info((3, 4, 3, 10), "completion-type-info", "$user", || {
            calls.set(calls.get() + 1);
            Some(TypeInfo::Simple("App\\Other".to_string()))
        });

        assert_eq!(calls.get(), 1);
        assert_eq!(first, Some(TypeInfo::Simple("App\\User".to_string())));
        assert_eq!(second, first);
    }

    #[test]
    fn test_request_type_cache_stores_negative_results() {
        let cache = RequestTypeCache::new("file:///test.php", Some(7));
        let calls = Cell::new(0usize);

        let first = cache.cached_string((0, 0, 0, 0), "member-type", "App\\User::missing", || {
            calls.set(calls.get() + 1);
            None
        });
        let second = cache.cached_string((0, 0, 0, 0), "member-type", "App\\User::missing", || {
            calls.set(calls.get() + 1);
            Some("App\\Never".to_string())
        });

        assert_eq!(calls.get(), 1);
        assert_eq!(first, None);
        assert_eq!(second, None);
    }

    #[test]
    fn test_request_type_cache_separates_context_and_document_version() {
        let first_cache = RequestTypeCache::new("file:///test.php", Some(7));
        let second_cache = RequestTypeCache::new("file:///test.php", Some(8));
        let calls = Cell::new(0usize);

        let first =
            first_cache.cached_type_info((3, 4, 3, 10), "completion-type-info", "$user", || {
                calls.set(calls.get() + 1);
                Some(TypeInfo::Simple("App\\User".to_string()))
            });
        let different_context =
            first_cache.cached_type_info((3, 4, 3, 10), "call-site-argument-type", "$user", || {
                calls.set(calls.get() + 1);
                Some(TypeInfo::Simple("App\\Request".to_string()))
            });
        let different_version =
            second_cache.cached_type_info((3, 4, 3, 10), "completion-type-info", "$user", || {
                calls.set(calls.get() + 1);
                Some(TypeInfo::Simple("App\\UserV2".to_string()))
            });

        assert_eq!(calls.get(), 3);
        assert_ne!(first, different_context);
        assert_ne!(first, different_version);
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
            ..Default::default()
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
    fn test_infer_static_call_expression_type_with_resolver() {
        let file_symbols = FileSymbols {
            namespace: Some("App\\Models".to_string()),
            use_statements: vec![UseStatement {
                fqn: "App\\Database\\UserBuilder".to_string(),
                alias: None,
                kind: UseKind::Class,
                range: (0, 0, 0, 0),
            }],
            symbols: vec![],
            ..Default::default()
        };

        let inferred = infer_static_call_expression_type(
            "User::query()",
            &file_symbols,
            |class_fqn, method_name| {
                assert_eq!(class_fqn, "App\\Models\\User");
                assert_eq!(method_name, "query");
                Some("App\\Database\\UserBuilder".to_string())
            },
        );

        assert_eq!(inferred.as_deref(), Some("App\\Database\\UserBuilder"));
        assert!(
            infer_static_call_expression_type("User::class", &file_symbols, |_, _| {
                Some("never".to_string())
            })
            .is_none()
        );
    }

    #[test]
    fn test_framework_string_key_context_detection() {
        let source = "<?php\nconfig('app.na');\nroute('dashboard.home');\n__('messages.welcome');\nview('users.show');\nRoute::get('/')->name('admin.index');\n";

        let config = framework_string_key_context_at_position(source, 1, 14)
            .expect("config string key context");
        assert_eq!(config.domain, "config");
        assert_eq!(config.prefix, "app.na");
        assert_eq!(config.key, "app.na");

        let route = framework_string_key_context_at_position(source, 2, 11)
            .expect("route string key context");
        assert_eq!(route.domain, "route");
        assert_eq!(route.prefix, "dash");
        assert_eq!(route.key, "dashboard.home");

        let translation = framework_string_key_context_at_position(source, 3, 13)
            .expect("translation string key context");
        assert_eq!(translation.domain, "translation");
        assert_eq!(translation.prefix, "messages.");

        let view = framework_string_key_context_at_position(source, 4, 12)
            .expect("view string key context");
        assert_eq!(view.domain, "view");
        assert_eq!(view.key, "users.show");

        let route_name = framework_string_key_context_at_position(source, 5, 29)
            .expect("route declaration name context");
        assert_eq!(route_name.domain, "route");
        assert_eq!(route_name.key, "admin.index");
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
                ..Default::default()
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
                ..Default::default()
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
                ..Default::default()
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
                        ..Default::default()
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
                ..Default::default()
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
                ..Default::default()
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
    fn test_compute_diagnostics_allows_phpdoc_array_suffix_argument_type() {
        let uri = "file:///phpdoc-array-suffix.php";
        let code = r#"<?php
namespace App;

/**
 * @param mixed[] $context
 */
function logInfo(array $context = []): void {}

function run(string $soapRequest): void {
    logInfo([$soapRequest]);
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
            "PHPDoc T[] should accept array literals, got: {:?}",
            messages
        );
    }

    #[test]
    fn test_compute_diagnostics_allows_psr_logger_context_array_suffix_type() {
        let uri = "file:///logger-context.php";
        let code = r#"<?php
namespace Psr\Log;

interface LoggerInterface
{
    /**
     * @param mixed[] $context
     */
    public function info(string $message, array $context = []): void;
}

namespace App;

use Psr\Log\LoggerInterface;

final class DeactivateConfirmService
{
    public function __construct(private LoggerInterface $logger) {}

    public function run(string $soapRequest): void
    {
        $this->logger->info('Prepared Deactivate Confirm SOAP request', [$soapRequest]);
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
            "Type mismatch for Psr\\Log\\LoggerInterface::info argument $context",
            "Unknown method: Psr\\Log\\LoggerInterface::info",
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
    fn test_compute_diagnostics_accepts_positive_int_literal_phpdoc_type() {
        let uri = "file:///positive-int-literal.php";
        let code = r#"<?php
namespace Symfony\Component\Validator\Constraints;

class Length {
    /**
     * @param positive-int|null $max
     */
    public function __construct(?int $max = null) {}
}

namespace App;

use Symfony\Component\Validator\Constraints\Length;

function build(): void {
    new Length(max: 255);
}
"#;

        let index = WorkspaceIndex::new();
        let parser = parse_and_index_php_file(&index, uri, code);

        let diagnostics = compute_diagnostics(
            uri,
            &parser,
            &index,
            DiagnosticsMode::BasicSemantic,
            PhpVersion::DEFAULT,
        );
        let messages = diagnostic_messages(&diagnostics);

        assert_no_diagnostic_containing(&messages, "Type mismatch");
        assert_no_diagnostic_containing(&messages, "positive-int");
    }

    #[test]
    fn test_compute_diagnostics_resolves_phpdoc_method_tags() {
        let uri = "file:///phpdoc-method-call.php";
        let code = r#"<?php
namespace Symfony\Component\HttpFoundation;

class Request {}

namespace SymfonyCasts\Bundle\VerifyEmail;

/**
 * @method void validateEmailConfirmationFromRequest(Request $request, string $userId, string $userEmail)
 */
interface VerifyEmailHelperInterface {}

namespace App;

use Symfony\Component\HttpFoundation\Request;
use SymfonyCasts\Bundle\VerifyEmail\VerifyEmailHelperInterface;

final class EmailVerifier
{
    public function __construct(private VerifyEmailHelperInterface $helper) {}

    public function handle(Request $request): void
    {
        $this->helper->validateEmailConfirmationFromRequest($request, '1', 'a@example.com');
    }
}
"#;

        let index = WorkspaceIndex::new();
        let parser = parse_and_index_php_file(&index, uri, code);

        let diagnostics = compute_diagnostics(
            uri,
            &parser,
            &index,
            DiagnosticsMode::BasicSemantic,
            PhpVersion::DEFAULT,
        );
        let messages = diagnostic_messages(&diagnostics);

        assert_no_diagnostic_containing(
            &messages,
            "Unknown method: SymfonyCasts\\Bundle\\VerifyEmail\\VerifyEmailHelperInterface::validateEmailConfirmationFromRequest",
        );
    }

    #[test]
    fn test_compute_diagnostics_ignores_phpdoc_method_tags_for_override_checks() {
        let uri = "file:///phpdoc-method-override-noise.php";
        let code = r#"<?php
namespace Vendor;

class Entity {}

class BaseRepository
{
    public function find(mixed $id): object|null
    {
        return null;
    }
}

namespace App;

use Vendor\BaseRepository;
use Vendor\Entity;

/**
 * @method Entity|null find($id)
 */
final class EntityRepository extends BaseRepository
{
}
"#;

        let index = WorkspaceIndex::new();
        let parser = parse_and_index_php_file(&index, uri, code);

        let diagnostics = compute_diagnostics(
            uri,
            &parser,
            &index,
            DiagnosticsMode::BasicSemantic,
            PhpVersion::DEFAULT,
        );
        let messages = diagnostic_messages(&diagnostics);

        assert_no_diagnostic_containing(
            &messages,
            "Incompatible override signature: App\\EntityRepository::find differs from Vendor\\BaseRepository::find",
        );
    }

    #[test]
    fn test_compute_diagnostics_allows_simplexml_dynamic_properties() {
        let stub_uri = "phpstub://SimpleXML/SimpleXML.php";
        let stub_code = "<?php\nclass SimpleXMLElement { /** @return static */ private function __get($name) {} }\n";
        let uri = "file:///simplexml-dynamic-properties.php";
        let code = r#"<?php
namespace App;

function status(\SimpleXMLElement $result): void {
    $statusCode = (string) $result->StatusCode;
    echo $statusCode;
}
"#;

        let index = WorkspaceIndex::new();
        let _stub_parser = parse_and_index_php_file(&index, stub_uri, stub_code);
        let parser = parse_and_index_php_file(&index, uri, code);

        let diagnostics = compute_diagnostics(
            uri,
            &parser,
            &index,
            DiagnosticsMode::BasicSemantic,
            PhpVersion::DEFAULT,
        );
        let messages = diagnostic_messages(&diagnostics);

        assert_no_diagnostic_containing(
            &messages,
            "Unknown property: SimpleXMLElement::$StatusCode",
        );
    }

    #[test]
    fn test_compute_diagnostics_accepts_non_empty_string_literals() {
        let uri = "file:///non-empty-string-literal.php";
        let code = r#"<?php
namespace App;

final class RailsClient
{
    /**
     * @param non-empty-string $path
     * @param array<string,mixed> $payload
     */
    public function post(string $path, array $payload): array
    {
        return [];
    }

    public function log(string $message): void {}
}

function run(RailsClient $client, string $suffix): void
{
    $client->post('/v1/billing/crm/get-personal-data', []);
    $client->log('Rails API HTTP error: ' . $suffix);
}
"#;

        let index = WorkspaceIndex::new();
        let parser = parse_and_index_php_file(&index, uri, code);

        let diagnostics = compute_diagnostics(
            uri,
            &parser,
            &index,
            DiagnosticsMode::BasicSemantic,
            PhpVersion::DEFAULT,
        );
        let messages = diagnostic_messages(&diagnostics);

        assert_no_diagnostic_containing(
            &messages,
            "Type mismatch for App\\RailsClient::post argument $path",
        );
        assert_no_diagnostic_containing(
            &messages,
            "Type mismatch for App\\RailsClient::log argument $message",
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
    fn test_compute_diagnostics_allows_template_phpdoc_override_signature() {
        let framework_uri = "file:///security-voter-framework.php";
        let framework_code = r#"<?php
namespace Symfony\Component\Security\Core\Authentication\Token;

interface TokenInterface {}

namespace Symfony\Component\Security\Core\Authorization\Voter;

use Symfony\Component\Security\Core\Authentication\Token\TokenInterface;

final class Vote {}

/**
 * @template TAttribute of string
 * @template TSubject
 */
abstract class Voter
{
    /**
     * @param TAttribute $attribute
     * @param TSubject $subject
     */
    abstract protected function voteOnAttribute(string $attribute, mixed $subject, TokenInterface $token, ?Vote $vote = null): bool;
}
"#;
        let app_uri = "file:///security-voter-app.php";
        let app_code = r#"<?php
namespace App;

use Symfony\Component\Security\Core\Authentication\Token\TokenInterface;
use Symfony\Component\Security\Core\Authorization\Voter\Vote;
use Symfony\Component\Security\Core\Authorization\Voter\Voter;

final class UserVoter extends Voter
{
    protected function voteOnAttribute(string $attribute, mixed $subject, TokenInterface $token, ?Vote $vote = null): bool
    {
        return true;
    }
}
"#;

        let index = WorkspaceIndex::new();
        let _framework_parser = parse_and_index_php_file(&index, framework_uri, framework_code);
        let parser = parse_and_index_php_file(&index, app_uri, app_code);

        let diagnostics = compute_diagnostics(
            app_uri,
            &parser,
            &index,
            DiagnosticsMode::BasicSemantic,
            PhpVersion::DEFAULT,
        );
        let messages = diagnostic_messages(&diagnostics);

        assert_no_diagnostic_containing(
            &messages,
            "Incompatible override signature: App\\UserVoter::voteOnAttribute differs from Symfony\\Component\\Security\\Core\\Authorization\\Voter\\Voter::voteOnAttribute",
        );
    }

    #[test]
    fn test_compute_diagnostics_allows_phpdoc_refined_array_override_signature() {
        let uri = "file:///billing-payload-overrides.php";
        let code = r#"<?php
namespace App;

interface BillingPayloadProcessor
{
    /**
     * @param array<int,array<string,mixed>> $payload
     * @return array<int,array<string,mixed>>
     */
    public function processWithBillingPayload(array $payload): array;
}

final class NpDataResponseService implements BillingPayloadProcessor
{
    public function processWithBillingPayload(array $payload): array
    {
        return $payload;
    }
}
"#;

        let index = WorkspaceIndex::new();
        let parser = parse_and_index_php_file(&index, uri, code);

        let diagnostics = compute_diagnostics(
            uri,
            &parser,
            &index,
            DiagnosticsMode::BasicSemantic,
            PhpVersion::DEFAULT,
        );
        let messages = diagnostic_messages(&diagnostics);

        assert_no_diagnostic_containing(
            &messages,
            "Incompatible override signature: App\\NpDataResponseService::processWithBillingPayload differs from App\\BillingPayloadProcessor::processWithBillingPayload",
        );
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
    fn test_formatting_auto_detects_project_tools_from_composer_metadata() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let tmp = std::env::temp_dir().join(format!(
            "php-lsp-format-detect-test-{}-{}",
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(
            tmp.join("composer.json"),
            r#"{
                "require-dev": {
                    "friendsofphp/php-cs-fixer": "^3.0",
                    "squizlabs/php_codesniffer": "^3.0"
                }
            }"#,
        )
        .unwrap();

        let config = FormattingConfig::default().resolve_for_workspace(Some(&tmp));
        assert_eq!(config.provider, "php-cs-fixer");
        assert_eq!(
            config.command_template().as_deref(),
            Some("vendor/bin/php-cs-fixer fix --using-cache=no --quiet {file}")
        );

        std::fs::write(
            tmp.join("composer.json"),
            r#"{
                "require-dev": {
                    "laravel/pint": "^1.0",
                    "friendsofphp/php-cs-fixer": "^3.0"
                }
            }"#,
        )
        .unwrap();
        let config = FormattingConfig::default().resolve_for_workspace(Some(&tmp));
        assert_eq!(config.provider, "pint");
        assert_eq!(
            config.command_template().as_deref(),
            Some("vendor/bin/pint --quiet {file}")
        );

        let disabled = FormattingConfig::from_options(Some("none"), None, None)
            .resolve_for_workspace(Some(&tmp));
        assert!(disabled.command_template().is_none());

        let _ = std::fs::remove_dir_all(tmp);
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
    fn test_untrusted_project_config_strips_executable_settings() {
        let mut settings = serde_json::json!({
            "allowProjectCommands": true,
            "phpVersion": "8.3",
            "diagnostics": { "mode": "syntax-only" },
            "includePaths": ["src"],
            "formatting": {
                "provider": "php-cs-fixer",
                "command": "sh -c 'touch /tmp/php-lsp-owned' {file}",
                "timeoutMs": 1000
            },
            "phpstan": {
                "enabled": true,
                "command": "sh -c 'touch /tmp/php-lsp-owned' {file}",
                "timeoutMs": 1000,
                "memory_limit": "1G"
            },
            "psalm": {
                "enabled": true,
                "command": "sh -c 'touch /tmp/php-lsp-owned' {file}",
                "timeoutMs": 1000
            }
        });

        let message = sanitize_project_settings_for_command_trust(
            &mut settings,
            Path::new("/workspace/.php-lsp.toml"),
            false,
        )
        .expect("expected executable project settings to be ignored");

        assert_eq!(settings["phpVersion"], "8.3");
        assert_eq!(settings["diagnostics"]["mode"], "syntax-only");
        assert_eq!(settings["includePaths"][0], "src");
        assert!(settings.get("allowProjectCommands").is_none());
        assert!(settings["formatting"].get("provider").is_none());
        assert!(settings["formatting"].get("command").is_none());
        assert_eq!(settings["formatting"]["timeoutMs"], 1000);
        assert!(settings["phpstan"].get("enabled").is_none());
        assert!(settings["phpstan"].get("command").is_none());
        assert_eq!(settings["phpstan"]["timeoutMs"], 1000);
        assert_eq!(settings["phpstan"]["memory_limit"], "1G");
        assert!(settings["psalm"].get("enabled").is_none());
        assert!(settings["psalm"].get("command").is_none());
        assert_eq!(settings["psalm"]["timeoutMs"], 1000);
        assert!(message.contains("formatting.command"));
        assert!(message.contains("phpstan.enabled"));
        assert!(message.contains("psalm.command"));
    }

    #[test]
    fn test_trusted_project_config_keeps_executable_settings_but_not_self_trust() {
        let mut settings = serde_json::json!({
            "allowProjectCommands": true,
            "formatting": {
                "provider": "pint",
                "command": "vendor/bin/pint --quiet {file}"
            },
            "phpstan": {
                "enabled": true,
                "command": "vendor/bin/phpstan analyse --error-format=json {file}"
            },
            "psalm": {
                "enabled": true,
                "command": "vendor/bin/psalm --output-format=json {file}"
            }
        });

        let message = sanitize_project_settings_for_command_trust(
            &mut settings,
            Path::new("/workspace/.php-lsp.toml"),
            true,
        );

        assert!(message.is_none());
        assert!(settings.get("allowProjectCommands").is_none());
        assert_eq!(settings["formatting"]["provider"], "pint");
        assert_eq!(
            settings["formatting"]["command"],
            "vendor/bin/pint --quiet {file}"
        );
        assert_eq!(settings["phpstan"]["enabled"], true);
        assert_eq!(settings["psalm"]["enabled"], true);
    }

    #[test]
    fn test_client_project_command_trust_overrides_global_config() {
        let global = serde_json::json!({ "allowProjectCommands": true });
        let client = serde_json::json!({ "allowProjectCommands": false });
        assert!(!project_commands_are_trusted(&global, &client));

        let client = serde_json::json!({});
        assert!(project_commands_are_trusted(&global, &client));

        let client = serde_json::json!({ "allowProjectCommands": true });
        assert!(project_commands_are_trusted(
            &serde_json::json!({}),
            &client
        ));
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
