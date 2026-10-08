//! Real JSON-RPC futures share one backend; the dispatch lock is released
//! before awaiting a handler, so lifecycle and requests can run concurrently.
mod support;
use php_lsp_types::uri::path_to_uri;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use support::*;

struct Rpc {
    service: Mutex<LspService<PhpLspBackend>>,
    next_id: AtomicI64,
    active: AtomicUsize,
    peak: AtomicUsize,
}

struct InFlight<'a>(&'a Rpc);
impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Rpc {
    fn id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }
    async fn send(&self, request: Request) -> serde_json::Value {
        let future = {
            let mut service = self.service.lock().unwrap();
            let mut context = Context::from_waker(futures::task::noop_waker_ref());
            assert!(matches!(
                service.poll_ready(&mut context),
                Poll::Ready(Ok(()))
            ));
            service.call(request)
        };
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        let _flight = InFlight(self);
        let response = future.await.expect("RPC service error");
        match response {
            Some(response) => {
                assert!(response.error().is_none(), "{response:?}");
                extract_result(Some(response))
            }
            None => serde_json::Value::Null,
        }
    }
}

#[derive(Default)]
struct IndexProgress {
    started: AtomicU64,
    ready: AtomicU64,
}

// Tokio's timeout cannot fire if both runtime threads block on a lock cycle.
// This independent watchdog makes a deadlock fail this test process instead
// of hanging cargo/CI indefinitely. It exits only the dedicated test binary.
struct Watchdog {
    stop: Option<std::sync::mpsc::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Watchdog {
    fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            if matches!(
                rx.recv_timeout(Duration::from_secs(60)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ) {
                use std::io::Write;
                let _ = std::io::stderr()
                    .lock()
                    .write_all(b"P2-19 concurrent lifecycle test stalled for 60 seconds\n");
                std::process::exit(101);
            }
        });
        Self {
            stop: Some(tx),
            worker: Some(worker),
        }
    }
}
impl Drop for Watchdog {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

struct Fixture {
    rpc: Arc<Rpc>,
    root: PathBuf,
    library_uri: String,
    caller_uri: String,
    caller: String,
    saved: String,
    progress: Arc<IndexProgress>,
}

fn source(padding: usize, crlf: bool) -> String {
    let doc = "/**\n * @property-read string $readable\n * @property-write string $writable\n * @method string fetch()\n */";
    let text = format!("<?php\nnamespace Foreign {{\n{doc}\nclass Owned {{}}\n}}\nnamespace Wanted {{\n{}/* 😀 */ {doc}\n#[Marker]\n\nclass Owned {{}}\n}}\n", "\n".repeat(padding));
    if crlf {
        text.replace('\n', "\r\n")
    } else {
        text
    }
}

fn expected_location(source: &str, uri: &str, name: &str) -> serde_json::Value {
    let start = source.find("namespace Wanted {").unwrap();
    let needle = if name == "fetch" {
        "fetch()".to_string()
    } else {
        format!("${name}")
    };
    let offset = start + source[start..].find(&needle).unwrap() + usize::from(name != "fetch");
    let (line, character) = utf16_position_for_offset(source, offset);
    json!({"uri": uri, "range": {
        "start": {"line": line, "character": character},
        "end": {"line": line, "character": character + name.encode_utf16().count() as u32}
    }})
}

impl Fixture {
    async fn new() -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("php-lsp-doc-stress-{}-{nonce}", std::process::id()));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("composer.json"),
            r#"{"autoload":{"psr-4":{"Wanted\\":"src/"}}}"#,
        )
        .unwrap();
        let saved = source(0, false);
        let caller = "<?php use Wanted\\Owned; function probe(Owned $value) { /* 😀 */ $value->readable; $value->writable = 'new'; $value->fetch(); }".to_string();
        fs::write(root.join("src/Owned.php"), &saved).unwrap();
        fs::write(root.join("src/Caller.php"), &caller).unwrap();
        let library_uri = path_to_uri(&root.join("src/Owned.php")).unwrap();
        let caller_uri = path_to_uri(&root.join("src/Caller.php")).unwrap();
        let (service, mut socket) = LspService::new(PhpLspBackend::new);
        let (tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
        let progress = Arc::new(IndexProgress::default());
        let socket_progress = progress.clone();
        let workspace = root.display().to_string();
        tokio::spawn(async move {
            while let Some(notification) = socket.next().await {
                if notification.method() == "phpLsp/indexingStatus" {
                    if let Some(params) = notification.params() {
                        if params["workspaceFolder"].as_str() == Some(workspace.as_str()) {
                            if let Some(run) = params["indexingRunId"].as_u64() {
                                socket_progress.started.fetch_max(run, Ordering::SeqCst);
                                if params["phase"] == "ready" {
                                    socket_progress.ready.fetch_max(run, Ordering::SeqCst);
                                }
                            }
                        }
                    }
                    let _ = tx.send(notification);
                }
            }
        });
        let rpc = Arc::new(Rpc {
            service: Mutex::new(service),
            next_id: AtomicI64::new(1),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        });
        rpc.send(initialize_request_with_options(
            rpc.id(),
            Some(&path_to_uri(&root).unwrap()),
            Some(json!({
                "stubExtensions": [], "diagnosticsMode": "off", "indexVendor": false,
                "indexing": {"maxFiles": 100, "maxEntries": 1000}
            })),
        ))
        .await;
        rpc.send(initialized_notification()).await;
        wait_for_indexing_phase(&mut notifications, "ready", Duration::from_secs(15)).await;
        rpc.send(did_open_notification(&caller_uri, &caller)).await;
        Self {
            rpc,
            root,
            library_uri,
            caller_uri,
            caller,
            saved,
            progress,
        }
    }
    async fn definition(&self, name: &str) -> serde_json::Value {
        let (line, character) = utf16_position_at(
            &self.caller,
            &format!(
                "{name}{}",
                if name == "fetch" {
                    "()"
                } else if name == "writable" {
                    " ="
                } else {
                    ";"
                }
            ),
        );
        self.rpc
            .send(definition_request(
                self.rpc.id(),
                &self.caller_uri,
                line,
                character + 1,
            ))
            .await
    }
    async fn assert_all(&self, source: &str, stage: &str) {
        for name in ["readable", "writable", "fetch"] {
            let actual = self.definition(name).await;
            assert_eq!(
                actual,
                expected_location(source, &self.library_uri, name),
                "{stage}, {name}: {actual}"
            );
        }
    }
    async fn finish(&self) {
        self.rpc.send(shutdown_request(self.rpc.id())).await;
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(php_lsp_index::cache::cache_file_path(&self.root));
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn phpdoc_definition_close_restores_disk_provenance_for_virtual_members() {
    let _watchdog = Watchdog::new();
    let fixture = Fixture::new().await;
    fixture.assert_all(&fixture.saved, "initial disk").await;
    fixture
        .rpc
        .send(did_open_notification(&fixture.library_uri, &fixture.saved))
        .await;
    let unsaved = source(4, true);
    fixture
        .rpc
        .send(did_change_full_notification(
            &fixture.library_uri,
            2,
            &unsaved,
        ))
        .await;
    fixture.assert_all(&unsaved, "unsaved open buffer").await;
    fixture
        .rpc
        .send(did_close_notification(&fixture.library_uri))
        .await;
    fixture
        .assert_all(&fixture.saved, "disk restored after close")
        .await;
    fixture.finish().await;
}

#[derive(Default)]
struct Activity {
    epoch: AtomicU64,
    source_case: AtomicUsize,
    lifecycle_running: AtomicBool,
    reindex_running: AtomicBool,
    queries: AtomicUsize,
    positives: AtomicUsize,
    during_edits: AtomicUsize,
    during_reindex: AtomicUsize,
    edits: AtomicUsize,
    closes: AtomicUsize,
    watched: AtomicUsize,
    full_reindexes: AtomicUsize,
}

fn without_owned_doc(source: &str) -> String {
    let scope = source.find("namespace Wanted {").unwrap();
    let start = scope + source[scope..].find("/**").unwrap();
    let end = start + source[start..].find("*/").unwrap() + 2;
    let mut result = source.to_string();
    // Keep line/UTF-16 layout to make foreign-comment fallbacks adversarial.
    let blank = source[start..end]
        .chars()
        .map(|ch| if ch == '\n' || ch == '\r' { ch } else { ' ' })
        .collect::<String>();
    result.replace_range(start..end, &blank);
    result
}

async fn wait_latest_index(fixture: &Fixture, after: u64) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let started = fixture.progress.started.load(Ordering::SeqCst);
            let ready = fixture.progress.ready.load(Ordering::SeqCst);
            if started > after && ready >= started {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "latest workspace reindex never became ready: after={after}, started={}, ready={}",
            fixture.progress.started.load(Ordering::SeqCst),
            fixture.progress.ready.load(Ordering::SeqCst)
        )
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phpdoc_definition_parallel_edit_close_and_reindex_stress() {
    const ROUNDS: usize = 64;
    const READERS: usize = 2;
    let _watchdog = Watchdog::new();
    let fixture = Arc::new(Fixture::new().await);
    fixture
        .rpc
        .send(did_open_notification(&fixture.library_uri, &fixture.saved))
        .await;
    fixture.assert_all(&fixture.saved, "pre-stress").await;
    let changed = source(4, true);
    let absent = without_owned_doc(&changed);
    let other_namespace = changed.replace("namespace Wanted {", "namespace Others {");
    let variants = Arc::new(vec![
        fixture.saved.clone(),
        changed,
        absent,
        other_namespace,
    ]);
    let activity = Arc::new(Activity::default());
    let barrier = Arc::new(tokio::sync::Barrier::new(READERS + 2));
    let mut workers = tokio::task::JoinSet::new();

    for reader in 0..READERS {
        let fixture = fixture.clone();
        let activity = activity.clone();
        let barrier = barrier.clone();
        let variants = variants.clone();
        workers.spawn(async move {
            barrier.wait().await;
            for iteration in 0..ROUNDS * 4 {
                for name in ["readable", "writable", "fetch"] {
                    let epoch = activity.epoch.load(Ordering::SeqCst);
                    let case = activity.source_case.load(Ordering::SeqCst);
                    let editing = activity.lifecycle_running.load(Ordering::SeqCst);
                    let reindexing = activity.reindex_running.load(Ordering::SeqCst);
                    let result = fixture.definition(name).await;
                    let after = activity.epoch.load(Ordering::SeqCst);
                    activity.queries.fetch_add(1, Ordering::SeqCst);
                    if editing { activity.during_edits.fetch_add(1, Ordering::SeqCst); }
                    if reindexing { activity.during_reindex.fetch_add(1, Ordering::SeqCst); }
                    if !result.is_null() {
                        assert!([0, 1].into_iter().any(|version| result == expected_location(&variants[version], &fixture.library_uri, name)), "reader {reader}, round {iteration}, {name}: foreign or invalid range: {result}");
                        // When no source writer crossed the request, absent
                        // annotations/namespaces must not resolve stale tags.
                        if epoch == after && epoch % 2 == 0 && case >= 2 {
                            panic!("stable absent owner/tag still resolved {name}: {result}");
                        }
                        activity.positives.fetch_add(1, Ordering::SeqCst);
                    }
                }
                tokio::task::yield_now().await;
            }
        });
    }
    {
        let fixture = fixture.clone();
        let activity = activity.clone();
        let barrier = barrier.clone();
        let variants = variants.clone();
        workers.spawn(async move {
            activity.lifecycle_running.store(true, Ordering::SeqCst);
            barrier.wait().await;
            let mut version = 1;
            for iteration in 0..ROUNDS {
                let case = 1 + iteration % 3;
                activity.epoch.fetch_add(1, Ordering::SeqCst);
                version += 1;
                fixture
                    .rpc
                    .send(did_change_full_notification(
                        &fixture.library_uri,
                        version,
                        &variants[case],
                    ))
                    .await;
                activity.source_case.store(case, Ordering::SeqCst);
                activity.edits.fetch_add(1, Ordering::SeqCst);
                activity.epoch.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
                if iteration % 4 == 0 {
                    activity.epoch.fetch_add(1, Ordering::SeqCst);
                    fixture
                        .rpc
                        .send(did_close_notification(&fixture.library_uri))
                        .await;
                    activity.source_case.store(0, Ordering::SeqCst);
                    activity.closes.fetch_add(1, Ordering::SeqCst);
                    activity.epoch.fetch_add(1, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                    activity.epoch.fetch_add(1, Ordering::SeqCst);
                    fixture
                        .rpc
                        .send(did_open_notification(&fixture.library_uri, &variants[case]))
                        .await;
                    version = 1;
                    activity.source_case.store(case, Ordering::SeqCst);
                    activity.epoch.fetch_add(1, Ordering::SeqCst);
                }
            }
            activity.lifecycle_running.store(false, Ordering::SeqCst);
        });
    }
    {
        let fixture = fixture.clone();
        let activity = activity.clone();
        let barrier = barrier.clone();
        workers.spawn(async move {
            activity.reindex_running.store(true, Ordering::SeqCst);
            barrier.wait().await;
            for iteration in 0..ROUNDS {
                fixture
                    .rpc
                    .send(did_change_watched_files_notification(vec![(
                        &fixture.library_uri,
                        2,
                    )]))
                    .await;
                activity.watched.fetch_add(1, Ordering::SeqCst);
                if iteration % 8 == 0 {
                    let before = fixture.progress.started.load(Ordering::SeqCst);
                    let composer = path_to_uri(&fixture.root.join("composer.json")).unwrap();
                    fixture
                        .rpc
                        .send(did_change_watched_files_notification(vec![(&composer, 2)]))
                        .await;
                    wait_latest_index(&fixture, before).await;
                    activity.full_reindexes.fetch_add(1, Ordering::SeqCst);
                }
                tokio::task::yield_now().await;
            }
            activity.reindex_running.store(false, Ordering::SeqCst);
        });
    }
    tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(worker) = workers.join_next().await {
            worker.expect("stress worker panicked");
        }
    })
    .await
    .expect("concurrent PHPDoc requests did not make progress");
    assert_eq!(
        activity.queries.load(Ordering::SeqCst),
        READERS * ROUNDS * 4 * 3
    );
    assert!(
        activity.positives.load(Ordering::SeqCst) > 0,
        "all requests returned null; stress coverage is vacuous"
    );
    assert!(activity.during_edits.load(Ordering::SeqCst) > 0);
    assert!(activity.during_reindex.load(Ordering::SeqCst) > 0);
    assert_eq!(activity.edits.load(Ordering::SeqCst), ROUNDS);
    assert_eq!(activity.closes.load(Ordering::SeqCst), ROUNDS / 4);
    assert_eq!(activity.watched.load(Ordering::SeqCst), ROUNDS);
    assert_eq!(activity.full_reindexes.load(Ordering::SeqCst), ROUNDS / 8);
    assert!(
        fixture.rpc.peak.load(Ordering::SeqCst) >= 2,
        "RPC handlers never overlapped"
    );
    println!("P2-19 stress: {} definitions, {} positive, {} during edits, {} during reindex, peak {} in flight; {} edits, {} closes, {} watches, {} full reindexes", activity.queries.load(Ordering::SeqCst), activity.positives.load(Ordering::SeqCst), activity.during_edits.load(Ordering::SeqCst), activity.during_reindex.load(Ordering::SeqCst), fixture.rpc.peak.load(Ordering::SeqCst), ROUNDS, ROUNDS / 4, ROUNDS, ROUNDS / 8);

    // A stable end state must produce exact definitions, not just permissible
    // historical snapshots or null responses during overlapping mutations.
    fixture
        .rpc
        .send(did_close_notification(&fixture.library_uri))
        .await;
    fixture
        .assert_all(&fixture.saved, "closed disk after stress")
        .await;
    fixture
        .rpc
        .send(did_open_notification(&fixture.library_uri, &variants[1]))
        .await;
    fixture
        .assert_all(&variants[1], "reopened buffer after stress")
        .await;
    fixture
        .rpc
        .send(did_change_full_notification(
            &fixture.library_uri,
            2,
            &variants[2],
        ))
        .await;
    for name in ["readable", "writable", "fetch"] {
        assert!(
            fixture.definition(name).await.is_null(),
            "removed {name} survived after stress"
        );
    }
    fixture
        .rpc
        .send(did_close_notification(&fixture.library_uri))
        .await;
    fixture
        .assert_all(&fixture.saved, "disk restored after final removal")
        .await;
    fixture.finish().await;
}
