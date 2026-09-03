use super::*;
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn newer_run_cancels_and_blocks_older_commits() {
    let coordinator = Arc::new(IndexingRunCoordinator::default());
    let root = PathBuf::from("/workspace/a");
    let old_guard = coordinator.start(root.clone());
    let old = old_guard.lease();
    let new_guard = coordinator.start(root.clone());

    assert!(old.token().is_cancelled());
    assert!(!old.is_current());
    assert!(!old.is_latest());
    assert!(old.commit_if_current(|| 1).is_none());
    assert!(new_guard.lease().is_current());
    assert!(coordinator.is_active(&root));
}

#[test]
fn commit_linearizes_before_a_new_run_can_start() {
    let coordinator = Arc::new(IndexingRunCoordinator::default());
    let root = PathBuf::from("/workspace/a");
    let old_guard = coordinator.start(root.clone());
    let old = old_guard.lease();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (committed_tx, committed_rx) = mpsc::channel();
    let old_thread = std::thread::spawn(move || {
        old.commit_if_current(|| {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            committed_tx.send(()).unwrap();
        })
    });

    entered_rx.recv().unwrap();
    let next_coordinator = coordinator.clone();
    let next_root = root.clone();
    let (started_tx, started_rx) = mpsc::channel();
    let new_thread = std::thread::spawn(move || {
        let guard = next_coordinator.start(next_root);
        started_tx.send(()).unwrap();
        guard
    });
    assert!(started_rx.recv_timeout(Duration::from_millis(50)).is_err());

    release_tx.send(()).unwrap();
    committed_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(old_thread.join().unwrap().is_some());
    let new_guard = new_thread.join().unwrap();
    started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(new_guard.lease().is_current());
    drop(old_guard);
    assert!(new_guard.lease().is_current());
}

#[test]
fn aggregate_commit_linearizes_before_replacement_run_start() {
    let coordinator = Arc::new(IndexingRunCoordinator::default());
    let root = PathBuf::from("/workspace/a");
    let old_guard = coordinator.start(root.clone());
    let old = old_guard.lease();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let commit_coordinator = coordinator.clone();
    let revision = coordinator.aggregate_source_revision();
    let commit = std::thread::spawn(move || {
        commit_coordinator.commit_aggregate_if_current(std::slice::from_ref(&old), revision, || {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        })
    });
    entered_rx.recv().unwrap();

    let next_coordinator = coordinator.clone();
    let (started_tx, started_rx) = mpsc::channel();
    let start = std::thread::spawn(move || {
        let guard = next_coordinator.start(root);
        started_tx.send(()).unwrap();
        guard
    });
    assert!(started_rx.recv_timeout(Duration::from_millis(50)).is_err());
    release_tx.send(()).unwrap();
    assert!(commit.join().unwrap().is_some());
    let next = start.join().unwrap();
    started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(next.lease().is_current());
}

#[test]
fn multi_root_commit_rejects_when_any_reserved_run_is_stale() {
    let coordinator = Arc::new(IndexingRunCoordinator::default());
    let root_a = PathBuf::from("/workspace/a");
    let root_b = PathBuf::from("/workspace/b");
    let old_a = coordinator.start(root_a.clone());
    let run_b = coordinator.start(root_b);
    let runs = vec![old_a.lease(), run_b.lease()];
    let _new_a = coordinator.start(root_a);

    assert!(coordinator
        .commit_if_all_current(&runs, || "must not commit")
        .is_none());
}

#[test]
fn dropping_old_guard_does_not_finish_newer_run() {
    let coordinator = Arc::new(IndexingRunCoordinator::default());
    let root = PathBuf::from("/workspace/a");
    let old = coordinator.start(root.clone());
    let new = coordinator.start(root.clone());
    drop(old);

    assert!(new.lease().is_current());
    assert!(coordinator.is_active(&root));
    drop(new);
    assert!(!coordinator.is_active(&root));
}

#[test]
fn roots_have_independent_active_runs() {
    let coordinator = Arc::new(IndexingRunCoordinator::default());
    let root_a = PathBuf::from("/workspace/a");
    let root_b = PathBuf::from("/workspace/b");
    let old_a = coordinator.start(root_a.clone());
    let run_b = coordinator.start(root_b.clone());
    let new_a = coordinator.start(root_a.clone());

    assert!(old_a.lease().token().is_cancelled());
    assert!(new_a.lease().is_current());
    assert!(run_b.lease().is_current());
    assert!(coordinator.is_active(&root_a));
    assert!(coordinator.is_active(&root_b));
}

#[test]
fn removing_workspace_cancels_run_and_invalidates_latest_identity() {
    let coordinator = Arc::new(IndexingRunCoordinator::default());
    let root = PathBuf::from("/workspace/a");
    let guard = coordinator.start(root.clone());
    let run = guard.lease();
    let identity = run.identity();

    coordinator.cancel_and_remove(&root);

    assert!(run.token().is_cancelled());
    assert!(!run.is_current());
    assert!(!coordinator.is_latest(&identity));
    assert!(!coordinator.is_active(&root));
}

#[test]
fn shutdown_cancellation_stops_all_roots_and_rejects_late_commits() {
    let coordinator = Arc::new(IndexingRunCoordinator::default());
    let run_a_guard = coordinator.start(PathBuf::from("/workspace/a"));
    let run_b_guard = coordinator.start(PathBuf::from("/workspace/b"));
    let run_a = run_a_guard.lease();
    let run_b = run_b_guard.lease();
    let identity_a = run_a.identity();
    let identity_b = run_b.identity();

    coordinator.cancel_all();

    assert!(run_a.token().is_cancelled());
    assert!(run_b.token().is_cancelled());
    assert!(!run_a.is_current());
    assert!(!run_b.is_current());
    assert!(!coordinator.is_latest(&identity_a));
    assert!(!coordinator.is_latest(&identity_b));
    assert!(run_a.commit_if_current(|| ()).is_none());
    assert!(run_b.commit_if_current(|| ()).is_none());
}

#[test]
fn guard_drop_during_unwind_cleans_active_run() {
    let coordinator = Arc::new(IndexingRunCoordinator::default());
    let root = PathBuf::from("/workspace/a");
    let panic_coordinator = coordinator.clone();
    let panic_root = root.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _guard = panic_coordinator.start(panic_root);
        panic!("synthetic indexing panic");
    }));

    assert!(result.is_err());
    assert!(!coordinator.is_active(&root));
}

#[tokio::test]
async fn aborting_task_drops_run_guard() {
    let coordinator = Arc::new(IndexingRunCoordinator::default());
    let root = PathBuf::from("/workspace/a");
    let task_coordinator = coordinator.clone();
    let task_root = root.clone();
    let task = tokio::spawn(async move {
        let _guard = task_coordinator.start(task_root);
        std::future::pending::<()>().await;
    });
    tokio::task::yield_now().await;
    assert!(coordinator.is_active(&root));

    task.abort();
    let _ = task.await;
    assert!(!coordinator.is_active(&root));
}

#[test]
fn stale_run_discards_prepared_cache_without_replacing_destination() {
    let root = std::env::temp_dir().join(format!(
        "php-lsp-stale-run-cache-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let destination = root.join("index.bin");
    std::fs::write(&destination, b"newer-cache").unwrap();
    let stale_cache = php_lsp_index::cache::IndexCache {
        schema_version: php_lsp_index::cache::CACHE_SCHEMA_VERSION,
        namespace: "workspace".to_string(),
        php_lsp_version: "test".to_string(),
        workspace_root: root.display().to_string(),
        config_hash: 1,
        stubs_hash: 2,
        created_at_unix_ms: 3,
        files: Vec::new(),
        top_level: php_lsp_index::cache::CachedTopLevelSymbols::default(),
    };
    let prepared = php_lsp_index::cache::prepare_cache_write(&destination, &stale_cache).unwrap();
    let coordinator = Arc::new(IndexingRunCoordinator::default());
    let old = coordinator.start(root.clone());
    let old_run = old.lease();
    let _new = coordinator.start(root.clone());

    assert!(old_run.commit_if_current(|| prepared.commit()).is_none());
    assert_eq!(std::fs::read(&destination).unwrap(), b"newer-cache");
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
    std::fs::remove_dir_all(root).unwrap();
}
