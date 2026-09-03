//! Workspace LSP handlers extracted from `server.rs`.

use crate::util::fs_walk::{walk_files, FileWalkOutcome, TraversalLimits, TraversalStopReason};
use crate::util::uri::path_to_uri;

use super::super::*;

fn commit_staged_stubs(
    run: &IndexingRunLease,
    staged: &WorkspaceIndex,
    destination: &WorkspaceIndex,
) -> bool {
    run.commit_index_if_current(|| replace_stub_symbols_from(staged, destination))
        .is_some()
}

struct RenamedOpenDocument {
    parser: FileParser,
    template: Option<TemplateDocument>,
    state: OpenDocumentState,
    requires_full_sync: bool,
}

struct RenamedOpenDocumentCommitContext<'a> {
    open_files: &'a DashMap<String, FileParser>,
    template_documents: &'a DashMap<String, TemplateDocument>,
    document_versions: &'a DashMap<String, OpenDocumentState>,
    documents_requiring_full_sync: &'a DashMap<String, u64>,
    closed_document_reload_tokens: &'a DashMap<String, u64>,
    uri_str: &'a str,
}

fn commit_renamed_open_document_with_hook<F>(
    ctx: RenamedOpenDocumentCommitContext<'_>,
    document: RenamedOpenDocument,
    before_parser_publish: F,
) -> bool
where
    F: FnOnce(),
{
    let dashmap::mapref::entry::Entry::Vacant(open_entry) =
        ctx.open_files.entry(ctx.uri_str.to_string())
    else {
        // A concurrent didOpen at the destination owns the newer document.
        return false;
    };

    match document.template {
        Some(template) => {
            ctx.template_documents
                .insert(ctx.uri_str.to_string(), template);
        }
        None => {
            ctx.template_documents.remove(ctx.uri_str);
        }
    }
    ctx.document_versions
        .insert(ctx.uri_str.to_string(), document.state);
    if document.requires_full_sync {
        ctx.documents_requiring_full_sync
            .insert(ctx.uri_str.to_string(), document.state.generation);
    } else {
        ctx.documents_requiring_full_sync.remove(ctx.uri_str);
    }
    ctx.closed_document_reload_tokens.remove(ctx.uri_str);
    before_parser_publish();

    // The vacant entry retains the destination shard lock until all companion
    // state has been staged, so snapshot readers cannot observe a partial move.
    open_entry.insert(document.parser);
    true
}

#[derive(Clone, Copy)]
struct DiskPhpIndexCommitContext<'a> {
    open_files: &'a DashMap<String, FileParser>,
    template_documents: &'a DashMap<String, TemplateDocument>,
    document_versions: &'a DashMap<String, OpenDocumentState>,
    index: &'a WorkspaceIndex,
    root_index: Option<&'a WorkspaceIndex>,
    uri_str: &'a str,
}

fn commit_disk_php_index_if_closed_with_hook<F>(
    ctx: DiskPhpIndexCommitContext<'_>,
    file_symbols: Option<php_lsp_types::FileSymbols>,
    references: Vec<php_lsp_types::SymbolReference>,
    before_index_commit: F,
) -> bool
where
    F: FnOnce(),
{
    let dashmap::mapref::entry::Entry::Vacant(_open_entry) =
        ctx.open_files.entry(ctx.uri_str.to_string())
    else {
        return false;
    };
    if ctx.template_documents.contains_key(ctx.uri_str)
        || ctx.document_versions.contains_key(ctx.uri_str)
    {
        return false;
    }

    before_index_commit();
    if let Some(file_symbols) = file_symbols {
        let root_symbols = file_symbols.clone();
        let root_references = references.clone();
        ctx.index
            .update_file_with_references(ctx.uri_str, file_symbols, references);
        if let Some(root_index) = ctx
            .root_index
            .filter(|root_index| !std::ptr::eq(*root_index, ctx.index))
        {
            root_index.update_file_with_references(ctx.uri_str, root_symbols, root_references);
        }
    } else {
        ctx.index.remove_file(ctx.uri_str);
        if let Some(root_index) = ctx
            .root_index
            .filter(|root_index| !std::ptr::eq(*root_index, ctx.index))
        {
            root_index.remove_file(ctx.uri_str);
        }
    }
    true
}

fn commit_disk_php_index_if_closed(
    ctx: DiskPhpIndexCommitContext<'_>,
    file_symbols: Option<php_lsp_types::FileSymbols>,
    references: Vec<php_lsp_types::SymbolReference>,
) -> bool {
    commit_disk_php_index_if_closed_with_hook(ctx, file_symbols, references, || {})
}

fn commit_workspace_disk_file_preserving_open(
    ctx: DiskPhpIndexCommitContext<'_>,
    file_symbols: php_lsp_types::FileSymbols,
    references: Vec<php_lsp_types::SymbolReference>,
) {
    if commit_disk_php_index_if_closed(ctx, Some(file_symbols), references) {
        return;
    }
    if let Some(snapshot) = open_document_snapshot_from_state(
        ctx.open_files,
        ctx.template_documents,
        ctx.document_versions,
        ctx.uri_str,
    ) {
        commit_open_document_index_snapshot_if_current(
            OpenDocumentIndexCommitContext {
                open_files: ctx.open_files,
                template_documents: ctx.template_documents,
                document_versions: ctx.document_versions,
                index: ctx.index,
                root_index: ctx.root_index,
                uri_str: ctx.uri_str,
            },
            &snapshot,
        );
    }
}

impl PhpLspBackend {
    pub(crate) async fn lsp_initialized(&self, _params: InitializedParams) {
        tracing::info!("php-lsp: initialized");

        self.client
            .log_message(MessageType::INFO, "php-lsp server initialized")
            .await;

        self.reload_fallback_stubs().await;

        let mut roots = self.workspace_roots.lock().await.clone();
        if roots.is_empty() {
            if let Some(root) = self.workspace_root.lock().await.clone() {
                roots.push(root);
            }
        }

        if roots.is_empty() {
            self.pending_initial_indexing_runs.lock().await.clear();
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

        let configs = {
            let current = self.runtime_state_snapshot().await;
            if current.configs.is_empty() {
                let _reload = self.configuration_reload.lock().await;
                let client_settings = self.client_settings.lock().await.clone();
                self.apply_effective_configuration_settings(&client_settings, &roots)
                    .await;
                self.runtime_state_snapshot().await.configs.clone()
            } else {
                current.configs.clone()
            }
        };
        let effective_roots: Vec<PathBuf> =
            configs.iter().map(|config| config.root.clone()).collect();
        let runtime_state = self.runtime_state_snapshot().await;
        let runtime_generation = runtime_state.generation;
        let mut indexing_guards = Vec::with_capacity(configs.len());
        let mut pending_initial_runs = {
            let mut pending = self.pending_initial_indexing_runs.lock().await;
            std::mem::take(&mut *pending)
        };
        for config in &configs {
            let guard = pending_initial_runs
                .iter()
                .position(|pending| {
                    pending.workspace_folder == config.workspace_folder
                        && Arc::ptr_eq(&pending.index, &config.index)
                        && pending.guard.lease().is_current()
                })
                .map(|position| pending_initial_runs.swap_remove(position).guard)
                .unwrap_or_else(|| self.start_indexing_run(&config.workspace_folder));
            self.indexing_status_publisher.publish_for_run(
                &guard.lease(),
                runtime_generation,
                workspace_discovery_indexing_status(&config.root),
            );
            indexing_guards.push(guard);
        }

        if let Some(first_root) = effective_roots.first() {
            *self.workspace_root.lock().await = Some(first_root.clone());
        }
        *self.namespace_map.lock().await = configs
            .iter()
            .find_map(|config| config.namespace_map.clone());

        // Load into per-root staging indexes, then publish only through the reserved run lease.
        for (config, guard) in configs.iter().zip(&indexing_guards) {
            self.indexing_status_publisher.publish_for_run(
                &guard.lease(),
                runtime_generation,
                serde_json::json!({
                    "phase": "loadingStubs",
                    "root": config.root.display().to_string(),
                    "message": "Loading PHP stubs"
                }),
            );
        }
        let stub_jobs = configs
            .iter()
            .zip(&indexing_guards)
            .map(|(config, guard)| (config.clone(), guard.lease()))
            .collect::<Vec<_>>();
        let loaded_stubs = tokio::task::spawn_blocking(move || {
            stub_jobs
                .into_iter()
                .map(|(config, run)| {
                    if !run.is_current() {
                        return 0;
                    }
                    let staged = WorkspaceIndex::new();
                    let loaded = load_configured_stubs(
                        &staged,
                        &config.root,
                        config.runtime_config.stubs_path.clone(),
                        config.runtime_config.stub_extensions.clone(),
                        config.runtime_config.php_version,
                        false,
                    );
                    if commit_staged_stubs(&run, &staged, &config.index) {
                        loaded
                    } else {
                        0
                    }
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_else(|_| vec![0; configs.len()]);

        for ((config, guard), loaded) in configs.iter().zip(&indexing_guards).zip(loaded_stubs) {
            self.indexing_status_publisher.publish_for_run(
                &guard.lease(),
                runtime_generation,
                serde_json::json!({
                    "phase": "stubsLoaded",
                    "root": config.root.display().to_string(),
                    "message": format!("Loaded {} stub files", loaded),
                    "stubFiles": loaded
                }),
            );
        }

        let client = self.client.clone();
        let open_files = self.open_files.clone();
        let template_documents = self.template_documents.clone();
        let twig_context_disk_cache = self.twig_context_disk_cache.clone();
        let semantic_tokens_cache = self.semantic_tokens_cache.clone();
        let reindex_document_versions = self.document_versions.clone();
        let diagnostics_publisher = self.diagnostics_publisher.clone();
        let indexing_status_publisher = self.indexing_status_publisher.clone();
        let reindex_index = self.index.clone();
        let vendor_autoload_cache = self.vendor_autoload_cache.clone();
        let vendor_lazy_loads = self.vendor_lazy_loads.clone();
        let vendor_load_epoch = self.vendor_load_epoch.clone();
        let work_done_progress_supported = *self.work_done_progress_supported.lock().await;
        let runtime_state_handle = self.runtime_state.clone();
        let aggregate_rebuild = self.aggregate_rebuild.clone();
        let external_symlinks = self.external_symlinks.clone();
        tokio::spawn(async move {
            let mut completed_configs = Vec::new();
            let mut completed_runs = Vec::new();
            let mut completed_reports = Vec::new();
            let mut completed_guards = Vec::new();
            for (config, indexing_guard) in configs.iter().zip(indexing_guards) {
                let indexing_run = indexing_guard.lease();
                if !indexing_run.is_current() {
                    continue;
                }
                let runtime = &config.runtime_config;
                let indexing_options = WorkspaceIndexingOptions {
                    include_paths: runtime.include_paths.clone(),
                    exclude_paths: runtime.exclude_paths.clone(),
                    traversal_limits: runtime.traversal_limits,
                    cache_config: workspace_index_cache_config(
                        Some(&config.root),
                        runtime.php_version,
                        &runtime.include_paths,
                        &runtime.exclude_paths,
                        runtime.traversal_limits,
                        runtime.stub_extensions.as_deref(),
                        runtime.stubs_path.as_deref(),
                    ),
                    work_done_progress_supported,
                };
                let indexing_report = match index_workspace(
                    &client,
                    &indexing_status_publisher,
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
                    &indexing_run,
                    &external_symlinks,
                    runtime_generation,
                )
                .await
                {
                    Ok(Some(report)) => report,
                    Ok(None) => continue,
                    Err(e) => {
                        tracing::error!("Background indexing failed: {}", e);
                        indexing_status_publisher.publish_for_run(
                            &indexing_run,
                            runtime_generation,
                            serde_json::json!({
                                "phase": "error",
                                "root": config.root.display().to_string(),
                                "message": format!("Indexing failed: {}", e)
                            }),
                        );
                        client
                            .log_message(MessageType::ERROR, format!("Indexing failed: {}", e))
                            .await;
                        continue;
                    }
                };
                if !indexing_run.is_current() {
                    continue;
                }

                if runtime.index_vendor {
                    preload_vendor_entrypoints(
                        config.index.clone(),
                        &config.root,
                        &indexing_options.exclude_paths,
                        runtime.traversal_limits,
                        runtime.php_version,
                        &vendor_autoload_cache,
                        &config.vendor_file_lru,
                        &vendor_load_epoch,
                        Some(&indexing_run),
                    )
                    .await;
                }
                if indexing_run.is_current() {
                    completed_configs.push(config.clone());
                    completed_runs.push(indexing_run);
                    completed_reports.push(indexing_report);
                    completed_guards.push(indexing_guard);
                }
            }

            let completed = completed_configs
                .into_iter()
                .zip(completed_runs)
                .zip(completed_reports)
                .zip(completed_guards)
                .map(
                    |(((expected_config, run), report), guard)| CompletedWorkspaceIndexingRun {
                        expected_config,
                        run,
                        _guard: guard,
                        report,
                    },
                )
                .collect();
            postprocess_workspace_indexing_runs(
                WorkspaceIndexingPostprocessContext {
                    open_files,
                    template_documents,
                    document_versions: reindex_document_versions,
                    diagnostics_publisher,
                    indexing_status_publisher,
                    aggregate_index: reindex_index,
                    aggregate_rebuild,
                    runtime_state: runtime_state_handle,
                    twig_context_disk_cache,
                    semantic_tokens_cache,
                    vendor_autoload_cache,
                    vendor_lazy_loads,
                    vendor_load_epoch,
                    external_symlinks,
                },
                completed,
            )
            .await;
        });
    }

    pub(crate) async fn lsp_did_change_workspace_folders(
        &self,
        params: DidChangeWorkspaceFoldersParams,
    ) {
        tracing::debug!("didChangeWorkspaceFolders");

        let removed_roots: Vec<PathBuf> = params
            .event
            .removed
            .iter()
            .filter_map(|folder| uri_to_path(folder.uri.as_str()))
            .collect();
        let added_roots: Vec<PathBuf> = params
            .event
            .added
            .iter()
            .filter_map(|folder| uri_to_path(folder.uri.as_str()))
            .collect();
        if removed_roots.is_empty() && added_roots.is_empty() {
            return;
        }
        let _reload = self.configuration_reload.lock().await;

        let roots = {
            let mut roots = self.workspace_roots.lock().await;
            roots.retain(|root| !removed_roots.iter().any(|removed| root == removed));
            for root in added_roots {
                push_unique_path(&mut roots, root);
            }
            roots.clone()
        };

        let client_settings = self.client_settings.lock().await.clone();
        let applied = self
            .apply_effective_configuration_settings(&client_settings, &roots)
            .await;
        self.apply_configuration_side_effects(applied).await;
    }

    pub(crate) async fn lsp_did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        let changes = self
            .external_symlinks
            .translate_events(params.changes)
            .await;
        tracing::debug!("didChangeWatchedFiles: {} change(s)", changes.len());

        if !changes.is_empty() {
            self.invalidate_request_fs_caches().await;
        }

        let roots = self.current_workspace_roots().await;
        let mut config_changed = false;
        let mut composer_metadata_changed: Option<PathBuf> = None;
        let mut composer_requires_workspace_reindex = false;
        let mut template_context_changed = false;
        for event in changes {
            if uri_is_project_config_file(&event.uri) {
                config_changed = true;
                continue;
            }

            if let Some((path, change)) = uri_composer_metadata_change(&event.uri) {
                if should_ignore_vendor_package_composer_metadata_change(&path, &roots) {
                    continue;
                }
                composer_metadata_changed = Some(path);
                if change == ComposerMetadataChange::ProjectAutoload {
                    composer_requires_workspace_reindex = true;
                }
                continue;
            }

            if is_twig_template_uri(event.uri.as_str()) {
                template_context_changed = true;
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

        if config_changed || composer_requires_workspace_reindex {
            self.reload_effective_configuration().await;
        }
        if let Some(path) = composer_metadata_changed {
            self.invalidate_composer_metadata(&path, composer_requires_workspace_reindex)
                .await;
        }
        if template_context_changed {
            self.refresh_open_twig_contexts_and_republish_diagnostics()
                .await;
        }
    }

    pub(crate) async fn lsp_did_change_configuration(&self, params: DidChangeConfigurationParams) {
        tracing::debug!("didChangeConfiguration");

        self.invalidate_request_fs_caches().await;
        let _reload = self.configuration_reload.lock().await;
        *self.client_settings.lock().await = params.settings.clone();
        self.reload_effective_configuration_under_lock().await;
    }

    pub(crate) async fn lsp_will_create_files(
        &self,
        _params: CreateFilesParams,
    ) -> Result<Option<WorkspaceEdit>> {
        Ok(None)
    }

    pub(crate) async fn lsp_did_create_files(&self, params: CreateFilesParams) {
        tracing::debug!("didCreateFiles: {} file(s)", params.files.len());

        if !params.files.is_empty() {
            self.invalidate_request_fs_caches().await;
        }

        for file in params.files {
            if let Ok(uri) = file.uri.parse::<Uri>() {
                self.reindex_php_file(&uri).await;
            }
        }
    }

    pub(crate) async fn lsp_will_rename_files(
        &self,
        _params: RenameFilesParams,
    ) -> Result<Option<WorkspaceEdit>> {
        Ok(None)
    }

    pub(crate) async fn lsp_did_rename_files(&self, params: RenameFilesParams) {
        tracing::debug!("didRenameFiles: {} file(s)", params.files.len());

        if !params.files.is_empty() {
            self.invalidate_request_fs_caches().await;
        }

        for file in params.files {
            let old_uri = file.old_uri.parse::<Uri>();
            let new_uri = file.new_uri.parse::<Uri>();
            if let (Ok(old_uri), Ok(new_uri)) = (old_uri, new_uri) {
                self.rename_php_file(&old_uri, &new_uri).await;
            }
        }
    }

    pub(crate) async fn lsp_will_delete_files(
        &self,
        _params: DeleteFilesParams,
    ) -> Result<Option<WorkspaceEdit>> {
        Ok(None)
    }

    pub(crate) async fn lsp_did_delete_files(&self, params: DeleteFilesParams) {
        tracing::debug!("didDeleteFiles: {} file(s)", params.files.len());

        if !params.files.is_empty() {
            self.invalidate_request_fs_caches().await;
        }

        for file in params.files {
            if let Ok(uri) = file.uri.parse::<Uri>() {
                self.remove_php_file(&uri).await;
            }
        }
    }

    // --- Language Features ---
}

pub(in crate::server) fn resolve_config_path(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        normalize_path(path)
    } else {
        normalize_path(&root.join(path))
    }
}

#[cfg(test)]
#[path = "workspace_tests.rs"]
mod tests;

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
#[cfg(test)]
pub(crate) fn collect_php_files(
    directories: &[PathBuf],
    root: &Path,
    exclude_paths: &[PathBuf],
) -> Vec<PathBuf> {
    collect_php_files_with_control(
        directories,
        root,
        exclude_paths,
        TraversalLimits::default(),
        || None,
    )
    .files
}

pub(crate) fn collect_php_files_with_control<Control>(
    directories: &[PathBuf],
    root: &Path,
    exclude_paths: &[PathBuf],
    limits: TraversalLimits,
    control: Control,
) -> FileWalkOutcome
where
    Control: FnMut() -> Option<TraversalStopReason>,
{
    collect_php_files_with_explicit_control(directories, &[], root, exclude_paths, limits, control)
}

pub(crate) fn collect_php_files_with_explicit_control<Control>(
    directories: &[PathBuf],
    explicit_files: &[PathBuf],
    root: &Path,
    exclude_paths: &[PathBuf],
    limits: TraversalLimits,
    control: Control,
) -> FileWalkOutcome
where
    Control: FnMut() -> Option<TraversalStopReason>,
{
    let mut roots = directories
        .iter()
        .map(|directory| {
            if directory.is_absolute() {
                directory.clone()
            } else {
                root.join(directory)
            }
        })
        .collect::<Vec<_>>();
    let explicit_files = explicit_files
        .iter()
        .map(|file| {
            if file.is_absolute() {
                file.clone()
            } else {
                root.join(file)
            }
        })
        .collect::<HashSet<_>>();
    roots.extend(explicit_files.iter().cloned());

    walk_files(
        &roots,
        limits,
        |path| path_is_excluded(path, root, exclude_paths),
        |path, is_root| {
            if is_root {
                return true;
            }
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_default();
            !name.starts_with('.') && !matches!(name.as_ref(), "vendor" | "node_modules")
        },
        |path| {
            explicit_files.contains(path)
                || path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("php"))
        },
        control,
    )
}

pub(in crate::server) async fn collect_php_files_blocking(
    directories: Vec<PathBuf>,
    root: PathBuf,
    exclude_paths: Vec<PathBuf>,
    explicit_files: Vec<PathBuf>,
    limits: TraversalLimits,
    cancellation: OperationCancellationToken,
) -> std::result::Result<FileWalkOutcome, String> {
    let path_label = root.display().to_string();
    let deadline = file_io_walk_deadline();
    run_file_io_blocking("workspace PHP file discovery", path_label, move || {
        collect_php_files_with_explicit_control(
            &directories,
            &explicit_files,
            &root,
            &exclude_paths,
            limits,
            || {
                if cancellation.is_cancelled() {
                    Some(TraversalStopReason::Cancelled)
                } else if Instant::now() >= deadline {
                    Some(TraversalStopReason::DeadlineExceeded)
                } else {
                    None
                }
            },
        )
    })
    .await
}

async fn collect_feature_symlink_aliases_blocking(
    root: PathBuf,
    workspace_folder: PathBuf,
    exclude_paths: Vec<PathBuf>,
    limits: TraversalLimits,
    cancellation: OperationCancellationToken,
) -> std::result::Result<FileWalkOutcome, String> {
    let roots = [
        workspace_folder.join(PROJECT_CONFIG_FILE_NAME),
        root.join(PROJECT_CONFIG_FILE_NAME),
        root.join("composer.json"),
        root.join("composer.lock"),
        root.join("vendor/composer"),
        root.join("app"),
        root.join("tests"),
        root.join("templates"),
        root.join("resources"),
        root.join("config"),
        root.join("routes"),
        root.join("lang"),
    ];
    let path_label = root.display().to_string();
    let deadline = file_io_walk_deadline();
    run_file_io_blocking("feature symlink discovery", path_label, move || {
        walk_files(
            &roots,
            TraversalLimits {
                max_files: None,
                max_entries: limits.max_entries,
            },
            |path| path_is_excluded(path, &root, &exclude_paths),
            |path, is_root| {
                if is_root {
                    return true;
                }
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy())
                    .unwrap_or_default();
                !name.starts_with('.')
                    && !matches!(name.as_ref(), "vendor" | "node_modules" | "target")
            },
            |_| false,
            || {
                if cancellation.is_cancelled() {
                    Some(TraversalStopReason::Cancelled)
                } else if Instant::now() >= deadline {
                    Some(TraversalStopReason::DeadlineExceeded)
                } else {
                    None
                }
            },
        )
    })
    .await
}

pub(in crate::server) fn uri_is_php_file(uri: &Uri) -> bool {
    if let Some(path) = uri_to_path(uri.as_str()) {
        return path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("php"));
    }

    uri.as_str().to_ascii_lowercase().ends_with(".php")
}

pub(in crate::server) fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

pub(in crate::server) fn workspace_roots_from_initialize(
    params: &InitializeParams,
) -> Vec<PathBuf> {
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

pub(in crate::server) fn project_config_candidates(root: &Path) -> Vec<PathBuf> {
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

pub(in crate::server) fn project_command_trust_setting(
    settings: &serde_json::Value,
) -> Option<bool> {
    settings_bool(
        settings,
        "allowProjectCommands",
        &["security", "allowProjectCommands"],
    )
}

pub(in crate::server) fn project_commands_are_trusted(
    trusted_settings: &serde_json::Value,
    client_settings: &serde_json::Value,
) -> bool {
    project_command_trust_setting(client_settings)
        .or_else(|| project_command_trust_setting(trusted_settings))
        .unwrap_or(false)
}

pub(in crate::server) fn remove_section_key(
    settings: &mut serde_json::Value,
    section: &str,
    key: &str,
) -> Option<serde_json::Value> {
    settings
        .get_mut(section)
        .and_then(|section| section.as_object_mut())
        .and_then(|section| section.remove(key))
}

pub(in crate::server) fn nested_bool(
    settings: &serde_json::Value,
    section: &str,
    key: &str,
) -> Option<bool> {
    settings
        .get(section)
        .and_then(|section| section.get(key))
        .and_then(|value| value.as_bool())
}

pub(in crate::server) fn nested_string<'a>(
    settings: &'a serde_json::Value,
    section: &str,
    key: &str,
) -> Option<&'a str> {
    settings
        .get(section)
        .and_then(|section| section.get(key))
        .and_then(|value| value.as_str())
}

pub(in crate::server) fn untrusted_project_formatter_provider_executes(provider: &str) -> bool {
    !matches!(
        provider.trim().to_ascii_lowercase().as_str(),
        "auto" | "none" | "custom"
    )
}

pub(in crate::server) fn sanitize_project_settings_for_command_trust(
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
    let (mut effective, mut messages) = load_global_configuration_settings();
    let client_settings = normalize_client_settings(client_settings);

    for root in workspace_roots {
        merge_project_configuration_for_root(&mut effective, &mut messages, root, &client_settings);
    }

    merge_json_objects(&mut effective, &client_settings);

    (effective, messages)
}

fn load_global_configuration_settings() -> (serde_json::Value, Vec<String>) {
    let mut settings = serde_json::json!({});
    let mut messages = Vec::new();
    if let Some(path) = global_config_candidates()
        .into_iter()
        .find(|path| path.exists())
    {
        match load_toml_settings(&path) {
            Ok(global) => {
                merge_json_objects(&mut settings, &global);
                messages.push(format!("Loaded global config: {}", path.display()));
            }
            Err(message) => messages.push(message),
        }
    }
    (settings, messages)
}

fn merge_project_configuration_for_root(
    effective: &mut serde_json::Value,
    messages: &mut Vec<String>,
    root: &Path,
    client_settings: &serde_json::Value,
) {
    let allow_project_commands = project_commands_are_trusted(effective, client_settings);
    for path in project_config_candidates(root) {
        if !path.exists() {
            continue;
        }
        match load_toml_settings(&path) {
            Ok(mut settings) => {
                for message in clamp_project_traversal_limits(&mut settings, effective, &path) {
                    messages.push(message);
                }
                if let Some(message) = sanitize_project_settings_for_command_trust(
                    &mut settings,
                    &path,
                    allow_project_commands,
                ) {
                    messages.push(message);
                }
                merge_json_objects(effective, &settings);
                messages.push(format!("Loaded project config: {}", path.display()));
                break;
            }
            Err(message) => messages.push(message),
        }
    }
}

fn clamp_project_traversal_limits(
    settings: &mut serde_json::Value,
    trusted_baseline: &serde_json::Value,
    path: &Path,
) -> Vec<String> {
    let mut messages = Vec::new();
    for (key, label, default_limit) in [
        (
            "indexingMaxFiles",
            "indexing.maxFiles",
            DEFAULT_INDEXING_MAX_FILES as u64,
        ),
        (
            "indexingMaxEntries",
            "indexing.maxEntries",
            DEFAULT_INDEXING_MAX_ENTRIES as u64,
        ),
    ] {
        let baseline = trusted_baseline
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(default_limit);
        let Some(requested) = settings.get(key).and_then(serde_json::Value::as_u64) else {
            continue;
        };
        let raises_limit = baseline != 0 && (requested == 0 || requested > baseline);
        if !raises_limit {
            continue;
        }
        if let Some(settings) = settings.as_object_mut() {
            settings.insert(key.to_string(), serde_json::Value::from(baseline));
        }
        messages.push(format!(
            "Ignored project indexing limit increase from {}: {}={} exceeds trusted cap {}. Raise it in global or VS Code configuration instead.",
            path.display(),
            label,
            requested,
            baseline
        ));
    }
    messages
}

pub(crate) fn load_workspace_runtime(
    workspace_roots: &[PathBuf],
    raw_client_settings: &serde_json::Value,
) -> LoadedWorkspaceRuntime {
    let client_snapshot = ClientConfigurationSnapshot::from_value(raw_client_settings);
    let (global_settings, mut messages) = load_global_configuration_settings();

    let mut fallback_settings = global_settings.clone();
    merge_json_objects(&mut fallback_settings, &client_snapshot.fallback_settings());
    let fallback = ResolvedRuntimeConfiguration::from_settings(&fallback_settings);

    let mut configs = Vec::with_capacity(workspace_roots.len());
    for workspace_folder in workspace_roots {
        let client_settings = client_snapshot.settings_for_workspace_folder(workspace_folder);
        let mut effective = global_settings.clone();
        merge_project_configuration_for_root(
            &mut effective,
            &mut messages,
            workspace_folder,
            &client_settings,
        );
        merge_json_objects(&mut effective, &client_settings);
        let runtime_config = ResolvedRuntimeConfiguration::from_settings(&effective);
        let mut config =
            discover_workspace_root_config(workspace_folder, runtime_config.composer_enabled);
        config.workspace_folder = workspace_folder.clone();
        config.runtime_config = runtime_config;
        configs.push(config);
    }

    LoadedWorkspaceRuntime {
        fallback,
        configs: dedup_workspace_configs(configs),
        messages,
    }
}

fn load_client_only_workspace_runtime(
    workspace_roots: &[PathBuf],
    raw_client_settings: &serde_json::Value,
    messages: Vec<String>,
) -> LoadedWorkspaceRuntime {
    let client_snapshot = ClientConfigurationSnapshot::from_value(raw_client_settings);
    let fallback =
        ResolvedRuntimeConfiguration::from_settings(&client_snapshot.fallback_settings());
    let configs = workspace_roots
        .iter()
        .map(|workspace_folder| WorkspaceRootConfig {
            workspace_folder: workspace_folder.clone(),
            root: workspace_folder.clone(),
            namespace_map: None,
            runtime_config: ResolvedRuntimeConfiguration::from_settings(
                &client_snapshot.settings_for_workspace_folder(workspace_folder),
            ),
            index: Arc::new(WorkspaceIndex::new()),
            vendor_file_lru: Arc::new(Mutex::new(VendorFileLru::default())),
        })
        .collect();
    LoadedWorkspaceRuntime {
        fallback,
        configs: dedup_workspace_configs(configs),
        messages,
    }
}

pub(in crate::server) async fn load_workspace_runtime_blocking(
    workspace_roots: Vec<PathBuf>,
    client_settings: serde_json::Value,
    label: &'static str,
) -> LoadedWorkspaceRuntime {
    let fallback_roots = workspace_roots.clone();
    let fallback_settings = client_settings.clone();
    let path_label = format!("{} workspace root(s)", workspace_roots.len());
    match run_file_io_blocking(label, path_label, move || {
        load_workspace_runtime(&workspace_roots, &client_settings)
    })
    .await
    {
        Ok(runtime) => runtime,
        Err(message) => {
            load_client_only_workspace_runtime(&fallback_roots, &fallback_settings, vec![message])
        }
    }
}

pub(in crate::server) fn uri_is_project_config_file(uri: &Uri) -> bool {
    uri_to_path(uri.as_str())
        .and_then(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
        })
        .is_some_and(|file_name| file_name == PROJECT_CONFIG_FILE_NAME)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::server) enum ComposerMetadataChange {
    ProjectAutoload,
    VendorAutoload,
}

pub(in crate::server) fn composer_metadata_change_for_path(
    path: &Path,
) -> Option<ComposerMetadataChange> {
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

pub(in crate::server) fn should_ignore_vendor_package_composer_metadata_change(
    path: &Path,
    roots: &[PathBuf],
) -> bool {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if file_name != "composer.json" && file_name != "composer.lock" {
        return false;
    }
    path_is_under_vendor_roots(path, roots)
}

pub(in crate::server) fn uri_composer_metadata_change(
    uri: &Uri,
) -> Option<(PathBuf, ComposerMetadataChange)> {
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
                    workspace_folder: root.to_path_buf(),
                    root: effective_root,
                    namespace_map: Some(namespace_map),
                    runtime_config: ResolvedRuntimeConfiguration::default(),
                    index: Arc::new(WorkspaceIndex::new()),
                    vendor_file_lru: Arc::new(Mutex::new(VendorFileLru::default())),
                }
            }
            Err(e) => {
                tracing::warn!("Failed to parse composer.json: {}", e);
                WorkspaceRootConfig {
                    workspace_folder: root.to_path_buf(),
                    root: root.to_path_buf(),
                    namespace_map: None,
                    runtime_config: ResolvedRuntimeConfiguration::default(),
                    index: Arc::new(WorkspaceIndex::new()),
                    vendor_file_lru: Arc::new(Mutex::new(VendorFileLru::default())),
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
        workspace_folder: root.to_path_buf(),
        root: root.to_path_buf(),
        namespace_map: None,
        runtime_config: ResolvedRuntimeConfiguration::default(),
        index: Arc::new(WorkspaceIndex::new()),
        vendor_file_lru: Arc::new(Mutex::new(VendorFileLru::default())),
    }
}

pub(in crate::server) fn dedup_workspace_configs(
    configs: Vec<WorkspaceRootConfig>,
) -> Vec<WorkspaceRootConfig> {
    let mut workspace_folders = Vec::new();
    let mut unique = Vec::new();

    for config in configs {
        if workspace_folders
            .iter()
            .any(|root| root == &config.workspace_folder)
        {
            continue;
        }
        workspace_folders.push(config.workspace_folder.clone());
        unique.push(config);
    }

    unique
}

pub(in crate::server) fn remove_indexed_vendor_symbols(
    index: &WorkspaceIndex,
    roots: &[PathBuf],
) -> usize {
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

pub(in crate::server) fn path_is_under_vendor_roots(path: &Path, roots: &[PathBuf]) -> bool {
    roots
        .iter()
        .any(|root| path.starts_with(root.join("vendor")))
}

/// Find composer.json in the workspace root or immediate subdirectories.
///
/// Searches the root first, then scans depth-1 subdirectories (skipping hidden
/// directories and common non-project dirs like `node_modules`, `vendor`).
pub(in crate::server) fn find_composer_json(root: &Path) -> Option<PathBuf> {
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

pub(in crate::server) fn read_php_source_lossy(file_path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(file_path)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub(in crate::server) fn parse_and_index_php_file(
    index: &WorkspaceIndex,
    file_path: &Path,
) -> bool {
    let uri = match path_to_uri(file_path) {
        Ok(uri) => uri,
        Err(err) => {
            tracing::warn!("{}", err);
            return false;
        }
    };
    let Ok(source) = read_php_source_lossy(file_path) else {
        return false;
    };
    let mut parser = FileParser::new();
    parser.parse_full(&source);
    let Some(tree) = parser.tree() else {
        return false;
    };

    let file_symbols = extract_file_symbols(tree, &source, &uri);
    let references = collect_symbol_references_in_file(tree, &source, &file_symbols);
    index.update_file_with_references(&uri, file_symbols, references);
    true
}

pub(in crate::server) fn parse_workspace_file_for_index(
    file_path: PathBuf,
) -> WorkspaceParseResult {
    let uri = match path_to_uri(&file_path) {
        Ok(uri) => uri,
        Err(err) => {
            return WorkspaceParseResult {
                path: file_path,
                uri: String::new(),
                file_symbols: None,
                references: Vec::new(),
                symbol_count: 0,
                error: Some(err.to_string()),
            };
        }
    };
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

pub(in crate::server) async fn parse_workspace_file_for_index_blocking(
    file_path: PathBuf,
    label: &'static str,
) -> std::result::Result<WorkspaceParseResult, String> {
    let path_label = file_path.display().to_string();
    run_file_io_blocking(label, path_label, move || {
        parse_workspace_file_for_index(file_path)
    })
    .await
}

pub(in crate::server) async fn parse_and_index_php_file_blocking(
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

pub(in crate::server) fn load_cached_vendor_file(
    index: &WorkspaceIndex,
    root: &Path,
    file_path: &Path,
    config: &IndexCacheConfig,
) -> bool {
    let source = match CacheSourceFile::workspace(root, file_path) {
        Ok(source) => source,
        Err(err) => {
            tracing::debug!("{}", err);
            return false;
        }
    };
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

pub(in crate::server) async fn load_cached_vendor_file_blocking(
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

pub(in crate::server) fn save_vendor_index_cache(
    index: &WorkspaceIndex,
    root: &Path,
    config: &IndexCacheConfig,
) {
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

pub(in crate::server) async fn save_vendor_index_cache_blocking(
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

pub(in crate::server) async fn commit_staged_vendor_file(
    index: &WorkspaceIndex,
    vendor_file_lru: &Arc<Mutex<VendorFileLru>>,
    staged_index: &WorkspaceIndex,
    uri: String,
    track_in_vendor_lru: bool,
    indexing_run: Option<&IndexingRunLease>,
    indexing_runs: Option<&IndexingRunCoordinator>,
) -> bool {
    let Some(file_symbols) = staged_index
        .file_symbols
        .get(&uri)
        .map(|symbols| symbols.value().as_ref().clone())
    else {
        return false;
    };
    let references = staged_index
        .file_references
        .get(&uri)
        .map(|references| references.value().clone())
        .unwrap_or_default();
    let mut lru = vendor_file_lru.lock().await;
    let commit = || {
        index.update_file_with_references(&uri, file_symbols, references);
        if track_in_vendor_lru {
            let evicted = lru.touch(uri);
            for uri in evicted {
                index.remove_file(&uri);
            }
        }
    };
    match indexing_run {
        Some(run) => run.commit_index_if_current(commit).is_some(),
        None if indexing_runs.is_some() => {
            indexing_runs
                .expect("checked indexing coordinator")
                .commit_unleased_index_mutation(commit);
            true
        }
        None => {
            commit();
            true
        }
    }
}

pub(in crate::server) async fn save_vendor_index_cache_for_run_blocking(
    index: Arc<WorkspaceIndex>,
    root: PathBuf,
    config: IndexCacheConfig,
    indexing_run: &IndexingRunLease,
) {
    let cache_path = cache::cache_file_path_for_namespace(&root, CacheNamespace::Vendor);
    let cache_path_for_prepare = cache_path.clone();
    let path_label = root.display().to_string();
    let prepared = run_file_io_blocking("vendor cache prepare", path_label, move || {
        let sources = indexed_vendor_cache_sources(&index, &root);
        if sources.is_empty() {
            return Ok(None);
        }
        let cache_to_save = cache::build_cache_from_sources(&index, &root, &sources, &config);
        cache::prepare_cache_write(&cache_path_for_prepare, &cache_to_save).map(Some)
    })
    .await;
    match prepared {
        Ok(Ok(Some(prepared))) => {
            if let Some(Err(error)) = indexing_run.commit_if_current(|| prepared.commit()) {
                tracing::warn!(
                    "Failed to save vendor index cache at {}: {}",
                    cache_path.display(),
                    error
                );
            }
        }
        Ok(Ok(None)) => {}
        Ok(Err(error)) => tracing::warn!(
            "Failed to prepare vendor index cache at {}: {}",
            cache_path.display(),
            error
        ),
        Err(message) => tracing::warn!("{}", message),
    }
}

pub(in crate::server) fn indexed_vendor_cache_sources(
    index: &WorkspaceIndex,
    root: &Path,
) -> Vec<CacheSourceFile> {
    let vendor_dir = root.join("vendor");
    let mut sources: Vec<CacheSourceFile> = index
        .file_symbols
        .iter()
        .filter_map(|entry| {
            let path = uri_to_path(entry.key())?;
            if path.starts_with(&vendor_dir) && path.is_file() {
                CacheSourceFile::workspace(root, &path).ok()
            } else {
                None
            }
        })
        .collect();
    sources.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    sources.dedup_by(|left, right| left.relative_path == right.relative_path);
    sources
}

#[allow(clippy::too_many_arguments)]
pub(in crate::server) async fn preload_vendor_entrypoints(
    index: Arc<WorkspaceIndex>,
    root: &Path,
    exclude_paths: &[PathBuf],
    traversal_limits: TraversalLimits,
    php_version: PhpVersion,
    vendor_autoload_cache: &Arc<Mutex<VendorAutoloadCache>>,
    vendor_file_lru: &Arc<Mutex<VendorFileLru>>,
    load_epoch: &Arc<tokio::sync::RwLock<u64>>,
    indexing_run: Option<&IndexingRunLease>,
) -> usize {
    let vendor_dir = root.join("vendor");
    if !vendor_dir.is_dir() {
        return 0;
    }

    let _epoch_guard = load_epoch.read().await;

    let Some(autoload) =
        cached_vendor_autoload_map_pinned(vendor_autoload_cache, &vendor_dir).await
    else {
        return 0;
    };
    let entrypoint_files = vendor_autoload_file_paths_from_map_blocking(
        autoload,
        root.to_path_buf(),
        exclude_paths.to_vec(),
    )
    .await;
    if entrypoint_files.is_empty() {
        return 0;
    }

    let cache_config =
        vendor_index_cache_config(root, php_version, exclude_paths, traversal_limits);
    let mut loaded = 0;
    for file_path in entrypoint_files {
        if indexing_run.is_some_and(|run| !run.is_current()) {
            break;
        }
        if !file_path.is_file() {
            continue;
        }

        let staged_index = Arc::new(WorkspaceIndex::new());
        let from_cache = load_cached_vendor_file_blocking(
            staged_index.clone(),
            root.to_path_buf(),
            file_path.clone(),
            cache_config.clone(),
        )
        .await;
        if from_cache
            || parse_and_index_php_file_blocking(
                staged_index.clone(),
                file_path.clone(),
                "vendor preload PHP file index",
            )
            .await
        {
            let Ok(uri) = path_to_uri(&file_path) else {
                continue;
            };
            if !commit_staged_vendor_file(
                &index,
                vendor_file_lru,
                &staged_index,
                uri,
                true,
                indexing_run,
                None,
            )
            .await
            {
                break;
            }
            loaded += 1;
        }
    }

    if loaded > 0 {
        if let Some(indexing_run) = indexing_run {
            save_vendor_index_cache_for_run_blocking(
                index,
                root.to_path_buf(),
                cache_config,
                indexing_run,
            )
            .await;
        } else {
            save_vendor_index_cache_blocking(index, root.to_path_buf(), cache_config).await;
        }
        tracing::debug!(
            "Preloaded {} vendor autoload entrypoint file(s) for {}",
            loaded,
            root.display()
        );
    }
    loaded
}

pub(in crate::server) struct WorkspaceIndexingReport {
    ready_status: serde_json::Value,
    started_at: Instant,
}

pub(in crate::server) struct CompletedWorkspaceIndexingRun {
    pub(in crate::server) expected_config: WorkspaceRootConfig,
    pub(in crate::server) run: IndexingRunLease,
    pub(in crate::server) _guard: IndexingRunGuard,
    pub(in crate::server) report: WorkspaceIndexingReport,
}

pub(in crate::server) struct WorkspaceIndexingPostprocessContext {
    pub(in crate::server) open_files: Arc<DashMap<String, FileParser>>,
    pub(in crate::server) template_documents: Arc<DashMap<String, TemplateDocument>>,
    pub(in crate::server) document_versions: Arc<DashMap<String, OpenDocumentState>>,
    pub(in crate::server) diagnostics_publisher: DiagnosticsPublisher,
    pub(in crate::server) indexing_status_publisher: IndexingStatusPublisher,
    pub(in crate::server) aggregate_index: Arc<WorkspaceIndex>,
    pub(in crate::server) aggregate_rebuild: Arc<Mutex<()>>,
    pub(in crate::server) runtime_state: Arc<Mutex<Arc<WorkspaceRuntimeState>>>,
    pub(in crate::server) twig_context_disk_cache: Arc<Mutex<TwigContextDiskCache>>,
    pub(in crate::server) semantic_tokens_cache: Arc<Mutex<SemanticTokensCache>>,
    pub(in crate::server) vendor_autoload_cache: Arc<Mutex<VendorAutoloadCache>>,
    pub(in crate::server) vendor_lazy_loads: Arc<VendorLazyLoadCoordinator>,
    pub(in crate::server) vendor_load_epoch: Arc<tokio::sync::RwLock<u64>>,
    pub(in crate::server) external_symlinks: Arc<ExternalSymlinkManager>,
}

fn current_completed_indexing_runs(
    completed: Vec<CompletedWorkspaceIndexingRun>,
    state: &WorkspaceRuntimeState,
) -> Vec<CompletedWorkspaceIndexingRun> {
    completed
        .into_iter()
        .filter_map(|mut completed| {
            if !completed.run.is_current() {
                return None;
            }
            let current = state.configs.iter().find(|current| {
                current.workspace_folder == completed.expected_config.workspace_folder
                    && Arc::ptr_eq(&current.index, &completed.expected_config.index)
            })?;
            completed.expected_config = current.clone();
            Some(completed)
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub(in crate::server) async fn rebuild_aggregate_for_indexing_runs(
    coordinator: &Arc<IndexingRunCoordinator>,
    aggregate_rebuild: &Arc<Mutex<()>>,
    aggregate_index: &Arc<WorkspaceIndex>,
    configs: Vec<WorkspaceRootConfig>,
    open_files: Arc<DashMap<String, FileParser>>,
    template_documents: Arc<DashMap<String, TemplateDocument>>,
    document_versions: Arc<DashMap<String, OpenDocumentState>>,
    runs: &[IndexingRunLease],
) -> bool {
    let _aggregate_rebuild = aggregate_rebuild.lock().await;
    if runs.iter().any(|run| !run.is_current()) {
        return false;
    }
    let expected_source_revision = coordinator.aggregate_source_revision();
    let mut source_indexes = configs
        .iter()
        .map(|config| config.index.clone())
        .collect::<Vec<_>>();
    source_indexes.sort_by_key(|index| Arc::as_ptr(index) as usize);
    source_indexes.dedup_by(|left, right| Arc::ptr_eq(left, right));
    let expected_index_revisions = source_indexes
        .iter()
        .map(|index| index.revision_snapshot())
        .collect::<Vec<_>>();
    let staged = Arc::new(WorkspaceIndex::new());
    let staged_for_build = staged.clone();
    let built = tokio::task::spawn_blocking(move || {
        rebuild_aggregate_index(
            &staged_for_build,
            &configs,
            &open_files,
            &template_documents,
            &document_versions,
        );
    })
    .await
    .is_ok();
    if !built {
        return false;
    }
    let coordinator = coordinator.clone();
    let runs = runs.to_vec();
    let aggregate_index = aggregate_index.clone();
    tokio::task::spawn_blocking(move || {
        coordinator
            .commit_aggregate_if_current(&runs, expected_source_revision, || {
                aggregate_index.replace_from_staged_if_sources_current(
                    &staged,
                    &source_indexes,
                    &expected_index_revisions,
                )
            })
            .is_some_and(|committed| committed)
    })
    .await
    .unwrap_or(false)
}

pub(in crate::server) async fn postprocess_workspace_indexing_runs(
    context: WorkspaceIndexingPostprocessContext,
    completed: Vec<CompletedWorkspaceIndexingRun>,
) {
    let WorkspaceIndexingPostprocessContext {
        open_files,
        template_documents,
        document_versions,
        diagnostics_publisher,
        indexing_status_publisher,
        aggregate_index,
        aggregate_rebuild,
        runtime_state,
        twig_context_disk_cache,
        semantic_tokens_cache,
        vendor_autoload_cache,
        vendor_lazy_loads,
        vendor_load_epoch,
        external_symlinks,
    } = context;

    let state = runtime_state.lock().await.clone();
    let completed = current_completed_indexing_runs(completed, &state);
    if completed.is_empty() {
        return;
    }

    let current_state = runtime_state.lock().await.clone();
    let runs = completed
        .iter()
        .map(|completed| completed.run.clone())
        .collect::<Vec<_>>();
    if !rebuild_aggregate_for_indexing_runs(
        &runs[0].coordinator(),
        &aggregate_rebuild,
        &aggregate_index,
        current_state.configs.clone(),
        open_files.clone(),
        template_documents.clone(),
        document_versions.clone(),
        &runs,
    )
    .await
    {
        return;
    }

    let state = runtime_state.lock().await.clone();
    let completed = current_completed_indexing_runs(completed, &state);
    if completed.is_empty() {
        return;
    }
    let runtime_generation = state.generation;
    let workspace_roots = state
        .configs
        .iter()
        .map(|config| config.root.clone())
        .collect::<Vec<_>>();
    let configs = completed
        .iter()
        .map(|completed| completed.expected_config.clone())
        .collect::<Vec<_>>();
    let runs = completed
        .iter()
        .map(|completed| completed.run.clone())
        .collect::<Vec<_>>();
    let workspace_folders = configs
        .iter()
        .map(|config| config.workspace_folder.clone())
        .collect::<Vec<_>>();

    {
        let mut cache = twig_context_disk_cache.lock().await;
        for (config, run) in configs.iter().zip(&runs) {
            run.commit_if_current(|| {
                cache.evict_index(&config.index);
            });
        }
    }
    refresh_open_twig_contexts_for_state(OpenTwigContextRefreshState {
        open_files: &open_files,
        template_documents: &template_documents,
        document_versions: &document_versions,
        index: &aggregate_index,
        fallback_index: &state.fallback_index,
        workspace_roots: &workspace_roots,
        workspace_configs: &state.configs,
        workspace_folders_filter: Some(&workspace_folders),
        indexing_runs: &runs,
        twig_context_disk_cache: &twig_context_disk_cache,
        semantic_tokens_cache: &semantic_tokens_cache,
    })
    .await;

    let open_file_uris = open_files
        .iter()
        .map(|entry| entry.key().clone())
        .collect::<Vec<_>>();
    for uri_str in open_file_uris {
        let Some(snapshot) = open_document_snapshot_from_state(
            &open_files,
            &template_documents,
            &document_versions,
            &uri_str,
        ) else {
            continue;
        };
        let Some(config) = workspace_config_for_uri_from_configs(&configs, &uri_str) else {
            continue;
        };
        let Some(completed_run) = completed
            .iter()
            .find(|completed| completed.run.workspace_folder() == config.workspace_folder)
        else {
            continue;
        };
        let run = &completed_run.run;
        if !run.is_current() {
            continue;
        }
        let committed = run
            .commit_index_if_current(|| {
                commit_open_document_index_snapshot_if_current(
                    OpenDocumentIndexCommitContext {
                        open_files: &open_files,
                        template_documents: &template_documents,
                        document_versions: &document_versions,
                        index: &aggregate_index,
                        root_index: Some(&config.index),
                        uri_str: &uri_str,
                    },
                    &snapshot,
                )
            })
            .unwrap_or(false);
        if !committed {
            continue;
        }
        let Ok(uri) = uri_str.parse::<Uri>() else {
            continue;
        };
        let Some(computation_sequence) =
            run.commit_if_current(|| diagnostics_publisher.start_computation(&uri_str))
        else {
            continue;
        };
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
            traversal_limits: runtime.traversal_limits,
            php_version: runtime.php_version,
            index_vendor: runtime.index_vendor,
            vendor_autoload_cache: vendor_autoload_cache.clone(),
            vendor_file_lru: config.vendor_file_lru.clone(),
            lazy_loads: vendor_lazy_loads.clone(),
            load_epoch: vendor_load_epoch.clone(),
            external_symlinks: Some(external_symlinks.clone()),
            runtime_generation,
            indexing_run: Some(run.clone()),
            indexing_runs: Some(run.coordinator()),
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
            if !run.is_current() {
                continue;
            }
        }
        let mut diagnostics = compute_source_diagnostics_blocking(
            uri_str.clone(),
            snapshot.source.clone(),
            config.index.clone(),
            diagnostics_config,
            version,
        )
        .await;
        if !run.is_current() {
            continue;
        }
        if let Some(template) = &template_document {
            diagnostics = template.map_diagnostics_to_original(
                diagnostics,
                diagnostics_config.mode == DiagnosticsMode::Off,
            );
        } else if diagnostics_config.mode == DiagnosticsMode::BasicSemantic && runtime.index_vendor
        {
            diagnostics = filter_lazy_resolved_symbol_diagnostics_with_context(
                &config.index,
                &vendor_lazy_context,
                diagnostics,
            )
            .await;
            if !run.is_current() {
                continue;
            }
        }
        let publish = DiagnosticPublishRequest {
            uri,
            diagnostics,
            version,
            expected_state: document_state,
            expected_template: template_document,
            require_idle_index: false,
            expected_runtime_generation: runtime_generation,
            indexing_workspace_folder: Some(config.workspace_folder.clone()),
            expected_indexing_run: Some(run.identity()),
            expected_runtime_config: runtime.clone(),
            expected_index: config.index.clone(),
            computation_sequence,
        };
        run.commit_if_current(|| diagnostics_publisher.publish(publish));
    }

    for completed in completed {
        let mut ready_status = completed.report.ready_status;
        if let Some(status) = ready_status.as_object_mut() {
            status.insert(
                "elapsedMs".to_string(),
                serde_json::Value::from(elapsed_ms(completed.report.started_at)),
            );
        }
        indexing_status_publisher.publish_for_run(&completed.run, runtime_generation, ready_status);
    }
}

/// Background workspace indexing.
///
/// Scans PHP files in the workspace and adds their symbols to the index.
#[allow(clippy::too_many_arguments)]
pub(in crate::server) async fn index_workspace(
    client: &Client,
    indexing_status_publisher: &IndexingStatusPublisher,
    live: WorkspaceLiveIndexContext<'_>,
    root: &Path,
    namespace_map: Option<&NamespaceMap>,
    options: &WorkspaceIndexingOptions,
    indexing_run: &IndexingRunLease,
    external_symlinks: &Arc<ExternalSymlinkManager>,
    runtime_generation: u64,
) -> std::result::Result<Option<WorkspaceIndexingReport>, String> {
    let root_label = root.display().to_string();
    let started_at = Instant::now();
    if !indexing_run.is_current() {
        tracing::debug!("Workspace indexing cancelled before start: {}", root_label);
        return Ok(None);
    }

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
    let explicit_files = namespace_map
        .map(|namespace_map| namespace_map.files.clone())
        .unwrap_or_default();
    let php_discovery = collect_php_files_blocking(
        source_dirs,
        root.to_path_buf(),
        options.exclude_paths.clone(),
        explicit_files,
        options.traversal_limits,
        indexing_run.token().clone(),
    )
    .await?;
    if php_discovery.stop_reason == Some(TraversalStopReason::DeadlineExceeded) {
        return Err(format!(
            "Workspace PHP file discovery exceeded {} ms for {}",
            FILE_IO_TIMEOUT_MS,
            root.display()
        ));
    }
    if !indexing_run.is_current() {
        tracing::debug!(
            "Workspace indexing cancelled after discovery: {}",
            root_label
        );
        return Ok(None);
    }
    let mut watcher_aliases = php_discovery.symlink_aliases.clone();
    match collect_feature_symlink_aliases_blocking(
        root.to_path_buf(),
        indexing_run.workspace_folder().to_path_buf(),
        options.exclude_paths.clone(),
        options.traversal_limits,
        indexing_run.token().clone(),
    )
    .await
    {
        Ok(feature_discovery) => {
            if feature_discovery.stop_reason.is_some() {
                tracing::warn!(
                    "Feature symlink discovery for {} stopped after {} entries: {:?}",
                    root.display(),
                    feature_discovery.stats.visited_entries,
                    feature_discovery.stop_reason
                );
            }
            watcher_aliases.extend(feature_discovery.symlink_aliases);
        }
        Err(message) => tracing::warn!("{}", message),
    }
    if !indexing_run.is_current() {
        return Ok(None);
    }
    external_symlinks
        .publish_workspace(
            indexing_run.workspace_folder().to_path_buf(),
            root.to_path_buf(),
            watcher_aliases,
            php_discovery.physical_files.clone(),
            indexing_run,
        )
        .await;
    let traversal_stop_reason = php_discovery.stop_reason;
    let traversal_stats = php_discovery.stats;
    let traversal_truncated = php_discovery.truncated();
    let (truncation_reason, truncation_limit) = match traversal_stop_reason {
        Some(TraversalStopReason::MaxFiles { limit }) => (Some("maxFiles"), Some(limit)),
        Some(TraversalStopReason::MaxEntries { limit }) => (Some("maxEntries"), Some(limit)),
        _ => (None, None),
    };
    if let (Some(reason), Some(limit)) = (truncation_reason, truncation_limit) {
        let message = format!(
            "Workspace file discovery for {} was truncated by indexing.{}={} after visiting {} entries",
            root.display(),
            reason,
            limit,
            traversal_stats.visited_entries
        );
        tracing::warn!("{}", message);
        client.log_message(MessageType::WARNING, message).await;
    }

    let all_files = php_discovery.files;

    let total = all_files.len();
    tracing::info!("Indexing {} PHP files", total);

    let cache_path = cache::cache_file_path(root);
    // Keep disk/cache data separate from the live index so an unsaved open
    // document is never overwritten, even transiently, during indexing.
    let disk_index = WorkspaceIndex::new();
    let cache_report = cache::load_valid_cached_files(
        &disk_index,
        &cache_path,
        root,
        &all_files,
        &options.cache_config,
    );
    let cached_uris: Vec<String> = disk_index
        .file_symbols
        .iter()
        .map(|entry| entry.key().clone())
        .collect();
    for uri_str in cached_uris {
        let Some(file_symbols) = disk_index
            .file_symbols
            .get(&uri_str)
            .map(|symbols| symbols.value().as_ref().clone())
        else {
            continue;
        };
        let references = disk_index
            .file_references
            .get(&uri_str)
            .map(|references| references.value().clone())
            .unwrap_or_default();
        if indexing_run
            .commit_index_if_current(|| {
                commit_workspace_disk_file_preserving_open(
                    DiskPhpIndexCommitContext {
                        open_files: live.open_files,
                        template_documents: live.template_documents,
                        document_versions: live.document_versions,
                        index: live.index,
                        root_index: Some(live.root_index),
                        uri_str: &uri_str,
                    },
                    file_symbols,
                    references,
                );
            })
            .is_none()
        {
            return Ok(None);
        }
    }
    if !indexing_run.is_current() {
        tracing::debug!(
            "Workspace indexing cancelled after cache load: {}",
            root_label
        );
        return Ok(None);
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

    indexing_status_publisher.publish_for_run(
        indexing_run,
        runtime_generation,
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
            "parseConcurrency": indexing_parse_concurrency(),
            "truncated": traversal_truncated,
            "truncationReason": truncation_reason,
            "truncationLimit": truncation_limit,
            "visitedEntries": traversal_stats.visited_entries
        }),
    );

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
        if !indexing_run.is_current() {
            parse_tasks.abort_all();
            tracing::debug!(
                "Workspace indexing cancelled after {}/{} files: {}",
                done,
                total,
                root_label
            );
            return Ok(None);
        }

        let parsed = match result {
            Ok(parsed) => parsed,
            Err(err) => {
                let message = format!("Workspace indexing task failed: {}", err);
                indexing_status_publisher.publish_for_run(
                    indexing_run,
                    runtime_generation,
                    serde_json::json!({
                        "phase": "error",
                        "root": root_label,
                        "message": message,
                        "indexedFiles": done,
                        "totalFiles": total,
                        "indexedSymbols": indexed_symbols,
                        "elapsedMs": elapsed_ms(started_at)
                    }),
                );
                return Err(message);
            }
        };

        if let Some(file_symbols) = parsed.file_symbols {
            disk_index.update_file_with_references(
                &parsed.uri,
                file_symbols.clone(),
                parsed.references.clone(),
            );
            if indexing_run
                .commit_index_if_current(|| {
                    commit_workspace_disk_file_preserving_open(
                        DiskPhpIndexCommitContext {
                            open_files: live.open_files,
                            template_documents: live.template_documents,
                            document_versions: live.document_versions,
                            index: live.index,
                            root_index: Some(live.root_index),
                            uri_str: &parsed.uri,
                        },
                        file_symbols,
                        parsed.references,
                    );
                })
                .is_none()
            {
                parse_tasks.abort_all();
                return Ok(None);
            }
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
            if !indexing_run.is_current() {
                parse_tasks.abort_all();
                tracing::debug!(
                    "Workspace indexing cancelled before scheduling more parse tasks: {}",
                    root_label
                );
                return Ok(None);
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
            indexing_status_publisher.publish_for_run(
                indexing_run,
                runtime_generation,
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
            );
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
        cache::build_cache_from_index(&disk_index, root, &all_files, &options.cache_config);
    let cache_path_for_prepare = cache_path.clone();
    let prepared = run_file_io_blocking(
        "workspace cache prepare",
        cache_path.display().to_string(),
        move || cache::prepare_cache_write(&cache_path_for_prepare, &cache_to_save),
    )
    .await;
    match prepared {
        Ok(Ok(prepared)) => {
            if let Some(Err(error)) = indexing_run.commit_if_current(|| prepared.commit()) {
                tracing::warn!(
                    "Failed to save workspace index cache at {}: {}",
                    cache_path.display(),
                    error
                );
            }
        }
        Ok(Err(error)) => tracing::warn!(
            "Failed to prepare workspace index cache at {}: {}",
            cache_path.display(),
            error
        ),
        Err(message) => tracing::warn!("{}", message),
    }

    let ready_status = serde_json::json!({
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
        "cachePath": cache_path.display().to_string(),
        "truncated": traversal_truncated,
        "truncationReason": truncation_reason,
        "truncationLimit": truncation_limit,
        "visitedEntries": traversal_stats.visited_entries
    });

    Ok(Some(WorkspaceIndexingReport {
        ready_status,
        started_at,
    }))
}

impl PhpLspBackend {
    pub(in crate::server) async fn remove_uri_from_current_runtime_indexes(&self, uri_str: &str) {
        for _ in 0..4 {
            let state = self.runtime_state_snapshot().await;
            let _aggregate_rebuild = self.aggregate_rebuild.lock().await;
            self.index.remove_file(uri_str);
            for index in workspace_indexes_for_uri(&state, uri_str, false) {
                index.remove_file(uri_str);
            }
            if Arc::ptr_eq(&state, &self.runtime_state_snapshot().await) {
                return;
            }
        }
    }

    async fn remove_uri_from_current_vendor_lrus(&self, uri_str: &str) {
        let Some(path) = uri_to_path(uri_str) else {
            return;
        };
        let state = self.runtime_state_snapshot().await;
        for config in workspace_configs_for_path_scope(&state.configs, &path) {
            config.vendor_file_lru.lock().await.remove(uri_str);
        }
        self.vendor_file_lru.lock().await.remove(uri_str);
    }

    pub(in crate::server) async fn commit_closed_php_snapshot_to_current_runtime(
        &self,
        uri_str: &str,
        file_symbols: Option<php_lsp_types::FileSymbols>,
        references: Vec<php_lsp_types::SymbolReference>,
    ) -> bool {
        for _ in 0..4 {
            let state = self.runtime_state_snapshot().await;
            let _aggregate_rebuild = self.aggregate_rebuild.lock().await;
            let dashmap::mapref::entry::Entry::Vacant(_open_entry) =
                self.open_files.entry(uri_str.to_string())
            else {
                return false;
            };
            if self.template_documents.contains_key(uri_str)
                || self.document_versions.contains_key(uri_str)
            {
                return false;
            }
            if let Some(file_symbols) = file_symbols.as_ref() {
                let indexes = workspace_indexes_for_uri(&state, uri_str, true);
                if indexes.is_empty() {
                    self.index.remove_file(uri_str);
                } else {
                    self.index.update_file_with_references(
                        uri_str,
                        file_symbols.clone(),
                        references.clone(),
                    );
                    for index in indexes {
                        index.update_file_with_references(
                            uri_str,
                            file_symbols.clone(),
                            references.clone(),
                        );
                    }
                }
            } else {
                self.index.remove_file(uri_str);
                for index in workspace_indexes_for_uri(&state, uri_str, false) {
                    index.remove_file(uri_str);
                }
            }
            drop(_open_entry);
            if Arc::ptr_eq(&state, &self.runtime_state_snapshot().await) {
                return true;
            }
        }
        false
    }

    pub(in crate::server) async fn path_is_excluded_by_config(&self, path: &Path) -> bool {
        let state = self.runtime_state_snapshot().await;
        workspace_config_for_path_from_configs(&state.configs, path).is_some_and(|config| {
            path_is_excluded(path, &config.root, &config.runtime_config.exclude_paths)
        })
    }

    /// Reindex one changed PHP file from the open buffer when available,
    /// otherwise from disk.
    pub(in crate::server) async fn reindex_php_file(&self, uri: &Uri) {
        let uri_str = uri.as_str().to_string();
        if !uri_is_php_file(uri) {
            return;
        }
        let refresh_twig_contexts = !is_blade_template_uri(&uri_str);
        if is_blade_template_uri(&uri_str) {
            let committed = if let Some(snapshot) = self.open_document_snapshot(&uri_str) {
                if let Some(expected) = snapshot.document_state {
                    self.synchronize_open_document_index_to_current_runtime(
                        &uri_str, expected, false,
                    )
                    .await
                } else {
                    false
                }
            } else {
                self.commit_closed_php_snapshot_to_current_runtime(&uri_str, None, Vec::new())
                    .await
            };
            self.semantic_tokens_cache.lock().await.remove(&uri_str);
            if committed && self.open_document_snapshot(&uri_str).is_some() {
                self.publish_diagnostics(uri).await;
            }
            return;
        }

        if let Some(path) = uri_to_path(&uri_str) {
            let roots = self.current_workspace_roots().await;
            if path_is_under_vendor_roots(&path, &roots)
                && workspace_indexes_for_uri(
                    self.runtime_state_snapshot().await.as_ref(),
                    &uri_str,
                    false,
                )
                .iter()
                .all(|index| !index.file_symbols.contains_key(&uri_str))
            {
                return;
            }
            let state = self.runtime_state_snapshot().await;
            if workspace_indexes_for_uri(&state, &uri_str, true).is_empty() {
                self.commit_closed_php_snapshot_to_current_runtime(&uri_str, None, Vec::new())
                    .await;
                self.semantic_tokens_cache.lock().await.remove(&uri_str);
                return;
            }
        }

        if let Some(snapshot) = self.open_document_snapshot(&uri_str) {
            if let Some(expected) = snapshot.document_state {
                self.synchronize_open_document_index_to_current_runtime(&uri_str, expected, true)
                    .await;
            }
            self.semantic_tokens_cache.lock().await.remove(&uri_str);
            self.publish_diagnostics(uri).await;
            if refresh_twig_contexts {
                self.refresh_open_twig_contexts_and_republish_diagnostics()
                    .await;
            }
            return;
        }

        let Some(path) = uri_to_path(&uri_str) else {
            return;
        };

        let (file_symbols, references) =
            match parse_workspace_file_for_index_blocking(path.clone(), "watched PHP file reindex")
                .await
            {
                Ok(parsed) => {
                    if let Some(error) = parsed.error.as_ref() {
                        tracing::debug!(
                            "Failed to reindex watched PHP file {}, removing from index: {}",
                            path.display(),
                            error
                        );
                    }
                    (parsed.file_symbols, parsed.references)
                }
                Err(message) => {
                    tracing::warn!(
                    "Failed to schedule watched PHP file reindex for {}, removing from index: {}",
                    path.display(),
                    message
                );
                    (None, Vec::new())
                }
            };

        let committed_disk = self
            .commit_closed_php_snapshot_to_current_runtime(&uri_str, file_symbols, references)
            .await;
        if !committed_disk {
            if let Some(snapshot) = self.open_document_snapshot(&uri_str) {
                if let Some(expected) = snapshot.document_state {
                    self.synchronize_open_document_index_to_current_runtime(
                        &uri_str, expected, true,
                    )
                    .await;
                }
            }
        }

        self.semantic_tokens_cache.lock().await.remove(&uri_str);
        if refresh_twig_contexts {
            self.refresh_open_twig_contexts_and_republish_diagnostics()
                .await;
        }
    }

    /// Remove one PHP file from all server-side caches/indexes.
    pub(in crate::server) async fn remove_php_file(&self, uri: &Uri) {
        if !uri_is_php_file(uri) {
            return;
        }

        let uri_str = uri.as_str().to_string();
        self.remove_uri_from_current_vendor_lrus(&uri_str).await;
        match self.open_files.entry(uri_str.clone()) {
            dashmap::mapref::entry::Entry::Occupied(entry) => {
                self.document_versions.remove(&uri_str);
                self.documents_requiring_full_sync.remove(&uri_str);
                self.template_documents.remove(&uri_str);
                entry.remove();
            }
            dashmap::mapref::entry::Entry::Vacant(_) => {
                self.document_versions.remove(&uri_str);
                self.documents_requiring_full_sync.remove(&uri_str);
                self.template_documents.remove(&uri_str);
            }
        }
        self.remove_uri_from_current_runtime_indexes(&uri_str).await;
        self.cancel_debounced_diagnostics(&uri_str).await;
        self.cancel_analyzer_run(&uri_str).await;
        self.cancel_formatter_run(&uri_str).await;
        self.semantic_tokens_cache.lock().await.remove(&uri_str);
        self.publish_empty_diagnostics_if_closed(uri.clone()).await;
        if !is_blade_template_uri(&uri_str) {
            self.refresh_open_twig_contexts_and_republish_diagnostics()
                .await;
        }
    }

    pub(in crate::server) async fn rename_php_file(&self, old_uri: &Uri, new_uri: &Uri) {
        let old_is_php = uri_is_php_file(old_uri);
        let new_is_php = uri_is_php_file(new_uri);

        if !old_is_php && !new_is_php {
            return;
        }

        let old_uri_str = old_uri.as_str().to_string();
        let (moved_parser, moved_template, moved_version, moved_full_sync_generation) =
            match self.open_files.entry(old_uri_str.clone()) {
                dashmap::mapref::entry::Entry::Occupied(entry) => {
                    let version = self
                        .document_versions
                        .remove(&old_uri_str)
                        .map(|(_, version)| version);
                    let template = self
                        .template_documents
                        .remove(&old_uri_str)
                        .map(|(_, template)| template);
                    let full_sync_generation = self
                        .documents_requiring_full_sync
                        .remove(&old_uri_str)
                        .map(|(_, generation)| generation);
                    (
                        Some(entry.remove()),
                        template,
                        version,
                        full_sync_generation,
                    )
                }
                dashmap::mapref::entry::Entry::Vacant(_) => {
                    let version = self
                        .document_versions
                        .remove(&old_uri_str)
                        .map(|(_, version)| version);
                    let template = self
                        .template_documents
                        .remove(&old_uri_str)
                        .map(|(_, template)| template);
                    let full_sync_generation = self
                        .documents_requiring_full_sync
                        .remove(&old_uri_str)
                        .map(|(_, generation)| generation);
                    (None, template, version, full_sync_generation)
                }
            };
        self.cancel_debounced_diagnostics(&old_uri_str).await;
        self.cancel_analyzer_run(&old_uri_str).await;
        self.cancel_analyzer_run(new_uri.as_str()).await;
        self.cancel_formatter_run(&old_uri_str).await;
        self.cancel_formatter_run(new_uri.as_str()).await;
        if old_is_php {
            self.remove_uri_from_current_runtime_indexes(&old_uri_str)
                .await;
            self.remove_uri_from_current_vendor_lrus(&old_uri_str).await;
            self.semantic_tokens_cache.lock().await.remove(&old_uri_str);
            self.publish_empty_diagnostics_if_closed(old_uri.clone())
                .await;
        }

        if !new_is_php {
            if old_is_php && !is_blade_template_uri(&old_uri_str) {
                self.refresh_open_twig_contexts_and_republish_diagnostics()
                    .await;
            }
            return;
        }

        let new_uri_str = new_uri.as_str().to_string();
        let new_request = self.request_context_for_uri(&new_uri_str).await;
        let new_index = new_request.index(&self.index);
        let new_excluded = if let Some(path) = uri_to_path(new_uri.as_str()) {
            self.path_is_excluded_by_config(&path).await
        } else {
            false
        };

        let Some(mut parser) = moved_parser else {
            self.reindex_php_file(new_uri).await;
            return;
        };
        let state = moved_version.unwrap_or_else(|| {
            tracing::warn!(
                "Open document {} had no version while it was renamed; assigning a new lifetime",
                old_uri_str
            );
            self.next_document_state(0)
        });
        let requires_full_sync = moved_full_sync_generation == Some(state.generation);

        let destination_is_template = is_blade_template_uri(&new_uri_str);
        let template = if destination_is_template {
            let source = moved_template
                .as_ref()
                .map(|template| template.original_source().to_string())
                .unwrap_or_else(|| parser.source());
            let template = preprocess_blade_template(&source);
            let mut virtual_parser = FileParser::new();
            virtual_parser.parse_full(template.virtual_source());
            parser = virtual_parser;
            Some(template)
        } else {
            if let Some(template) = moved_template {
                let mut source_parser = FileParser::new();
                source_parser.parse_full(template.original_source());
                parser = source_parser;
            }
            None
        };

        let indexed_file = if destination_is_template || new_excluded {
            None
        } else {
            parser.tree().map(|tree| {
                let source = parser.source();
                let file_symbols = extract_file_symbols(tree, &source, &new_uri_str);
                let references = collect_symbol_references_in_file(tree, &source, &file_symbols);
                (file_symbols, references)
            })
        };

        let committed = commit_renamed_open_document_with_hook(
            RenamedOpenDocumentCommitContext {
                open_files: &self.open_files,
                template_documents: &self.template_documents,
                document_versions: &self.document_versions,
                documents_requiring_full_sync: &self.documents_requiring_full_sync,
                closed_document_reload_tokens: &self.closed_document_reload_tokens,
                uri_str: &new_uri_str,
            },
            RenamedOpenDocument {
                parser,
                template,
                state,
                requires_full_sync,
            },
            || {
                if let Some((file_symbols, references)) = indexed_file {
                    update_aggregate_and_root_index(
                        &self.index,
                        &new_index,
                        &new_uri_str,
                        file_symbols,
                        references,
                    );
                } else {
                    remove_from_aggregate_and_root_index(&self.index, &new_index, &new_uri_str);
                }
            },
        );
        if !committed {
            tracing::debug!(
                "Skipped stale rename state for {} because the destination is already open",
                new_uri_str
            );
            return;
        }
        self.synchronize_open_document_index_to_current_runtime(
            &new_uri_str,
            state,
            !destination_is_template,
        )
        .await;

        self.semantic_tokens_cache.lock().await.remove(&new_uri_str);
        if !new_excluded {
            self.publish_diagnostics(new_uri).await;
        }
    }
}
