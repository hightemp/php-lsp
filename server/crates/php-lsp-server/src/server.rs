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
use crate::util::lsp_text::{lsp_position_to_byte, range_from_tuple, text_at_lsp_range};
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
use php_lsp_parser::signature_help::signature_help_context_at_position;
use php_lsp_parser::symbols::extract_file_symbols;
use php_lsp_parser::utf16::{range_byte_to_utf16, utf16_col_to_byte, Utf16LineIndex};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
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
pub(crate) use indexing::vendor::{parse_vendor_autoload_map, resolve_vendor_paths_from_map};
use indexing::workspace::*;
pub(crate) use indexing::workspace::{
    collect_php_files, discover_workspace_root_config, load_effective_configuration_settings,
    path_is_excluded, workspace_index_directories,
};
pub(crate) use lsp::code_action::*;
use lsp::completion_helpers::*;
use lsp::conversions::*;
use lsp::diagnostics::*;
pub(crate) use lsp::diagnostics::{
    compute_diagnostics_with_config, lazy_resolvable_diagnostic_fqn,
};
use lsp::document_symbols::*;
use lsp::external_command::*;
use lsp::inlay_hints::*;
use lsp::rename::*;
use lsp::templates::*;

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
        let Some(tool) = lsp::formatting::detect_project_formatter_tool(workspace_root) else {
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
#[path = "server_tests.rs"]
mod tests;
