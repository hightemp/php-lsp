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
    let documents_requiring_full_sync = Arc::new(DashMap::new());
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
    let writer_full_sync = Arc::clone(&documents_requiring_full_sync);
    let writer_reload_tokens = Arc::clone(&reload_tokens);
    let (staged_tx, staged_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let writer = std::thread::spawn(move || {
        commit_renamed_open_document_with_hook(
            RenamedOpenDocumentCommitContext {
                open_files: &writer_open_files,
                template_documents: &writer_templates,
                document_versions: &writer_versions,
                documents_requiring_full_sync: &writer_full_sync,
                closed_document_reload_tokens: &writer_reload_tokens,
                uri_str: uri,
            },
            RenamedOpenDocument {
                parser,
                template: Some(template),
                state,
                requires_full_sync: true,
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
    assert_eq!(
        documents_requiring_full_sync
            .get(uri)
            .map(|generation| *generation),
        Some(state.generation)
    );
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
            root_index: None,
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
                root_index: None,
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

#[test]
fn project_traversal_limits_can_only_reduce_the_trusted_baseline() {
    let mut settings = serde_json::json!({
        "indexingMaxFiles": 0,
        "indexingMaxEntries": 2_000_000
    });
    let trusted = serde_json::json!({
        "indexingMaxFiles": 250_000,
        "indexingMaxEntries": 500_000
    });
    let messages = clamp_project_traversal_limits(
        &mut settings,
        &trusted,
        Path::new("/workspace/.php-lsp.toml"),
    );

    assert_eq!(settings["indexingMaxFiles"], 250_000);
    assert_eq!(settings["indexingMaxEntries"], 500_000);
    assert_eq!(messages.len(), 2);

    let mut reductions = serde_json::json!({
        "indexingMaxFiles": 50_000,
        "indexingMaxEntries": 100_000
    });
    assert!(clamp_project_traversal_limits(
        &mut reductions,
        &trusted,
        Path::new("/workspace/.php-lsp.toml"),
    )
    .is_empty());
    assert_eq!(reductions["indexingMaxFiles"], 50_000);
    assert_eq!(reductions["indexingMaxEntries"], 100_000);
}

#[cfg(unix)]
#[test]
fn workspace_collection_follows_external_symlinks_without_cycles() {
    use std::os::unix::fs::symlink;

    let root =
        std::env::temp_dir().join(format!("php-lsp-workspace-symlink-{}", std::process::id()));
    let external = std::env::temp_dir().join(format!(
        "php-lsp-workspace-symlink-external-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&external);
    std::fs::create_dir_all(root.join("nested")).expect("create workspace tree");
    std::fs::create_dir_all(&external).expect("create external tree");
    std::fs::write(external.join("External.php"), "<?php class External {}")
        .expect("write external PHP file");
    symlink(&external, root.join("linked")).expect("link external tree");
    symlink(&root, root.join("nested/back")).expect("link workspace cycle");

    let files = collect_php_files(std::slice::from_ref(&root), &root, &[]);
    assert_eq!(files, vec![root.join("linked/External.php")]);

    std::fs::remove_dir_all(root).expect("remove workspace tree");
    std::fs::remove_dir_all(external).expect("remove external tree");
}

#[test]
fn explicit_composer_file_is_kept_even_without_php_extension() {
    let root = std::env::temp_dir().join(format!(
        "php-lsp-composer-explicit-file-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create workspace");
    let bootstrap = root.join("bootstrap.inc");
    std::fs::write(&bootstrap, "<?php function bootstrap_helper(): void {}")
        .expect("write Composer file");

    let outcome = collect_php_files_with_explicit_control(
        &[],
        std::slice::from_ref(&bootstrap),
        &root,
        &[],
        TraversalLimits::default(),
        || None,
    );
    assert_eq!(outcome.files, vec![bootstrap]);

    std::fs::remove_dir_all(root).expect("remove workspace");
}

#[cfg(unix)]
#[tokio::test]
async fn feature_alias_discovery_covers_project_config_and_vendor_composer_links() {
    use std::os::unix::fs::symlink;

    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "php-lsp-feature-alias-{}-{nonce}",
        std::process::id()
    ));
    let external = std::env::temp_dir().join(format!(
        "php-lsp-feature-alias-external-{}-{nonce}",
        std::process::id()
    ));
    let effective_root = root.join("app");
    std::fs::create_dir_all(effective_root.join("vendor")).expect("create workspace vendor");
    std::fs::create_dir_all(external.join("composer")).expect("create external composer");
    std::fs::write(external.join("project.toml"), "[indexing]\nmaxFiles = 10\n")
        .expect("write external config");
    std::fs::write(external.join("composer/installed.json"), "[]")
        .expect("write installed metadata");
    symlink(
        external.join("project.toml"),
        root.join(PROJECT_CONFIG_FILE_NAME),
    )
    .expect("link project config");
    symlink(
        external.join("composer"),
        effective_root.join("vendor/composer"),
    )
    .expect("link Composer metadata directory");

    let outcome = collect_feature_symlink_aliases_blocking(
        effective_root.clone(),
        root.clone(),
        Vec::new(),
        TraversalLimits {
            max_files: Some(100),
            max_entries: Some(1_000),
        },
        OperationCancellationToken::new(),
    )
    .await
    .expect("feature alias discovery");
    assert!(outcome
        .symlink_aliases
        .iter()
        .any(|alias| alias.logical_path == root.join(PROJECT_CONFIG_FILE_NAME)));
    assert!(outcome
        .symlink_aliases
        .iter()
        .any(|alias| alias.logical_path == effective_root.join("vendor/composer")));

    std::fs::remove_dir_all(root).expect("remove workspace");
    std::fs::remove_dir_all(external).expect("remove external files");
}

#[test]
fn client_only_runtime_fallback_preserves_resource_scoped_settings() {
    let root_a = PathBuf::from("/workspace/root-a");
    let root_b = PathBuf::from("/workspace/root-b");
    let payload = serde_json::json!({
        "configurationVersion": 2,
        "global": {
            "phpVersion": "8.1",
            "diagnosticsMode": "basic-semantic"
        },
        "workspaceFolders": [
            {
                "uri": php_lsp_types::uri::path_to_uri(&root_a).unwrap(),
                "settings": {
                    "phpVersion": "7.4",
                    "diagnosticsMode": "off"
                }
            },
            {
                "uri": php_lsp_types::uri::path_to_uri(&root_b).unwrap(),
                "settings": {
                    "phpVersion": "8.3",
                    "indexVendor": false
                }
            }
        ]
    });

    let loaded = load_client_only_workspace_runtime(
        &[root_a.clone(), root_b.clone()],
        &payload,
        vec!["configuration load timed out".to_string()],
    );
    let config_a = loaded
        .configs
        .iter()
        .find(|config| config.workspace_folder == root_a)
        .expect("root A fallback config");
    let config_b = loaded
        .configs
        .iter()
        .find(|config| config.workspace_folder == root_b)
        .expect("root B fallback config");

    assert_eq!(
        loaded.fallback.php_version,
        PhpVersion::parse("8.1").unwrap()
    );
    assert_eq!(
        config_a.runtime_config.php_version,
        PhpVersion::parse("7.4").unwrap()
    );
    assert_eq!(
        config_a.runtime_config.diagnostics_mode,
        DiagnosticsMode::Off
    );
    assert_eq!(
        config_b.runtime_config.php_version,
        PhpVersion::parse("8.3").unwrap()
    );
    assert!(!config_b.runtime_config.index_vendor);
    assert_eq!(loaded.messages, vec!["configuration load timed out"]);
}
