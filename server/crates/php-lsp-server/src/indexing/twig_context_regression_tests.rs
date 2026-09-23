//! Regression coverage for context lifecycle and cancellation boundaries.
use super::*;

#[tokio::test(flavor = "current_thread")]
async fn refresh_budget_covers_waiting_for_semantic_cache_before_commit() {
    use crate::server::lsp::templates::{
        refresh_open_twig_contexts_for_state, OpenTwigContextRefreshState,
    };
    let fixture = Fixture::new();
    fixture.controller(0, "Item");
    let coordinator = fixture.cache.lock().await.coordinator.clone();
    coordinator.hooks.budget_ms.store(100, Ordering::SeqCst);
    let runtime = WorkspaceRuntimeState {
        configs: vec![root_config(&fixture)],
        generation: 1,
        ..Default::default()
    };
    coordinator.configure(&runtime);
    let versions = Arc::new(DashMap::new());
    let uri = open_refresh_target(&fixture, &versions);
    fixture.context().await;
    let old = fixture.templates.get(&uri).unwrap().value().clone();
    let semantic_tokens = Arc::new(Mutex::new(SemanticTokensCache::default()));
    let guard = semantic_tokens.lock().await;
    let roots = vec![fixture.root.clone()];
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        refresh_open_twig_contexts_for_state(OpenTwigContextRefreshState {
            open_files: &fixture.files,
            template_documents: &fixture.templates,
            document_versions: &versions,
            index: &fixture.index,
            fallback_index: &runtime.fallback_index,
            workspace_roots: &roots,
            workspace_configs: &runtime.configs,
            workspace_folders_filter: None,
            runtime_generation: runtime.generation,
            indexing_runs: &[],
            twig_context_disk_cache: &fixture.cache,
            semantic_tokens_cache: &semantic_tokens,
        }),
    )
    .await;
    drop(guard);
    assert!(result
        .expect("semantic lock wait exceeded refresh budget")
        .is_empty());
    assert!(fixture
        .templates
        .get(&uri)
        .unwrap()
        .has_same_source_and_twig_context(&old));
}

#[tokio::test(flavor = "current_thread")]
async fn superseded_queued_job_finishes_without_waiting_for_the_worker_permit() {
    let fixture = Fixture::new();
    fixture.controller(0, "Item");
    let coordinator = fixture.cache.lock().await.coordinator.clone();
    let permit = coordinator.permits.clone().acquire_owned().await.unwrap();
    let mut old = Box::pin(fixture.context_with(TraversalLimits::default()));
    assert!(futures::poll!(old.as_mut()).is_pending());
    // The spawned job has not run/subscribed to cancellation yet.
    fixture.invalidate("src/Controller0.php").await;
    let latest = fixture.spawn();
    assert!(tokio::time::timeout(Duration::from_secs(1), old)
        .await
        .unwrap()
        .is_none());
    assert_eq!(coordinator.hooks.starts.load(Ordering::SeqCst), 0);
    drop(permit);
    assert!(latest.await.unwrap().is_some());
    assert_eq!(coordinator.hooks.starts.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn expired_view_cannot_publish_but_does_not_expire_cached_facts() {
    let fixture = Fixture::new();
    fixture.controller(0, "Item");
    let mut old = fixture.context().await;
    let work = source_work(&fixture.root);
    old.limit_deadline(Instant::now());
    assert!(old.commit_if_current(|| true).is_none());
    assert_eq!(
        fixture.context().await.commit_if_current(|| true),
        Some(true)
    );
    assert_eq!(source_work(&fixture.root), work);
}

fn root_config(fixture: &Fixture) -> WorkspaceRootConfig {
    WorkspaceRootConfig {
        workspace_folder: fixture.root.clone(),
        root: fixture.root.clone(),
        namespace_map: None,
        runtime_config: ResolvedRuntimeConfiguration {
            traversal_limits: TraversalLimits::default(),
            exclude_paths: Vec::new(),
            ..Default::default()
        },
        index: fixture.index.clone(),
        vendor_file_lru: Arc::new(Mutex::new(VendorFileLru::default())),
    }
}

fn open_refresh_target(fixture: &Fixture, versions: &DashMap<String, OpenDocumentState>) -> String {
    let uri = fixture.uri("templates/caller0.twig");
    let template = preprocess_twig_template("{{ item }}", &[]);
    let mut parser = FileParser::new();
    parser.parse_full(template.virtual_source());
    fixture.files.insert(uri.clone(), parser);
    fixture.templates.insert(uri.clone(), template);
    versions.insert(
        uri.clone(),
        OpenDocumentState {
            version: 1,
            generation: 1,
        },
    );
    uri
}

async fn refresh_fixture(
    fixture: &Fixture,
    runtime: &WorkspaceRuntimeState,
    versions: &Arc<DashMap<String, OpenDocumentState>>,
) -> Vec<String> {
    use crate::server::lsp::templates::{
        refresh_open_twig_contexts_for_state, OpenTwigContextRefreshState,
    };
    let roots = runtime
        .configs
        .iter()
        .map(|config| config.root.clone())
        .collect::<Vec<_>>();
    refresh_open_twig_contexts_for_state(OpenTwigContextRefreshState {
        open_files: &fixture.files,
        template_documents: &fixture.templates,
        document_versions: versions,
        index: &fixture.index,
        fallback_index: &runtime.fallback_index,
        workspace_roots: &roots,
        workspace_configs: &runtime.configs,
        workspace_folders_filter: None,
        runtime_generation: runtime.generation,
        indexing_runs: &[],
        twig_context_disk_cache: &fixture.cache,
        semantic_tokens_cache: &Arc::new(Mutex::new(SemanticTokensCache::default())),
    })
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn refresh_deadline_is_shared_across_roots() {
    let a = Fixture::new();
    let mut b = Fixture::new();
    b.cache = a.cache.clone();
    b.files = a.files.clone();
    b.templates = a.templates.clone();
    let coordinator = a.cache.lock().await.coordinator.clone();
    coordinator.hooks.budget_ms.store(100, Ordering::SeqCst);
    let runtime = WorkspaceRuntimeState {
        configs: vec![root_config(&a), root_config(&b)],
        generation: 1,
        ..Default::default()
    };
    coordinator.configure(&runtime);
    let versions = Arc::new(DashMap::new());
    open_refresh_target(&a, &versions);
    open_refresh_target(&b, &versions);
    let permit = coordinator.permits.clone().acquire_owned().await.unwrap();
    assert!(refresh_fixture(&a, &runtime, &versions).await.is_empty());
    assert_eq!(
        coordinator.state.lock().unwrap().next_job,
        1,
        "another root must not restart the elapsed pass budget"
    );
    drop(permit);
}

#[tokio::test(flavor = "current_thread")]
async fn short_refresh_waiter_does_not_cancel_shared_long_request() {
    let fixture = Fixture::new();
    fixture.controller(0, "Item");
    let coordinator = fixture.cache.lock().await.coordinator.clone();
    let runtime = WorkspaceRuntimeState {
        configs: vec![root_config(&fixture)],
        generation: 1,
        ..Default::default()
    };
    coordinator.configure(&runtime);
    let versions = Arc::new(DashMap::new());
    open_refresh_target(&fixture, &versions);
    let (entered, release, hooks) = pause_next(&fixture).await;
    let long = fixture.spawn();
    reached(entered).await;
    hooks.budget_ms.store(100, Ordering::SeqCst);
    assert!(refresh_fixture(&fixture, &runtime, &versions)
        .await
        .is_empty());
    let job = coordinator
        .state
        .lock()
        .unwrap()
        .roots
        .values()
        .next()
        .unwrap()
        .job
        .as_ref()
        .unwrap()
        .clone();
    assert_eq!(job.waiters.load(Ordering::SeqCst), 1);
    assert!(!job.stopped());
    release.send(()).unwrap();
    assert!(long.await.unwrap().is_some());
    assert_eq!(hooks.starts.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn failed_root_preserves_context_and_still_refreshes_another_warm_root() {
    let a = Fixture::new();
    let mut b = Fixture::new();
    assert!(a.root < b.root);
    b.cache = a.cache.clone();
    b.files = a.files.clone();
    b.templates = a.templates.clone();
    a.controller(0, "First");
    b.controller(0, "Second");
    let coordinator = a.cache.lock().await.coordinator.clone();
    let runtime = WorkspaceRuntimeState {
        configs: vec![root_config(&a), root_config(&b)],
        generation: 1,
        ..Default::default()
    };
    coordinator.configure(&runtime);
    let versions = Arc::new(DashMap::new());
    let a_uri = open_refresh_target(&a, &versions);
    let b_uri = open_refresh_target(&b, &versions);
    a.context().await;
    b.context().await;
    let unchanged = a.templates.get(&a_uri).unwrap().value().clone();
    a.invalidate("src/Controller0.php").await;
    let (entered, release, _) = pause_next(&a).await;
    let pass = refresh_fixture(&a, &runtime, &versions);
    let cancel = async {
        reached(entered).await;
        a.invalidate("src/Controller0.php").await;
        release.send(()).unwrap();
    };
    let (refreshed, ()) = tokio::join!(pass, cancel);
    assert_eq!(refreshed, vec![b_uri]);
    assert!(a
        .templates
        .get(&a_uri)
        .unwrap()
        .has_same_source_and_twig_context(&unchanged));
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn form_source_alias_uses_open_buffer_and_its_definition_ranges() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let disk = "<?php class FormType extends \\Symfony\\Component\\Form\\AbstractType { public function buildForm($builder) { $builder->add('diskField'); } }";
    let opened = "<?php\n\nclass FormType extends \\Symfony\\Component\\Form\\AbstractType {\npublic function buildForm($builder) {\n$builder->add('openField');\n}\n}";
    fixture.write("external/Form.php", disk);
    std::fs::create_dir_all(fixture.root.join("forms")).unwrap();
    for name in ["A.php", "Я.php"] {
        symlink(
            fixture.root.join("external/Form.php"),
            fixture.root.join("forms").join(name),
        )
        .unwrap();
    }
    fixture.index_php("forms/A.php", disk, false);
    fixture.write("src/Controller.php", "<?php $form = $this->createForm(FormType::class); $this->render('page.twig', ['form' => $form->createView()]);");
    assert!(variable(&fixture.context().await, "page.twig", "form")
        .unwrap()
        .contains("diskField"));
    fixture.index_php("forms/Я.php", opened, true);
    // Keep the indexed symbol at A so dependency resolution must find open alias Я.
    fixture.index_php("forms/A.php", disk, false);
    fixture.invalidate("forms/Я.php").await;
    let context = fixture.context().await;
    let variables = context.variables("page.twig");
    let form = variables
        .iter()
        .find(|variable| variable.name == "form")
        .unwrap();
    assert!(form.type_text.contains("openField") && !form.type_text.contains("diskField"));
    let definitions = &form.shape_definitions;
    assert!(!definitions.is_empty());
    assert!(
        definitions
            .iter()
            .all(|definition| definition.uri == fixture.uri("forms/Я.php")),
        "{definitions:?}"
    );
    assert!(
        definitions.iter().any(|definition| definition.range.0 == 4),
        "{definitions:?}"
    );
    let exact = opened.replace("openField", "exactField");
    fixture.index_php("forms/A.php", &exact, true);
    fixture.invalidate("forms/A.php").await;
    let context = fixture.context().await;
    assert!(variable(&context, "page.twig", "form")
        .unwrap()
        .contains("exactField"));
    fixture.files.remove(&fixture.uri("forms/A.php"));
    fixture.files.remove(&fixture.uri("forms/Я.php"));
    fixture.invalidate("forms/A.php").await;
    fixture.invalidate("forms/Я.php").await;
    assert!(variable(&fixture.context().await, "page.twig", "form")
        .unwrap()
        .contains("diskField"));
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn two_open_aliases_keep_distinct_buffers_and_suppress_disk() {
    let fixture = Fixture::new();
    fixture.write(
        "external/Controller.php",
        "<?php $this->render('page.twig', ['disk' => 1]);",
    );
    for name in ["A", "Я"] {
        std::os::unix::fs::symlink(
            fixture.root.join("external/Controller.php"),
            fixture.root.join(format!("src/{name}.php")),
        )
        .unwrap();
    }
    fixture.context().await;
    for (name, field) in [("Я", "last"), ("A", "first")] {
        fixture.index_php(
            &format!("src/{name}.php"),
            &format!("<?php $this->render('page.twig', ['{field}' => 1]);"),
            true,
        );
        fixture.invalidate(&format!("src/{name}.php")).await;
    }
    let values = fixture.context().await.variables("page.twig");
    assert_eq!(
        values
            .iter()
            .map(|value| value.name.as_str())
            .collect::<Vec<_>>(),
        ["first", "last"]
    );
    fixture.files.remove(&fixture.uri("src/A.php"));
    fixture.invalidate("src/A.php").await;
    assert_eq!(
        fixture
            .context()
            .await
            .variables("page.twig")
            .iter()
            .map(|value| value.name.as_str())
            .collect::<Vec<_>>(),
        ["last"]
    );
    fixture.files.remove(&fixture.uri("src/Я.php"));
    fixture.invalidate("src/Я.php").await;
    assert_eq!(
        fixture
            .context()
            .await
            .variables("page.twig")
            .iter()
            .map(|value| value.name.as_str())
            .collect::<Vec<_>>(),
        ["disk"]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn rename_preserves_original_source_across_php_twig_and_blade() {
    for (from, to) in [
        ("php", "twig"),
        ("twig", "php"),
        ("twig", "blade.php"),
        ("blade.php", "twig"),
    ] {
        let (service, _socket) = tower_lsp::LspService::new(PhpLspBackend::new);
        let backend = service.inner();
        let a = Fixture::new();
        let b = Fixture::new();
        let old = a.uri(&format!("templates/old.{from}"));
        let new = b.uri(&format!("templates/new.{to}"));
        let source = "<?php class Original {} ?> {{ original }}";
        let mut parser = FileParser::new();
        if let Some(kind) = template_kind_for_document(&old, "") {
            parser = backend.open_template_document(&old, source, kind, &[]);
        } else {
            parser.parse_full(source);
        }
        backend.open_files.insert(old.clone(), parser);
        let state = OpenDocumentState {
            version: 3,
            generation: 7,
        };
        backend.document_versions.insert(old.clone(), state);
        backend
            .lsp_did_rename_files(RenameFilesParams {
                files: vec![FileRename {
                    old_uri: old.clone(),
                    new_uri: new.clone(),
                }],
            })
            .await;
        assert!(!backend.open_files.contains_key(&old));
        assert!(!backend.template_documents.contains_key(&old));
        assert_eq!(backend.current_document_state(&new), Some(state));
        if let Some(kind) = template_kind_for_document(&new, "") {
            let template = backend.template_document(&new).unwrap();
            assert_eq!(template.kind(), kind);
            assert_eq!(template.original_source(), source);
            assert_eq!(
                backend.open_files.get(&new).unwrap().source(),
                template.virtual_source()
            );
        } else {
            assert!(!backend.template_documents.contains_key(&new));
            assert_eq!(backend.open_files.get(&new).unwrap().source(), source);
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn twig_rename_does_not_overwrite_a_reopened_destination() {
    let (service, _socket) = tower_lsp::LspService::new(PhpLspBackend::new);
    let backend = service.inner();
    let fixture = Fixture::new();
    let old = fixture.uri("templates/old.twig");
    let new = fixture.uri("templates/new.twig");
    for (uri, source, generation) in [(&old, "old source", 1), (&new, "new source", 2)] {
        backend.open_files.insert(
            uri.clone(),
            backend.open_template_document(uri, source, TemplateKind::Twig, &[]),
        );
        backend.document_versions.insert(
            uri.clone(),
            OpenDocumentState {
                version: 1,
                generation,
            },
        );
    }
    backend
        .lsp_did_rename_files(RenameFilesParams {
            files: vec![FileRename {
                old_uri: old.clone(),
                new_uri: new.clone(),
            }],
        })
        .await;
    assert_eq!(
        backend.template_document(&new).unwrap().original_source(),
        "new source"
    );
    assert_eq!(backend.current_document_state(&new).unwrap().generation, 2);
    assert!(!backend.open_files.contains_key(&old));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn joining_during_last_waiter_release_starts_live_work() {
    let fixture = Fixture::new();
    fixture.controller(0, "Item");
    let coordinator = fixture.cache.lock().await.coordinator.clone();
    let (entered, release_worker, hooks) = pause_next(&fixture).await;
    let first = fixture.spawn();
    reached(entered).await;
    let old_job = coordinator
        .state
        .lock()
        .unwrap()
        .roots
        .values()
        .next()
        .unwrap()
        .job
        .as_ref()
        .unwrap()
        .clone();
    let (dropped_tx, dropped_rx) = std::sync::mpsc::channel();
    let (release_drop, wait_drop) = std::sync::mpsc::channel();
    *old_job.last_waiter_pause.lock().unwrap() = Some((dropped_tx, wait_drop));
    first.abort();
    reached(dropped_rx).await;
    let second = fixture.spawn();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let joined_or_replaced = coordinator
                .state
                .lock()
                .unwrap()
                .roots
                .values()
                .next()
                .unwrap()
                .job
                .as_ref()
                .is_some_and(|job| {
                    job.id != old_job.id || old_job.waiters.load(Ordering::SeqCst) > 0
                });
            if joined_or_replaced {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    release_drop.send(()).unwrap();
    let _ = first.await;
    release_worker.send(()).unwrap();
    let context = second
        .await
        .unwrap()
        .expect("new waiter must not join abandoned work");
    assert_eq!(
        variable(&context, "caller0.twig", "item").as_deref(),
        Some("Item")
    );
    assert_eq!(hooks.peak.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn save_preserves_unrelated_source_cache() {
    let (service, _socket) = tower_lsp::LspService::new(PhpLspBackend::new);
    let backend = service.inner();
    let mut fixture = Fixture::new();
    fixture.cache = backend.twig_context_disk_cache.clone();
    for number in 0..4 {
        fixture.controller(number, "Item");
        fixture.caller(number);
    }
    fixture.context().await;
    let before = source_work(&fixture.root);
    backend
        .lsp_did_save(DidSaveTextDocumentParams {
            text_document: TextDocumentIdentifier {
                uri: fixture.uri("src/Controller0.php").parse().unwrap(),
            },
            text: None,
        })
        .await;
    fixture.context().await;
    let after = source_work(&fixture.root);
    assert!(
        after.0 - before.0 <= 1,
        "saving one source reread {} files (before {before:?}, after {after:?})",
        after.0 - before.0
    );
}

#[tokio::test(flavor = "current_thread")]
async fn rename_moves_unsaved_twig_caller() {
    let (service, _socket) = tower_lsp::LspService::new(PhpLspBackend::new);
    let backend = service.inner();
    let fixture = Fixture::new();
    fixture.caller(0);
    let old = fixture.uri("templates/caller0.twig");
    let new = fixture.uri("templates/renamed.twig");
    let parser =
        backend.open_template_document(&old, "unsaved: no include", TemplateKind::Twig, &[]);
    backend.open_files.insert(old.clone(), parser);
    backend.document_versions.insert(
        old.clone(),
        OpenDocumentState {
            version: 2,
            generation: 1,
        },
    );
    std::fs::rename(
        fixture.root.join("templates/caller0.twig"),
        fixture.root.join("templates/renamed.twig"),
    )
    .unwrap();
    backend
        .lsp_did_rename_files(RenameFilesParams {
            files: vec![FileRename {
                old_uri: old.clone(),
                new_uri: new.clone(),
            }],
        })
        .await;
    assert!(
        !backend.template_documents.contains_key(&old),
        "renamed Twig caller remains bound to old URI"
    );
    assert_eq!(
        backend
            .template_documents
            .get(&new)
            .map(|d| d.original_source().to_string())
            .as_deref(),
        Some("unsaved: no include")
    );
}

async fn assert_cancelled_publication(mode: &str) {
    let fixture = Fixture::new();
    fixture.controller(0, "Item");
    let coordinator = fixture.cache.lock().await.coordinator.clone();
    let (rx, release_worker, hooks) = pause_next(&fixture).await;
    if mode == "deadline" {
        hooks.budget_ms.store(200, Ordering::SeqCst);
    }
    let request = fixture.spawn();
    reached(rx).await;
    let job = coordinator
        .state
        .lock()
        .unwrap()
        .roots
        .values()
        .next()
        .unwrap()
        .job
        .as_ref()
        .unwrap()
        .clone();
    let index = fixture.index.clone();
    let revision = index.revision_snapshot();
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocker = tokio::task::spawn_blocking(move || {
        index.with_revision(revision, || {
            held_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
        });
    });
    reached(held_rx).await;
    release_worker.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if hooks.active.load(Ordering::SeqCst) == 0 && coordinator.state.try_lock().is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("publication waits at index barrier while holding root state");
    let shutdown = if mode == "shutdown" {
        let coordinator = coordinator.clone();
        Some(tokio::task::spawn_blocking(move || {
            TwigContextDiskCache { coordinator }.shutdown()
        }))
    } else {
        None
    };
    if mode == "shutdown" {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !coordinator.stopped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    } else {
        if mode == "cancel" {
            request.abort();
        }
        let _ = request.await;
        assert_eq!(job.waiters.load(Ordering::SeqCst), 0);
    }
    release_tx.send(()).unwrap();
    if let Some(shutdown) = shutdown {
        shutdown.await.unwrap();
    }
    blocker.await.unwrap();
    let mut result = job.result.subscribe();
    let outcome = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let value = result.borrow().clone();
            if let JobResult::Done(snapshot) = value {
                break snapshot;
            }
            result.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(
        outcome.is_none(),
        "cancelled job published a ready snapshot"
    );
    assert!(coordinator
        .state
        .lock()
        .unwrap()
        .roots
        .values()
        .all(|slot| slot.snapshot.is_none()));
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn open_alias_replaces_disk_render() {
    let fixture = Fixture::new();
    fixture.write(
        "external/Controller.php",
        "<?php $this->render('page.twig', ['item' => new Old()]);",
    );
    for alias in ["A", "B"] {
        std::os::unix::fs::symlink(
            fixture.root.join("external/Controller.php"),
            fixture.root.join(format!("src/{alias}.php")),
        )
        .unwrap();
    }
    fixture.context().await;
    fixture.index_php("src/B.php", "<?php // removed last render", true);
    fixture.invalidate("src/B.php").await;
    assert!(
        fixture.context().await.variables("page.twig").is_empty(),
        "open physical alias retains stale disk render"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn one_refresh_must_not_restart_timeout_for_each_template() {
    use crate::server::lsp::templates::{
        refresh_open_twig_contexts_for_state, OpenTwigContextRefreshState,
    };
    let fixture = Fixture::new();
    let coordinator = fixture.cache.lock().await.coordinator.clone();
    coordinator.hooks.budget_ms.store(100, Ordering::SeqCst);
    let config = WorkspaceRootConfig {
        workspace_folder: fixture.root.clone(),
        root: fixture.root.clone(),
        namespace_map: None,
        runtime_config: ResolvedRuntimeConfiguration {
            traversal_limits: TraversalLimits::default(),
            exclude_paths: Vec::new(),
            ..Default::default()
        },
        index: fixture.index.clone(),
        vendor_file_lru: Arc::new(Mutex::new(VendorFileLru::default())),
    };
    let runtime = WorkspaceRuntimeState {
        configs: vec![config],
        generation: 1,
        ..Default::default()
    };
    coordinator.configure(&runtime);
    let versions = Arc::new(DashMap::new());
    for (number, relative) in ["templates/a.twig", "templates/b.twig"].iter().enumerate() {
        let uri = fixture.uri(relative);
        let template = preprocess_twig_template("{{ item }}", &[]);
        let mut parser = FileParser::new();
        parser.parse_full(template.virtual_source());
        fixture.files.insert(uri.clone(), parser);
        fixture.templates.insert(uri.clone(), template);
        versions.insert(
            uri,
            OpenDocumentState {
                version: 1,
                generation: number as u64 + 1,
            },
        );
    }
    // Model an earlier blocking worker still owning the single permit after
    // its requester timed out; neither of this pass's jobs can start.
    let held_permit = coordinator.permits.clone().acquire_owned().await.unwrap();
    let semantic_tokens = Arc::new(Mutex::new(SemanticTokensCache::default()));
    let aggregate = Arc::new(WorkspaceIndex::new());
    let roots = vec![fixture.root.clone()];
    let started = Instant::now();
    let refreshed = refresh_open_twig_contexts_for_state(OpenTwigContextRefreshState {
        open_files: &fixture.files,
        template_documents: &fixture.templates,
        document_versions: &versions,
        index: &aggregate,
        fallback_index: &runtime.fallback_index,
        workspace_roots: &roots,
        workspace_configs: &runtime.configs,
        workspace_folders_filter: None,
        runtime_generation: runtime.generation,
        indexing_runs: &[],
        twig_context_disk_cache: &fixture.cache,
        semantic_tokens_cache: &semantic_tokens,
    })
    .await;
    let elapsed = started.elapsed();
    drop(held_permit);
    assert!(refreshed.is_empty());
    let attempts = coordinator.state.lock().unwrap().next_job;
    assert_eq!(
        attempts, 1,
        "one refresh pass retried the same failed root {attempts} times in {elapsed:?}; \
         each additional open Twig consumes another full timeout"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_waiter_does_not_publish_after_index_barrier() {
    assert_cancelled_publication("cancel").await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_deadline_does_not_publish_after_index_barrier() {
    assert_cancelled_publication("deadline").await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_does_not_publish_after_index_barrier() {
    assert_cancelled_publication("shutdown").await;
}

async fn assert_view_commit_after_barrier(deadline_only: bool) {
    let fixture = Fixture::new();
    fixture.controller(0, "Item");
    let mut view = fixture.context().await;
    let deadline = Instant::now() + Duration::from_millis(100);
    if deadline_only {
        view.limit_deadline(deadline);
    }
    let coordinator = fixture.cache.lock().await.coordinator.clone();
    let index = fixture.index.clone();
    let revision = index.revision_snapshot();
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocker = tokio::task::spawn_blocking(move || {
        index.with_revision(revision, || {
            held_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
        });
    });
    reached(held_rx).await;
    let commit = tokio::task::spawn_blocking(move || view.commit_if_current(|| true));
    tokio::time::timeout(Duration::from_secs(2), async {
        while coordinator.state.try_lock().is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let shutdown = if deadline_only {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
        None
    } else {
        let shutdown_coordinator = coordinator.clone();
        let shutdown = tokio::task::spawn_blocking(move || {
            TwigContextDiskCache {
                coordinator: shutdown_coordinator,
            }
            .shutdown()
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !coordinator.stopped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        Some(shutdown)
    };
    release_tx.send(()).unwrap();
    blocker.await.unwrap();
    if let Some(shutdown) = shutdown {
        shutdown.await.unwrap();
    }
    assert!(
        commit.await.unwrap().is_none(),
        "view committed after shutdown/deadline while waiting for index barrier"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_rejects_view_commit_waiting_for_index_barrier() {
    assert_view_commit_after_barrier(false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deadline_rejects_view_commit_waiting_for_index_barrier() {
    assert_view_commit_after_barrier(true).await;
}

#[tokio::test(flavor = "current_thread")]
async fn saves_of_php_and_twig_keep_other_roots_warm() {
    let (service, _socket) = tower_lsp::LspService::new(PhpLspBackend::new);
    let backend = service.inner();
    let mut a = Fixture::new();
    let mut b = Fixture::new();
    a.cache = backend.twig_context_disk_cache.clone();
    b.cache = a.cache.clone();
    a.controller(0, "A");
    a.caller(0);
    b.controller(0, "B");
    b.caller(0);
    a.context().await;
    b.context().await;
    let b_before = source_work(&b.root);
    for relative in ["src/Controller0.php", "templates/caller0.twig"] {
        let before = source_work(&a.root);
        backend
            .lsp_did_save(DidSaveTextDocumentParams {
                text_document: TextDocumentIdentifier {
                    uri: a.uri(relative).parse().unwrap(),
                },
                text: None,
            })
            .await;
        a.context().await;
        b.context().await;
        assert_eq!(
            source_work(&b.root),
            b_before,
            "save invalidated another root"
        );
        assert_eq!(source_work(&a.root).0 - before.0, 1);
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn open_alias_edits_and_close_replace_and_restore_disk_for_php_and_twig() {
    use std::os::unix::fs::symlink;
    for alias in ["A", "B"] {
        let fixture = Fixture::new();
        fixture.write(
            "external/Controller.php",
            "<?php $this->render('A.twig', ['item' => new Disk(), 'payload' => ['field' => 1]]);",
        );
        fixture.write(
            "external/caller.twig",
            "{% include 'partial.twig' with { value: item } %}",
        );
        for name in ["A", "B"] {
            symlink(
                fixture.root.join("external/Controller.php"),
                fixture.root.join(format!("src/{name}.php")),
            )
            .unwrap();
            symlink(
                fixture.root.join("external/caller.twig"),
                fixture.root.join(format!("templates/{name}.twig")),
            )
            .unwrap();
        }
        assert_eq!(
            variable(&fixture.context().await, "partial.twig", "value").as_deref(),
            Some("Disk")
        );
        let relative = format!("src/{alias}.php");
        fixture.index_php(
            &relative,
            "<?php $this->render('A.twig', ['item' => new Unsaved()]);",
            true,
        );
        fixture.invalidate(&relative).await;
        assert_eq!(
            variable(&fixture.context().await, "partial.twig", "value").as_deref(),
            Some("Unsaved")
        );
        fixture.files.remove(&fixture.uri(&relative));
        fixture.invalidate(&relative).await;
        assert_eq!(
            variable(&fixture.context().await, "partial.twig", "value").as_deref(),
            Some("Disk")
        );
        let relative = format!("templates/{alias}.twig");
        fixture.templates.insert(
            fixture.uri(&relative),
            preprocess_twig_template(
                "{% include 'partial.twig' with { changed: item, payload: payload } %}",
                &[],
            ),
        );
        fixture.invalidate(&relative).await;
        assert_eq!(
            variable(&fixture.context().await, "partial.twig", "changed").as_deref(),
            Some("Disk"),
            "include edited through alias {alias} lost the rendered template name"
        );
        let payload = fixture
            .context()
            .await
            .variables("partial.twig")
            .into_iter()
            .find(|variable| variable.name == "payload")
            .unwrap();
        assert!(payload
            .shape_definitions
            .iter()
            .any(
                |definition| definition.uri == fixture.uri("src/A.php") && definition.range.0 == 0
            ));
        fixture.write("external/Controller.php", "<?php\n$this->render('A.twig', ['item' => new Updated(), 'payload' => [\n'field' => 1]]);");
        fixture.invalidate("src/A.php").await;
        let updated = fixture.context().await;
        assert_eq!(
            variable(&updated, "partial.twig", "changed").as_deref(),
            Some("Updated")
        );
        let payload = updated
            .variables("partial.twig")
            .into_iter()
            .find(|variable| variable.name == "payload")
            .unwrap();
        assert!(payload
            .shape_definitions
            .iter()
            .any(
                |definition| definition.uri == fixture.uri("src/A.php") && definition.range.0 == 2
            ));
        fixture.templates.insert(
            fixture.uri(&relative),
            preprocess_twig_template("removed include", &[]),
        );
        fixture.invalidate(&relative).await;
        assert!(fixture.context().await.variables("partial.twig").is_empty());
        fixture.templates.remove(&fixture.uri(&relative));
        fixture.invalidate(&relative).await;
        assert_eq!(
            variable(&fixture.context().await, "partial.twig", "value").as_deref(),
            Some("Updated")
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn deleting_open_twig_drops_overlay_and_parser() {
    let (service, _socket) = tower_lsp::LspService::new(PhpLspBackend::new);
    let backend = service.inner();
    let fixture = Fixture::new();
    let uri = fixture.uri("templates/deleted.twig");
    backend.open_files.insert(
        uri.clone(),
        backend.open_template_document(
            &uri,
            "{% include 'partial.twig' with { value: 1 } %}",
            TemplateKind::Twig,
            &[],
        ),
    );
    backend.document_versions.insert(
        uri.clone(),
        OpenDocumentState {
            version: 2,
            generation: 1,
        },
    );
    backend
        .lsp_did_delete_files(DeleteFilesParams {
            files: vec![FileDelete { uri: uri.clone() }],
        })
        .await;
    assert!(!backend.template_documents.contains_key(&uri));
    assert!(!backend.open_files.contains_key(&uri));
    assert!(!backend.document_versions.contains_key(&uri));
}
