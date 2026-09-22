use super::*;

struct Fixture {
    root: PathBuf,
    index: Arc<WorkspaceIndex>,
    files: Arc<DashMap<String, FileParser>>,
    templates: Arc<DashMap<String, TemplateDocument>>,
    cache: Arc<Mutex<TwigContextDiskCache>>,
}
impl Fixture {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("php-lsp-twig-index-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("templates")).unwrap();
        Self {
            root,
            index: Arc::new(WorkspaceIndex::new()),
            files: Arc::new(DashMap::new()),
            templates: Arc::new(DashMap::new()),
            cache: Arc::new(Mutex::new(TwigContextDiskCache::default())),
        }
    }
    fn uri(&self, relative: &str) -> String {
        path_to_uri(&self.root.join(relative)).unwrap()
    }
    fn write(&self, relative: &str, source: &str) {
        let path = self.root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    fn controller(&self, number: usize, kind: &str) {
        self.write(&format!("src/Controller{number}.php"), &format!("<?php function show{number}() {{ $this->render('caller{number}.twig', ['item' => new {kind}()]); }}"));
    }
    fn caller(&self, number: usize) {
        self.write(
            &format!("templates/caller{number}.twig"),
            &format!("{{% include 'partial.twig' with {{ value{number}: item }} %}}"),
        );
    }
    fn index_php(&self, relative: &str, source: &str, open: bool) {
        let uri = self.uri(relative);
        let mut parser = FileParser::new();
        parser.parse_full(source);
        self.index.update_file(
            &uri,
            extract_file_symbols(parser.tree().unwrap(), source, &uri),
        );
        if open {
            self.files.insert(uri, parser);
        }
    }
    async fn invalidate(&self, relative: &str) {
        self.cache
            .lock()
            .await
            .evict_entries_for_source_uri(&self.uri(relative));
    }
    async fn context(&self) -> TwigContextView {
        self.context_with(TraversalLimits::default()).await.unwrap()
    }
    async fn context_with(&self, limits: TraversalLimits) -> Option<TwigContextView> {
        twig_context_for_state(
            &self.root,
            &self.files,
            &self.templates,
            &self.index,
            &self.cache,
            limits,
            &[],
            None,
            None,
        )
        .await
    }
    fn spawn(&self) -> tokio::task::JoinHandle<Option<TwigContextView>> {
        let root = self.root.clone();
        let files = self.files.clone();
        let templates = self.templates.clone();
        let index = self.index.clone();
        let cache = self.cache.clone();
        tokio::spawn(async move {
            twig_context_for_state(
                &root,
                &files,
                &templates,
                &index,
                &cache,
                TraversalLimits::default(),
                &[],
                None,
                None,
            )
            .await
        })
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        SOURCE_WORK
            .get_or_init(StdMutex::default)
            .lock()
            .unwrap()
            .retain(|path, _| !path.starts_with(&self.root));
        EVALUATIONS
            .get_or_init(StdMutex::default)
            .lock()
            .unwrap()
            .retain(|path, _| !path.starts_with(&self.root));
    }
}
fn variable(view: &TwigContextView, template: &str, name: &str) -> Option<String> {
    view.variables(template)
        .into_iter()
        .find(|variable| variable.name == name)
        .map(|variable| variable.type_text)
}
fn evaluations(path: &Path) -> usize {
    *EVALUATIONS
        .get_or_init(StdMutex::default)
        .lock()
        .unwrap()
        .get(path)
        .unwrap_or(&0)
}

#[tokio::test(flavor = "current_thread")]
async fn two_thousand_sources_are_linear_and_warm_queries_do_no_source_work() {
    let fixture = Fixture::new();
    for number in 0..2000 {
        fixture.controller(number, &format!("Item{number}"));
        if number < 1999 {
            fixture.caller(number);
        }
    }
    fixture.write("templates/partial.twig", "{{ value0 }}");
    let context = fixture.context().await;
    assert_eq!(context.variables("partial.twig").len(), 1999);
    assert_eq!(source_work(&fixture.root), (4000, 2000));
    assert!(!context.snapshot.partial);
    for _ in 0..3 {
        assert_eq!(
            fixture.context().await.variables("partial.twig").len(),
            1999
        );
    }
    assert_eq!(source_work(&fixture.root), (4000, 2000));
    let before_other = evaluations(&fixture.root.join("src/Controller1.php"));
    let before_twig = evaluations(&fixture.root.join("templates/caller1.twig"));
    fixture.controller(0, "ChangedItem");
    fixture.invalidate("src/Controller0.php").await;
    assert_eq!(
        variable(&fixture.context().await, "partial.twig", "value0").as_deref(),
        Some("ChangedItem")
    );
    assert_eq!(source_work(&fixture.root), (4001, 2001));
    assert_eq!(
        evaluations(&fixture.root.join("src/Controller1.php")),
        before_other
    );
    assert_eq!(
        evaluations(&fixture.root.join("templates/caller1.twig")),
        before_twig
    );
}

#[tokio::test(flavor = "current_thread")]
async fn unsaved_php_and_twig_replace_disk_edges_and_close_restores_disk() {
    let fixture = Fixture::new();
    fixture.controller(0, "DiskItem");
    fixture.caller(0);
    assert_eq!(
        variable(&fixture.context().await, "partial.twig", "value0").as_deref(),
        Some("DiskItem")
    );
    let before = source_work(&fixture.root);
    let open =
        "<?php function show() { $this->render('caller0.twig', ['item' => new OpenItem()]); }";
    fixture.index_php("src/Controller0.php", open, true);
    fixture.invalidate("src/Controller0.php").await;
    assert_eq!(
        variable(&fixture.context().await, "partial.twig", "value0").as_deref(),
        Some("OpenItem")
    );
    assert_eq!(
        source_work(&fixture.root),
        before,
        "the open PHP parser must replace disk parsing"
    );
    fixture.index_php("src/Controller0.php", "<?php // removed render", true);
    fixture.invalidate("src/Controller0.php").await;
    assert!(fixture.context().await.direct("caller0.twig").is_empty());
    fixture.files.remove(&fixture.uri("src/Controller0.php"));
    fixture.invalidate("src/Controller0.php").await;
    assert_eq!(
        variable(&fixture.context().await, "partial.twig", "value0").as_deref(),
        Some("DiskItem")
    );
    fixture.templates.insert(
        fixture.uri("templates/caller0.twig"),
        preprocess_twig_template("removed include", &[]),
    );
    fixture.invalidate("templates/caller0.twig").await;
    assert!(fixture.context().await.variables("partial.twig").is_empty());
    fixture
        .templates
        .remove(&fixture.uri("templates/caller0.twig"));
    fixture.invalidate("templates/caller0.twig").await;
    assert_eq!(
        variable(&fixture.context().await, "partial.twig", "value0").as_deref(),
        Some("DiskItem")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn full_directory_and_partial_invalidations_rebuild_inventory() {
    let fixture = Fixture::new();
    fixture.controller(0, "First");
    fixture.caller(0);
    fixture.context().await;
    std::fs::remove_file(fixture.root.join("src/Controller0.php")).unwrap();
    fixture.controller(1, "Second");
    fixture.caller(1);
    fixture.cache.lock().await.invalidate_results();
    let context = fixture.context().await;
    assert!(context.direct("caller0.twig").is_empty());
    assert_eq!(
        variable(&context, "partial.twig", "value1").as_deref(),
        Some("Second")
    );
    fixture.write(
        "src/group.v2/New.php",
        "<?php $this->render('new.twig', ['created' => new Created()]);",
    );
    fixture.invalidate("src/group.v2").await;
    assert_eq!(
        variable(&fixture.context().await, "new.twig", "created").as_deref(),
        Some("Created")
    );
    std::fs::rename(
        fixture.root.join("src/group.v2"),
        fixture.root.join("src/renamed.v2"),
    )
    .unwrap();
    fixture.invalidate("src/group.v2").await;
    fixture.invalidate("src/renamed.v2").await;
    let context = fixture.context().await;
    assert!(context
        .snapshot
        .php
        .contains_key(&fixture.uri("src/renamed.v2/New.php")));
    assert!(!context
        .snapshot
        .php
        .contains_key(&fixture.uri("src/group.v2/New.php")));

    let capped = Fixture::new();
    capped.controller(0, "First");
    capped.controller(1, "Second");
    let limits = TraversalLimits {
        max_files: Some(1),
        max_entries: Some(100),
    };
    assert!(capped.context_with(limits).await.unwrap().snapshot.partial);
    let before = source_work(&capped.root);
    capped.context_with(limits).await.unwrap();
    assert_eq!(source_work(&capped.root), before);
    std::fs::remove_file(capped.root.join("src/Controller0.php")).unwrap();
    capped.invalidate("src/Controller0.php").await;
    assert_eq!(
        variable(
            &capped.context_with(limits).await.unwrap(),
            "caller1.twig",
            "item"
        )
        .as_deref(),
        Some("Second")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn failed_reads_never_resurrect_old_source_facts() {
    for relative in ["src/Controller0.php", "templates/caller0.twig"] {
        let fixture = Fixture::new();
        fixture.controller(0, "OldItem");
        fixture.caller(0);
        fixture.context().await;
        std::fs::write(fixture.root.join(relative), [0xff, 0xfe]).unwrap();
        fixture.invalidate(relative).await;
        let first = fixture.context().await.variables("partial.twig");
        assert!(first.iter().all(|value| value.type_text != "OldItem"));
        fixture.write("templates/unrelated.twig", "plain");
        fixture.invalidate("templates/unrelated.twig").await;
        assert!(fixture
            .context()
            .await
            .variables("partial.twig")
            .iter()
            .all(|value| value.type_text != "OldItem"));
        if relative.ends_with(".php") {
            fixture.controller(0, "NewItem");
        } else {
            fixture.caller(0);
        }
        fixture.invalidate(relative).await;
        assert!(fixture
            .context()
            .await
            .variables("partial.twig")
            .iter()
            .any(|value| value.type_text
                == if relative.ends_with(".php") {
                    "NewItem"
                } else {
                    "OldItem"
                }));
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn atomic_replacement_invalidates_every_physical_alias() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    fixture.write(
        "external/Controller.php",
        "<?php $this->render('page.twig', ['old' => new Old()]);",
    );
    symlink(
        fixture.root.join("external/Controller.php"),
        fixture.root.join("src/A.php"),
    )
    .unwrap();
    symlink(
        fixture.root.join("external/Controller.php"),
        fixture.root.join("src/B.php"),
    )
    .unwrap();
    fixture.context().await;
    fixture.invalidate("src/B.php").await;
    fixture.context().await;
    fixture.write(
        "external/replacement.php",
        "<?php $this->render('page.twig', ['new' => new New()]);",
    );
    std::fs::rename(
        fixture.root.join("external/replacement.php"),
        fixture.root.join("external/Controller.php"),
    )
    .unwrap();
    fixture.invalidate("src/A.php").await;
    let context = fixture.context().await;
    assert!(variable(&context, "page.twig", "old").is_none());
    assert_eq!(
        variable(&context, "page.twig", "new").as_deref(),
        Some("New")
    );
    assert!(!context.snapshot.php.contains_key(&fixture.uri("src/B.php")));
    assert!(context
        .snapshot
        .bindings
        .contains_key(&fixture.uri("src/B.php")));
}

#[tokio::test(flavor = "current_thread")]
async fn index_dependencies_include_changed_declarations_and_previous_misses() {
    let fixture = Fixture::new();
    let controller = "<?php function show(Service $service) { $this->render('page.twig', ['item' => $service->item()]); }";
    fixture.write("src/Controller.php", controller);
    fixture.controller(0, "Independent");
    fixture.index_php(
        "types/Service.php",
        "<?php class Service { public function item(): OldItem {} }",
        false,
    );
    assert_eq!(
        variable(&fixture.context().await, "page.twig", "item").as_deref(),
        Some("OldItem")
    );
    let before = source_work(&fixture.root);
    let evaluations_before = evaluations(&fixture.root.join("src/Controller0.php"));
    fixture.index_php(
        "types/Service.php",
        "<?php class Service { public function item(): NewItem {} }",
        false,
    );
    assert_eq!(
        variable(&fixture.context().await, "page.twig", "item").as_deref(),
        Some("NewItem")
    );
    assert_eq!(source_work(&fixture.root), before);
    assert_eq!(
        evaluations(&fixture.root.join("src/Controller0.php")),
        evaluations_before
    );
    fixture.index.remove_file(&fixture.uri("types/Service.php"));
    assert_ne!(
        variable(&fixture.context().await, "page.twig", "item").as_deref(),
        Some("NewItem")
    );
    fixture.index_php(
        "types/Service.php",
        "<?php class Service { public function item(): FoundItem {} }",
        false,
    );
    assert_eq!(
        variable(&fixture.context().await, "page.twig", "item").as_deref(),
        Some("FoundItem")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn unrelated_roots_keep_independent_warm_contexts() {
    let a = Fixture::new();
    let mut b = Fixture::new();
    b.cache = a.cache.clone();
    a.controller(0, "First");
    b.controller(0, "Second");
    a.context().await;
    b.context().await;
    let before = source_work(&b.root);
    a.controller(0, "Changed");
    a.invalidate("src/Controller0.php").await;
    a.context().await;
    assert_eq!(
        variable(&b.context().await, "caller0.twig", "item").as_deref(),
        Some("Second")
    );
    assert_eq!(source_work(&b.root), before);
}

#[tokio::test(flavor = "current_thread")]
async fn form_dependency_reads_are_shared_and_index_changes_revalidate_cached_bodies() {
    let fixture = Fixture::new();
    let old = "<?php class ExampleType extends \\Symfony\\Component\\Form\\AbstractType { public function buildForm($builder) { $builder->add('oldField'); } }";
    fixture.write("forms/ExampleType.php", old);
    fixture.index_php("forms/ExampleType.php", old, false);
    for number in 0..5 {
        fixture.write(&format!("src/C{number}.php"), &format!("<?php function show{number}() {{ $form = $this->createForm(ExampleType::class); $this->render('page{number}.twig', ['form' => $form->createView()]); }}"));
    }
    let context = fixture.context().await;
    assert!(variable(&context, "page0.twig", "form")
        .unwrap()
        .contains("oldField"));
    assert_eq!(
        source_work(&fixture.root),
        (6, 6),
        "shared FormType source must be read/parsed once"
    );
    let new = old.replace("oldField", "newField");
    fixture.write("forms/ExampleType.php", &new);
    fixture.index_php("forms/ExampleType.php", &new, false);
    let context = fixture.context().await;
    let form = variable(&context, "page0.twig", "form").unwrap();
    assert!(
        form.contains("newField") && !form.contains("oldField"),
        "{form}"
    );
    assert_eq!(source_work(&fixture.root), (7, 7));
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn form_atomic_replacement_through_new_index_alias_revalidates_canonical_cache() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let old = "<?php class ExampleType extends \\Symfony\\Component\\Form\\AbstractType { public function buildForm($builder) { $builder->add('oldField'); } }";
    fixture.write("external/Form.php", old);
    std::fs::create_dir_all(fixture.root.join("forms")).unwrap();
    symlink(
        fixture.root.join("external/Form.php"),
        fixture.root.join("forms/A.php"),
    )
    .unwrap();
    symlink(
        fixture.root.join("external/Form.php"),
        fixture.root.join("forms/B.php"),
    )
    .unwrap();
    fixture.index_php("forms/A.php", old, false);
    fixture.write("src/Controller.php", "<?php function show() { $form = $this->createForm(ExampleType::class); $this->render('page.twig', ['form' => $form->createView()]); }");
    assert!(variable(&fixture.context().await, "page.twig", "form")
        .unwrap()
        .contains("oldField"));
    let new = old.replace("oldField", "newField");
    fixture.write("external/New.php", &new);
    std::fs::rename(
        fixture.root.join("external/New.php"),
        fixture.root.join("external/Form.php"),
    )
    .unwrap();
    fixture.index_php("forms/B.php", &new, false);
    let form = variable(&fixture.context().await, "page.twig", "form").unwrap();
    assert!(
        form.contains("newField") && !form.contains("oldField"),
        "{form}"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn incremental_alias_events_keep_single_deterministic_shape_definition() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    fixture.write(
        "external/Controller.php",
        "<?php $this->render('page.twig', ['payload' => ['foo' => 1]]);",
    );
    symlink(
        fixture.root.join("external/Controller.php"),
        fixture.root.join("src/A.php"),
    )
    .unwrap();
    symlink(
        fixture.root.join("external/Controller.php"),
        fixture.root.join("src/Я.php"),
    )
    .unwrap();
    let before = fixture.context().await.variables("page.twig");
    assert_eq!(before[0].shape_definitions.len(), 1);
    assert_eq!(before[0].shape_definitions[0].uri, fixture.uri("src/A.php"));
    fixture.invalidate("src/Я.php").await;
    assert_eq!(fixture.context().await.variables("page.twig"), before);
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn physical_file_budget_keeps_independent_sources_after_alias_and_unicode_events() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    fixture.write(
        "external/Source.php",
        "<?php $this->render('a.twig', ['a' => new A()]);",
    );
    symlink(
        fixture.root.join("external/Source.php"),
        fixture.root.join("src/A.php"),
    )
    .unwrap();
    symlink(
        fixture.root.join("external/Source.php"),
        fixture.root.join("src/B.php"),
    )
    .unwrap();
    fixture.write(
        "src/Z.php",
        "<?php $this->render('z.twig', ['z' => new Z()]);",
    );
    let limits = TraversalLimits {
        max_files: Some(2),
        max_entries: None,
    };
    let before = fixture
        .context_with(limits)
        .await
        .unwrap()
        .variables("z.twig");
    assert!(!before.is_empty());
    fixture.invalidate("src/B.php").await;
    let alias = fixture.context_with(limits).await.unwrap();
    assert_eq!(alias.variables("z.twig"), before);
    assert!(!alias.snapshot.partial);
    fixture.write(
        "src/Я.php",
        "<?php $this->render('ya.twig', ['ya' => new Ya()]);",
    );
    fixture.invalidate("src/Я.php").await;
    let updated = fixture.context_with(limits).await.unwrap();
    assert_eq!(updated.variables("z.twig"), before);
    assert!(updated.variables("ya.twig").is_empty());
    assert!(updated.snapshot.partial);
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn deleting_target_through_an_unseen_alias_removes_cached_context() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    fixture.write(
        "external/Source.php",
        "<?php $this->render('page.twig', ['item' => new Item()]);",
    );
    symlink(
        fixture.root.join("external/Source.php"),
        fixture.root.join("src/A.php"),
    )
    .unwrap();
    symlink(
        fixture.root.join("external/Source.php"),
        fixture.root.join("src/B.php"),
    )
    .unwrap();
    assert!(!fixture.context().await.variables("page.twig").is_empty());
    std::fs::remove_file(fixture.root.join("external/Source.php")).unwrap();
    fixture.invalidate("src/B.php").await;
    assert!(fixture.context().await.variables("page.twig").is_empty());
}

async fn pause_next(
    fixture: &Fixture,
) -> (
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Sender<()>,
    TestHooks,
) {
    let hooks = fixture.cache.lock().await.coordinator.hooks.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let (release, wait) = std::sync::mpsc::channel();
    *hooks.pause.lock().unwrap() = Some((tx, wait));
    (rx, release, hooks)
}
async fn reached(receiver: std::sync::mpsc::Receiver<()>) {
    tokio::task::spawn_blocking(move || {
        receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("worker reached barrier")
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_waiters_share_worker_and_one_cancel_does_not_cancel_others() {
    let fixture = Fixture::new();
    fixture.controller(0, "Item");
    let (rx, release, hooks) = pause_next(&fixture).await;
    let first = fixture.spawn();
    reached(rx).await;
    let mut followers = Vec::new();
    for _ in 0..8 {
        followers.push(fixture.spawn());
    }
    tokio::task::yield_now().await;
    first.abort();
    let _ = first.await;
    release.send(()).unwrap();
    for follower in followers {
        assert!(follower.await.unwrap().is_some());
    }
    assert_eq!(hooks.starts.load(Ordering::SeqCst), 1);
    assert_eq!(hooks.peak.load(Ordering::SeqCst), 1);
    assert_eq!(source_work(&fixture.root), (1, 1));
}

#[tokio::test(flavor = "current_thread")]
async fn timeout_holds_permit_until_the_blocking_worker_really_exits() {
    let fixture = Fixture::new();
    fixture.controller(0, "Item");
    let (rx, release, hooks) = pause_next(&fixture).await;
    hooks.budget_ms.store(200, Ordering::SeqCst);
    let first = fixture.spawn();
    reached(rx).await;
    assert!(first.await.unwrap().is_none());
    assert_eq!(hooks.active.load(Ordering::SeqCst), 1);
    hooks.budget_ms.store(FILE_IO_TIMEOUT_MS, Ordering::SeqCst);
    let second = fixture.spawn();
    tokio::task::yield_now().await;
    assert_eq!(hooks.starts.load(Ordering::SeqCst), 1);
    release.send(()).unwrap();
    assert!(second.await.unwrap().is_some());
    assert_eq!(hooks.peak.load(Ordering::SeqCst), 1);
    assert_eq!(hooks.starts.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn supersession_root_removal_and_shutdown_stop_old_workers_before_io() {
    for mode in 0..3 {
        let fixture = Fixture::new();
        fixture.controller(0, "Old");
        let (rx, release, hooks) = pause_next(&fixture).await;
        let old = fixture.spawn();
        reached(rx).await;
        match mode {
            0 => {
                fixture.controller(0, "New");
                fixture.invalidate("src/Controller0.php").await;
            }
            1 => {
                fixture.cache.lock().await.evict_index(&fixture.index);
            }
            _ => {
                fixture.cache.lock().await.shutdown();
            }
        }
        release.send(()).unwrap();
        assert!(old.await.unwrap().is_none());
        assert_eq!(source_work(&fixture.root), (0, 0));
        assert_eq!(hooks.active.load(Ordering::SeqCst), 0);
        if mode == 0 {
            assert_eq!(
                variable(&fixture.context().await, "caller0.twig", "item").as_deref(),
                Some("New")
            );
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn stale_views_cannot_publish_after_index_or_source_changes() {
    let fixture = Fixture::new();
    fixture.controller(0, "Item");
    let old = fixture.context().await;
    assert_eq!(old.commit_if_current(|| true), Some(true));
    fixture.index_php("src/Type.php", "<?php class NewType {}", false);
    assert!(old.commit_if_current(|| true).is_none());
    let old = fixture.context().await;
    fixture.invalidate("src/Controller0.php").await;
    assert!(old.commit_if_current(|| true).is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn full_cache_invalidation_revalidates_sources_and_clears_framework_results() {
    let fixture = Fixture::new();
    fixture.controller(0, "Old");
    fixture.context().await;
    let framework = Arc::new(Mutex::new(FrameworkStringKeyCache::default()));
    framework.lock().await.insert(
        FrameworkStringKeyCacheKey {
            root: fixture.root.clone(),
            domain: "config".into(),
            traversal_limits: TraversalLimits::default(),
            exclude_paths: Vec::new(),
        },
        Vec::new(),
    );
    assert_eq!(framework.lock().await.len(), 1);
    assert_eq!(fixture.cache.lock().await.len(), 1);
    fixture.controller(0, "New");
    clear_request_fs_caches(&framework, &fixture.cache).await;
    assert_eq!(framework.lock().await.len(), 0);
    assert_eq!(
        variable(&fixture.context().await, "caller0.twig", "item").as_deref(),
        Some("New")
    );
    fixture.cache.lock().await.clear();
    assert_eq!(fixture.cache.lock().await.len(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn runtime_generation_and_removed_roots_reject_pinned_requests() {
    let fixture = Fixture::new();
    fixture.controller(0, "Item");
    let old = fixture.context().await;
    let coordinator = fixture.cache.lock().await.coordinator.clone();
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
    coordinator.configure(&WorkspaceRuntimeState {
        configs: vec![config],
        generation: 1,
        ..Default::default()
    });
    assert!(old.commit_if_current(|| true).is_none());
    let current = twig_context_for_state(
        &fixture.root,
        &fixture.files,
        &fixture.templates,
        &fixture.index,
        &fixture.cache,
        TraversalLimits::default(),
        &[],
        None,
        Some(1),
    )
    .await
    .unwrap();
    coordinator.configure(&WorkspaceRuntimeState {
        generation: 2,
        ..Default::default()
    });
    assert!(current.commit_if_current(|| true).is_none());
    assert!(twig_context_for_state(
        &fixture.root,
        &fixture.files,
        &fixture.templates,
        &fixture.index,
        &fixture.cache,
        TraversalLimits::default(),
        &[],
        None,
        Some(1)
    )
    .await
    .is_none());
    assert_eq!(fixture.cache.lock().await.len(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn replacement_indexing_run_does_not_join_a_superseded_worker() {
    let fixture = Fixture::new();
    fixture.controller(0, "Item");
    let runs = Arc::new(IndexingRunCoordinator::default());
    let old_guard = runs.start(fixture.root.clone());
    let old_lease = old_guard.lease();
    let (rx, release, _) = pause_next(&fixture).await;
    let root = fixture.root.clone();
    let files = fixture.files.clone();
    let templates = fixture.templates.clone();
    let index = fixture.index.clone();
    let cache = fixture.cache.clone();
    let old = tokio::spawn(async move {
        twig_context_for_state(
            &root,
            &files,
            &templates,
            &index,
            &cache,
            TraversalLimits::default(),
            &[],
            Some(&old_lease),
            None,
        )
        .await
    });
    reached(rx).await;
    let new_guard = runs.start(fixture.root.clone());
    let new_lease = new_guard.lease();
    let root = fixture.root.clone();
    let files = fixture.files.clone();
    let templates = fixture.templates.clone();
    let index = fixture.index.clone();
    let cache = fixture.cache.clone();
    let new = tokio::spawn(async move {
        twig_context_for_state(
            &root,
            &files,
            &templates,
            &index,
            &cache,
            TraversalLimits::default(),
            &[],
            Some(&new_lease),
            None,
        )
        .await
    });
    tokio::task::yield_now().await;
    release.send(()).unwrap();
    assert!(old.await.unwrap().is_none());
    assert!(new.await.unwrap().is_some());
}

#[tokio::test(flavor = "current_thread")]
async fn root_cache_capacity_evicts_least_recently_used_snapshots() {
    let fixture = Fixture::new();
    for number in 0..TWIG_CONTEXT_DISK_CACHE_CAPACITY {
        twig_context_for_state(
            &fixture.root.join(number.to_string()),
            &fixture.files,
            &fixture.templates,
            &fixture.index,
            &fixture.cache,
            TraversalLimits::default(),
            &[],
            None,
            None,
        )
        .await
        .unwrap();
    }
    twig_context_for_state(
        &fixture.root.join("0"),
        &fixture.files,
        &fixture.templates,
        &fixture.index,
        &fixture.cache,
        TraversalLimits::default(),
        &[],
        None,
        None,
    )
    .await
    .unwrap();
    twig_context_for_state(
        &fixture.root.join("new"),
        &fixture.files,
        &fixture.templates,
        &fixture.index,
        &fixture.cache,
        TraversalLimits::default(),
        &[],
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        fixture.cache.lock().await.len(),
        TWIG_CONTEXT_DISK_CACHE_CAPACITY
    );
    let coordinator = fixture.cache.lock().await.coordinator.clone();
    let state = coordinator.state.lock().unwrap();
    assert!(state
        .roots
        .keys()
        .any(|key| key.root == fixture.root.join("0")));
    assert!(!state
        .roots
        .keys()
        .any(|key| key.root == fixture.root.join("1")));
}
