//! Workspace LSP handlers extracted from `server.rs`.

use crate::util::uri::path_to_uri;

use super::super::*;

struct RenamedOpenDocument {
    parser: FileParser,
    template: Option<TemplateDocument>,
    state: OpenDocumentState,
}

struct RenamedOpenDocumentCommitContext<'a> {
    open_files: &'a DashMap<String, FileParser>,
    template_documents: &'a DashMap<String, TemplateDocument>,
    document_versions: &'a DashMap<String, OpenDocumentState>,
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
        ctx.index
            .update_file_with_references(ctx.uri_str, file_symbols, references);
    } else {
        ctx.index.remove_file(ctx.uri_str);
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
                uri_str: ctx.uri_str,
            },
            &snapshot,
        );
    }
}

impl PhpLspBackend {
    pub(crate) async fn lsp_initialized(&self, _params: InitializedParams) {
        tracing::info!("php-lsp: initialized");
        let indexing_run_state = self.indexing_run.clone();
        let indexing_token = self.start_indexing_run().await;

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
            finish_indexing_run_state(&indexing_run_state, &indexing_token).await;
            return;
        }

        let composer_enabled = *self.composer_enabled.lock().await;
        let configs = discover_workspace_root_configs_blocking(
            roots,
            composer_enabled,
            "workspace discovery",
        )
        .await;
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
        let template_documents = self.template_documents.clone();
        let twig_context_disk_cache = self.twig_context_disk_cache.clone();
        let semantic_tokens_cache = self.semantic_tokens_cache.clone();
        let reindex_document_versions = self.document_versions.clone();
        let diagnostics_publisher = self.diagnostics_publisher.clone();
        let reindex_index = self.index.clone();
        let diagnostics_mode = *self.diagnostics_mode.lock().await;
        let diagnostic_severity = *self.diagnostic_severity.lock().await;
        let diagnostic_budget = *self.diagnostic_budget.lock().await;
        let diagnostics_config = DiagnosticsRuntimeConfig {
            mode: diagnostics_mode,
            severity: diagnostic_severity,
            budget: diagnostic_budget,
            php_version,
        };
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
            stub_extensions.as_deref(),
            client_stubs_path.as_deref(),
        );
        let indexing_options = WorkspaceIndexingOptions {
            include_paths,
            exclude_paths,
            cache_config,
            work_done_progress_supported,
        };
        let vendor_lazy_context = VendorLazyIndexContext {
            index: index.clone(),
            workspace_configs: configs.clone(),
            exclude_paths: indexing_options.exclude_paths.clone(),
            php_version,
            index_vendor,
            vendor_autoload_cache: vendor_autoload_cache.clone(),
            vendor_file_lru: vendor_file_lru.clone(),
        };
        tokio::spawn(async move {
            for config in &configs {
                if finish_indexing_run_if_cancelled(&indexing_run_state, &indexing_token).await {
                    return;
                }
                if let Err(e) = index_workspace(
                    &client,
                    WorkspaceLiveIndexContext {
                        index: &index,
                        open_files: &open_files,
                        template_documents: &template_documents,
                        document_versions: &reindex_document_versions,
                    },
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
                    finish_indexing_run_state(&indexing_run_state, &indexing_token).await;
                    return;
                }
                if finish_indexing_run_if_cancelled(&indexing_run_state, &indexing_token).await {
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
            if finish_indexing_run_if_cancelled(&indexing_run_state, &indexing_token).await {
                return;
            }
            finish_indexing_run_state(&indexing_run_state, &indexing_token).await;

            let workspace_roots: Vec<PathBuf> =
                configs.iter().map(|config| config.root.clone()).collect();
            twig_context_disk_cache.lock().await.clear();
            refresh_open_twig_contexts_for_state(OpenTwigContextRefreshState {
                open_files: &open_files,
                template_documents: &template_documents,
                document_versions: &reindex_document_versions,
                index: &reindex_index,
                workspace_roots: &workspace_roots,
                twig_context_disk_cache: &twig_context_disk_cache,
                semantic_tokens_cache: &semantic_tokens_cache,
            })
            .await;
            if finish_indexing_run_if_cancelled(&indexing_run_state, &indexing_token).await {
                return;
            }
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
                commit_open_document_index_snapshot_if_current(
                    OpenDocumentIndexCommitContext {
                        open_files: &open_files,
                        template_documents: &template_documents,
                        document_versions: &reindex_document_versions,
                        index: &reindex_index,
                        uri_str: &uri_str,
                    },
                    &snapshot,
                );
                if let Ok(uri) = uri_str.parse::<Uri>() {
                    let document_state = snapshot.document_state;
                    let version = document_state.map(|state| state.version);
                    let template_document = snapshot.template_document.clone();
                    if diagnostics_config.mode == DiagnosticsMode::BasicSemantic
                        && template_document.is_none()
                        && index_vendor
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
                        reindex_index.clone(),
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
                        && index_vendor
                    {
                        diags = filter_lazy_resolved_symbol_diagnostics_with_context(
                            &reindex_index,
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
                    });
                }
            }
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

            let removed_files = remove_indexed_files_under_roots(
                &self.index,
                &self.open_files,
                &self.template_documents,
                &self.document_versions,
                &removed_roots,
            );
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
        let added_configs = discover_workspace_root_configs_blocking(
            added_roots,
            composer_enabled,
            "workspace folder discovery",
        )
        .await;

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
        let open_files = self.open_files.clone();
        let template_documents = self.template_documents.clone();
        let document_versions = self.document_versions.clone();
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
            stub_extensions.as_deref(),
            client_stubs_path.as_deref(),
        );
        let indexing_options = WorkspaceIndexingOptions {
            include_paths,
            exclude_paths,
            cache_config,
            work_done_progress_supported,
        };
        let indexing_run_state = self.indexing_run.clone();
        let indexing_token = self.start_indexing_run().await;
        tokio::spawn(async move {
            for config in &added_configs {
                if finish_indexing_run_if_cancelled(&indexing_run_state, &indexing_token).await {
                    return;
                }
                if let Err(e) = index_workspace(
                    &client,
                    WorkspaceLiveIndexContext {
                        index: &index,
                        open_files: &open_files,
                        template_documents: &template_documents,
                        document_versions: &document_versions,
                    },
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
                if finish_indexing_run_if_cancelled(&indexing_run_state, &indexing_token).await {
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
            finish_indexing_run_state(&indexing_run_state, &indexing_token).await;
        });
    }

    pub(crate) async fn lsp_did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        tracing::debug!("didChangeWatchedFiles: {} change(s)", params.changes.len());

        if !params.changes.is_empty() {
            self.invalidate_request_fs_caches().await;
        }

        let roots = self.current_workspace_roots().await;
        let mut config_changed = false;
        let mut composer_metadata_changed: Option<PathBuf> = None;
        let mut composer_requires_workspace_reindex = false;
        for event in params.changes {
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
        if let Some(path) = composer_metadata_changed {
            self.invalidate_composer_metadata(&path, composer_requires_workspace_reindex)
                .await;
        }
    }

    pub(crate) async fn lsp_did_change_configuration(&self, params: DidChangeConfigurationParams) {
        tracing::debug!("didChangeConfiguration");

        self.invalidate_request_fs_caches().await;
        *self.client_settings.lock().await = params.settings.clone();
        self.reload_effective_configuration().await;
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
mod tests {
    use super::*;
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    fn parsed_document(
        uri: &str,
        source: &str,
    ) -> (
        FileParser,
        php_lsp_types::FileSymbols,
        Vec<php_lsp_types::SymbolReference>,
    ) {
        let mut parser = FileParser::new();
        parser.parse_full(source);
        let tree = parser.tree().expect("parsed PHP tree");
        let file_symbols = extract_file_symbols(tree, source, uri);
        let references = collect_symbol_references_in_file(tree, source, &file_symbols);
        (parser, file_symbols, references)
    }

    fn indexed_symbol_names(index: &WorkspaceIndex, uri: &str) -> Vec<String> {
        index
            .file_symbols
            .get(uri)
            .map(|symbols| {
                symbols
                    .symbols
                    .iter()
                    .map(|symbol| symbol.name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn publish_open_php_document(
        open_files: &DashMap<String, FileParser>,
        template_documents: &DashMap<String, TemplateDocument>,
        document_versions: &DashMap<String, OpenDocumentState>,
        index: &WorkspaceIndex,
        uri: &str,
        source: &str,
        state: OpenDocumentState,
    ) {
        publish_open_php_document_with_hook(
            open_files,
            template_documents,
            document_versions,
            index,
            uri,
            source,
            state,
            || {},
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn publish_open_php_document_with_hook<F>(
        open_files: &DashMap<String, FileParser>,
        template_documents: &DashMap<String, TemplateDocument>,
        document_versions: &DashMap<String, OpenDocumentState>,
        index: &WorkspaceIndex,
        uri: &str,
        source: &str,
        state: OpenDocumentState,
        before_parser_publish: F,
    ) where
        F: FnOnce(),
    {
        let (parser, file_symbols, references) = parsed_document(uri, source);
        match open_files.entry(uri.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(mut entry) => {
                template_documents.remove(uri);
                document_versions.insert(uri.to_string(), state);
                index.update_file_with_references(uri, file_symbols, references);
                before_parser_publish();
                entry.insert(parser);
            }
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                template_documents.remove(uri);
                document_versions.insert(uri.to_string(), state);
                index.update_file_with_references(uri, file_symbols, references);
                before_parser_publish();
                entry.insert(parser);
            }
        }
    }

    #[test]
    fn open_php_index_and_parser_commit_has_no_reader_gap() {
        let uri = "file:///open-commit-race.php";
        let old_source = "<?php function oldName(): void {}";
        let new_source = "<?php function newName(): void {}";
        let open_files = Arc::new(DashMap::new());
        let template_documents = Arc::new(DashMap::new());
        let document_versions = Arc::new(DashMap::new());
        let index = Arc::new(WorkspaceIndex::new());

        publish_open_php_document(
            &open_files,
            &template_documents,
            &document_versions,
            &index,
            uri,
            old_source,
            OpenDocumentState {
                version: 1,
                generation: 51,
            },
        );

        let writer_open_files = Arc::clone(&open_files);
        let writer_templates = Arc::clone(&template_documents);
        let writer_versions = Arc::clone(&document_versions);
        let writer_index = Arc::clone(&index);
        let (staged_tx, staged_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            publish_open_php_document_with_hook(
                &writer_open_files,
                &writer_templates,
                &writer_versions,
                &writer_index,
                uri,
                new_source,
                OpenDocumentState {
                    version: 2,
                    generation: 51,
                },
                || {
                    staged_tx.send(()).expect("report staged PHP commit");
                    release_rx.recv().expect("release staged PHP commit");
                },
            );
        });

        staged_rx.recv().expect("PHP index and state are staged");
        let reader_open_files = Arc::clone(&open_files);
        let reader_templates = Arc::clone(&template_documents);
        let reader_versions = Arc::clone(&document_versions);
        let reader_index = Arc::clone(&index);
        let (index_tx, index_rx) = mpsc::channel();
        let (open_lock_tx, open_lock_rx) = mpsc::channel();
        let (snapshot_tx, snapshot_rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let indexed_names = indexed_symbol_names(&reader_index, uri);
            index_tx
                .send(indexed_names.clone())
                .expect("return staged index symbols");
            let snapshot = open_document_snapshot_from_state_with_lock_hook(
                &reader_open_files,
                &reader_templates,
                &reader_versions,
                uri,
                || {
                    open_lock_tx
                        .send(())
                        .expect("report acquired open-document lock");
                },
            )
            .expect("open PHP snapshot");
            let snapshot_names = snapshot
                .file_symbols
                .symbols
                .iter()
                .map(|symbol| symbol.name.clone())
                .collect::<Vec<_>>();
            snapshot_tx
                .send((
                    indexed_names,
                    snapshot.source,
                    snapshot_names,
                    snapshot.document_state,
                ))
                .expect("return PHP snapshot");
        });

        assert_eq!(
            index_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("reader captured staged index"),
            vec!["newName"]
        );
        assert!(
            open_lock_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "source snapshot must wait while the new index is staged"
        );

        release_tx.send(()).expect("release PHP commit");
        writer.join().expect("PHP writer joined");
        open_lock_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("snapshot acquired committed parser");
        let (indexed_names, source, snapshot_names, state) = snapshot_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("snapshot after PHP commit");
        reader.join().expect("PHP reader joined");

        assert_eq!(indexed_names, vec!["newName"]);
        assert_eq!(source, new_source);
        assert_eq!(snapshot_names, vec!["newName"]);
        assert_eq!(
            state,
            Some(OpenDocumentState {
                version: 2,
                generation: 51,
            })
        );
    }

    #[test]
    fn renamed_template_publish_is_atomic_for_snapshot_readers() {
        let uri = "file:///renamed.blade.php";
        let open_files = Arc::new(DashMap::new());
        let template_documents = Arc::new(DashMap::new());
        let document_versions = Arc::new(DashMap::new());
        let reload_tokens = Arc::new(DashMap::new());
        reload_tokens.insert(uri.to_string(), 17);

        let template = preprocess_blade_template("{{ $renamed }}");
        let mut parser = FileParser::new();
        parser.parse_full(template.virtual_source());
        let state = OpenDocumentState {
            version: 4,
            generation: 23,
        };

        let writer_open_files = Arc::clone(&open_files);
        let writer_templates = Arc::clone(&template_documents);
        let writer_versions = Arc::clone(&document_versions);
        let writer_reload_tokens = Arc::clone(&reload_tokens);
        let (staged_tx, staged_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            commit_renamed_open_document_with_hook(
                RenamedOpenDocumentCommitContext {
                    open_files: &writer_open_files,
                    template_documents: &writer_templates,
                    document_versions: &writer_versions,
                    closed_document_reload_tokens: &writer_reload_tokens,
                    uri_str: uri,
                },
                RenamedOpenDocument {
                    parser,
                    template: Some(template),
                    state,
                },
                || {
                    staged_tx.send(()).expect("report staged rename");
                    release_rx.recv().expect("release staged rename");
                },
            )
        });

        staged_rx.recv().expect("rename reached staged state");
        let reader_open_files = Arc::clone(&open_files);
        let reader_templates = Arc::clone(&template_documents);
        let reader_versions = Arc::clone(&document_versions);
        let (reader_started_tx, reader_started_rx) = mpsc::channel();
        let (snapshot_tx, snapshot_rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            reader_started_tx.send(()).expect("reader started");
            let snapshot = open_document_snapshot_from_state(
                &reader_open_files,
                &reader_templates,
                &reader_versions,
                uri,
            )
            .map(|snapshot| {
                (
                    snapshot.source,
                    snapshot
                        .template_document
                        .map(|template| template.original_source().to_string()),
                    snapshot.document_state,
                )
            });
            snapshot_tx.send(snapshot).expect("return snapshot");
        });

        reader_started_rx.recv().expect("reader attempted snapshot");
        assert!(
            snapshot_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "snapshot reader must wait while companion state is staged"
        );
        release_tx.send(()).expect("release rename commit");

        assert!(writer.join().expect("rename writer joined"));
        let (source, original_source, actual_state) = snapshot_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("snapshot after rename commit")
            .expect("open snapshot");
        reader.join().expect("snapshot reader joined");

        assert!(source.contains("$renamed"));
        assert_eq!(original_source.as_deref(), Some("{{ $renamed }}"));
        assert_eq!(actual_state, Some(state));
        assert!(!reload_tokens.contains_key(uri));
    }

    #[test]
    fn stale_open_reindex_snapshot_cannot_overwrite_a_newer_change() {
        let uri = "file:///watched-race.php";
        let open_files = DashMap::new();
        let template_documents = DashMap::new();
        let document_versions = DashMap::new();
        let index = WorkspaceIndex::new();
        let old_state = OpenDocumentState {
            version: 1,
            generation: 31,
        };
        publish_open_php_document(
            &open_files,
            &template_documents,
            &document_versions,
            &index,
            uri,
            "<?php function oldName(): void {}",
            old_state,
        );
        let stale_snapshot = open_document_snapshot_from_state(
            &open_files,
            &template_documents,
            &document_versions,
            uri,
        )
        .expect("old open snapshot");

        publish_open_php_document(
            &open_files,
            &template_documents,
            &document_versions,
            &index,
            uri,
            "<?php function newName(): void {}",
            OpenDocumentState {
                version: 2,
                generation: 32,
            },
        );
        assert!(!commit_open_document_index_snapshot_if_current(
            OpenDocumentIndexCommitContext {
                open_files: &open_files,
                template_documents: &template_documents,
                document_versions: &document_versions,
                index: &index,
                uri_str: uri,
            },
            &stale_snapshot,
        ));

        assert_eq!(indexed_symbol_names(&index, uri), vec!["newName"]);
    }

    #[test]
    fn delayed_workspace_disk_index_never_overwrites_an_unsaved_open_document() {
        let uri = "file:///workspace-race.php";
        let open_files = Arc::new(DashMap::new());
        let template_documents = Arc::new(DashMap::new());
        let document_versions = Arc::new(DashMap::new());
        let index = Arc::new(WorkspaceIndex::new());
        let (_, disk_symbols, disk_references) =
            parsed_document(uri, "<?php function savedName(): void {}");

        let disk_open_files = Arc::clone(&open_files);
        let disk_templates = Arc::clone(&template_documents);
        let disk_versions = Arc::clone(&document_versions);
        let disk_index = Arc::clone(&index);
        let (parsed_tx, parsed_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let disk_writer = std::thread::spawn(move || {
            parsed_tx.send(()).expect("disk parse completed");
            release_rx.recv().expect("release disk indexing");
            commit_workspace_disk_file_preserving_open(
                DiskPhpIndexCommitContext {
                    open_files: &disk_open_files,
                    template_documents: &disk_templates,
                    document_versions: &disk_versions,
                    index: &disk_index,
                    uri_str: uri,
                },
                disk_symbols,
                disk_references,
            );
        });

        parsed_rx.recv().expect("delayed disk parse");
        publish_open_php_document(
            &open_files,
            &template_documents,
            &document_versions,
            &index,
            uri,
            "<?php function unsavedName(): void {}",
            OpenDocumentState {
                version: 7,
                generation: 41,
            },
        );
        release_tx.send(()).expect("release disk writer");
        disk_writer.join().expect("disk writer joined");

        assert_eq!(indexed_symbol_names(&index, uri), vec!["unsavedName"]);
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

pub(in crate::server) async fn collect_php_files_blocking(
    directories: Vec<PathBuf>,
    root: PathBuf,
    exclude_paths: Vec<PathBuf>,
) -> std::result::Result<Vec<PathBuf>, String> {
    let path_label = root.display().to_string();
    run_file_io_blocking("workspace PHP file discovery", path_label, move || {
        collect_php_files(&directories, &root, &exclude_paths)
    })
    .await
}

/// Recursively collect .php files from a directory.
pub(in crate::server) fn collect_php_files_recursive(
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

pub(in crate::server) async fn load_effective_configuration_settings_blocking(
    workspace_roots: Vec<PathBuf>,
    client_settings: serde_json::Value,
) -> (serde_json::Value, Vec<String>) {
    let fallback_client_settings = client_settings.clone();
    let path_label = format!("{} workspace root(s)", workspace_roots.len());
    match run_file_io_blocking("configuration load", path_label, move || {
        load_effective_configuration_settings(&workspace_roots, &client_settings)
    })
    .await
    {
        Ok(result) => result,
        Err(message) => (
            normalize_client_settings(&fallback_client_settings),
            vec![message],
        ),
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

pub(in crate::server) async fn discover_workspace_root_configs_blocking(
    roots: Vec<PathBuf>,
    composer_enabled: bool,
    label: &'static str,
) -> Vec<WorkspaceRootConfig> {
    let fallback_roots = roots.clone();
    let path_label = format!("{} workspace root(s)", roots.len());
    match run_file_io_blocking(label, path_label, move || {
        dedup_workspace_configs(
            roots
                .iter()
                .map(|root| discover_workspace_root_config(root, composer_enabled))
                .collect(),
        )
    })
    .await
    {
        Ok(configs) => configs,
        Err(message) => {
            tracing::warn!("{}", message);
            dedup_workspace_configs(
                fallback_roots
                    .into_iter()
                    .map(|root| WorkspaceRootConfig {
                        root,
                        namespace_map: None,
                    })
                    .collect(),
            )
        }
    }
}

pub(in crate::server) fn dedup_workspace_configs(
    configs: Vec<WorkspaceRootConfig>,
) -> Vec<WorkspaceRootConfig> {
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

pub(in crate::server) fn remove_indexed_files_under_roots(
    index: &WorkspaceIndex,
    open_files: &DashMap<String, FileParser>,
    template_documents: &DashMap<String, TemplateDocument>,
    document_versions: &DashMap<String, OpenDocumentState>,
    roots: &[PathBuf],
) -> usize {
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

    let mut removed = 0;
    for uri in uris {
        let dashmap::mapref::entry::Entry::Vacant(_open_entry) = open_files.entry(uri.clone())
        else {
            continue;
        };
        if template_documents.contains_key(&uri) || document_versions.contains_key(&uri) {
            continue;
        }
        index.remove_file(&uri);
        removed += 1;
    }

    removed
}

pub(in crate::server) fn remove_indexed_file_symbols(
    index: &WorkspaceIndex,
    open_files: &DashMap<String, FileParser>,
    template_documents: &DashMap<String, TemplateDocument>,
    document_versions: &DashMap<String, OpenDocumentState>,
    roots: &[PathBuf],
) -> usize {
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

    let mut removed = 0;
    for uri in uris {
        let dashmap::mapref::entry::Entry::Vacant(_open_entry) = open_files.entry(uri.clone())
        else {
            continue;
        };
        if template_documents.contains_key(&uri) || document_versions.contains_key(&uri) {
            continue;
        }
        index.remove_file(&uri);
        removed += 1;
    }

    removed
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

pub(in crate::server) async fn touch_vendor_file_lru(
    index: &WorkspaceIndex,
    vendor_file_lru: &Arc<Mutex<VendorFileLru>>,
    file_path: &Path,
) {
    let uri = match path_to_uri(file_path) {
        Ok(uri) => uri,
        Err(err) => {
            tracing::debug!("{}", err);
            return;
        }
    };
    let evicted = vendor_file_lru.lock().await.touch(uri);
    for uri in evicted {
        index.remove_file(&uri);
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

pub(in crate::server) async fn preload_vendor_entrypoints(
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
    let entrypoint_files = vendor_autoload_file_paths_from_map_blocking(
        autoload,
        root.to_path_buf(),
        exclude_paths.to_vec(),
    )
    .await;
    if entrypoint_files.is_empty() {
        return 0;
    }

    let cache_config = vendor_index_cache_config(root, php_version, exclude_paths);
    let mut loaded = 0;
    for file_path in entrypoint_files {
        if !file_path.is_file() {
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
            touch_vendor_file_lru(&index, vendor_file_lru, &file_path).await;
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
pub(in crate::server) async fn index_workspace(
    client: &Client,
    live: WorkspaceLiveIndexContext<'_>,
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
    let php_files = collect_php_files_blocking(
        source_dirs,
        root.to_path_buf(),
        options.exclude_paths.clone(),
    )
    .await?;
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
            .map(|symbols| symbols.value().clone())
        else {
            continue;
        };
        commit_workspace_disk_file_preserving_open(
            DiskPhpIndexCommitContext {
                open_files: live.open_files,
                template_documents: live.template_documents,
                document_versions: live.document_versions,
                index: live.index,
                uri_str: &uri_str,
            },
            file_symbols,
            disk_index
                .file_references
                .get(&uri_str)
                .map(|references| references.value().clone())
                .unwrap_or_default(),
        );
    }
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
            disk_index.update_file_with_references(
                &parsed.uri,
                file_symbols.clone(),
                parsed.references.clone(),
            );
            commit_workspace_disk_file_preserving_open(
                DiskPhpIndexCommitContext {
                    open_files: live.open_files,
                    template_documents: live.template_documents,
                    document_versions: live.document_versions,
                    index: live.index,
                    uri_str: &parsed.uri,
                },
                file_symbols,
                parsed.references,
            );
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
        cache::build_cache_from_index(&disk_index, root, &all_files, &options.cache_config);
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

impl PhpLspBackend {
    pub(in crate::server) async fn path_is_excluded_by_config(&self, path: &Path) -> bool {
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
    pub(in crate::server) async fn reindex_php_file(&self, uri: &Uri) {
        let uri_str = uri.as_str().to_string();
        if !uri_is_php_file(uri) {
            return;
        }
        let refresh_twig_contexts = !is_blade_template_uri(&uri_str);
        if is_blade_template_uri(&uri_str) {
            let committed = if let Some(snapshot) = self.open_document_snapshot(&uri_str) {
                commit_open_document_index_snapshot_if_current(
                    OpenDocumentIndexCommitContext {
                        open_files: &self.open_files,
                        template_documents: &self.template_documents,
                        document_versions: &self.document_versions,
                        index: &self.index,
                        uri_str: &uri_str,
                    },
                    &snapshot,
                )
            } else {
                commit_disk_php_index_if_closed(
                    DiskPhpIndexCommitContext {
                        open_files: &self.open_files,
                        template_documents: &self.template_documents,
                        document_versions: &self.document_versions,
                        index: &self.index,
                        uri_str: &uri_str,
                    },
                    None,
                    Vec::new(),
                )
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
                && !self.index.file_symbols.contains_key(&uri_str)
            {
                return;
            }
            if self.path_is_excluded_by_config(&path).await {
                commit_disk_php_index_if_closed(
                    DiskPhpIndexCommitContext {
                        open_files: &self.open_files,
                        template_documents: &self.template_documents,
                        document_versions: &self.document_versions,
                        index: &self.index,
                        uri_str: &uri_str,
                    },
                    None,
                    Vec::new(),
                );
                self.semantic_tokens_cache.lock().await.remove(&uri_str);
                return;
            }
        }

        if let Some(snapshot) = self.open_document_snapshot(&uri_str) {
            commit_open_document_index_snapshot_if_current(
                OpenDocumentIndexCommitContext {
                    open_files: &self.open_files,
                    template_documents: &self.template_documents,
                    document_versions: &self.document_versions,
                    index: &self.index,
                    uri_str: &uri_str,
                },
                &snapshot,
            );
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

        let committed_disk = commit_disk_php_index_if_closed(
            DiskPhpIndexCommitContext {
                open_files: &self.open_files,
                template_documents: &self.template_documents,
                document_versions: &self.document_versions,
                index: &self.index,
                uri_str: &uri_str,
            },
            file_symbols,
            references,
        );
        if !committed_disk {
            if let Some(snapshot) = self.open_document_snapshot(&uri_str) {
                commit_open_document_index_snapshot_if_current(
                    OpenDocumentIndexCommitContext {
                        open_files: &self.open_files,
                        template_documents: &self.template_documents,
                        document_versions: &self.document_versions,
                        index: &self.index,
                        uri_str: &uri_str,
                    },
                    &snapshot,
                );
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
        self.vendor_file_lru.lock().await.remove(&uri_str);
        match self.open_files.entry(uri_str.clone()) {
            dashmap::mapref::entry::Entry::Occupied(entry) => {
                self.document_versions.remove(&uri_str);
                self.template_documents.remove(&uri_str);
                entry.remove();
            }
            dashmap::mapref::entry::Entry::Vacant(_) => {
                self.document_versions.remove(&uri_str);
                self.template_documents.remove(&uri_str);
            }
        }
        self.index.remove_file(&uri_str);
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
        let (moved_parser, moved_template, moved_version) =
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
                    (Some(entry.remove()), template, version)
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
                    (None, template, version)
                }
            };
        self.cancel_debounced_diagnostics(&old_uri_str).await;
        self.cancel_analyzer_run(&old_uri_str).await;
        self.cancel_analyzer_run(new_uri.as_str()).await;
        self.cancel_formatter_run(&old_uri_str).await;
        self.cancel_formatter_run(new_uri.as_str()).await;
        if old_is_php {
            self.index.remove_file(&old_uri_str);
            self.vendor_file_lru.lock().await.remove(&old_uri_str);
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
                closed_document_reload_tokens: &self.closed_document_reload_tokens,
                uri_str: &new_uri_str,
            },
            RenamedOpenDocument {
                parser,
                template,
                state,
            },
            || {
                if let Some((file_symbols, references)) = indexed_file {
                    self.index
                        .update_file_with_references(&new_uri_str, file_symbols, references);
                } else {
                    self.index.remove_file(&new_uri_str);
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

        self.semantic_tokens_cache.lock().await.remove(&new_uri_str);
        if !new_excluded {
            self.publish_diagnostics(new_uri).await;
        }
    }
}
