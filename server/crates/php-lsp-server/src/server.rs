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
use crate::util::lsp_text::{
    lsp_position_to_byte, range_from_byte_range, range_from_lsp_tuple, text_at_lsp_range,
};
use crate::util::uri::uri_to_path;
use dashmap::DashMap;
use php_lsp_completion::context::detect_context_at_byte_col;
use php_lsp_completion::provider::provide_completions_at_range;
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
use std::collections::{hash_map::DefaultHasher, HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::task::{JoinHandle, JoinSet};
use tower_lsp::jsonrpc::Result;
use tower_lsp::ls_types::request::{GotoImplementationParams, GotoImplementationResponse};
use tower_lsp::ls_types::*;
use tower_lsp::{Client, LanguageServer};
use tracing::Instrument;

#[path = "indexing/mod.rs"]
mod indexing;
#[path = "lsp/mod.rs"]
mod lsp;
use indexing::cache::*;
pub(crate) use indexing::stubs::load_configured_stubs;
use indexing::stubs::*;
use indexing::vendor::*;
pub(crate) use indexing::vendor::{
    parse_vendor_autoload_map, resolve_vendor_paths_from_map, vendor_autoload_file_paths_from_map,
    vendor_namespace_exists_from_map,
};
use indexing::workspace::*;
pub(crate) use indexing::workspace::{
    collect_php_files, discover_workspace_root_config, load_effective_configuration_settings,
    path_is_excluded, workspace_index_directories,
};
pub(crate) use lsp::code_action::*;
use lsp::completion_helpers::*;
use lsp::conversions::*;
#[cfg(test)]
pub(crate) use lsp::diagnostics::compute_diagnostics_with_config;
use lsp::diagnostics::*;
pub(crate) use lsp::diagnostics::{
    compute_diagnostics_with_runtime_config, lazy_resolvable_diagnostic_fqn,
    lazy_resolved_symbol_diagnostic_is_satisfied,
};
pub(crate) use lsp::document_links::static_php_include_target_paths_for_source;
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
const DIAGNOSTIC_PUBLISHER_MAX_SHARDS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OpenDocumentState {
    version: i32,
    generation: u64,
}

struct OpenDocumentSnapshot {
    tree: tree_sitter::Tree,
    source: String,
    template_document: Option<TemplateDocument>,
    document_state: Option<OpenDocumentState>,
    file_symbols: php_lsp_types::FileSymbols,
}

fn open_document_snapshot_from_state_with_lock_hook<F>(
    open_files: &DashMap<String, FileParser>,
    template_documents: &DashMap<String, TemplateDocument>,
    document_versions: &DashMap<String, OpenDocumentState>,
    uri_str: &str,
    after_open_lock: F,
) -> Option<OpenDocumentSnapshot>
where
    F: FnOnce(),
{
    // The parser entry is the primary per-document lock. Writers publish the
    // parser, template, and version while holding its write guard, so clone
    // every request-facing component before releasing this read guard.
    let parser = open_files.get(uri_str)?;
    after_open_lock();
    let tree = parser.tree()?.clone();
    let source = parser.source();
    let template_document = template_documents
        .get(uri_str)
        .map(|document| document.value().clone());
    let document_state = document_versions.get(uri_str).map(|state| *state);
    drop(parser);
    let file_symbols = extract_file_symbols(&tree, &source, uri_str);

    Some(OpenDocumentSnapshot {
        tree,
        source,
        template_document,
        document_state,
        file_symbols,
    })
}

fn open_document_snapshot_from_state(
    open_files: &DashMap<String, FileParser>,
    template_documents: &DashMap<String, TemplateDocument>,
    document_versions: &DashMap<String, OpenDocumentState>,
    uri_str: &str,
) -> Option<OpenDocumentSnapshot> {
    open_document_snapshot_from_state_with_lock_hook(
        open_files,
        template_documents,
        document_versions,
        uri_str,
        || {},
    )
}

struct OpenDocumentIndexCommitContext<'a> {
    open_files: &'a DashMap<String, FileParser>,
    template_documents: &'a DashMap<String, TemplateDocument>,
    document_versions: &'a DashMap<String, OpenDocumentState>,
    index: &'a WorkspaceIndex,
    root_index: Option<&'a WorkspaceIndex>,
    uri_str: &'a str,
}

fn commit_open_document_index_snapshot_if_current_with_hook<F>(
    ctx: OpenDocumentIndexCommitContext<'_>,
    snapshot: &OpenDocumentSnapshot,
    before_index_commit: F,
) -> bool
where
    F: FnOnce(),
{
    let references = snapshot.template_document.is_none().then(|| {
        collect_symbol_references_in_file(&snapshot.tree, &snapshot.source, &snapshot.file_symbols)
    });
    let dashmap::mapref::entry::Entry::Occupied(_open_entry) =
        ctx.open_files.entry(ctx.uri_str.to_string())
    else {
        return false;
    };
    if snapshot.document_state.is_none()
        || ctx.document_versions.get(ctx.uri_str).map(|state| *state) != snapshot.document_state
    {
        return false;
    }
    let current_template = ctx
        .template_documents
        .get(ctx.uri_str)
        .map(|template| template.value().clone());
    let template_matches = match (&snapshot.template_document, &current_template) {
        (None, None) => true,
        (Some(expected), Some(current)) => current.has_same_source_and_twig_context(expected),
        _ => false,
    };
    if !template_matches {
        return false;
    }

    before_index_commit();
    if let Some(references) = references {
        let root_index = ctx.root_index.unwrap_or(ctx.index);
        update_aggregate_and_root_index(
            ctx.index,
            root_index,
            ctx.uri_str,
            snapshot.file_symbols.clone(),
            references,
        );
    } else {
        let root_index = ctx.root_index.unwrap_or(ctx.index);
        remove_from_aggregate_and_root_index(ctx.index, root_index, ctx.uri_str);
    }
    true
}

fn commit_open_document_index_snapshot_if_current(
    ctx: OpenDocumentIndexCommitContext<'_>,
    snapshot: &OpenDocumentSnapshot,
) -> bool {
    commit_open_document_index_snapshot_if_current_with_hook(ctx, snapshot, || {})
}

#[derive(Clone, Copy)]
struct ClosedPhpIndexCommitContext<'a> {
    open_files: &'a DashMap<String, FileParser>,
    document_versions: &'a DashMap<String, OpenDocumentState>,
    reload_tokens: &'a DashMap<String, u64>,
    index: &'a WorkspaceIndex,
    root_index: Option<&'a WorkspaceIndex>,
    uri_str: &'a str,
    token: u64,
}

fn commit_closed_php_index_if_current_with_hook<F>(
    ctx: ClosedPhpIndexCommitContext<'_>,
    file_symbols: Option<php_lsp_types::FileSymbols>,
    references: Vec<php_lsp_types::SymbolReference>,
    before_open_lock: F,
) -> bool
where
    F: FnOnce(),
{
    before_open_lock();
    let dashmap::mapref::entry::Entry::Vacant(_open_entry) =
        ctx.open_files.entry(ctx.uri_str.to_string())
    else {
        return false;
    };
    if ctx.document_versions.contains_key(ctx.uri_str)
        || ctx
            .reload_tokens
            .get(ctx.uri_str)
            .is_none_or(|current| *current != ctx.token)
    {
        return false;
    }

    if let Some(file_symbols) = file_symbols {
        let root_index = ctx.root_index.unwrap_or(ctx.index);
        update_aggregate_and_root_index(
            ctx.index,
            root_index,
            ctx.uri_str,
            file_symbols,
            references,
        );
    } else {
        let root_index = ctx.root_index.unwrap_or(ctx.index);
        remove_from_aggregate_and_root_index(ctx.index, root_index, ctx.uri_str);
    }
    ctx.reload_tokens.remove(ctx.uri_str);
    true
}

fn commit_closed_php_index_if_current(
    ctx: ClosedPhpIndexCommitContext<'_>,
    file_symbols: Option<php_lsp_types::FileSymbols>,
    references: Vec<php_lsp_types::SymbolReference>,
) -> bool {
    commit_closed_php_index_if_current_with_hook(ctx, file_symbols, references, || {})
}

struct DiagnosticPublishRequest {
    uri: Uri,
    diagnostics: Vec<Diagnostic>,
    version: Option<i32>,
    expected_state: Option<OpenDocumentState>,
    expected_template: Option<TemplateDocument>,
    require_idle_index: bool,
    expected_runtime_generation: u64,
    indexing_workspace_folder: Option<PathBuf>,
    expected_runtime_config: ResolvedRuntimeConfiguration,
    expected_index: Arc<WorkspaceIndex>,
    computation_sequence: u64,
}

#[derive(Clone)]
struct DiagnosticsPublisher {
    shards: Arc<Vec<DiagnosticsPublisherShard>>,
    open_files: Arc<DashMap<String, FileParser>>,
    document_versions: Arc<DashMap<String, OpenDocumentState>>,
    template_documents: Arc<DashMap<String, TemplateDocument>>,
    next_computation_sequence: Arc<AtomicU64>,
    current_computation_sequences: Arc<DashMap<String, u64>>,
}

struct DiagnosticsPublisherShard {
    pending: Arc<StdMutex<HashMap<String, DiagnosticPublishRequest>>>,
    wake: mpsc::Sender<()>,
}

impl DiagnosticsPublisher {
    fn new(
        client: Client,
        open_files: Arc<DashMap<String, FileParser>>,
        document_versions: Arc<DashMap<String, OpenDocumentState>>,
        template_documents: Arc<DashMap<String, TemplateDocument>>,
        indexing_run: Arc<Mutex<HashMap<PathBuf, OperationCancellationToken>>>,
        runtime_state: Arc<Mutex<Arc<WorkspaceRuntimeState>>>,
    ) -> Self {
        let shard_count = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .clamp(1, DIAGNOSTIC_PUBLISHER_MAX_SHARDS);
        let next_computation_sequence = Arc::new(AtomicU64::new(1));
        let current_computation_sequences = Arc::new(DashMap::new());
        let shards = (0..shard_count)
            .map(|_| {
                spawn_diagnostics_publish_worker(
                    client.clone(),
                    open_files.clone(),
                    document_versions.clone(),
                    template_documents.clone(),
                    indexing_run.clone(),
                    runtime_state.clone(),
                    current_computation_sequences.clone(),
                )
            })
            .collect();
        Self {
            shards: Arc::new(shards),
            open_files,
            document_versions,
            template_documents,
            next_computation_sequence,
            current_computation_sequences,
        }
    }

    fn start_computation(&self, uri_str: &str) -> u64 {
        let sequence = self
            .next_computation_sequence
            .fetch_add(1, Ordering::Relaxed);
        self.current_computation_sequences
            .insert(uri_str.to_string(), sequence);
        sequence
    }

    fn publish(&self, request: DiagnosticPublishRequest) {
        let mut hasher = DefaultHasher::new();
        request.uri.as_str().hash(&mut hasher);
        let shard = &self.shards[hasher.finish() as usize % self.shards.len()];
        let uri_str = request.uri.as_str().to_string();
        match shard.pending.lock() {
            Ok(mut pending) => {
                if pending.get(&uri_str).is_some_and(|current| {
                    current.computation_sequence > request.computation_sequence
                        || current.expected_runtime_generation > request.expected_runtime_generation
                        || (diagnostic_publish_request_is_current(
                            current,
                            &self.open_files,
                            &self.document_versions,
                            &self.template_documents,
                        ) && !diagnostic_publish_request_is_current(
                            &request,
                            &self.open_files,
                            &self.document_versions,
                            &self.template_documents,
                        ))
                }) {
                    return;
                }
                pending.insert(uri_str, request);
            }
            Err(poisoned) => {
                let mut pending = poisoned.into_inner();
                if pending.get(&uri_str).is_some_and(|current| {
                    current.computation_sequence > request.computation_sequence
                        || current.expected_runtime_generation > request.expected_runtime_generation
                        || (diagnostic_publish_request_is_current(
                            current,
                            &self.open_files,
                            &self.document_versions,
                            &self.template_documents,
                        ) && !diagnostic_publish_request_is_current(
                            &request,
                            &self.open_files,
                            &self.document_versions,
                            &self.template_documents,
                        ))
                }) {
                    return;
                }
                pending.insert(uri_str, request);
            }
        }
        match shard.wake.try_send(()) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(())) => {}
            Err(mpsc::error::TrySendError::Closed(())) => {
                tracing::debug!("Skipping diagnostics because the publisher has stopped");
            }
        }
    }
}

fn diagnostic_publish_request_is_current(
    request: &DiagnosticPublishRequest,
    open_files: &DashMap<String, FileParser>,
    document_versions: &DashMap<String, OpenDocumentState>,
    template_documents: &DashMap<String, TemplateDocument>,
) -> bool {
    let uri_str = request.uri.as_str();
    let snapshot = open_document_snapshot_from_state(
        open_files,
        template_documents,
        document_versions,
        uri_str,
    );
    let (current_state, current_template) = snapshot
        .map(|snapshot| (snapshot.document_state, snapshot.template_document))
        .unwrap_or((None, None));
    if current_state != request.expected_state {
        return false;
    }
    match (&request.expected_template, &current_template) {
        (None, None) => true,
        (Some(snapshot), Some(current)) => current.has_same_source_and_twig_context(snapshot),
        _ => false,
    }
}

fn spawn_diagnostics_publish_worker(
    client: Client,
    open_files: Arc<DashMap<String, FileParser>>,
    document_versions: Arc<DashMap<String, OpenDocumentState>>,
    template_documents: Arc<DashMap<String, TemplateDocument>>,
    indexing_run: Arc<Mutex<HashMap<PathBuf, OperationCancellationToken>>>,
    runtime_state: Arc<Mutex<Arc<WorkspaceRuntimeState>>>,
    current_computation_sequences: Arc<DashMap<String, u64>>,
) -> DiagnosticsPublisherShard {
    let pending: Arc<StdMutex<HashMap<String, DiagnosticPublishRequest>>> =
        Arc::new(StdMutex::new(HashMap::new()));
    let worker_pending = pending.clone();
    let (wake, mut receiver) = mpsc::channel(1);
    tokio::spawn(async move {
        while receiver.recv().await.is_some() {
            loop {
                let requests: Vec<_> = match worker_pending.lock() {
                    Ok(mut pending) => pending.drain().map(|(_, request)| request).collect(),
                    Err(poisoned) => poisoned
                        .into_inner()
                        .drain()
                        .map(|(_, request)| request)
                        .collect(),
                };
                if requests.is_empty() {
                    break;
                }
                for request in requests {
                    let uri_str = request.uri.as_str().to_string();
                    if current_computation_sequences
                        .get(&uri_str)
                        .is_none_or(|sequence| *sequence != request.computation_sequence)
                    {
                        continue;
                    }
                    if !diagnostic_publish_request_is_current(
                        &request,
                        &open_files,
                        &document_versions,
                        &template_documents,
                    ) {
                        continue;
                    }
                    let runtime_snapshot = runtime_state.lock().await.clone();
                    if !diagnostic_runtime_request_is_current(&request, &runtime_snapshot) {
                        continue;
                    }
                    if request.require_idle_index
                        && indexing_run_is_active_for_workspace(
                            &indexing_run,
                            request.indexing_workspace_folder.as_deref(),
                        )
                        .await
                    {
                        continue;
                    }
                    if !diagnostic_publish_request_is_current(
                        &request,
                        &open_files,
                        &document_versions,
                        &template_documents,
                    ) {
                        continue;
                    }

                    let publish_started = Instant::now();
                    let publish_span = tracing::debug_span!(
                        "diagnostics.publish",
                        uri = %uri_str,
                        version = ?request.version,
                        duration_ms = tracing::field::Empty,
                    );
                    client
                        .publish_diagnostics(request.uri, request.diagnostics, request.version)
                        .instrument(publish_span.clone())
                        .await;
                    publish_span
                        .record("duration_ms", publish_started.elapsed().as_millis() as u64);
                }
            }
        }
    });
    DiagnosticsPublisherShard { pending, wake }
}

fn diagnostic_runtime_request_is_current(
    request: &DiagnosticPublishRequest,
    state: &WorkspaceRuntimeState,
) -> bool {
    let workspace = workspace_config_for_uri_from_configs(&state.configs, request.uri.as_str());
    let (runtime_config, index) = workspace
        .as_ref()
        .map(|workspace| (&workspace.runtime_config, &workspace.index))
        .unwrap_or((&state.fallback, &state.fallback_index));
    runtime_config.php_version == request.expected_runtime_config.php_version
        && runtime_config.diagnostics_mode == request.expected_runtime_config.diagnostics_mode
        && runtime_config.diagnostic_severity == request.expected_runtime_config.diagnostic_severity
        && runtime_config.diagnostic_budget == request.expected_runtime_config.diagnostic_budget
        && runtime_config.phpstan == request.expected_runtime_config.phpstan
        && runtime_config.psalm == request.expected_runtime_config.psalm
        && Arc::ptr_eq(index, &request.expected_index)
}

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
    index: &'a WorkspaceIndex,
    tree: &'a tree_sitter::Tree,
    source_uri: &'a str,
    source: &'a str,
    file_symbols: &'a php_lsp_types::FileSymbols,
    type_cache: &'a RequestTypeCache,
    line: u32,
    byte_col: u32,
}

const DEFAULT_MEMBER_TYPE_DIAGNOSTIC_NODE_BUDGET: usize = 512;
const DEFAULT_PARTIAL_ANALYSIS_DIAGNOSTIC: bool = true;

fn document_version_is_newer(current: Option<i32>, incoming: i32) -> bool {
    current.is_none_or(|current| incoming > current)
}

fn open_document_state_can_replace(
    current: OpenDocumentState,
    incoming: OpenDocumentState,
) -> bool {
    incoming.generation > current.generation
        || (incoming.generation == current.generation && incoming.version >= current.version)
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
    indexing_active: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

#[derive(Clone)]
struct WorkspaceIndexingCancellation {
    workspace_folder: PathBuf,
    token: OperationCancellationToken,
}

impl OperationCancellationToken {
    fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            indexing_active: Arc::new(AtomicBool::new(true)),
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

    fn mark_indexing_complete(&self) {
        self.indexing_active.store(false, Ordering::SeqCst);
    }

    fn is_indexing_active(&self) -> bool {
        self.indexing_active.load(Ordering::SeqCst) && !self.is_cancelled()
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

async fn finish_indexing_run_state(
    indexing_run: &Arc<Mutex<HashMap<PathBuf, OperationCancellationToken>>>,
    token: &OperationCancellationToken,
) {
    let mut current = indexing_run.lock().await;
    current.retain(|_, active| !active.is_same(token));
}

async fn finish_indexing_run_if_cancelled(
    indexing_run: &Arc<Mutex<HashMap<PathBuf, OperationCancellationToken>>>,
    token: &OperationCancellationToken,
) -> bool {
    if token.is_cancelled() {
        finish_indexing_run_state(indexing_run, token).await;
        true
    } else {
        false
    }
}

async fn indexing_run_is_active_for_workspace(
    indexing_run: &Arc<Mutex<HashMap<PathBuf, OperationCancellationToken>>>,
    workspace_folder: Option<&Path>,
) -> bool {
    let Some(workspace_folder) = workspace_folder else {
        return false;
    };
    indexing_run
        .lock()
        .await
        .get(workspace_folder)
        .is_some_and(OperationCancellationToken::is_indexing_active)
}

fn diagnostics_mode_for_indexing_state(
    mode: DiagnosticsMode,
    indexing_active: bool,
) -> DiagnosticsMode {
    if indexing_active && mode == DiagnosticsMode::BasicSemantic {
        DiagnosticsMode::SyntaxOnly
    } else {
        mode
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

async fn clear_request_fs_caches(
    framework_string_key_cache: &Arc<Mutex<FrameworkStringKeyCache>>,
    twig_context_disk_cache: &Arc<Mutex<TwigContextDiskCache>>,
) {
    framework_string_key_cache.lock().await.clear();
    twig_context_disk_cache.lock().await.clear();
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

    #[cfg(test)]
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

    async fn resolve_for_workspace_blocking(&self, workspace_root: Option<&Path>) -> Self {
        if self.provider != "auto" {
            return self.clone();
        }

        let Some(workspace_root) = workspace_root.map(Path::to_path_buf) else {
            return self.clone();
        };
        let Some(tool) =
            lsp::formatting::detect_project_formatter_tool_blocking(workspace_root).await
        else {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DiagnosticBudgetConfig {
    pub(crate) member_type_node_budget: Option<usize>,
    pub(crate) partial_analysis_diagnostic: bool,
}

impl Default for DiagnosticBudgetConfig {
    fn default() -> Self {
        Self {
            member_type_node_budget: Some(DEFAULT_MEMBER_TYPE_DIAGNOSTIC_NODE_BUDGET),
            partial_analysis_diagnostic: DEFAULT_PARTIAL_ANALYSIS_DIAGNOSTIC,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DiagnosticsRuntimeConfig {
    pub(crate) mode: DiagnosticsMode,
    pub(crate) severity: DiagnosticSeverityConfig,
    pub(crate) budget: DiagnosticBudgetConfig,
    pub(crate) php_version: PhpVersion,
}

impl Default for DiagnosticsRuntimeConfig {
    fn default() -> Self {
        Self {
            mode: DiagnosticsMode::default(),
            severity: DiagnosticSeverityConfig::default(),
            budget: DiagnosticBudgetConfig::default(),
            php_version: PhpVersion::DEFAULT,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedRuntimeConfiguration {
    php_version: PhpVersion,
    diagnostics_mode: DiagnosticsMode,
    diagnostic_severity: DiagnosticSeverityConfig,
    diagnostic_budget: DiagnosticBudgetConfig,
    composer_enabled: bool,
    index_vendor: bool,
    include_paths: Vec<PathBuf>,
    exclude_paths: Vec<PathBuf>,
    stub_extensions: Option<Vec<String>>,
    log_level: String,
    stubs_path: Option<PathBuf>,
    formatting: FormattingConfig,
    phpstan: PhpStanConfig,
    psalm: PsalmConfig,
    analyzer_code_actions: AnalyzerCodeActionConfig,
}

#[derive(Debug, Clone)]
struct ClientWorkspaceConfiguration {
    workspace_folder: PathBuf,
    settings: serde_json::Value,
}

#[derive(Debug, Clone)]
struct ClientConfigurationSnapshot {
    global_settings: serde_json::Value,
    workspace_folders: Vec<ClientWorkspaceConfiguration>,
    bundled_stubs_path: Option<PathBuf>,
}

impl ClientConfigurationSnapshot {
    fn from_value(raw: &serde_json::Value) -> Self {
        if raw
            .get("configurationVersion")
            .and_then(serde_json::Value::as_u64)
            == Some(2)
        {
            let global_settings = raw
                .get("global")
                .map(normalize_client_settings)
                .unwrap_or_else(|| serde_json::json!({}));
            let workspace_folders = raw
                .get("workspaceFolders")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|entry| {
                    let uri = entry.get("uri")?.as_str()?;
                    let workspace_folder = uri_to_path(uri)?;
                    let settings = entry
                        .get("settings")
                        .map(normalize_client_settings)
                        .unwrap_or_else(|| serde_json::json!({}));
                    Some(ClientWorkspaceConfiguration {
                        workspace_folder,
                        settings,
                    })
                })
                .collect();
            let bundled_stubs_path = raw
                .get("bundledStubsPath")
                .and_then(serde_json::Value::as_str)
                .map(PathBuf::from);
            return Self {
                global_settings,
                workspace_folders,
                bundled_stubs_path,
            };
        }

        let global_settings = normalize_client_settings(raw);
        let bundled_stubs_path = global_settings
            .get("bundledStubsPath")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from);
        Self {
            global_settings,
            workspace_folders: Vec::new(),
            bundled_stubs_path,
        }
    }

    fn settings_for_workspace_folder(&self, root: &Path) -> serde_json::Value {
        let mut settings = self.global_settings.clone();
        if let Some(folder) = self
            .workspace_folders
            .iter()
            .find(|folder| folder.workspace_folder == root)
        {
            merge_json_objects(&mut settings, &folder.settings);
        }
        self.add_bundled_stubs_path(&mut settings);
        settings
    }

    fn fallback_settings(&self) -> serde_json::Value {
        let mut settings = self.global_settings.clone();
        self.add_bundled_stubs_path(&mut settings);
        settings
    }

    fn add_bundled_stubs_path(&self, settings: &mut serde_json::Value) {
        let Some(path) = self.bundled_stubs_path.as_ref() else {
            return;
        };
        let Some(object) = settings.as_object_mut() else {
            *settings = serde_json::json!({
                "bundledStubsPath": path.to_string_lossy().to_string()
            });
            return;
        };
        object.insert(
            "bundledStubsPath".to_string(),
            serde_json::Value::String(path.to_string_lossy().to_string()),
        );
    }
}

struct LoadedWorkspaceRuntime {
    fallback: ResolvedRuntimeConfiguration,
    configs: Vec<WorkspaceRootConfig>,
    messages: Vec<String>,
}

impl Default for ResolvedRuntimeConfiguration {
    fn default() -> Self {
        Self {
            php_version: PhpVersion::DEFAULT,
            diagnostics_mode: DiagnosticsMode::default(),
            diagnostic_severity: DiagnosticSeverityConfig::default(),
            diagnostic_budget: DiagnosticBudgetConfig::default(),
            composer_enabled: true,
            index_vendor: true,
            include_paths: Vec::new(),
            exclude_paths: Vec::new(),
            stub_extensions: None,
            log_level: "info".to_string(),
            stubs_path: None,
            formatting: FormattingConfig::default(),
            phpstan: PhpStanConfig::default(),
            psalm: PsalmConfig::default(),
            analyzer_code_actions: AnalyzerCodeActionConfig::default(),
        }
    }
}

impl ResolvedRuntimeConfiguration {
    fn from_settings(raw_settings: &serde_json::Value) -> Self {
        let settings = php_lsp_settings(raw_settings);
        let mut resolved = Self::default();

        if let Some(raw) = settings_string(settings, "phpVersion", &["phpVersion"]) {
            if let Some(parsed) = PhpVersion::parse(raw) {
                resolved.php_version = parsed;
            } else {
                tracing::warn!("Ignoring invalid phpVersion: {}", raw);
            }
        }
        if let Some(raw) = settings_string(settings, "diagnosticsMode", &["diagnostics", "mode"]) {
            if let Some(parsed) = DiagnosticsMode::parse(raw) {
                resolved.diagnostics_mode = parsed;
            } else {
                tracing::warn!("Ignoring invalid diagnostics mode: {}", raw);
            }
        }
        if let Some(raw) = settings_value(
            settings,
            "diagnosticsSeverity",
            &["diagnostics", "severity"],
        ) {
            if let Some(parsed) = DiagnosticSeverityConfig::parse(raw) {
                resolved.diagnostic_severity = parsed;
            } else {
                tracing::warn!("Ignoring invalid diagnostics severity settings: {raw}");
            }
        }
        resolved.diagnostic_budget = diagnostic_budget_config_from_settings(settings);

        if let Some(enabled) = settings_bool(settings, "composerEnabled", &["composer", "enabled"])
        {
            resolved.composer_enabled = enabled;
        }
        if let Some(enabled) = settings_bool(settings, "indexVendor", &["indexVendor"]) {
            resolved.index_vendor = enabled;
        }
        if let Some(paths) = settings_string_array(settings, "includePaths", &["includePaths"]) {
            resolved.include_paths = normalize_config_paths(paths);
        }
        if let Some(paths) = settings_string_array(settings, "excludePaths", &["excludePaths"]) {
            resolved.exclude_paths = normalize_config_paths(paths);
        }
        resolved.stub_extensions =
            settings_string_array(settings, "stubExtensions", &["stubs", "extensions"]);
        if let Some(level) = settings_string(settings, "logLevel", &["logLevel"]) {
            resolved.log_level = level.trim().to_ascii_lowercase();
        }
        resolved.stubs_path = settings_string_aliases(
            settings,
            "stubsPath",
            &[&["stubs", "path"], &["bundledStubsPath"]],
        )
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);

        let formatting_defaults = FormattingConfig::default();
        let formatting_provider =
            settings_string(settings, "formattingProvider", &["formatting", "provider"]);
        let formatting_command =
            settings_value(settings, "formattingCommand", &["formatting", "command"])
                .and_then(serde_json::Value::as_str);
        let formatting_timeout_ms = settings_u64_aliases(
            settings,
            "formattingTimeoutMs",
            &[&["formatting", "timeoutMs"], &["formatting", "timeout"]],
        );
        let formatting_provider = formatting_provider.map(str::to_string).unwrap_or_else(|| {
            if formatting_command.is_some() {
                "custom".to_string()
            } else {
                formatting_defaults.provider.clone()
            }
        });
        resolved.formatting = FormattingConfig::from_options(
            Some(&formatting_provider),
            formatting_command,
            formatting_timeout_ms.or(Some(formatting_defaults.timeout_ms)),
        );

        if let Some(enabled) = settings_bool(settings, "phpstanEnabled", &["phpstan", "enabled"]) {
            resolved.phpstan.enabled = enabled;
        }
        if let Some(command) = settings_string(settings, "phpstanCommand", &["phpstan", "command"])
        {
            let command = command.trim();
            if !command.is_empty() {
                resolved.phpstan.command = command.to_string();
            }
        }
        if let Some(timeout_ms) = settings_u64_aliases(
            settings,
            "phpstanTimeoutMs",
            &[&["phpstan", "timeoutMs"], &["phpstan", "timeout"]],
        ) {
            resolved.phpstan.timeout_ms = timeout_ms.max(1_000);
        }
        if let Some(memory_limit) = settings_string_aliases(
            settings,
            "phpstanMemoryLimit",
            &[&["phpstan", "memoryLimit"], &["phpstan", "memory_limit"]],
        ) {
            let memory_limit = memory_limit.trim();
            resolved.phpstan.memory_limit =
                (!memory_limit.is_empty()).then(|| memory_limit.to_string());
        }

        if let Some(enabled) = settings_bool(settings, "psalmEnabled", &["psalm", "enabled"]) {
            resolved.psalm.enabled = enabled;
        }
        if let Some(command) = settings_string(settings, "psalmCommand", &["psalm", "command"]) {
            let command = command.trim();
            if !command.is_empty() {
                resolved.psalm.command = command.to_string();
            }
        }
        if let Some(timeout_ms) = settings_u64_aliases(
            settings,
            "psalmTimeoutMs",
            &[&["psalm", "timeoutMs"], &["psalm", "timeout"]],
        ) {
            resolved.psalm.timeout_ms = timeout_ms.max(1_000);
        }
        if let Some(enabled) = settings_bool(
            settings,
            "analyzerCodeActionsEnabled",
            &["analyzerCodeActions", "enabled"],
        ) {
            resolved.analyzer_code_actions.enabled = enabled;
        }

        resolved
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct AppliedConfiguration {
    diagnostics_changed: bool,
    stubs_changed: bool,
    indexing_changed: bool,
}

#[derive(Default)]
struct WorkspaceConfigurationApplication {
    diagnostics_workspace_folders: Vec<PathBuf>,
    stubs_workspace_folders: Vec<PathBuf>,
    indexing_workspace_folders: Vec<PathBuf>,
    republish_all_diagnostics: bool,
    rebuild_aggregate: bool,
    reload_fallback_stubs: bool,
    previous_indexes: Vec<Arc<WorkspaceIndex>>,
    removed_workspace_folders: Vec<PathBuf>,
}

impl WorkspaceConfigurationApplication {
    fn record(&mut self, workspace_folder: &Path, changes: AppliedConfiguration) {
        if changes.diagnostics_changed {
            push_unique_path(
                &mut self.diagnostics_workspace_folders,
                workspace_folder.to_path_buf(),
            );
        }
        if changes.stubs_changed {
            push_unique_path(
                &mut self.stubs_workspace_folders,
                workspace_folder.to_path_buf(),
            );
        }
        if changes.indexing_changed {
            push_unique_path(
                &mut self.indexing_workspace_folders,
                workspace_folder.to_path_buf(),
            );
        }
    }
}

fn runtime_configuration_changes(
    previous: &ResolvedRuntimeConfiguration,
    current: &ResolvedRuntimeConfiguration,
) -> AppliedConfiguration {
    AppliedConfiguration {
        diagnostics_changed: previous.php_version != current.php_version
            || previous.diagnostics_mode != current.diagnostics_mode
            || previous.diagnostic_severity != current.diagnostic_severity
            || previous.diagnostic_budget != current.diagnostic_budget
            || previous.phpstan != current.phpstan
            || previous.psalm != current.psalm,
        stubs_changed: previous.php_version != current.php_version
            || previous.stub_extensions != current.stub_extensions
            || previous.stubs_path != current.stubs_path,
        indexing_changed: previous.composer_enabled != current.composer_enabled
            || previous.index_vendor != current.index_vendor
            || previous.include_paths != current.include_paths
            || previous.exclude_paths != current.exclude_paths,
    }
}

#[derive(Debug, Clone)]
struct WorkspaceIndexingOptions {
    include_paths: Vec<PathBuf>,
    exclude_paths: Vec<PathBuf>,
    cache_config: IndexCacheConfig,
    work_done_progress_supported: bool,
}

#[derive(Clone, Copy)]
struct WorkspaceLiveIndexContext<'a> {
    index: &'a WorkspaceIndex,
    root_index: &'a WorkspaceIndex,
    open_files: &'a DashMap<String, FileParser>,
    template_documents: &'a DashMap<String, TemplateDocument>,
    document_versions: &'a DashMap<String, OpenDocumentState>,
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FrameworkStringKeyCacheKey {
    root: PathBuf,
    domain: String,
}

#[derive(Debug)]
struct FrameworkStringKeyCache {
    capacity: usize,
    entries: HashMap<FrameworkStringKeyCacheKey, Vec<crate::framework::FrameworkStringKey>>,
    order: VecDeque<FrameworkStringKeyCacheKey>,
}

impl Default for FrameworkStringKeyCache {
    fn default() -> Self {
        Self {
            capacity: FRAMEWORK_STRING_KEY_CACHE_CAPACITY,
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }
}

impl FrameworkStringKeyCache {
    fn get(
        &mut self,
        key: &FrameworkStringKeyCacheKey,
    ) -> Option<Vec<crate::framework::FrameworkStringKey>> {
        let value = self.entries.get(key).cloned()?;
        self.touch(key.clone());
        Some(value)
    }

    fn insert(
        &mut self,
        key: FrameworkStringKeyCacheKey,
        value: Vec<crate::framework::FrameworkStringKey>,
    ) {
        self.entries.insert(key.clone(), value);
        self.touch(key);
        self.evict_over_capacity();
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    fn touch(&mut self, key: FrameworkStringKeyCacheKey) {
        if let Some(position) = self.order.iter().position(|existing| existing == &key) {
            self.order.remove(position);
        }
        self.order.push_back(key);
    }

    fn evict_over_capacity(&mut self) {
        while self.order.len() > self.capacity {
            if let Some(key) = self.order.pop_front() {
                self.entries.remove(&key);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TwigContextDiskCacheKey {
    root: PathBuf,
    index_identity: usize,
    template_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TwigContextFileVariables {
    uri: String,
    variables: Vec<TemplateVariableType>,
}

#[derive(Debug)]
struct TwigContextDiskCache {
    capacity: usize,
    entries: HashMap<TwigContextDiskCacheKey, Vec<TwigContextFileVariables>>,
    order: VecDeque<TwigContextDiskCacheKey>,
}

impl Default for TwigContextDiskCache {
    fn default() -> Self {
        Self {
            capacity: TWIG_CONTEXT_DISK_CACHE_CAPACITY,
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }
}

impl TwigContextDiskCache {
    fn get(&mut self, key: &TwigContextDiskCacheKey) -> Option<Vec<TwigContextFileVariables>> {
        let value = self.entries.get(key).cloned()?;
        self.touch(key.clone());
        Some(value)
    }

    fn insert(&mut self, key: TwigContextDiskCacheKey, value: Vec<TwigContextFileVariables>) {
        self.entries.insert(key.clone(), value);
        self.touch(key);
        self.evict_over_capacity();
    }

    fn evict_entries_for_source_uri(&mut self, source_uri: &str) -> usize {
        let stale_keys: HashSet<_> = self
            .entries
            .iter()
            .filter(|(_, files)| files.iter().any(|file| file.uri == source_uri))
            .map(|(key, _)| key.clone())
            .collect();
        let evicted = stale_keys.len();
        if evicted == 0 {
            return 0;
        }

        self.entries.retain(|key, _| !stale_keys.contains(key));
        self.order.retain(|key| !stale_keys.contains(key));
        evicted
    }

    fn evict_index(&mut self, index: &Arc<WorkspaceIndex>) -> usize {
        let index_identity = Arc::as_ptr(index) as usize;
        let stale_keys: HashSet<_> = self
            .entries
            .keys()
            .filter(|key| key.index_identity == index_identity)
            .cloned()
            .collect();
        let evicted = stale_keys.len();
        self.entries.retain(|key, _| !stale_keys.contains(key));
        self.order.retain(|key| !stale_keys.contains(key));
        evicted
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    fn touch(&mut self, key: TwigContextDiskCacheKey) {
        if let Some(position) = self.order.iter().position(|existing| existing == &key) {
            self.order.remove(position);
        }
        self.order.push_back(key);
    }

    fn evict_over_capacity(&mut self) {
        while self.order.len() > self.capacity {
            if let Some(key) = self.order.pop_front() {
                self.entries.remove(&key);
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct WorkspaceRootConfig {
    pub(crate) workspace_folder: PathBuf,
    pub(crate) root: PathBuf,
    pub(crate) namespace_map: Option<NamespaceMap>,
    runtime_config: ResolvedRuntimeConfiguration,
    index: Arc<WorkspaceIndex>,
    vendor_file_lru: Arc<Mutex<VendorFileLru>>,
}

#[derive(Clone)]
struct WorkspaceRuntimeState {
    fallback: ResolvedRuntimeConfiguration,
    fallback_index: Arc<WorkspaceIndex>,
    configs: Vec<WorkspaceRootConfig>,
    generation: u64,
}

impl Default for WorkspaceRuntimeState {
    fn default() -> Self {
        Self {
            fallback: ResolvedRuntimeConfiguration::default(),
            fallback_index: Arc::new(WorkspaceIndex::new()),
            configs: Vec::new(),
            generation: 0,
        }
    }
}

#[derive(Clone)]
struct WorkspaceRequestContext {
    state: Arc<WorkspaceRuntimeState>,
    workspace: Option<WorkspaceRootConfig>,
}

impl WorkspaceRequestContext {
    fn runtime_config(&self) -> &ResolvedRuntimeConfiguration {
        self.workspace
            .as_ref()
            .map(|workspace| &workspace.runtime_config)
            .unwrap_or(&self.state.fallback)
    }

    fn index(&self, _aggregate: &Arc<WorkspaceIndex>) -> Arc<WorkspaceIndex> {
        self.workspace
            .as_ref()
            .map(|workspace| workspace.index.clone())
            .unwrap_or_else(|| self.state.fallback_index.clone())
    }

    fn root(&self) -> Option<&Path> {
        self.workspace
            .as_ref()
            .map(|workspace| workspace.root.as_path())
    }

    fn namespace_map(&self) -> Option<&NamespaceMap> {
        self.workspace
            .as_ref()
            .and_then(|workspace| workspace.namespace_map.as_ref())
    }
}

fn workspace_config_for_path_from_configs(
    configs: &[WorkspaceRootConfig],
    path: &Path,
) -> Option<WorkspaceRootConfig> {
    configs
        .iter()
        .filter_map(|config| {
            let folder_score = path
                .starts_with(&config.workspace_folder)
                .then(|| (3u8, config.workspace_folder.components().count()));
            let root_score = path
                .starts_with(&config.root)
                .then(|| (2u8, config.root.components().count()));
            let include_score = config
                .runtime_config
                .include_paths
                .iter()
                .map(|include| resolve_config_path(&config.root, include))
                .filter(|include| path.starts_with(include))
                .map(|include| (1u8, include.components().count()))
                .max();
            folder_score
                .into_iter()
                .chain(root_score)
                .chain(include_score)
                .max()
                .map(|score| (config, score))
        })
        .max_by(|(left, left_score), (right, right_score)| {
            left_score
                .cmp(right_score)
                .then_with(|| right.workspace_folder.cmp(&left.workspace_folder))
        })
        .map(|(config, _)| config.clone())
}

fn workspace_config_for_uri_from_configs(
    configs: &[WorkspaceRootConfig],
    uri_str: &str,
) -> Option<WorkspaceRootConfig> {
    let path = uri_to_path(uri_str)?;
    workspace_config_for_path_from_configs(configs, &path)
}

fn workspace_configs_for_path_scope(
    configs: &[WorkspaceRootConfig],
    path: &Path,
) -> Vec<WorkspaceRootConfig> {
    configs
        .iter()
        .filter(|config| {
            path.starts_with(&config.workspace_folder)
                || path.starts_with(&config.root)
                || config
                    .runtime_config
                    .include_paths
                    .iter()
                    .map(|include| resolve_config_path(&config.root, include))
                    .any(|include| path.starts_with(include))
        })
        .cloned()
        .collect()
}

fn workspace_indexes_for_uri(
    state: &WorkspaceRuntimeState,
    uri_str: &str,
    eligible_only: bool,
) -> Vec<Arc<WorkspaceIndex>> {
    let (mut indexes, has_workspace_scope) = uri_to_path(uri_str)
        .map(|path| {
            let scoped = workspace_configs_for_path_scope(&state.configs, &path);
            let has_workspace_scope = !scoped.is_empty();
            let indexes = scoped
                .into_iter()
                .filter(|config| {
                    !eligible_only
                        || !path_is_excluded(
                            &path,
                            &config.root,
                            &config.runtime_config.exclude_paths,
                        ) && (!path.starts_with(config.root.join("vendor"))
                            || config.runtime_config.index_vendor)
                })
                .map(|config| config.index)
                .collect::<Vec<_>>();
            (indexes, has_workspace_scope)
        })
        .unwrap_or_default();
    if !has_workspace_scope {
        indexes.push(state.fallback_index.clone());
    }
    let mut unique = Vec::new();
    for index in indexes {
        if !unique.iter().any(|existing| Arc::ptr_eq(existing, &index)) {
            unique.push(index);
        }
    }
    unique
}

fn update_aggregate_and_root_index(
    aggregate: &WorkspaceIndex,
    root_index: &WorkspaceIndex,
    uri: &str,
    file_symbols: php_lsp_types::FileSymbols,
    references: Vec<php_lsp_types::SymbolReference>,
) {
    root_index.update_file_with_references(uri, file_symbols.clone(), references.clone());
    if !std::ptr::eq(aggregate, root_index) {
        aggregate.update_file_with_references(uri, file_symbols, references);
    }
}

fn remove_from_aggregate_and_root_index(
    aggregate: &WorkspaceIndex,
    root_index: &WorkspaceIndex,
    uri: &str,
) {
    root_index.remove_file(uri);
    if !std::ptr::eq(aggregate, root_index) {
        aggregate.remove_file(uri);
    }
}

fn clear_non_stub_symbols(
    index: &WorkspaceIndex,
    open_files: Option<&DashMap<String, FileParser>>,
) {
    let uris: Vec<String> = index
        .file_symbols
        .iter()
        .filter(|entry| {
            !entry.key().starts_with("phpstub://")
                && open_files.is_none_or(|open_files| !open_files.contains_key(entry.key()))
        })
        .map(|entry| entry.key().clone())
        .collect();
    for uri in uris {
        index.remove_file(&uri);
    }
}

fn copy_non_stub_symbols(source: &WorkspaceIndex, destination: &WorkspaceIndex) {
    for entry in source.file_symbols.iter() {
        if entry.key().starts_with("phpstub://") {
            continue;
        }
        let uri = entry.key().clone();
        let references = source
            .file_references
            .get(&uri)
            .map(|references| references.value().clone())
            .unwrap_or_default();
        destination.update_file_with_references(&uri, entry.value().as_ref().clone(), references);
    }
}

fn rebuild_aggregate_index(
    aggregate: &WorkspaceIndex,
    configs: &[WorkspaceRootConfig],
    open_files: &DashMap<String, FileParser>,
    template_documents: &DashMap<String, TemplateDocument>,
    document_versions: &DashMap<String, OpenDocumentState>,
) {
    let uris: Vec<String> = aggregate
        .file_symbols
        .iter()
        .map(|entry| entry.key().clone())
        .collect();
    for uri in uris {
        aggregate.remove_file(&uri);
    }
    for config in configs {
        copy_non_stub_symbols(&config.index, aggregate);
    }
    let open_uris: Vec<String> = open_files.iter().map(|entry| entry.key().clone()).collect();
    for uri in open_uris {
        let Some(snapshot) = open_document_snapshot_from_state(
            open_files,
            template_documents,
            document_versions,
            &uri,
        ) else {
            continue;
        };
        let Some(references) = snapshot.template_document.is_none().then(|| {
            collect_symbol_references_in_file(
                &snapshot.tree,
                &snapshot.source,
                &snapshot.file_symbols,
            )
        }) else {
            continue;
        };
        aggregate.update_file_with_references(&uri, snapshot.file_symbols, references);
    }
}

const VENDOR_FILE_LRU_CAPACITY: usize = 512;
const FRAMEWORK_STRING_KEY_CACHE_CAPACITY: usize = 32;
const TWIG_CONTEXT_DISK_CACHE_CAPACITY: usize = 64;
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
    pub(crate) classmap: Vec<PathBuf>,
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

fn diagnostic_member_type_node_budget_setting(settings: &serde_json::Value) -> Option<u64> {
    settings_u64_aliases(
        settings,
        "diagnosticsMemberTypeNodeBudget",
        &[
            &["diagnostics", "memberTypeNodeBudget"],
            &["diagnostics", "memberTypeBudget"],
        ],
    )
}

fn diagnostic_member_type_node_budget_from_u64(raw_budget: u64) -> Option<Option<usize>> {
    if raw_budget == 0 {
        return Some(None);
    }
    usize::try_from(raw_budget).ok().map(Some)
}

pub(crate) fn diagnostic_budget_config_from_settings(
    settings: &serde_json::Value,
) -> DiagnosticBudgetConfig {
    let settings = php_lsp_settings(settings);
    let mut config = DiagnosticBudgetConfig::default();

    if let Some(raw_budget) = diagnostic_member_type_node_budget_setting(settings) {
        if let Some(parsed) = diagnostic_member_type_node_budget_from_u64(raw_budget) {
            config.member_type_node_budget = parsed;
        }
    }

    if let Some(enabled) = settings_bool(
        settings,
        "diagnosticsPartialAnalysisDiagnostic",
        &["diagnostics", "partialAnalysisDiagnostic"],
    ) {
        config.partial_analysis_diagnostic = enabled;
    }

    config
}

/// Main LSP backend holding all state.
pub struct PhpLspBackend {
    /// Client handle for sending notifications to VS Code.
    client: Client,
    /// Open document parsers (URI string → FileParser).
    open_files: Arc<DashMap<String, FileParser>>,
    /// Open Blade-like template documents backed by virtual PHP parsers.
    template_documents: Arc<DashMap<String, TemplateDocument>>,
    /// Latest LSP version and server-side lifetime generation for each open document.
    document_versions: Arc<DashMap<String, OpenDocumentState>>,
    /// Document generations that must receive a full-text change before ranged edits resume.
    documents_requiring_full_sync: Arc<DashMap<String, u64>>,
    /// Close-reload token for restoring the on-disk index after discarding unsaved edits.
    closed_document_reload_tokens: Arc<DashMap<String, u64>>,
    /// Monotonic source for document lifetime generations; LSP versions may reset on reopen.
    next_document_generation: AtomicU64,
    /// Per-document debounce tasks for fast diagnostics after didChange.
    diagnostic_debounce_tasks: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
    /// Serializes diagnostic notifications per document without blocking handlers on client I/O.
    diagnostics_publisher: DiagnosticsPublisher,
    /// Per-document external analyzer runs that can be cancelled by newer document events.
    analyzer_runs: Arc<Mutex<HashMap<String, OperationCancellationToken>>>,
    /// Per-document external formatter runs that can be cancelled by newer document events.
    formatter_runs: Arc<Mutex<HashMap<String, OperationCancellationToken>>>,
    /// Current background workspace indexing run.
    indexing_run: Arc<Mutex<HashMap<PathBuf, OperationCancellationToken>>>,
    /// Global workspace symbol index.
    index: Arc<WorkspaceIndex>,
    aggregate_rebuild: Arc<Mutex<()>>,
    /// Workspace root path (set during initialize).
    workspace_root: Mutex<Option<PathBuf>>,
    /// Workspace roots from initialize/workspaceFolders after composer discovery.
    workspace_roots: Mutex<Vec<PathBuf>>,
    /// Namespace map from composer.json.
    namespace_map: Mutex<Option<NamespaceMap>>,
    /// Atomically published per-workspace configuration, ownership, and indexes.
    runtime_state: Arc<Mutex<Arc<WorkspaceRuntimeState>>>,
    configuration_reload: Mutex<()>,
    next_runtime_generation: AtomicU64,
    /// Trace level from InitializeParams (off/messages/verbose).
    trace_level: Mutex<TraceValue>,
    /// Last explicit client initialization/configuration settings.
    client_settings: Mutex<serde_json::Value>,
    /// Path to bundled phpstorm-stubs (from client initializationOptions).
    #[cfg(test)]
    stubs_path: Mutex<Option<PathBuf>>,
    /// Target PHP version from client initializationOptions.
    #[cfg(test)]
    php_version: Mutex<PhpVersion>,
    /// Diagnostics level from phpLsp.diagnostics.mode.
    #[cfg(test)]
    diagnostics_mode: Mutex<DiagnosticsMode>,
    /// Per-category severity controls for php-lsp diagnostics.
    #[cfg(test)]
    diagnostic_severity: Mutex<DiagnosticSeverityConfig>,
    /// Latency budget controls for expensive in-process diagnostics.
    #[cfg(test)]
    diagnostic_budget: Mutex<DiagnosticBudgetConfig>,
    /// PHPStan subprocess diagnostics configuration.
    #[cfg(test)]
    phpstan_config: Mutex<PhpStanConfig>,
    /// Psalm subprocess diagnostics configuration.
    #[cfg(test)]
    psalm_config: Mutex<PsalmConfig>,
    /// Opt-in code actions for external analyzer diagnostics.
    #[cfg(test)]
    analyzer_code_actions: Mutex<AnalyzerCodeActionConfig>,
    /// Whether composer.json autoload discovery is enabled.
    #[cfg(test)]
    composer_enabled: Mutex<bool>,
    /// Whether lazy vendor indexing is enabled.
    #[cfg(test)]
    index_vendor: Mutex<bool>,
    /// Additional files/directories included in workspace indexing.
    #[cfg(test)]
    include_paths: Mutex<Vec<PathBuf>>,
    /// Files/directories excluded from workspace indexing.
    #[cfg(test)]
    exclude_paths: Mutex<Vec<PathBuf>>,
    /// Configured phpstorm-stubs extension directory names.
    ///
    /// `None` means use defaults. `Some([])` means stubs were explicitly disabled
    /// by setting an empty extensions list.
    #[cfg(test)]
    stub_extensions: Mutex<Option<Vec<String>>>,
    /// Configured server log level label.
    #[cfg(test)]
    log_level: Mutex<String>,
    /// Whether the client advertised window/workDoneProgress support.
    work_done_progress_supported: Mutex<bool>,
    /// External formatter configuration.
    #[cfg(test)]
    formatting_config: Mutex<FormattingConfig>,
    /// Last semantic token snapshots used for full/delta requests.
    semantic_tokens_cache: Arc<Mutex<SemanticTokensCache>>,
    /// Bounded cache for static framework string-key scans.
    framework_string_key_cache: Arc<Mutex<FrameworkStringKeyCache>>,
    /// Bounded cache for disk-backed Twig render-context scans.
    twig_context_disk_cache: Arc<Mutex<TwigContextDiskCache>>,
    /// Parsed Composer vendor metadata keyed by vendor directory.
    vendor_autoload_cache: Arc<Mutex<VendorAutoloadCache>>,
    /// Index/FQN-scoped coordinator for concurrent lazy vendor loads.
    vendor_lazy_loads: Arc<VendorLazyLoadCoordinator>,
    /// Composer/autoload epoch barrier shared by request and invalidation paths.
    vendor_load_epoch: Arc<tokio::sync::RwLock<u64>>,
    /// Bounded set of lazy-indexed vendor files currently kept in the symbol index.
    vendor_file_lru: Arc<Mutex<VendorFileLru>>,
}

impl PhpLspBackend {
    pub fn new(client: Client) -> Self {
        let open_files = Arc::new(DashMap::new());
        let template_documents = Arc::new(DashMap::new());
        let document_versions = Arc::new(DashMap::new());
        let indexing_run = Arc::new(Mutex::new(HashMap::new()));
        let runtime_state = Arc::new(Mutex::new(Arc::new(WorkspaceRuntimeState::default())));
        let diagnostics_publisher = DiagnosticsPublisher::new(
            client.clone(),
            open_files.clone(),
            document_versions.clone(),
            template_documents.clone(),
            indexing_run.clone(),
            runtime_state.clone(),
        );
        PhpLspBackend {
            client,
            open_files,
            template_documents,
            document_versions,
            documents_requiring_full_sync: Arc::new(DashMap::new()),
            closed_document_reload_tokens: Arc::new(DashMap::new()),
            next_document_generation: AtomicU64::new(1),
            diagnostic_debounce_tasks: Arc::new(Mutex::new(HashMap::new())),
            diagnostics_publisher,
            analyzer_runs: Arc::new(Mutex::new(HashMap::new())),
            formatter_runs: Arc::new(Mutex::new(HashMap::new())),
            indexing_run,
            index: Arc::new(WorkspaceIndex::new()),
            aggregate_rebuild: Arc::new(Mutex::new(())),
            workspace_root: Mutex::new(None),
            workspace_roots: Mutex::new(Vec::new()),
            namespace_map: Mutex::new(None),
            runtime_state,
            configuration_reload: Mutex::new(()),
            next_runtime_generation: AtomicU64::new(1),
            trace_level: Mutex::new(TraceValue::Off),
            client_settings: Mutex::new(serde_json::json!({})),
            #[cfg(test)]
            stubs_path: Mutex::new(None),
            #[cfg(test)]
            php_version: Mutex::new(PhpVersion::DEFAULT),
            #[cfg(test)]
            diagnostics_mode: Mutex::new(DiagnosticsMode::default()),
            #[cfg(test)]
            diagnostic_severity: Mutex::new(DiagnosticSeverityConfig::default()),
            #[cfg(test)]
            diagnostic_budget: Mutex::new(DiagnosticBudgetConfig::default()),
            #[cfg(test)]
            phpstan_config: Mutex::new(PhpStanConfig::default()),
            #[cfg(test)]
            psalm_config: Mutex::new(PsalmConfig::default()),
            #[cfg(test)]
            analyzer_code_actions: Mutex::new(AnalyzerCodeActionConfig::default()),
            #[cfg(test)]
            composer_enabled: Mutex::new(true),
            #[cfg(test)]
            index_vendor: Mutex::new(true),
            #[cfg(test)]
            include_paths: Mutex::new(Vec::new()),
            #[cfg(test)]
            exclude_paths: Mutex::new(Vec::new()),
            #[cfg(test)]
            stub_extensions: Mutex::new(None),
            #[cfg(test)]
            log_level: Mutex::new("info".to_string()),
            work_done_progress_supported: Mutex::new(false),
            #[cfg(test)]
            formatting_config: Mutex::new(FormattingConfig::default()),
            semantic_tokens_cache: Arc::new(Mutex::new(SemanticTokensCache::default())),
            framework_string_key_cache: Arc::new(Mutex::new(FrameworkStringKeyCache::default())),
            twig_context_disk_cache: Arc::new(Mutex::new(TwigContextDiskCache::default())),
            vendor_autoload_cache: Arc::new(Mutex::new(VendorAutoloadCache::default())),
            vendor_lazy_loads: Arc::new(VendorLazyLoadCoordinator::default()),
            vendor_load_epoch: Arc::new(tokio::sync::RwLock::new(0)),
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
        self.current_document_state(uri_str)
            .map(|state| state.version)
    }

    fn current_document_state(&self, uri_str: &str) -> Option<OpenDocumentState> {
        self.document_versions.get(uri_str).map(|state| *state)
    }

    fn open_document_snapshot(&self, uri_str: &str) -> Option<OpenDocumentSnapshot> {
        open_document_snapshot_from_state(
            &self.open_files,
            &self.template_documents,
            &self.document_versions,
            uri_str,
        )
    }

    async fn synchronize_open_document_index_to_current_runtime(
        &self,
        uri_str: &str,
        expected: OpenDocumentState,
        should_index: bool,
    ) -> bool {
        for _ in 0..4 {
            let state = self.runtime_state_snapshot().await;
            let _aggregate_rebuild = self.aggregate_rebuild.lock().await;
            let indexes = workspace_indexes_for_uri(&state, uri_str, should_index);
            let committed = if should_index {
                let Some(primary_index) = indexes.first() else {
                    self.index.remove_file(uri_str);
                    let current = self.runtime_state_snapshot().await;
                    if Arc::ptr_eq(&state, &current) {
                        return true;
                    }
                    continue;
                };
                let Some(snapshot) = self.open_document_snapshot(uri_str) else {
                    return false;
                };
                if snapshot.document_state != Some(expected) {
                    return false;
                }
                let committed = commit_open_document_index_snapshot_if_current(
                    OpenDocumentIndexCommitContext {
                        open_files: &self.open_files,
                        template_documents: &self.template_documents,
                        document_versions: &self.document_versions,
                        index: &self.index,
                        root_index: Some(primary_index),
                        uri_str,
                    },
                    &snapshot,
                );
                if committed {
                    let references = snapshot.template_document.is_none().then(|| {
                        collect_symbol_references_in_file(
                            &snapshot.tree,
                            &snapshot.source,
                            &snapshot.file_symbols,
                        )
                    });
                    if let Some(references) = references {
                        for index in indexes.iter().skip(1) {
                            index.update_file_with_references(
                                uri_str,
                                snapshot.file_symbols.clone(),
                                references.clone(),
                            );
                        }
                    }
                }
                committed
            } else {
                let dashmap::mapref::entry::Entry::Occupied(_entry) =
                    self.open_files.entry(uri_str.to_string())
                else {
                    return false;
                };
                if self.current_document_state(uri_str) != Some(expected) {
                    return false;
                }
                self.index.remove_file(uri_str);
                for index in &indexes {
                    index.remove_file(uri_str);
                }
                true
            };
            if !committed {
                return false;
            }
            let current = self.runtime_state_snapshot().await;
            if Arc::ptr_eq(&state, &current) {
                return true;
            }
        }
        tracing::debug!(
            "Runtime configuration kept changing while synchronizing open document index: {}",
            uri_str
        );
        false
    }

    fn next_document_state(&self, version: i32) -> OpenDocumentState {
        OpenDocumentState {
            version,
            generation: self
                .next_document_generation
                .fetch_add(1, Ordering::Relaxed),
        }
    }

    async fn invalidate_request_fs_caches(&self) {
        clear_request_fs_caches(
            &self.framework_string_key_cache,
            &self.twig_context_disk_cache,
        )
        .await;
    }

    async fn invalidate_twig_context_disk_cache_for_source_uri(&self, source_uri: &str) {
        let evicted = self
            .twig_context_disk_cache
            .lock()
            .await
            .evict_entries_for_source_uri(source_uri);
        if evicted > 0 {
            tracing::debug!(
                "Evicted {} Twig render-context disk cache entries for changed PHP source {}",
                evicted,
                source_uri
            );
        }
    }

    async fn cancel_debounced_diagnostics(&self, uri_str: &str) {
        if let Some(handle) = self.diagnostic_debounce_tasks.lock().await.remove(uri_str) {
            handle.abort();
        }
    }

    async fn cancel_debounced_diagnostics_if_current(
        &self,
        uri_str: &str,
        expected: OpenDocumentState,
    ) -> bool {
        let mut tasks = self.diagnostic_debounce_tasks.lock().await;
        if self.current_document_state(uri_str) != Some(expected) {
            return false;
        }
        if let Some(handle) = tasks.remove(uri_str) {
            handle.abort();
        }
        true
    }

    async fn cancel_debounced_diagnostics_if_closed(&self, uri_str: &str) -> bool {
        let mut tasks = self.diagnostic_debounce_tasks.lock().await;
        if self.current_document_state(uri_str).is_some() {
            return false;
        }
        if let Some(handle) = tasks.remove(uri_str) {
            handle.abort();
        }
        true
    }

    async fn start_analyzer_run(
        &self,
        uri_str: &str,
        expected: Option<OpenDocumentState>,
    ) -> Option<OperationCancellationToken> {
        let token = OperationCancellationToken::new();
        let mut runs = self.analyzer_runs.lock().await;
        if self.current_document_state(uri_str) != expected {
            return None;
        }
        if let Some(previous) = runs.insert(uri_str.to_string(), token.clone()) {
            previous.cancel();
        }
        Some(token)
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

    async fn cancel_analyzer_run_if_current(
        &self,
        uri_str: &str,
        expected: OpenDocumentState,
    ) -> bool {
        let mut runs = self.analyzer_runs.lock().await;
        if self.current_document_state(uri_str) != Some(expected) {
            return false;
        }
        if let Some(token) = runs.remove(uri_str) {
            token.cancel();
        }
        true
    }

    async fn cancel_analyzer_run_if_closed(&self, uri_str: &str) -> bool {
        let mut runs = self.analyzer_runs.lock().await;
        if self.current_document_state(uri_str).is_some() {
            return false;
        }
        if let Some(token) = runs.remove(uri_str) {
            token.cancel();
        }
        true
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

    async fn cancel_formatter_run_if_current(
        &self,
        uri_str: &str,
        expected: OpenDocumentState,
    ) -> bool {
        let mut runs = self.formatter_runs.lock().await;
        if self.current_document_state(uri_str) != Some(expected) {
            return false;
        }
        if let Some(token) = runs.remove(uri_str) {
            token.cancel();
        }
        true
    }

    async fn cancel_formatter_run_if_closed(&self, uri_str: &str) -> bool {
        let mut runs = self.formatter_runs.lock().await;
        if self.current_document_state(uri_str).is_some() {
            return false;
        }
        if let Some(token) = runs.remove(uri_str) {
            token.cancel();
        }
        true
    }

    async fn clear_semantic_tokens_if_closed(&self, uri_str: &str) -> bool {
        let mut cache = self.semantic_tokens_cache.lock().await;
        if self.current_document_state(uri_str).is_some() {
            return false;
        }
        cache.remove(uri_str);
        true
    }

    async fn publish_empty_diagnostics_if_closed(&self, uri: Uri) -> bool {
        let uri_str = uri.as_str().to_string();
        if self.current_document_state(&uri_str).is_some() {
            return false;
        }
        let request = self.request_context_for_uri(&uri_str).await;
        let computation_sequence = self.diagnostics_publisher.start_computation(&uri_str);
        let expected_runtime_generation = request.state.generation;
        let expected_runtime_config = request.runtime_config().clone();
        let expected_index = request.index(&self.index);
        self.diagnostics_publisher
            .publish(DiagnosticPublishRequest {
                uri,
                diagnostics: vec![],
                version: None,
                expected_state: None,
                expected_template: None,
                require_idle_index: false,
                expected_runtime_generation,
                indexing_workspace_folder: None,
                expected_runtime_config,
                expected_index,
                computation_sequence,
            });
        true
    }

    async fn start_indexing_run(&self, workspace_folder: &Path) -> OperationCancellationToken {
        let token = OperationCancellationToken::new();
        let mut runs = self.indexing_run.lock().await;
        if let Some(previous) = runs.insert(workspace_folder.to_path_buf(), token.clone()) {
            previous.cancel();
        }
        token
    }

    async fn schedule_fast_diagnostics(&self, uri: Uri, expected: OpenDocumentState) {
        let version = expected.version;
        let uri_str = uri.as_str().to_string();
        let open_files = self.open_files.clone();
        let template_documents = self.template_documents.clone();
        let document_versions = self.document_versions.clone();
        let diagnostics_publisher = self.diagnostics_publisher.clone();
        let computation_sequence = diagnostics_publisher.start_computation(&uri_str);
        let request = self.request_context_for_uri(&uri_str).await;
        let index = request.index(&self.index);
        let indexing_run = self.indexing_run.clone();
        let runtime_config = request.runtime_config().clone();
        let expected_runtime_config = runtime_config.clone();
        let expected_index = index.clone();
        let expected_runtime_generation = request.state.generation;
        let indexing_workspace_folder = request
            .workspace
            .as_ref()
            .map(|workspace| workspace.workspace_folder.clone());
        let diagnostics_mode = runtime_config.diagnostics_mode;
        let diagnostic_severity = runtime_config.diagnostic_severity;
        let diagnostic_budget = runtime_config.diagnostic_budget;
        let php_version = runtime_config.php_version;
        let debounce = Duration::from_millis(DID_CHANGE_DIAGNOSTICS_DEBOUNCE_MS);
        let task_uri_str = uri_str.clone();

        let handle = tokio::spawn(async move {
            tokio::time::sleep(debounce).await;

            let Some(snapshot) = open_document_snapshot_from_state(
                &open_files,
                &template_documents,
                &document_versions,
                &task_uri_str,
            ) else {
                return;
            };
            if snapshot.document_state != Some(expected) {
                return;
            }
            let source = snapshot.source;
            let template_document = snapshot.template_document;

            let indexing_active = indexing_run_is_active_for_workspace(
                &indexing_run,
                indexing_workspace_folder.as_deref(),
            )
            .await;
            let effective_diagnostics_mode =
                diagnostics_mode_for_indexing_state(diagnostics_mode, indexing_active);
            let mut diagnostics_config = DiagnosticsRuntimeConfig {
                mode: effective_diagnostics_mode,
                severity: diagnostic_severity,
                budget: diagnostic_budget,
                php_version,
            };
            let mut diagnostics = compute_source_diagnostics_blocking(
                task_uri_str.clone(),
                source.clone(),
                index.clone(),
                diagnostics_config,
                Some(version),
            )
            .await;
            if let Some(template) = &template_document {
                diagnostics = template.map_diagnostics_to_original(
                    diagnostics,
                    diagnostics_config.mode == DiagnosticsMode::Off,
                );
            }

            if diagnostics_config.mode == DiagnosticsMode::BasicSemantic
                && indexing_run_is_active_for_workspace(
                    &indexing_run,
                    indexing_workspace_folder.as_deref(),
                )
                .await
            {
                diagnostics_config.mode = DiagnosticsMode::SyntaxOnly;
                diagnostics = compute_source_diagnostics_blocking(
                    task_uri_str.clone(),
                    source,
                    index.clone(),
                    diagnostics_config,
                    Some(version),
                )
                .await;
                if let Some(template) = &template_document {
                    diagnostics = template.map_diagnostics_to_original(
                        diagnostics,
                        diagnostics_config.mode == DiagnosticsMode::Off,
                    );
                }
            }

            diagnostics_publisher.publish(DiagnosticPublishRequest {
                uri,
                diagnostics,
                version: Some(version),
                expected_state: Some(expected),
                expected_template: template_document,
                require_idle_index: diagnostics_config.mode == DiagnosticsMode::BasicSemantic,
                expected_runtime_generation,
                indexing_workspace_folder,
                expected_runtime_config,
                expected_index,
                computation_sequence,
            });
        });

        let mut debounce_tasks = self.diagnostic_debounce_tasks.lock().await;
        if self.current_document_state(&uri_str) != Some(expected) {
            handle.abort();
            return;
        }
        if let Some(previous) = debounce_tasks.insert(uri_str, handle) {
            previous.abort();
        }
    }

    #[cfg(test)]
    async fn apply_configuration_settings(
        &self,
        raw_settings: &serde_json::Value,
    ) -> AppliedConfiguration {
        self.apply_resolved_configuration(ResolvedRuntimeConfiguration::from_settings(raw_settings))
            .await
    }

    #[cfg(test)]
    async fn apply_resolved_configuration(
        &self,
        resolved: ResolvedRuntimeConfiguration,
    ) -> AppliedConfiguration {
        let ResolvedRuntimeConfiguration {
            php_version,
            diagnostics_mode,
            diagnostic_severity,
            diagnostic_budget,
            composer_enabled,
            index_vendor,
            include_paths,
            exclude_paths,
            stub_extensions,
            log_level,
            stubs_path,
            formatting,
            phpstan,
            psalm,
            analyzer_code_actions,
        } = resolved;
        let mut applied = AppliedConfiguration::default();

        {
            let mut current = self.php_version.lock().await;
            if *current != php_version {
                *current = php_version;
                applied.diagnostics_changed = true;
                applied.stubs_changed = true;
            }
        }
        {
            let mut current = self.diagnostics_mode.lock().await;
            if *current != diagnostics_mode {
                *current = diagnostics_mode;
                applied.diagnostics_changed = true;
            }
        }
        {
            let mut current = self.diagnostic_severity.lock().await;
            if *current != diagnostic_severity {
                *current = diagnostic_severity;
                applied.diagnostics_changed = true;
            }
        }
        {
            let mut current = self.diagnostic_budget.lock().await;
            if *current != diagnostic_budget {
                *current = diagnostic_budget;
                applied.diagnostics_changed = true;
            }
        }
        {
            let mut current = self.composer_enabled.lock().await;
            if *current != composer_enabled {
                *current = composer_enabled;
                applied.indexing_changed = true;
            }
        }

        let index_vendor_changed = {
            let mut current = self.index_vendor.lock().await;
            if *current != index_vendor {
                *current = index_vendor;
                true
            } else {
                false
            }
        };
        if index_vendor_changed {
            applied.indexing_changed = true;
            if !index_vendor {
                self.vendor_autoload_cache.lock().await.clear();
                let evicted = self.vendor_file_lru.lock().await.clear();
                for uri in evicted {
                    self.index.remove_file(&uri);
                }
                let roots = self.workspace_roots.lock().await.clone();
                remove_indexed_vendor_symbols(&self.index, &roots);
            }
        }

        {
            let mut current = self.include_paths.lock().await;
            if *current != include_paths {
                *current = include_paths;
                applied.indexing_changed = true;
            }
        }
        {
            let mut current = self.exclude_paths.lock().await;
            if *current != exclude_paths {
                *current = exclude_paths;
                applied.indexing_changed = true;
            }
        }
        {
            let mut current = self.stub_extensions.lock().await;
            if *current != stub_extensions {
                *current = stub_extensions;
                applied.stubs_changed = true;
            }
        }
        {
            let mut current = self.stubs_path.lock().await;
            if *current != stubs_path {
                *current = stubs_path;
                applied.stubs_changed = true;
            }
        }

        *self.log_level.lock().await = log_level;

        {
            let mut current = self.formatting_config.lock().await;
            if *current != formatting {
                *current = formatting;
            }
        }
        {
            let mut current = self.phpstan_config.lock().await;
            if *current != phpstan {
                *current = phpstan;
                applied.diagnostics_changed = true;
            }
        }
        {
            let mut current = self.psalm_config.lock().await;
            if *current != psalm {
                *current = psalm;
                applied.diagnostics_changed = true;
            }
        }
        {
            let mut current = self.analyzer_code_actions.lock().await;
            if *current != analyzer_code_actions {
                *current = analyzer_code_actions;
            }
        }

        applied
    }

    async fn apply_effective_configuration_settings(
        &self,
        client_settings: &serde_json::Value,
        workspace_roots: &[PathBuf],
    ) -> WorkspaceConfigurationApplication {
        let loaded = load_workspace_runtime_blocking(
            workspace_roots.to_vec(),
            client_settings.clone(),
            "configuration load",
        )
        .await;
        for message in &loaded.messages {
            if message.contains("failed") || message.starts_with("Ignored executable") {
                tracing::warn!("{}", message);
                self.client
                    .log_message(MessageType::WARNING, message.clone())
                    .await;
            } else {
                tracing::info!("{}", message);
                self.client
                    .log_message(MessageType::INFO, message.clone())
                    .await;
            }
        }
        let previous_state = self.runtime_state.lock().await.clone();
        let mut configs = loaded.configs;
        let mut application = WorkspaceConfigurationApplication::default();
        application
            .previous_indexes
            .push(previous_state.fallback_index.clone());
        for config in &previous_state.configs {
            if !application
                .previous_indexes
                .iter()
                .any(|index| Arc::ptr_eq(index, &config.index))
            {
                application.previous_indexes.push(config.index.clone());
            }
        }
        let fallback_changes =
            runtime_configuration_changes(&previous_state.fallback, &loaded.fallback);
        application.republish_all_diagnostics = fallback_changes.diagnostics_changed;
        application.reload_fallback_stubs = fallback_changes.stubs_changed;
        let fallback_index =
            if !fallback_changes.stubs_changed && !fallback_changes.indexing_changed {
                previous_state.fallback_index.clone()
            } else {
                application.reload_fallback_stubs = true;
                application.republish_all_diagnostics = true;
                Arc::new(WorkspaceIndex::new())
            };
        for config in &mut configs {
            if let Some(previous) = previous_state
                .configs
                .iter()
                .find(|previous| previous.workspace_folder == config.workspace_folder)
            {
                let mut changes =
                    runtime_configuration_changes(&previous.runtime_config, &config.runtime_config);
                if previous.root != config.root || previous.namespace_map != config.namespace_map {
                    changes = AppliedConfiguration {
                        diagnostics_changed: true,
                        stubs_changed: true,
                        indexing_changed: true,
                    };
                }
                application.record(&config.workspace_folder, changes);
                let index_replaced = changes.stubs_changed || changes.indexing_changed;
                if !index_replaced {
                    config.index = previous.index.clone();
                    config.vendor_file_lru = previous.vendor_file_lru.clone();
                } else {
                    push_unique_path(
                        &mut application.stubs_workspace_folders,
                        config.workspace_folder.clone(),
                    );
                    push_unique_path(
                        &mut application.indexing_workspace_folders,
                        config.workspace_folder.clone(),
                    );
                }
                application.rebuild_aggregate |= index_replaced;
            } else {
                application.record(
                    &config.workspace_folder,
                    AppliedConfiguration {
                        diagnostics_changed: true,
                        stubs_changed: true,
                        indexing_changed: true,
                    },
                );
                application.rebuild_aggregate = true;
            }
        }
        if previous_state.configs.iter().any(|previous| {
            !configs
                .iter()
                .any(|current| current.workspace_folder == previous.workspace_folder)
        }) {
            application.removed_workspace_folders = previous_state
                .configs
                .iter()
                .filter(|previous| {
                    !configs
                        .iter()
                        .any(|current| current.workspace_folder == previous.workspace_folder)
                })
                .map(|previous| previous.workspace_folder.clone())
                .collect();
            application.republish_all_diagnostics = true;
            application.rebuild_aggregate = true;
        }
        let generation = self.next_runtime_generation.fetch_add(1, Ordering::Relaxed);
        let runtime_state = Arc::new(WorkspaceRuntimeState {
            fallback: loaded.fallback,
            fallback_index,
            configs,
            generation,
        });
        tracing::debug!(
            "Publishing workspace runtime generation {} with {} root context(s)",
            runtime_state.generation,
            runtime_state.configs.len()
        );
        *self.runtime_state.lock().await = runtime_state.clone();
        if let Some(first) = runtime_state.configs.first() {
            *self.workspace_root.lock().await = Some(first.root.clone());
        } else {
            *self.workspace_root.lock().await = None;
        }
        *self.namespace_map.lock().await = runtime_state
            .configs
            .iter()
            .find_map(|config| config.namespace_map.clone());
        application
    }

    async fn apply_configuration_side_effects(
        &self,
        application: WorkspaceConfigurationApplication,
    ) {
        self.cancel_indexing_runs_for_workspace_folders(&application.removed_workspace_folders)
            .await;
        let mut runtime_sensitive_folders = application.diagnostics_workspace_folders.clone();
        for folder in &application.stubs_workspace_folders {
            push_unique_path(&mut runtime_sensitive_folders, folder.clone());
        }
        self.cancel_runtime_sensitive_runs_for_workspace_folders(
            &runtime_sensitive_folders,
            application.republish_all_diagnostics,
        )
        .await;
        self.resynchronize_open_documents_after_runtime_change(&application.previous_indexes)
            .await;
        if application.rebuild_aggregate {
            let _aggregate_rebuild = self.aggregate_rebuild.lock().await;
            let configs = self.runtime_state_snapshot().await.configs.clone();
            let index = self.index.clone();
            let open_files = self.open_files.clone();
            let template_documents = self.template_documents.clone();
            let document_versions = self.document_versions.clone();
            let _ = tokio::task::spawn_blocking(move || {
                rebuild_aggregate_index(
                    &index,
                    &configs,
                    &open_files,
                    &template_documents,
                    &document_versions,
                );
            })
            .await;
        }
        if application.reload_fallback_stubs {
            self.reload_fallback_stubs().await;
        }
        if !application.stubs_workspace_folders.is_empty() {
            self.reload_configured_stubs(&application.stubs_workspace_folders)
                .await;
        }
        if !application.indexing_workspace_folders.is_empty() {
            self.reindex_workspace_folders(&application.indexing_workspace_folders)
                .await;
        }
        if application.republish_all_diagnostics {
            self.republish_open_diagnostics().await;
        } else if !application.diagnostics_workspace_folders.is_empty()
            || !application.stubs_workspace_folders.is_empty()
        {
            let mut folders = application.diagnostics_workspace_folders;
            for folder in application.stubs_workspace_folders {
                push_unique_path(&mut folders, folder);
            }
            self.republish_open_diagnostics_for_workspace_folders(&folders)
                .await;
        }
    }

    async fn cancel_indexing_runs_for_workspace_folders(&self, workspace_folders: &[PathBuf]) {
        let mut runs = self.indexing_run.lock().await;
        for workspace_folder in workspace_folders {
            if let Some(token) = runs.remove(workspace_folder) {
                token.cancel();
            }
        }
    }

    async fn cancel_runtime_sensitive_runs_for_workspace_folders(
        &self,
        workspace_folders: &[PathBuf],
        all_documents: bool,
    ) {
        let state = self.runtime_state_snapshot().await;
        let uris: Vec<String> = self
            .open_files
            .iter()
            .filter_map(|entry| {
                if all_documents {
                    return Some(entry.key().clone());
                }
                let config = workspace_config_for_uri_from_configs(&state.configs, entry.key())?;
                workspace_folders
                    .contains(&config.workspace_folder)
                    .then(|| entry.key().clone())
            })
            .collect();
        for uri in uris {
            self.cancel_debounced_diagnostics(&uri).await;
            self.cancel_analyzer_run(&uri).await;
        }
    }

    async fn resynchronize_open_documents_after_runtime_change(
        &self,
        previous_indexes: &[Arc<WorkspaceIndex>],
    ) {
        let open_uris: Vec<String> = self
            .open_files
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        for uri_str in open_uris {
            let Some(expected) = self.current_document_state(&uri_str) else {
                continue;
            };
            let state = self.runtime_state_snapshot().await;
            let current_indexes = workspace_indexes_for_uri(&state, &uri_str, false);
            for previous in previous_indexes {
                if !current_indexes
                    .iter()
                    .any(|current| Arc::ptr_eq(current, previous))
                {
                    previous.remove_file(&uri_str);
                }
            }
            let should_index = !self.template_documents.contains_key(&uri_str);
            self.synchronize_open_document_index_to_current_runtime(
                &uri_str,
                expected,
                should_index,
            )
            .await;
        }
    }

    async fn reload_effective_configuration(&self) {
        let _reload = self.configuration_reload.lock().await;
        self.reload_effective_configuration_under_lock().await;
    }

    async fn reload_effective_configuration_under_lock(&self) {
        let client_settings = self.client_settings.lock().await.clone();
        let workspace_roots = self.workspace_roots.lock().await.clone();
        let applied = self
            .apply_effective_configuration_settings(&client_settings, &workspace_roots)
            .await;
        self.apply_configuration_side_effects(applied).await;
    }

    async fn reload_fallback_stubs(&self) {
        let state = self.runtime_state_snapshot().await;
        let index = state.fallback_index.clone();
        let runtime = state.fallback.clone();
        let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        tokio::task::spawn_blocking(move || {
            load_configured_stubs(
                &index,
                &root,
                runtime.stubs_path,
                runtime.stub_extensions,
                runtime.php_version,
                true,
            )
        })
        .await
        .unwrap_or(0);
    }

    async fn reload_configured_stubs(&self, workspace_folders: &[PathBuf]) {
        let configs: Vec<WorkspaceRootConfig> = self
            .runtime_state_snapshot()
            .await
            .configs
            .iter()
            .filter(|config| workspace_folders.contains(&config.workspace_folder))
            .cloned()
            .collect();
        let Some(first) = configs.first() else {
            return;
        };
        let root_label = first.root.display().to_string();

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
            configs
                .iter()
                .map(|config| {
                    remove_stub_symbols(&config.index);
                    load_configured_stubs(
                        &config.index,
                        &config.root,
                        config.runtime_config.stubs_path.clone(),
                        config.runtime_config.stub_extensions.clone(),
                        config.runtime_config.php_version,
                        false,
                    )
                })
                .sum::<usize>()
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

    async fn reindex_workspace_folders(&self, workspace_folders: &[PathBuf]) {
        let state = self.runtime_state_snapshot().await;
        let all_configs = state.configs.clone();
        let runtime_generation = state.generation;
        let configs: Vec<WorkspaceRootConfig> = all_configs
            .iter()
            .filter(|config| workspace_folders.contains(&config.workspace_folder))
            .cloned()
            .collect();
        if configs.is_empty() {
            return;
        }
        let effective_roots: Vec<PathBuf> = all_configs
            .iter()
            .map(|config| config.root.clone())
            .collect();

        if let Some(first_root) = effective_roots.first() {
            *self.workspace_root.lock().await = Some(first_root.clone());
        }
        *self.namespace_map.lock().await = configs
            .iter()
            .find_map(|config| config.namespace_map.clone());

        let clear_configs = configs.clone();
        let clear_open_files = self.open_files.clone();
        let _ = tokio::task::spawn_blocking(move || {
            for config in &clear_configs {
                clear_non_stub_symbols(&config.index, Some(&clear_open_files));
            }
        })
        .await;
        {
            let _aggregate_rebuild = self.aggregate_rebuild.lock().await;
            let aggregate = self.index.clone();
            let rebuild_configs = all_configs.clone();
            let open_files = self.open_files.clone();
            let template_documents = self.template_documents.clone();
            let document_versions = self.document_versions.clone();
            let _ = tokio::task::spawn_blocking(move || {
                rebuild_aggregate_index(
                    &aggregate,
                    &rebuild_configs,
                    &open_files,
                    &template_documents,
                    &document_versions,
                );
            })
            .await;
        }
        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "php-lsp: reindexing {} workspace root(s) after an indexing configuration change",
                    configs.len()
                ),
            )
            .await;

        let client = self.client.clone();
        let open_files = self.open_files.clone();
        let template_documents = self.template_documents.clone();
        let twig_context_disk_cache = self.twig_context_disk_cache.clone();
        let semantic_tokens_cache = self.semantic_tokens_cache.clone();
        let reindex_document_versions = self.document_versions.clone();
        let diagnostics_publisher = self.diagnostics_publisher.clone();
        let reindex_index = self.index.clone();
        let vendor_autoload_cache = self.vendor_autoload_cache.clone();
        let vendor_lazy_loads = self.vendor_lazy_loads.clone();
        let vendor_load_epoch = self.vendor_load_epoch.clone();
        let work_done_progress_supported = *self.work_done_progress_supported.lock().await;
        let indexing_run_state = self.indexing_run.clone();
        let runtime_state_handle = self.runtime_state.clone();
        let aggregate_rebuild = self.aggregate_rebuild.clone();
        let mut indexing_tokens = Vec::with_capacity(configs.len());
        for config in &configs {
            indexing_tokens.push(self.start_indexing_run(&config.workspace_folder).await);
        }
        tokio::spawn(async move {
            let mut completed_configs = Vec::new();
            let mut completed_tokens = Vec::new();
            for (config, indexing_token) in configs.iter().zip(&indexing_tokens) {
                if finish_indexing_run_if_cancelled(&indexing_run_state, indexing_token).await {
                    continue;
                }
                let runtime = &config.runtime_config;
                let indexing_options = WorkspaceIndexingOptions {
                    include_paths: runtime.include_paths.clone(),
                    exclude_paths: runtime.exclude_paths.clone(),
                    cache_config: workspace_index_cache_config(
                        Some(&config.root),
                        runtime.php_version,
                        &runtime.include_paths,
                        &runtime.exclude_paths,
                        runtime.stub_extensions.as_deref(),
                        runtime.stubs_path.as_deref(),
                    ),
                    work_done_progress_supported,
                };
                if let Err(e) = index_workspace(
                    &client,
                    WorkspaceLiveIndexContext {
                        index: &config.index,
                        root_index: &config.index,
                        open_files: &open_files,
                        template_documents: &template_documents,
                        document_versions: &reindex_document_versions,
                    },
                    &config.root,
                    config.namespace_map.as_ref(),
                    &indexing_options,
                    indexing_token,
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
                    finish_indexing_run_state(&indexing_run_state, indexing_token).await;
                    continue;
                }
                if finish_indexing_run_if_cancelled(&indexing_run_state, indexing_token).await {
                    continue;
                }

                if runtime.index_vendor {
                    preload_vendor_entrypoints(
                        config.index.clone(),
                        &config.root,
                        &indexing_options.exclude_paths,
                        runtime.php_version,
                        &vendor_autoload_cache,
                        &config.vendor_file_lru,
                        &vendor_load_epoch,
                    )
                    .await;
                }
                if !indexing_token.is_cancelled() {
                    indexing_token.mark_indexing_complete();
                    completed_configs.push(config.clone());
                    completed_tokens.push(indexing_token.clone());
                } else {
                    finish_indexing_run_state(&indexing_run_state, indexing_token).await;
                }
            }

            let current_state = runtime_state_handle.lock().await.clone();
            let mut configs = Vec::new();
            let mut post_tokens = Vec::new();
            for (config, token) in completed_configs.into_iter().zip(completed_tokens) {
                let is_current = current_state.configs.iter().any(|current| {
                    current.workspace_folder == config.workspace_folder
                        && Arc::ptr_eq(&current.index, &config.index)
                });
                if !token.is_cancelled() && is_current {
                    configs.push(config);
                    post_tokens.push(token);
                } else {
                    finish_indexing_run_state(&indexing_run_state, &token).await;
                }
            }
            if configs.is_empty() {
                return;
            }

            let current_state = runtime_state_handle.lock().await.clone();
            let active_workspace_folders: HashSet<PathBuf> = configs
                .iter()
                .zip(&post_tokens)
                .filter(|(config, token)| {
                    !token.is_cancelled()
                        && current_state.configs.iter().any(|current| {
                            current.workspace_folder == config.workspace_folder
                                && Arc::ptr_eq(&current.index, &config.index)
                        })
                })
                .map(|(config, _)| config.workspace_folder.clone())
                .collect();
            if active_workspace_folders.is_empty() {
                for token in post_tokens {
                    finish_indexing_run_state(&indexing_run_state, &token).await;
                }
                return;
            }

            {
                let _aggregate_rebuild = aggregate_rebuild.lock().await;
                let aggregate = reindex_index.clone();
                let rebuild_configs = current_state.configs.clone();
                let rebuild_open_files = open_files.clone();
                let rebuild_templates = template_documents.clone();
                let rebuild_versions = reindex_document_versions.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    rebuild_aggregate_index(
                        &aggregate,
                        &rebuild_configs,
                        &rebuild_open_files,
                        &rebuild_templates,
                        &rebuild_versions,
                    );
                })
                .await;
            }

            let workspace_roots: Vec<PathBuf> = current_state
                .configs
                .iter()
                .map(|config| config.root.clone())
                .collect();
            let completed_workspace_folders: Vec<PathBuf> =
                active_workspace_folders.into_iter().collect();
            {
                let mut cache = twig_context_disk_cache.lock().await;
                for config in &configs {
                    cache.evict_index(&config.index);
                }
            }
            let indexing_cancellations: Vec<WorkspaceIndexingCancellation> = configs
                .iter()
                .zip(&post_tokens)
                .map(|(config, token)| WorkspaceIndexingCancellation {
                    workspace_folder: config.workspace_folder.clone(),
                    token: token.clone(),
                })
                .collect();
            refresh_open_twig_contexts_for_state(OpenTwigContextRefreshState {
                open_files: &open_files,
                template_documents: &template_documents,
                document_versions: &reindex_document_versions,
                index: &reindex_index,
                fallback_index: &current_state.fallback_index,
                workspace_roots: &workspace_roots,
                workspace_configs: &current_state.configs,
                workspace_folders_filter: Some(&completed_workspace_folders),
                indexing_cancellations: &indexing_cancellations,
                twig_context_disk_cache: &twig_context_disk_cache,
                semantic_tokens_cache: &semantic_tokens_cache,
            })
            .await;
            let open_file_uris: Vec<String> =
                open_files.iter().map(|entry| entry.key().clone()).collect();
            for uri_str in open_file_uris {
                let Some(snapshot) = open_document_snapshot_from_state(
                    &open_files,
                    &template_documents,
                    &reindex_document_versions,
                    &uri_str,
                ) else {
                    continue;
                };
                let Some(config) = workspace_config_for_uri_from_configs(&configs, &uri_str) else {
                    continue;
                };
                let Some(post_token) = configs
                    .iter()
                    .position(|candidate| candidate.workspace_folder == config.workspace_folder)
                    .and_then(|position| post_tokens.get(position))
                else {
                    continue;
                };
                if post_token.is_cancelled() {
                    continue;
                }
                commit_open_document_index_snapshot_if_current(
                    OpenDocumentIndexCommitContext {
                        open_files: &open_files,
                        template_documents: &template_documents,
                        document_versions: &reindex_document_versions,
                        index: &reindex_index,
                        root_index: Some(&config.index),
                        uri_str: &uri_str,
                    },
                    &snapshot,
                );
                if let Ok(uri) = uri_str.parse::<Uri>() {
                    let computation_sequence = diagnostics_publisher.start_computation(&uri_str);
                    let runtime = &config.runtime_config;
                    let diagnostics_config = DiagnosticsRuntimeConfig {
                        mode: runtime.diagnostics_mode,
                        severity: runtime.diagnostic_severity,
                        budget: runtime.diagnostic_budget,
                        php_version: runtime.php_version,
                    };
                    let vendor_lazy_context = VendorLazyIndexContext {
                        index: config.index.clone(),
                        workspace_configs: vec![config.clone()],
                        exclude_paths: runtime.exclude_paths.clone(),
                        php_version: runtime.php_version,
                        index_vendor: runtime.index_vendor,
                        vendor_autoload_cache: vendor_autoload_cache.clone(),
                        vendor_file_lru: config.vendor_file_lru.clone(),
                        lazy_loads: vendor_lazy_loads.clone(),
                        load_epoch: vendor_load_epoch.clone(),
                    };
                    let document_state = snapshot.document_state;
                    let version = document_state.map(|state| state.version);
                    let template_document = snapshot.template_document.clone();
                    if diagnostics_config.mode == DiagnosticsMode::BasicSemantic
                        && template_document.is_none()
                        && runtime.index_vendor
                    {
                        preresolve_open_file_diagnostic_dependencies(
                            &snapshot.tree,
                            &snapshot.source,
                            &snapshot.file_symbols,
                            &vendor_lazy_context,
                        )
                        .await;
                    }
                    let mut diags = compute_source_diagnostics_blocking(
                        uri_str.clone(),
                        snapshot.source.clone(),
                        config.index.clone(),
                        diagnostics_config,
                        version,
                    )
                    .await;
                    if let Some(template) = &template_document {
                        diags = template.map_diagnostics_to_original(
                            diags,
                            diagnostics_config.mode == DiagnosticsMode::Off,
                        );
                    } else if diagnostics_config.mode == DiagnosticsMode::BasicSemantic
                        && runtime.index_vendor
                    {
                        diags = filter_lazy_resolved_symbol_diagnostics_with_context(
                            &config.index,
                            &vendor_lazy_context,
                            diags,
                        )
                        .await;
                    }
                    diagnostics_publisher.publish(DiagnosticPublishRequest {
                        uri,
                        diagnostics: diags,
                        version,
                        expected_state: document_state,
                        expected_template: template_document,
                        require_idle_index: diagnostics_config.mode
                            == DiagnosticsMode::BasicSemantic,
                        expected_runtime_generation: runtime_generation,
                        indexing_workspace_folder: Some(config.workspace_folder.clone()),
                        expected_runtime_config: runtime.clone(),
                        expected_index: config.index.clone(),
                        computation_sequence,
                    });
                }
            }
            for token in post_tokens {
                finish_indexing_run_state(&indexing_run_state, &token).await;
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

    async fn republish_open_diagnostics_for_workspace_folders(
        &self,
        workspace_folders: &[PathBuf],
    ) {
        let state = self.runtime_state_snapshot().await;
        let open_uris: Vec<Uri> = self
            .open_files
            .iter()
            .filter_map(|entry| {
                let config = workspace_config_for_uri_from_configs(&state.configs, entry.key())?;
                workspace_folders
                    .contains(&config.workspace_folder)
                    .then(|| entry.key().parse::<Uri>().ok())
                    .flatten()
            })
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
                self.runtime_state_snapshot()
                    .await
                    .configs
                    .iter()
                    .map(|config| config.root.clone()),
            );
        }
        roots
    }

    async fn invalidate_composer_metadata(&self, path: &Path, reindex_workspace: bool) {
        self.invalidate_request_fs_caches().await;
        let mut vendor_epoch = self.vendor_load_epoch.write().await;
        *vendor_epoch = vendor_epoch.wrapping_add(1);
        self.vendor_autoload_cache.lock().await.clear();
        let evicted = self.vendor_file_lru.lock().await.clear();
        for uri in evicted {
            self.index.remove_file(&uri);
        }

        let state = self.runtime_state_snapshot().await;
        let mut affected_configs: Vec<WorkspaceRootConfig> = state
            .configs
            .iter()
            .filter(|config| {
                path.starts_with(&config.workspace_folder) || path.starts_with(&config.root)
            })
            .cloned()
            .collect();
        if affected_configs.is_empty() {
            affected_configs = state.configs.clone();
        }
        let mut removed_vendor_files = 0;
        for config in &affected_configs {
            config.vendor_file_lru.lock().await.clear();
            removed_vendor_files +=
                remove_indexed_vendor_symbols(&config.index, std::slice::from_ref(&config.root));
        }
        {
            let _aggregate_rebuild = self.aggregate_rebuild.lock().await;
            let aggregate = self.index.clone();
            let rebuild_configs = state.configs.clone();
            let open_files = self.open_files.clone();
            let template_documents = self.template_documents.clone();
            let document_versions = self.document_versions.clone();
            let _ = tokio::task::spawn_blocking(move || {
                rebuild_aggregate_index(
                    &aggregate,
                    &rebuild_configs,
                    &open_files,
                    &template_documents,
                    &document_versions,
                );
            })
            .await;
        }
        drop(vendor_epoch);
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
            let workspace_folders = affected_configs
                .iter()
                .map(|config| config.workspace_folder.clone())
                .collect::<Vec<_>>();
            self.reindex_workspace_folders(&workspace_folders).await;
        } else {
            self.refresh_open_twig_contexts().await;
            let workspace_folders = affected_configs
                .iter()
                .map(|config| config.workspace_folder.clone())
                .collect::<Vec<_>>();
            self.republish_open_diagnostics_for_workspace_folders(&workspace_folders)
                .await;
        }
    }

    async fn runtime_state_snapshot(&self) -> Arc<WorkspaceRuntimeState> {
        self.runtime_state.lock().await.clone()
    }

    async fn request_context_for_uri(&self, uri_str: &str) -> WorkspaceRequestContext {
        let state = self.runtime_state_snapshot().await;
        let workspace = workspace_config_for_uri_from_configs(&state.configs, uri_str);
        WorkspaceRequestContext { state, workspace }
    }

    #[cfg(test)]
    async fn workspace_config_for_uri(&self, uri_str: &str) -> Option<WorkspaceRootConfig> {
        self.request_context_for_uri(uri_str).await.workspace
    }

    #[cfg(test)]
    async fn runtime_config_for_uri(&self, uri_str: &str) -> ResolvedRuntimeConfiguration {
        self.request_context_for_uri(uri_str)
            .await
            .runtime_config()
            .clone()
    }

    #[cfg(test)]
    async fn workspace_root_for_uri(&self, uri_str: &str) -> Option<PathBuf> {
        self.workspace_config_for_uri(uri_str)
            .await
            .map(|config| config.root)
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
