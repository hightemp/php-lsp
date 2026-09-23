//! Incremental Twig render/include context indexing.

use super::super::*;

pub(in crate::server) fn read_twig_context_source(path: &Path) -> std::io::Result<String> {
    #[cfg(test)]
    note_source_work(path, false);
    std::fs::read_to_string(path)
}

pub(in crate::server) fn parse_twig_context_source(
    parser: &mut FileParser,
    source: &str,
    uri: &str,
) {
    #[cfg(test)]
    if let Some(path) = uri_to_path(uri) {
        note_source_work(&path, true);
    }
    #[cfg(not(test))]
    let _ = uri;
    parser.parse_full(source);
}

#[cfg(test)]
static SOURCE_WORK: std::sync::OnceLock<StdMutex<HashMap<PathBuf, (usize, usize)>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
fn note_source_work(path: &Path, parse: bool) {
    let mut work = SOURCE_WORK.get_or_init(StdMutex::default).lock().unwrap();
    let counts = work.entry(path.to_path_buf()).or_default();
    if parse {
        counts.1 += 1;
    } else {
        counts.0 += 1;
    }
}

#[cfg(test)]
pub(in crate::server) fn source_work(root: &Path) -> (usize, usize) {
    SOURCE_WORK
        .get_or_init(StdMutex::default)
        .lock()
        .unwrap()
        .iter()
        .filter(|(path, _)| path.starts_with(root))
        .fold((0, 0), |total, (_, value)| {
            (total.0 + value.0, total.1 + value.1)
        })
}

use crate::server::lsp::templates::{
    evaluate_twig_include_calls, evaluate_twig_render_calls, merge_twig_context_variables,
    twig_include_calls, twig_render_calls, TwigContextPhpSourceResolver,
    TwigContextResolvedPhpSource, TwigIncludeCall, TwigRenderCall,
};
use crate::util::fs_walk::{physical_identity, walk_files, PhysicalIdentity, TraversalStopReason};
use crate::util::uri::path_to_uri;
use php_lsp_index::workspace::WorkspaceIndexRevision;
use std::collections::{BTreeMap, BTreeSet};
use tokio::sync::{watch, Semaphore};

pub(in crate::server) const TWIG_CONTEXT_PHP_FILE_SCAN_LIMIT: usize = 2048;
pub(in crate::server) const TWIG_CONTEXT_TEMPLATE_FILE_SCAN_LIMIT: usize = 2048;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RootKey {
    root: PathBuf,
    index: usize,
    limits: TraversalLimits,
    excludes: Vec<PathBuf>,
}

#[derive(Clone)]
struct SourceFacts {
    hash: u64,
    source: Arc<str>,
    symbols: Arc<php_lsp_types::FileSymbols>,
    symbol_uri: String,
    renders: Vec<TwigRenderCall>,
    includes: Vec<TwigIncludeCall>,
}

impl SourceFacts {
    fn symbols_for(&self, uri: &str) -> Arc<php_lsp_types::FileSymbols> {
        if self.symbol_uri == uri {
            return self.symbols.clone();
        }
        let mut symbols = self.symbols.as_ref().clone();
        for symbol in &mut symbols.symbols {
            symbol.uri = uri.to_string();
        }
        Arc::new(symbols)
    }
}

#[derive(Clone)]
struct PhpContribution {
    hash: u64,
    variables: Arc<HashMap<String, Vec<TemplateVariableType>>>,
    dependencies: HashSet<String>,
    symbols: HashSet<String>,
    whole_index: bool,
}

#[derive(Clone)]
struct TwigContribution {
    hash: u64,
    caller_variables: Vec<TemplateVariableType>,
    variables: Arc<HashMap<String, Vec<TemplateVariableType>>>,
}

#[derive(Default, Clone)]
struct Snapshot {
    inventory_php: BTreeSet<String>,
    inventory_twig: BTreeSet<String>,
    disk: HashMap<PhysicalIdentity, Arc<SourceFacts>>,
    bindings: HashMap<String, PhysicalIdentity>,
    physical_paths: HashMap<PathBuf, PhysicalIdentity>,
    overlays: HashMap<String, Arc<SourceFacts>>,
    failed_disk: HashSet<PhysicalIdentity>,
    missing_uris: HashSet<String>,
    php: BTreeMap<String, PhpContribution>,
    twig: BTreeMap<String, TwigContribution>,
    index_files: HashMap<String, Arc<php_lsp_types::FileSymbols>>,
    direct: HashMap<String, Vec<TemplateVariableType>>,
    variables: HashMap<String, Vec<TemplateVariableType>>,
    render_callers: HashMap<String, BTreeSet<String>>,
    include_callers: HashMap<String, BTreeSet<String>>,
    revision: Option<WorkspaceIndexRevision>,
    partial: bool,
}

#[derive(Default)]
struct RootSlot {
    epoch: u64,
    dirty: HashSet<String>,
    snapshot: Option<Arc<Snapshot>>,
    ready_epoch: Option<u64>,
    reinventory: bool,
    job: Option<Arc<Job>>,
}

#[derive(Default)]
struct CoordinatorState {
    roots: HashMap<RootKey, RootSlot>,
    order: VecDeque<RootKey>,
    next_job: u64,
    generation: Option<u64>,
    active: Option<HashSet<RootKey>>,
}

#[derive(Clone)]
enum JobResult {
    Pending,
    Done(Option<Arc<Snapshot>>),
}

struct Job {
    id: u64,
    epoch: u64,
    revision: WorkspaceIndexRevision,
    generation: Option<u64>,
    lease: Option<IndexingRunLease>,
    deadline: Instant,
    cancel: watch::Sender<bool>,
    result: watch::Sender<JobResult>,
    waiters: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    last_waiter_pause: StdMutex<Option<WorkerPause>>,
}

impl Job {
    fn try_join(&self) -> bool {
        self.waiters
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                (count > 0).then(|| count + 1)
            })
            .is_ok()
    }

    fn cancel(&self) {
        self.cancel.send_replace(true);
    }
    fn stopped(&self) -> bool {
        *self.cancel.borrow()
            || self.waiters.load(Ordering::SeqCst) == 0
            || Instant::now() >= self.deadline
            || self
                .lease
                .as_ref()
                .is_some_and(|run| run.token().is_cancelled())
    }
}

struct Waiter(Arc<Job>);
impl Drop for Waiter {
    fn drop(&mut self) {
        if self.0.waiters.fetch_sub(1, Ordering::SeqCst) == 1 {
            #[cfg(test)]
            if let Some((reached, release)) = self.0.last_waiter_pause.lock().unwrap().take() {
                let _ = reached.send(());
                let _ = release.recv_timeout(Duration::from_secs(5));
            }
            self.0.cancel();
        }
    }
}

pub(in crate::server) struct Coordinator {
    state: StdMutex<CoordinatorState>,
    permits: Arc<Semaphore>,
    stopped: AtomicBool,
    #[cfg(test)]
    hooks: TestHooks,
}

impl Default for Coordinator {
    fn default() -> Self {
        Self {
            state: StdMutex::new(CoordinatorState::default()),
            permits: Arc::new(Semaphore::new(1)),
            stopped: AtomicBool::new(false),
            #[cfg(test)]
            hooks: TestHooks::default(),
        }
    }
}

/// The name denotes cached disk-derived context, not a persisted cache file.
/// Kept at the existing backend boundary so unrelated filesystem caches stay independent.
#[derive(Default)]
pub(in crate::server) struct TwigContextDiskCache {
    pub(in crate::server) coordinator: Arc<Coordinator>,
}

impl TwigContextDiskCache {
    pub(in crate::server) fn invalidate_results(&mut self) {
        for slot in self.coordinator.state.lock().unwrap().roots.values_mut() {
            slot.epoch += 1;
            slot.reinventory = true;
            if let Some(snapshot) = &slot.snapshot {
                slot.dirty.extend(snapshot.bindings.keys().cloned());
                slot.dirty.extend(snapshot.missing_uris.iter().cloned());
            }
            if let Some(job) = slot.job.take() {
                job.cancel();
            }
        }
    }
    #[cfg(test)]
    pub(in crate::server) fn clear(&mut self) {
        let mut state = self.coordinator.state.lock().unwrap();
        for slot in state.roots.values() {
            if let Some(job) = &slot.job {
                job.cancel();
            }
        }
        state.roots.clear();
        state.order.clear();
    }

    pub(in crate::server) fn shutdown(&self) {
        self.coordinator.stopped.store(true, Ordering::SeqCst);
        self.coordinator.permits.close();
        for slot in self.coordinator.state.lock().unwrap().roots.values() {
            if let Some(job) = &slot.job {
                job.cancel();
            }
        }
    }

    pub(in crate::server) fn evict_entries_for_source_uri(&mut self, uri: &str) -> usize {
        let path = uri_to_path(uri);
        let mut count = 0;
        for (key, slot) in &mut self.coordinator.state.lock().unwrap().roots {
            if path
                .as_ref()
                .is_some_and(|path| path.starts_with(&key.root))
                || slot.snapshot.as_ref().is_some_and(|snapshot| {
                    snapshot.bindings.contains_key(uri)
                        || snapshot.overlays.contains_key(uri)
                        || snapshot.missing_uris.contains(uri)
                })
            {
                slot.epoch += 1;
                slot.dirty.insert(uri.to_string());
                if path.as_ref().is_some_and(|path| {
                    path.extension().is_none_or(|ext| {
                        !ext.eq_ignore_ascii_case("php") && !ext.eq_ignore_ascii_case("twig")
                    })
                }) || slot
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.partial)
                {
                    slot.reinventory = true;
                    if let Some(snapshot) = &slot.snapshot {
                        slot.dirty.extend(snapshot.bindings.keys().cloned());
                        slot.dirty.extend(snapshot.missing_uris.iter().cloned());
                    }
                }
                if let Some(job) = slot.job.take() {
                    job.cancel();
                }
                count += 1;
            }
        }
        count
    }

    pub(in crate::server) fn evict_index(&mut self, index: &Arc<WorkspaceIndex>) -> usize {
        let identity = Arc::as_ptr(index) as usize;
        let mut state = self.coordinator.state.lock().unwrap();
        let before = state.roots.len();
        state.roots.retain(|key, slot| {
            if key.index != identity {
                return true;
            }
            if let Some(job) = &slot.job {
                job.cancel();
            }
            false
        });
        state.order.retain(|key| key.index != identity);
        before - state.roots.len()
    }

    #[cfg(test)]
    pub(in crate::server) fn len(&self) -> usize {
        self.coordinator.state.lock().unwrap().roots.len()
    }
}

impl Coordinator {
    pub(in crate::server) fn deadline(&self) -> Instant {
        let budget = Duration::from_millis(FILE_IO_TIMEOUT_MS);
        #[cfg(test)]
        let budget =
            Duration::from_millis(self.hooks.budget_ms.load(Ordering::SeqCst).max(1)).min(budget);
        Instant::now() + budget
    }

    pub(in crate::server) fn configure(&self, runtime: &WorkspaceRuntimeState) {
        let active = runtime
            .configs
            .iter()
            .map(|config| RootKey {
                root: config.root.clone(),
                index: Arc::as_ptr(&config.index) as usize,
                limits: config.runtime_config.traversal_limits,
                excludes: config.runtime_config.exclude_paths.clone(),
            })
            .collect::<HashSet<_>>();
        let mut state = self.state.lock().unwrap();
        state.generation = Some(runtime.generation);
        state.roots.retain(|key, slot| {
            slot.epoch += 1;
            slot.reinventory = true;
            if let Some(snapshot) = &slot.snapshot {
                slot.dirty.extend(snapshot.bindings.keys().cloned());
                slot.dirty.extend(snapshot.missing_uris.iter().cloned());
            }
            if let Some(job) = slot.job.take() {
                job.cancel();
            }
            active.contains(key)
        });
        state.order.retain(|key| active.contains(key));
        state.active = Some(active);
    }
}

pub(in crate::server) struct TwigContextView {
    coordinator: Arc<Coordinator>,
    key: RootKey,
    epoch: u64,
    generation: Option<u64>,
    index: Arc<WorkspaceIndex>,
    snapshot: Arc<Snapshot>,
    deadline: Instant,
}

impl TwigContextView {
    pub(in crate::server) fn limit_deadline(&mut self, deadline: Instant) {
        self.deadline = self.deadline.min(deadline);
    }

    pub(in crate::server) fn variables(&self, template: &str) -> Vec<TemplateVariableType> {
        self.snapshot
            .variables
            .get(template)
            .cloned()
            .unwrap_or_default()
    }
    #[cfg(test)]
    pub(in crate::server) fn direct(&self, template: &str) -> Vec<TemplateVariableType> {
        self.snapshot
            .direct
            .get(template)
            .cloned()
            .unwrap_or_default()
    }
    /// Call with the target parser entry already locked. The callback must not
    /// mutate WorkspaceIndex, whose mutation barrier protects this revision.
    pub(in crate::server) fn commit_if_current<T>(&self, commit: impl FnOnce() -> T) -> Option<T> {
        let state = self.coordinator.state.lock().unwrap();
        let slot = state.roots.get(&self.key)?;
        if self.coordinator.stopped.load(Ordering::SeqCst)
            || state.generation != self.generation
            || slot.epoch != self.epoch
            || slot.ready_epoch != Some(self.epoch)
            || !slot
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| Arc::ptr_eq(snapshot, &self.snapshot))
        {
            return None;
        }
        self.index
            .with_revision(self.snapshot.revision?, || {
                if self.coordinator.stopped.load(Ordering::SeqCst)
                    || Instant::now() >= self.deadline
                {
                    None
                } else {
                    Some(commit())
                }
            })
            .flatten()
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::server) async fn twig_context_for_state(
    root: &Path,
    open_files: &Arc<DashMap<String, FileParser>>,
    templates: &Arc<DashMap<String, TemplateDocument>>,
    index: &Arc<WorkspaceIndex>,
    cache: &Arc<Mutex<TwigContextDiskCache>>,
    limits: TraversalLimits,
    excludes: &[PathBuf],
    run: Option<&IndexingRunLease>,
    generation: Option<u64>,
) -> Option<TwigContextView> {
    let coordinator = cache.lock().await.coordinator.clone();
    if coordinator.stopped.load(Ordering::SeqCst) || run.is_some_and(|run| !run.is_current()) {
        return None;
    }
    let key = RootKey {
        root: root.to_path_buf(),
        index: Arc::as_ptr(index) as usize,
        limits,
        excludes: excludes.to_vec(),
    };
    let revision = index.revision_snapshot();
    let (job, start) = {
        let mut state = coordinator.state.lock().unwrap();
        if generation.is_some() && state.generation != generation {
            return None;
        }
        if state
            .active
            .as_ref()
            .is_some_and(|active| !active.contains(&key))
        {
            return None;
        }
        let generation = state.generation;
        state.order.retain(|existing| existing != &key);
        state.order.push_back(key.clone());
        while state.order.len() > TWIG_CONTEXT_DISK_CACHE_CAPACITY {
            if let Some(old) = state.order.pop_front() {
                if let Some(slot) = state.roots.remove(&old) {
                    if let Some(job) = slot.job {
                        job.cancel();
                    }
                }
            }
        }
        state.next_job += 1;
        let id = state.next_job;
        let slot = state.roots.entry(key.clone()).or_default();
        if slot.ready_epoch == Some(slot.epoch) {
            if let Some(snapshot) = slot
                .snapshot
                .as_ref()
                .filter(|snapshot| snapshot.revision == Some(revision))
            {
                return Some(TwigContextView {
                    coordinator: coordinator.clone(),
                    key,
                    epoch: slot.epoch,
                    generation,
                    index: index.clone(),
                    snapshot: snapshot.clone(),
                    deadline: coordinator.deadline(),
                });
            }
        }
        if let Some(job) = slot.job.as_ref().filter(|job| {
            job.epoch == slot.epoch
                && job.revision == revision
                && !job.stopped()
                && run.is_none_or(|run| {
                    job.lease
                        .as_ref()
                        .is_some_and(|owner| owner.run_id() == run.run_id())
                })
                && job.try_join()
        }) {
            (job.clone(), None)
        } else {
            if let Some(old) = slot.job.take() {
                old.cancel();
            }
            let job = Arc::new(Job {
                id,
                epoch: slot.epoch,
                revision,
                generation,
                lease: run.cloned(),
                deadline: coordinator.deadline(),
                cancel: watch::channel(false).0,
                result: watch::channel(JobResult::Pending).0,
                waiters: std::sync::atomic::AtomicUsize::new(1),
                #[cfg(test)]
                last_waiter_pause: StdMutex::new(None),
            });
            let work = (slot.snapshot.clone(), slot.dirty.clone(), slot.reinventory);
            slot.job = Some(job.clone());
            (job, Some(work))
        }
    };
    let waiter = Waiter(job.clone());
    if let Some((previous, dirty, reinventory)) = start {
        let coordinator = coordinator.clone();
        let key = key.clone();
        let job = job.clone();
        let index = index.clone();
        let open_files = open_files.clone();
        let templates = templates.clone();
        let run = run.cloned();
        tokio::spawn(async move {
            let mut cancellation = job.cancel.subscribe();
            let permit = if job.stopped() {
                None
            } else {
                tokio::select! {
                    permit = coordinator.permits.clone().acquire_owned() => permit.ok(),
                    _ = cancellation.changed() => None,
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(job.deadline)) => None,
                }
            };
            let result = if let Some(permit) = permit.filter(|_| !job.stopped()) {
                let worker_job = job.clone();
                let worker_key = key.clone();
                let worker_index = index.clone();
                let worker_run = run.clone();
                #[cfg(test)]
                let hooks = coordinator.hooks.clone();
                tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    #[cfg(test)]
                    let _active = hooks.enter();
                    let control = || {
                        worker_job.stopped()
                            || worker_index.revision_snapshot() != worker_job.revision
                            || worker_run.as_ref().is_some_and(|run| !run.is_current())
                    };
                    if control() {
                        return None;
                    }
                    build_snapshot(
                        &worker_key,
                        &worker_index,
                        &open_files,
                        &templates,
                        previous,
                        &dirty,
                        reinventory,
                        &control,
                        worker_job.revision,
                    )
                })
                .await
                .ok()
                .flatten()
            } else {
                None
            };
            let publish = || {
                if job.stopped() || coordinator.stopped.load(Ordering::SeqCst) {
                    return None;
                }
                let mut state = coordinator.state.lock().unwrap();
                if state.generation != job.generation {
                    return None;
                }
                let slot = state.roots.get_mut(&key)?;
                if slot.epoch != job.epoch
                    || slot.job.as_ref().is_none_or(|active| active.id != job.id)
                {
                    return None;
                }
                let snapshot = Arc::new(result?);
                index
                    .with_revision(job.revision, || {
                        if job.stopped() || coordinator.stopped.load(Ordering::SeqCst) {
                            return None;
                        }
                        slot.snapshot = Some(snapshot.clone());
                        slot.ready_epoch = Some(job.epoch);
                        slot.dirty.clear();
                        slot.reinventory = false;
                        Some(snapshot)
                    })
                    .flatten()
            };
            let result = match &run {
                Some(run) => run.commit_if_current(publish).flatten(),
                None => publish(),
            };
            {
                let mut state = coordinator.state.lock().unwrap();
                if let Some(slot) = state.roots.get_mut(&key) {
                    if slot.job.as_ref().is_some_and(|active| active.id == job.id) {
                        slot.job = None;
                    }
                }
            }
            job.result.send_replace(JobResult::Done(result));
        });
    }
    let mut receiver = job.result.subscribe();
    let outcome = async {
        loop {
            let current = receiver.borrow().clone();
            if let JobResult::Done(result) = current {
                return result;
            }
            if receiver.changed().await.is_err() {
                return None;
            }
        }
    };
    let snapshot = tokio::time::timeout_at(tokio::time::Instant::from_std(job.deadline), outcome)
        .await
        .ok()
        .flatten();
    if snapshot.is_none() && Instant::now() >= job.deadline {
        job.cancel();
        tracing::warn!("Twig context refresh timed out for {}", root.display());
    }
    drop(waiter);
    Some(TwigContextView {
        coordinator,
        key,
        epoch: job.epoch,
        generation: job.generation,
        index: index.clone(),
        snapshot: snapshot?,
        deadline: job.deadline,
    })
}

fn ordered_source_uris(uris: BTreeSet<String>) -> Vec<String> {
    let mut paths = uris
        .into_iter()
        .map(|uri| {
            (
                uri_to_path(&uri).unwrap_or_else(|| PathBuf::from(&uri)),
                uri,
            )
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths.into_iter().map(|(_, uri)| uri).collect()
}

fn source_hash(source: &str) -> u64 {
    cache::stable_hash_strings([source])
}

fn is_php_source(uri: &str) -> bool {
    uri_to_path(uri)
        .and_then(|path| path.extension().map(|ext| ext.eq_ignore_ascii_case("php")))
        .unwrap_or(false)
}

fn eligible_disk_source(path: &Path, key: &RootKey, php: bool) -> bool {
    if path_is_excluded(path, &key.root, &key.excludes) {
        return false;
    }
    let bases = if php {
        vec!["src", "app", "tests"]
    } else {
        vec!["templates", "resources/views", "app/templates"]
    };
    bases
        .iter()
        .any(|base| path.starts_with(key.root.join(base)))
        && path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case(if php { "php" } else { "twig" }))
        && !is_blade_template_uri(&path.to_string_lossy())
        && (!php
            || !path
                .strip_prefix(&key.root)
                .unwrap_or(path)
                .parent()
                .unwrap_or(Path::new(""))
                .components()
                .any(|part| {
                    let name = part.as_os_str().to_string_lossy();
                    name.starts_with('.')
                        || matches!(name.as_ref(), "vendor" | "node_modules" | "target" | "var")
                }))
}

fn discover(key: &RootKey, php: bool, stopped: &dyn Fn() -> bool) -> (BTreeSet<String>, bool) {
    let roots = if php {
        vec![
            key.root.join("src"),
            key.root.join("app"),
            key.root.join("tests"),
        ]
    } else {
        vec![
            key.root.join("templates"),
            key.root.join("resources/views"),
            key.root.join("app/templates"),
        ]
    };
    let outcome = walk_files(
        &roots,
        key.limits.capped_files(if php {
            TWIG_CONTEXT_PHP_FILE_SCAN_LIMIT
        } else {
            TWIG_CONTEXT_TEMPLATE_FILE_SCAN_LIMIT
        }),
        |path| path_is_excluded(path, &key.root, &key.excludes),
        |path, is_root| {
            !php || is_root
                || path.file_name().is_none_or(|name| {
                    let name = name.to_string_lossy();
                    !name.starts_with('.')
                        && !matches!(name.as_ref(), "vendor" | "node_modules" | "target" | "var")
                })
        },
        |path| eligible_disk_source(path, key, php),
        || stopped().then_some(TraversalStopReason::Cancelled),
    );
    let partial = outcome.truncated();
    (
        outcome
            .files
            .into_iter()
            .filter_map(|path| path_to_uri(&path).ok())
            .collect(),
        partial,
    )
}

struct OpenSource {
    source: Arc<str>,
    symbols: Option<Arc<php_lsp_types::FileSymbols>>,
}

struct SourceLoader<'a> {
    disk: HashMap<PhysicalIdentity, Arc<SourceFacts>>,
    bindings: HashMap<String, PhysicalIdentity>,
    physical_paths: HashMap<PathBuf, PhysicalIdentity>,
    overlays: HashMap<String, Arc<SourceFacts>>,
    open: HashMap<String, OpenSource>,
    open_aliases: HashMap<(PhysicalIdentity, bool), String>,
    dirty: &'a HashSet<String>,
    dirty_ids: HashSet<PhysicalIdentity>,
    validated: HashSet<PhysicalIdentity>,
    failed: HashSet<PhysicalIdentity>,
    missing: HashSet<String>,
    accessed: HashSet<String>,
    stopped: &'a dyn Fn() -> bool,
}

impl SourceLoader<'_> {
    fn open_source_uri(&mut self, uri: &str) -> Option<String> {
        if self.open.contains_key(uri) {
            return Some(uri.to_string());
        }
        let identity = self.identity(uri)?;
        self.open_aliases
            .get(&(identity, is_php_source(uri)))
            .cloned()
    }

    fn identity(&mut self, uri: &str) -> Option<PhysicalIdentity> {
        if let Some(identity) = self.bindings.get(uri) {
            if !self.dirty.contains(uri) && !self.dirty_ids.contains(identity) {
                return Some(identity.clone());
            }
        }
        let path = uri_to_path(uri)?;
        let identity = physical_identity(&path).ok()?;
        if let Ok(canonical) = std::fs::canonicalize(&path) {
            if let Some(old) = self.physical_paths.insert(canonical, identity.clone()) {
                if old != identity {
                    // A canonical pathname can survive atomic replacement.
                    // Reuse syntax only after the new inode's bytes match.
                    self.dirty_ids.insert(old.clone());
                    self.dirty_ids.insert(identity.clone());
                    self.validated.remove(&identity);
                }
                if !self.disk.contains_key(&identity) {
                    if let Some(facts) = self.disk.get(&old).cloned() {
                        self.disk.insert(identity.clone(), facts);
                    }
                }
            }
        }
        self.bindings.insert(uri.to_string(), identity.clone());
        Some(identity)
    }

    fn select(
        &mut self,
        uri: &str,
        seen: &mut HashSet<PhysicalIdentity>,
        cap: usize,
        partial: &mut bool,
    ) -> bool {
        if self.open.contains_key(uri) {
            return true;
        }
        let Some(identity) = self.identity(uri) else {
            return false;
        };
        // Open URIs own separate overlays; disk aliases must not contribute
        // older bytes of the same physical file alongside those overlays.
        if self
            .open_aliases
            .contains_key(&(identity.clone(), is_php_source(uri)))
        {
            self.accessed.insert(uri.to_string());
            return false;
        }
        if seen.contains(&identity) {
            self.accessed.insert(uri.to_string());
            return false;
        }
        if seen.len() >= cap {
            *partial = true;
            return false;
        }
        seen.insert(identity);
        true
    }

    fn facts(
        &self,
        uri: &str,
        source: Arc<str>,
        symbols: Option<Arc<php_lsp_types::FileSymbols>>,
    ) -> Option<Arc<SourceFacts>> {
        if (self.stopped)() {
            return None;
        }
        let php = is_php_source(uri);
        let symbols = if php {
            if let Some(symbols) = symbols {
                symbols
            } else {
                let mut parser = FileParser::new();
                parse_twig_context_source(&mut parser, &source, uri);
                if (self.stopped)() {
                    return None;
                }
                Arc::new(
                    parser
                        .tree()
                        .map(|tree| extract_file_symbols(tree, &source, uri))
                        .unwrap_or_default(),
                )
            }
        } else {
            Arc::new(php_lsp_types::FileSymbols::default())
        };
        let renders = if php {
            twig_render_calls(&source, self.stopped)
        } else {
            Vec::new()
        };
        let includes = if php {
            Vec::new()
        } else {
            twig_include_calls(&source, self.stopped)
        };
        if (self.stopped)() {
            return None;
        }
        Some(Arc::new(SourceFacts {
            hash: source_hash(&source),
            source,
            symbols,
            symbol_uri: uri.to_string(),
            renders,
            includes,
        }))
    }

    fn get(&mut self, uri: &str) -> Option<Arc<SourceFacts>> {
        if (self.stopped)() {
            return None;
        }
        self.accessed.insert(uri.to_string());
        if let Some(open) = self.open.get(uri) {
            let hash = source_hash(&open.source);
            if let Some(cached) = self.overlays.get(uri).filter(|cached| cached.hash == hash) {
                return Some(cached.clone());
            }
            let facts = self.facts(uri, open.source.clone(), open.symbols.clone())?;
            self.overlays.insert(uri.to_string(), facts.clone());
            return Some(facts);
        }
        if self.missing.contains(uri) {
            return None;
        }
        if let Some(identity) = self.bindings.get(uri) {
            if (!self.dirty.contains(uri) && !self.dirty_ids.contains(identity))
                || self.validated.contains(identity)
            {
                if let Some(cached) = self.disk.get(identity) {
                    return Some(cached.clone());
                }
            }
        }
        let path = uri_to_path(uri)?;
        let Some(identity) = self.identity(uri) else {
            self.bindings.remove(uri);
            self.missing.insert(uri.to_string());
            return None;
        };
        self.bindings.insert(uri.to_string(), identity.clone());
        if self.failed.contains(&identity) {
            self.missing.insert(uri.to_string());
            return None;
        }
        if self.validated.contains(&identity)
            || (!self.dirty.contains(uri) && !self.dirty_ids.contains(&identity))
        {
            if let Some(cached) = self.disk.get(&identity) {
                return Some(cached.clone());
            }
        }
        if (self.stopped)() {
            return None;
        }
        let Ok(source) = read_twig_context_source(&path) else {
            self.disk.remove(&identity);
            self.failed.insert(identity);
            self.missing.insert(uri.to_string());
            return None;
        };
        if (self.stopped)() {
            return None;
        }
        self.validated.insert(identity.clone());
        let hash = source_hash(&source);
        if let Some(cached) = self
            .disk
            .get(&identity)
            .filter(|cached| cached.hash == hash)
        {
            return Some(cached.clone());
        }
        let facts = self.facts(uri, source.into(), None)?;
        self.disk.insert(identity, facts.clone());
        Some(facts)
    }
}

#[allow(clippy::too_many_arguments)]
fn build_snapshot(
    key: &RootKey,
    index: &WorkspaceIndex,
    open_files: &DashMap<String, FileParser>,
    templates: &DashMap<String, TemplateDocument>,
    previous: Option<Arc<Snapshot>>,
    dirty: &HashSet<String>,
    reinventory: bool,
    stopped: &dyn Fn() -> bool,
    revision: WorkspaceIndexRevision,
) -> Option<Snapshot> {
    let mut snapshot = previous.as_deref().cloned().unwrap_or_default();
    let unknown_removed_source = dirty.iter().any(|uri| {
        !snapshot.bindings.contains_key(uri)
            && !open_files.contains_key(uri)
            && !templates.contains_key(uri)
            && uri_to_path(uri).is_some_and(|path| !path.exists())
    });
    let reinventory = reinventory
        || unknown_removed_source
        || dirty.iter().filter_map(|uri| uri_to_path(uri)).any(|path| {
            path.is_dir()
                || snapshot
                    .inventory_php
                    .iter()
                    .chain(&snapshot.inventory_twig)
                    .any(|uri| {
                        uri_to_path(uri)
                            .is_some_and(|known| known != path && known.starts_with(&path))
                    })
        });
    let index_files = index
        .file_symbols
        .iter()
        .take_while(|_| !stopped())
        .map(|entry| (entry.key().clone(), entry.value().clone()))
        .collect::<HashMap<_, _>>();
    let changed_index_files = index_files
        .iter()
        .filter(|(uri, symbols)| {
            snapshot
                .index_files
                .get(*uri)
                .is_none_or(|old| !Arc::ptr_eq(old, symbols))
        })
        .map(|(uri, _)| uri.clone())
        .chain(
            snapshot
                .index_files
                .keys()
                .filter(|uri| !index_files.contains_key(*uri))
                .cloned(),
        )
        .collect::<HashSet<_>>();
    let changed_symbols = changed_index_files
        .iter()
        .flat_map(|uri| {
            index_files
                .get(uri)
                .into_iter()
                .chain(snapshot.index_files.get(uri))
        })
        .flat_map(|file| {
            file.symbols
                .iter()
                .map(|symbol| symbol.fqn.to_ascii_lowercase())
        })
        .collect::<HashSet<_>>();
    if previous.is_none() || reinventory {
        let (php, partial_php) = discover(key, true, stopped);
        if stopped() {
            return None;
        }
        let (twig, partial_twig) = discover(key, false, stopped);
        snapshot.inventory_php = php;
        snapshot.inventory_twig = twig;
        snapshot.partial = partial_php || partial_twig;
    } else {
        for uri in dirty {
            if stopped() {
                return None;
            }
            let Some(path) = uri_to_path(uri) else {
                continue;
            };
            for (inventory, php) in [
                (&mut snapshot.inventory_php, true),
                (&mut snapshot.inventory_twig, false),
            ] {
                if eligible_disk_source(&path, key, php) && path.is_file() {
                    inventory.insert(uri.clone());
                } else {
                    inventory.remove(uri);
                }
            }
        }
    }
    if stopped() {
        return None;
    }
    let mut open = HashMap::new();
    for entry in open_files.iter() {
        if stopped() {
            return None;
        }
        let uri = entry.key();
        if !uri.ends_with(".php")
            || is_blade_template_uri(uri)
            || !index.file_symbols.contains_key(uri)
        {
            continue;
        }
        let symbols = index
            .file_symbols
            .get(uri)
            .map(|symbols| symbols.value().clone());
        open.insert(
            uri.clone(),
            OpenSource {
                source: entry.value().source().into(),
                symbols,
            },
        );
    }
    for entry in templates.iter() {
        if stopped() {
            return None;
        }
        let uri = entry.key();
        if entry.kind() != TemplateKind::Twig
            || !uri_to_path(uri).is_some_and(|path| {
                path.starts_with(&key.root) && !path_is_excluded(&path, &key.root, &key.excludes)
            })
        {
            continue;
        }
        open.insert(
            uri.clone(),
            OpenSource {
                source: entry.original_source().into(),
                symbols: None,
            },
        );
    }
    let mut source_dirty = dirty.clone();
    if reinventory {
        source_dirty.extend(snapshot.bindings.keys().cloned());
        source_dirty.extend(snapshot.missing_uris.iter().cloned());
    }
    source_dirty.extend(
        changed_index_files
            .iter()
            .filter(|uri| {
                snapshot.bindings.contains_key(*uri) || snapshot.missing_uris.contains(*uri)
            })
            .cloned(),
    );
    let dirty = &source_dirty;
    let mut dirty_ids = HashSet::new();
    for uri in dirty {
        if stopped() {
            return None;
        }
        if let Some(canonical) = uri_to_path(uri).and_then(|path| std::fs::canonicalize(path).ok())
        {
            if let Some(identity) = snapshot.physical_paths.get(&canonical) {
                dirty_ids.insert(identity.clone());
            }
        }
        if let Some(identity) = snapshot.bindings.get(uri) {
            dirty_ids.insert(identity.clone());
        }
        if let Some(identity) = uri_to_path(uri).and_then(|path| physical_identity(&path).ok()) {
            dirty_ids.insert(identity);
        }
    }
    let mut effective_dirty = dirty.clone();
    effective_dirty.extend(
        snapshot
            .bindings
            .iter()
            .filter(|(_, identity)| dirty_ids.contains(*identity))
            .map(|(uri, _)| uri.clone()),
    );
    let dirty = &effective_dirty;
    let mut loader = SourceLoader {
        disk: snapshot.disk.clone(),
        bindings: snapshot.bindings.clone(),
        physical_paths: snapshot.physical_paths.clone(),
        overlays: snapshot.overlays.clone(),
        open,
        open_aliases: HashMap::new(),
        dirty,
        failed: snapshot
            .failed_disk
            .difference(&dirty_ids)
            .cloned()
            .collect(),
        dirty_ids,
        validated: HashSet::new(),
        missing: snapshot.missing_uris.difference(dirty).cloned().collect(),
        accessed: HashSet::new(),
        stopped,
    };
    for uri in ordered_source_uris(loader.open.keys().cloned().collect()) {
        if stopped() {
            return None;
        }
        if let Some(identity) = loader.identity(&uri) {
            loader
                .open_aliases
                .entry((identity, is_php_source(&uri)))
                .or_insert(uri);
        }
    }
    let loader = RefCell::new(loader);
    let mut php_uris = snapshot.inventory_php.clone();
    let mut twig_uris = snapshot.inventory_twig.clone();
    for uri in loader.borrow().open.keys() {
        if uri.ends_with(".php") {
            php_uris.insert(uri.clone());
        } else {
            twig_uris.insert(uri.clone());
        }
    }
    let mut contributions = BTreeMap::new();
    let mut direct_groups = HashMap::<String, Vec<Vec<TemplateVariableType>>>::new();

    let mut php_seen = HashSet::new();
    for uri in ordered_source_uris(php_uris) {
        if stopped() {
            return None;
        }
        if !loader.borrow_mut().select(
            &uri,
            &mut php_seen,
            key.limits
                .max_files
                .unwrap_or(TWIG_CONTEXT_PHP_FILE_SCAN_LIMIT)
                .min(TWIG_CONTEXT_PHP_FILE_SCAN_LIMIT),
            &mut snapshot.partial,
        ) {
            continue;
        }
        let Some(facts) = loader.borrow_mut().get(&uri) else {
            continue;
        };
        let contribution = if let Some(old) = snapshot.php.get(&uri).filter(|old| {
            old.hash == facts.hash
                && old.dependencies.is_disjoint(dirty)
                && old.dependencies.is_disjoint(&changed_index_files)
                && old.symbols.is_disjoint(&changed_symbols)
                && (!old.whole_index || changed_index_files.is_empty())
        }) {
            loader
                .borrow_mut()
                .accessed
                .extend(old.dependencies.iter().cloned());
            old.clone()
        } else {
            let dependencies = RefCell::new(HashSet::new());
            let lookup = |symbol: &php_lsp_types::SymbolInfo| {
                dependencies.borrow_mut().insert(symbol.uri.clone());
                let uri = loader
                    .borrow_mut()
                    .open_source_uri(&symbol.uri)
                    .unwrap_or_else(|| symbol.uri.clone());
                dependencies.borrow_mut().insert(uri.clone());
                let facts = loader.borrow_mut().get(&uri)?;
                Some(TwigContextResolvedPhpSource {
                    file_symbols: Some(facts.symbols_for(&uri)),
                    uri,
                    source: facts.source.clone(),
                })
            };
            let resolver = TwigContextPhpSourceResolver {
                lookup: &lookup,
                cancelled: stopped,
            };
            #[cfg(test)]
            note_evaluation(&uri);
            let (variables, reads) = index.trace_read_dependencies(|| {
                evaluate_twig_render_calls(
                    &uri,
                    &facts.source,
                    &facts.symbols_for(&uri),
                    &facts.renders,
                    index,
                    &resolver,
                )
            });
            let mut dependencies = dependencies.into_inner();
            dependencies.extend(reads.files);
            PhpContribution {
                hash: facts.hash,
                variables: Arc::new(variables),
                dependencies,
                symbols: reads.symbols,
                whole_index: reads.whole_index,
            }
        };
        if let Some(old) = snapshot.php.get(&uri) {
            for target in old
                .variables
                .keys()
                .filter(|target| !contribution.variables.contains_key(*target))
            {
                if let Some(callers) = snapshot.render_callers.get_mut(target) {
                    callers.remove(&uri);
                }
            }
        }
        for (target, variables) in contribution.variables.iter() {
            snapshot
                .render_callers
                .entry(target.clone())
                .or_default()
                .insert(uri.clone());
            direct_groups
                .entry(target.clone())
                .or_default()
                .push(variables.clone());
        }
        contributions.insert(uri, contribution);
    }
    for (uri, old) in snapshot
        .php
        .iter()
        .filter(|(uri, _)| !contributions.contains_key(*uri))
    {
        for target in old.variables.keys() {
            if let Some(callers) = snapshot.render_callers.get_mut(target) {
                callers.remove(uri);
            }
        }
    }
    snapshot
        .render_callers
        .retain(|_, callers| !callers.is_empty());
    snapshot.php = contributions;
    snapshot.direct = direct_groups
        .into_iter()
        .map(|(target, groups)| (target, merge_twig_context_variables(groups)))
        .collect();
    let mut all_groups = snapshot
        .direct
        .iter()
        .map(|(target, variables)| (target.clone(), vec![variables.clone()]))
        .collect::<HashMap<_, _>>();
    let mut twig_contributions = BTreeMap::new();
    let mut twig_seen = HashSet::new();
    let twig_uris = ordered_source_uris(twig_uris);
    let mut caller_names = HashMap::<String, BTreeSet<String>>::new();
    for uri in &twig_uris {
        if stopped() {
            return None;
        }
        if let Some(name) = twig_template_name_for_uri(uri, &key.root) {
            let source_uri = loader
                .borrow_mut()
                .open_source_uri(uri)
                .unwrap_or_else(|| uri.clone());
            caller_names.entry(source_uri).or_default().insert(name);
        }
    }
    for uri in twig_uris {
        if stopped() {
            return None;
        }
        if !loader.borrow_mut().select(
            &uri,
            &mut twig_seen,
            key.limits
                .max_files
                .unwrap_or(TWIG_CONTEXT_TEMPLATE_FILE_SCAN_LIMIT)
                .min(TWIG_CONTEXT_TEMPLATE_FILE_SCAN_LIMIT),
            &mut snapshot.partial,
        ) {
            continue;
        }
        let Some(facts) = loader.borrow_mut().get(&uri) else {
            continue;
        };
        let Some(names) = caller_names.get(&uri) else {
            continue;
        };
        // A buffer may supply bytes for another logical template name. Keep
        // render bindings separate from the URI owning those unsaved bytes.
        let caller_variables = merge_twig_context_variables(
            names
                .iter()
                .filter_map(|name| snapshot.direct.get(name).cloned()),
        );
        let contribution = snapshot
            .twig
            .get(&uri)
            .filter(|old| old.hash == facts.hash && old.caller_variables == caller_variables)
            .cloned()
            .unwrap_or_else(|| {
                #[cfg(test)]
                note_evaluation(&uri);
                TwigContribution {
                    hash: facts.hash,
                    variables: Arc::new(evaluate_twig_include_calls(
                        &facts.source,
                        &facts.includes,
                        &caller_variables,
                        stopped,
                    )),
                    caller_variables,
                }
            });
        if let Some(old) = snapshot.twig.get(&uri) {
            for target in old
                .variables
                .keys()
                .filter(|target| !contribution.variables.contains_key(*target))
            {
                if let Some(callers) = snapshot.include_callers.get_mut(target) {
                    callers.remove(&uri);
                }
            }
        }
        for (target, variables) in contribution.variables.iter() {
            snapshot
                .include_callers
                .entry(target.clone())
                .or_default()
                .insert(uri.clone());
            all_groups
                .entry(target.clone())
                .or_default()
                .push(variables.clone());
        }
        twig_contributions.insert(uri, contribution);
    }
    if stopped() {
        return None;
    }
    for (uri, old) in snapshot
        .twig
        .iter()
        .filter(|(uri, _)| !twig_contributions.contains_key(*uri))
    {
        for target in old.variables.keys() {
            if let Some(callers) = snapshot.include_callers.get_mut(target) {
                callers.remove(uri);
            }
        }
    }
    snapshot
        .include_callers
        .retain(|_, callers| !callers.is_empty());
    snapshot.twig = twig_contributions;
    snapshot.index_files = index_files;
    snapshot.variables = all_groups
        .into_iter()
        .map(|(target, groups)| (target, merge_twig_context_variables(groups)))
        .collect();
    let mut loader = loader.into_inner();
    loader
        .bindings
        .retain(|uri, _| loader.accessed.contains(uri));
    let live = loader.bindings.values().cloned().collect::<HashSet<_>>();
    loader.disk.retain(|identity, _| live.contains(identity));
    loader
        .overlays
        .retain(|uri, _| loader.open.contains_key(uri) && loader.accessed.contains(uri));
    snapshot.disk = loader.disk;
    snapshot.physical_paths = loader
        .physical_paths
        .into_iter()
        .filter(|(_, identity)| live.contains(identity))
        .collect();
    snapshot.bindings = loader.bindings;
    snapshot.overlays = loader.overlays;
    snapshot.failed_disk = loader.failed.intersection(&live).cloned().collect();
    snapshot.missing_uris = loader
        .missing
        .intersection(&loader.accessed)
        .cloned()
        .collect();
    snapshot.revision = Some(revision);
    Some(snapshot)
}

#[cfg(test)]
type WorkerPause = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);

#[cfg(test)]
#[derive(Clone)]
struct TestHooks {
    budget_ms: Arc<AtomicU64>,
    active: Arc<std::sync::atomic::AtomicUsize>,
    peak: Arc<std::sync::atomic::AtomicUsize>,
    starts: Arc<std::sync::atomic::AtomicUsize>,
    pause: Arc<StdMutex<Option<WorkerPause>>>,
}
#[cfg(test)]
impl Default for TestHooks {
    fn default() -> Self {
        Self {
            budget_ms: Arc::new(AtomicU64::new(FILE_IO_TIMEOUT_MS)),
            active: Arc::default(),
            peak: Arc::default(),
            starts: Arc::default(),
            pause: Arc::default(),
        }
    }
}
#[cfg(test)]
impl TestHooks {
    fn enter(&self) -> ActiveWorker {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        self.starts.fetch_add(1, Ordering::SeqCst);
        let guard = ActiveWorker(self.active.clone());
        let pause = self.pause.lock().unwrap().take();
        if let Some((reached, release)) = pause {
            let _ = reached.send(());
            let _ = release.recv();
        }
        guard
    }
}
#[cfg(test)]
struct ActiveWorker(Arc<std::sync::atomic::AtomicUsize>);
#[cfg(test)]
impl Drop for ActiveWorker {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
static EVALUATIONS: std::sync::OnceLock<StdMutex<HashMap<PathBuf, usize>>> =
    std::sync::OnceLock::new();
#[cfg(test)]
fn note_evaluation(uri: &str) {
    if let Some(path) = uri_to_path(uri) {
        *EVALUATIONS
            .get_or_init(StdMutex::default)
            .lock()
            .unwrap()
            .entry(path)
            .or_default() += 1;
    }
}

#[cfg(test)]
#[path = "twig_context_tests.rs"]
mod tests;
