//! Vendor indexing helpers.

use crate::util::fs_walk::{merge_physical_file_groups, walk_files, TraversalStopReason};

use super::super::*;

#[derive(Debug, Clone)]
pub(crate) struct VendorAutoloadCacheEntry {
    pub(crate) map: VendorAutoloadMap,
}

const METHOD_MEMBER_KINDS: &[php_lsp_types::PhpSymbolKind] =
    &[php_lsp_types::PhpSymbolKind::Method];
const PROPERTY_MEMBER_KINDS: &[php_lsp_types::PhpSymbolKind] =
    &[php_lsp_types::PhpSymbolKind::Property];
const CLASS_CONSTANT_MEMBER_KINDS: &[php_lsp_types::PhpSymbolKind] = &[
    php_lsp_types::PhpSymbolKind::ClassConstant,
    php_lsp_types::PhpSymbolKind::EnumCase,
];
const CLASS_LIKE_KINDS: &[php_lsp_types::PhpSymbolKind] = &[
    php_lsp_types::PhpSymbolKind::Class,
    php_lsp_types::PhpSymbolKind::Interface,
    php_lsp_types::PhpSymbolKind::Trait,
    php_lsp_types::PhpSymbolKind::Enum,
];
const FUNCTION_KINDS: &[php_lsp_types::PhpSymbolKind] = &[php_lsp_types::PhpSymbolKind::Function];
const GLOBAL_CONSTANT_KINDS: &[php_lsp_types::PhpSymbolKind] =
    &[php_lsp_types::PhpSymbolKind::GlobalConstant];
const NAMESPACE_KINDS: &[php_lsp_types::PhpSymbolKind] = &[php_lsp_types::PhpSymbolKind::Namespace];
const MAX_VENDOR_HIERARCHY_DEPTH: usize = 32;
const MAX_STABLE_MEMBER_RESOLUTION_RETRIES: usize = 4;
const MAX_VENDOR_EPOCH_RETRIES: usize = 4;

fn member_kinds_for_ref_kind(ref_kind: RefKind) -> Option<&'static [php_lsp_types::PhpSymbolKind]> {
    match ref_kind {
        RefKind::Constructor | RefKind::MethodCall => Some(METHOD_MEMBER_KINDS),
        RefKind::PropertyAccess | RefKind::StaticPropertyAccess => Some(PROPERTY_MEMBER_KINDS),
        RefKind::ClassConstant => Some(CLASS_CONSTANT_MEMBER_KINDS),
        _ => None,
    }
}

fn top_level_kinds_for_ref_kind(
    ref_kind: RefKind,
) -> Option<&'static [php_lsp_types::PhpSymbolKind]> {
    match ref_kind {
        RefKind::ClassName => Some(CLASS_LIKE_KINDS),
        RefKind::FunctionCall => Some(FUNCTION_KINDS),
        RefKind::GlobalConstant => Some(GLOBAL_CONSTANT_KINDS),
        RefKind::NamespaceName => Some(NAMESPACE_KINDS),
        _ => None,
    }
}

pub(in crate::server) fn resolve_fqn_with_ref_kind(
    index: &WorkspaceIndex,
    fqn: &str,
    ref_kind: RefKind,
    allow_global_fallback: bool,
) -> Option<std::sync::Arc<php_lsp_types::SymbolInfo>> {
    if let Some(expected_kinds) = member_kinds_for_ref_kind(ref_kind) {
        return index.resolve_member_matching_kinds(fqn, expected_kinds);
    }

    let expected_kinds = top_level_kinds_for_ref_kind(ref_kind)?;
    if let Some(symbol) = index.resolve_fqn_matching_kinds(fqn, expected_kinds) {
        return Some(symbol);
    }

    if allow_global_fallback && matches!(ref_kind, RefKind::FunctionCall | RefKind::GlobalConstant)
    {
        if let Some((_, short_name)) = fqn.rsplit_once('\\') {
            return index.resolve_fqn_matching_kinds(short_name, expected_kinds);
        }
    }

    None
}

#[derive(Debug, Default)]
pub(crate) struct VendorAutoloadCache {
    pub(crate) by_vendor_dir: HashMap<PathBuf, VendorAutoloadCacheEntry>,
    #[cfg(test)]
    pub(crate) before_insert_pause: Option<VendorCacheInsertPause>,
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) struct VendorCacheInsertPause {
    pub(crate) reached: tokio::sync::mpsc::UnboundedSender<()>,
    pub(crate) release: Arc<tokio::sync::Notify>,
}

impl VendorAutoloadCache {
    pub(crate) fn clear(&mut self) {
        self.by_vendor_dir.clear();
    }
}

#[derive(Debug)]
pub(crate) struct VendorFileLru {
    pub(crate) capacity: usize,
    uris: VecDeque<String>,
}

impl Default for VendorFileLru {
    fn default() -> Self {
        Self {
            capacity: VENDOR_FILE_LRU_CAPACITY,
            uris: VecDeque::new(),
        }
    }
}

impl VendorFileLru {
    #[cfg(test)]
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            uris: VecDeque::new(),
        }
    }

    pub(crate) fn touch(&mut self, uri: String) -> Vec<String> {
        if let Some(position) = self.uris.iter().position(|existing| existing == &uri) {
            self.uris.remove(position);
        }
        self.uris.push_back(uri);

        let mut evicted = Vec::new();
        while self.uris.len() > self.capacity {
            if let Some(uri) = self.uris.pop_front() {
                evicted.push(uri);
            }
        }
        evicted
    }

    pub(crate) fn remove(&mut self, uri: &str) {
        if let Some(position) = self.uris.iter().position(|existing| existing == uri) {
            self.uris.remove(position);
        }
    }

    pub(crate) fn clear(&mut self) -> Vec<String> {
        self.uris.drain(..).collect()
    }
}

#[derive(Clone)]
pub(in crate::server) struct VendorLazyIndexContext {
    pub(in crate::server) index: Arc<WorkspaceIndex>,
    pub(in crate::server) workspace_configs: Vec<WorkspaceRootConfig>,
    pub(in crate::server) exclude_paths: Vec<PathBuf>,
    pub(in crate::server) traversal_limits: TraversalLimits,
    pub(in crate::server) php_version: PhpVersion,
    pub(in crate::server) index_vendor: bool,
    pub(in crate::server) vendor_autoload_cache: Arc<Mutex<VendorAutoloadCache>>,
    pub(in crate::server) vendor_file_lru: Arc<Mutex<VendorFileLru>>,
    pub(in crate::server) lazy_loads: Arc<VendorLazyLoadCoordinator>,
    pub(in crate::server) load_epoch: Arc<tokio::sync::RwLock<u64>>,
    pub(in crate::server) external_symlinks: Option<Arc<ExternalSymlinkManager>>,
    pub(in crate::server) runtime_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct VendorLoadKey {
    index_identity: usize,
    epoch: u64,
    class_fqn: String,
}

#[derive(Debug, Clone)]
struct VendorClassLoadOutcome {
    snapshot: Option<php_lsp_index::workspace::CommittedTypeSnapshot>,
}

#[derive(Debug, Clone, Default)]
pub(in crate::server) struct VendorHierarchySnapshot {
    generations: Vec<php_lsp_index::workspace::TypeIndexGeneration>,
    complete: bool,
}

impl VendorHierarchySnapshot {
    fn is_current(&self, index: &WorkspaceIndex) -> bool {
        !self.generations.is_empty()
            && self
                .generations
                .iter()
                .all(|generation| index.type_generation_is_current(generation))
    }
}

#[derive(Default)]
pub(crate) struct VendorLazyLoadCoordinator {
    class_loads:
        DashMap<VendorLoadKey, tokio::sync::watch::Receiver<Option<VendorClassLoadOutcome>>>,
    hierarchy_loads:
        DashMap<VendorLoadKey, tokio::sync::watch::Receiver<Option<VendorHierarchySnapshot>>>,
    #[cfg(test)]
    after_path_resolution: Mutex<Option<VendorLoadPause>>,
}

#[cfg(test)]
#[derive(Clone)]
struct VendorLoadPause {
    reached: tokio::sync::mpsc::UnboundedSender<()>,
    release: Arc<tokio::sync::Notify>,
}

impl VendorLazyLoadCoordinator {
    #[cfg(test)]
    pub(in crate::server) fn in_flight_class_loads(&self) -> usize {
        self.class_loads.len()
    }

    #[cfg(test)]
    pub(in crate::server) async fn pause_next_load_after_path_resolution(
        &self,
        reached: tokio::sync::mpsc::UnboundedSender<()>,
        release: Arc<tokio::sync::Notify>,
    ) {
        *self.after_path_resolution.lock().await = Some(VendorLoadPause { reached, release });
    }

    #[cfg(test)]
    async fn run_after_path_resolution_hook(&self) {
        let pause = self.after_path_resolution.lock().await.take();
        if let Some(pause) = pause {
            let _ = pause.reached.send(());
            pause.release.notified().await;
        }
    }
}

fn remove_class_load_if_channel_matches(
    coordinator: &VendorLazyLoadCoordinator,
    key: &VendorLoadKey,
    receiver: &tokio::sync::watch::Receiver<Option<VendorClassLoadOutcome>>,
) {
    if let dashmap::mapref::entry::Entry::Occupied(entry) =
        coordinator.class_loads.entry(key.clone())
    {
        if entry.get().same_channel(receiver) {
            entry.remove();
        }
    }
}

fn remove_hierarchy_load_if_channel_matches(
    coordinator: &VendorLazyLoadCoordinator,
    key: &VendorLoadKey,
    receiver: &tokio::sync::watch::Receiver<Option<VendorHierarchySnapshot>>,
) {
    if let dashmap::mapref::entry::Entry::Occupied(entry) =
        coordinator.hierarchy_loads.entry(key.clone())
    {
        if entry.get().same_channel(receiver) {
            entry.remove();
        }
    }
}

fn vendor_load_key(context: &VendorLazyIndexContext, class_fqn: &str, epoch: u64) -> VendorLoadKey {
    VendorLoadKey {
        index_identity: Arc::as_ptr(&context.index) as usize,
        epoch,
        class_fqn: class_fqn.trim_start_matches('\\').to_ascii_lowercase(),
    }
}

async fn wait_for_class_load(
    mut receiver: tokio::sync::watch::Receiver<Option<VendorClassLoadOutcome>>,
) -> Option<php_lsp_index::workspace::CommittedTypeSnapshot> {
    loop {
        if let Some(outcome) = receiver.borrow().clone() {
            return outcome.snapshot;
        }
        if receiver.changed().await.is_err() {
            return None;
        }
    }
}

async fn wait_for_hierarchy_load(
    mut receiver: tokio::sync::watch::Receiver<Option<VendorHierarchySnapshot>>,
) -> VendorHierarchySnapshot {
    loop {
        if let Some(outcome) = receiver.borrow().clone() {
            return outcome;
        }
        if receiver.changed().await.is_err() {
            return VendorHierarchySnapshot::default();
        }
    }
}

pub(crate) fn parse_vendor_autoload_map(vendor_dir: &Path) -> Option<VendorAutoloadMap> {
    let installed_json = vendor_dir.join("composer/installed.json");
    if !installed_json.exists() {
        return None;
    }

    let content = std::fs::read_to_string(&installed_json).ok()?;
    let data: serde_json::Value = serde_json::from_str(&content).ok()?;

    // installed.json can be {"packages": [...]} or just [...]
    let packages = data
        .get("packages")
        .and_then(|p| p.as_array())
        .or_else(|| data.as_array())?;

    let mut map = VendorAutoloadMap::default();

    for pkg in packages {
        let install_path = pkg
            .get("install-path")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let pkg_dir = vendor_package_dir(vendor_dir, install_path);

        if let Some(autoload) = pkg.get("autoload") {
            append_vendor_autoload(&mut map, &pkg_dir, autoload);
        }
        if let Some(autoload) = pkg.get("autoload-dev") {
            append_vendor_autoload(&mut map, &pkg_dir, autoload);
        }
    }

    Some(map)
}

pub(in crate::server) async fn parse_vendor_autoload_map_blocking(
    vendor_dir: PathBuf,
) -> Option<VendorAutoloadMap> {
    let path_label = vendor_dir.display().to_string();
    run_file_io_blocking("vendor autoload parse", path_label, move || {
        parse_vendor_autoload_map(&vendor_dir)
    })
    .await
    .ok()
    .flatten()
}

async fn index_class_uncached_with_context(
    context: &VendorLazyIndexContext,
    class_fqn: &str,
) -> Option<php_lsp_index::workspace::CommittedTypeSnapshot> {
    let requested_class_fqn = class_fqn.trim_start_matches('\\');
    if let Some(snapshot) = context.index.get_committed_type(requested_class_fqn) {
        return Some(snapshot);
    }

    for config in &context.workspace_configs {
        let mut all_paths = config
            .namespace_map
            .as_ref()
            .map(|ns_map| ns_map.resolve_class_to_paths(requested_class_fqn))
            .unwrap_or_default();

        let vendor_dir = config.root.join("vendor");
        if context.index_vendor && vendor_dir.is_dir() && all_paths.is_empty() {
            if let Some(vendor_map) =
                cached_vendor_autoload_map_pinned(&context.vendor_autoload_cache, &vendor_dir).await
            {
                if let Some(vendor_resolution) = resolve_vendor_paths_from_map_with_limits_blocking(
                    requested_class_fqn,
                    vendor_map,
                    context.traversal_limits,
                    config.root.clone(),
                    context.exclude_paths.clone(),
                )
                .await
                {
                    if let Some(external_symlinks) = context.external_symlinks.as_ref() {
                        let external_symlinks = external_symlinks.clone();
                        let workspace_folder = config.workspace_folder.clone();
                        let logical_root = config.root.clone();
                        let runtime_generation = context.runtime_generation;
                        let aliases = vendor_resolution.symlink_aliases.clone();
                        let physical_files = vendor_resolution.physical_files.clone();
                        tokio::spawn(async move {
                            external_symlinks
                                .publish_additional_aliases(
                                    workspace_folder,
                                    logical_root,
                                    runtime_generation,
                                    aliases,
                                    physical_files,
                                )
                                .await;
                        });
                    }
                    all_paths.extend(vendor_resolution.paths);
                }
            }
        }

        #[cfg(test)]
        context.lazy_loads.run_after_path_resolution_hook().await;

        for path in &all_paths {
            let abs = if path.is_absolute() {
                path.clone()
            } else {
                config.root.join(path)
            };

            if path_is_excluded(&abs, &config.root, &context.exclude_paths) {
                continue;
            }

            let is_vendor_file = abs.starts_with(config.root.join("vendor"));
            let vendor_cache_config = is_vendor_file.then(|| {
                vendor_index_cache_config(
                    &config.root,
                    context.php_version,
                    &context.exclude_paths,
                    context.traversal_limits,
                )
            });
            if let Some(cache_config) = vendor_cache_config.as_ref() {
                if load_cached_vendor_file_blocking(
                    context.index.clone(),
                    config.root.clone(),
                    abs.clone(),
                    cache_config.clone(),
                )
                .await
                {
                    touch_vendor_file_lru(&context.index, &context.vendor_file_lru, &abs).await;
                    tracing::debug!("Lazy-indexed vendor file from cache: {}", abs.display());
                    if let Some(snapshot) = context.index.get_committed_type(requested_class_fqn) {
                        return Some(snapshot);
                    }
                    tracing::debug!(
                        "Lazy vendor cache file {} did not contain requested class {}",
                        abs.display(),
                        requested_class_fqn
                    );
                    continue;
                }
            }

            if parse_and_index_php_file_blocking(
                context.index.clone(),
                abs.clone(),
                "lazy PHP file index",
            )
            .await
            {
                if is_vendor_file {
                    touch_vendor_file_lru(&context.index, &context.vendor_file_lru, &abs).await;
                }
                tracing::debug!("Lazy-indexed file: {}", abs.display());
                if let Some(snapshot) = context.index.get_committed_type(requested_class_fqn) {
                    if is_vendor_file {
                        if let Some(cache_config) = vendor_cache_config {
                            save_vendor_index_cache_blocking(
                                context.index.clone(),
                                config.root.clone(),
                                cache_config,
                            )
                            .await;
                        }
                    }
                    return Some(snapshot);
                }
                tracing::debug!(
                    "Lazy-indexed file {} did not contain requested class {}",
                    abs.display(),
                    requested_class_fqn
                );
            }
        }
    }

    None
}

async fn ensure_class_indexed_with_context(
    context: &VendorLazyIndexContext,
    class_fqn: &str,
) -> Option<php_lsp_index::workspace::CommittedTypeSnapshot> {
    for _ in 0..MAX_VENDOR_EPOCH_RETRIES {
        let epoch_guard = context.load_epoch.clone().read_owned().await;
        let epoch = *epoch_guard;
        let result =
            ensure_class_indexed_at_epoch(context, class_fqn, epoch, Some(epoch_guard)).await;
        if *context.load_epoch.read().await == epoch {
            return result;
        }
    }
    None
}

async fn ensure_class_indexed_at_epoch(
    context: &VendorLazyIndexContext,
    class_fqn: &str,
    epoch: u64,
    epoch_guard: Option<tokio::sync::OwnedRwLockReadGuard<u64>>,
) -> Option<php_lsp_index::workspace::CommittedTypeSnapshot> {
    let requested_class_fqn = class_fqn.trim_start_matches('\\');
    if let Some(snapshot) = context.index.get_committed_type(requested_class_fqn) {
        return Some(snapshot);
    }

    let key = vendor_load_key(context, requested_class_fqn, epoch);
    let (receiver, leader) = match context.lazy_loads.class_loads.entry(key.clone()) {
        dashmap::mapref::entry::Entry::Occupied(entry) => (entry.get().clone(), None),
        dashmap::mapref::entry::Entry::Vacant(entry) => {
            let (sender, receiver) = tokio::sync::watch::channel(None);
            entry.insert(receiver.clone());
            (receiver, Some(sender))
        }
    };

    if let Some(sender) = leader {
        let load_context = context.clone();
        let load_class_fqn = requested_class_fqn.to_string();
        let coordinator = context.lazy_loads.clone();
        let task_key = key.clone();
        let task_receiver = sender.subscribe();
        tokio::spawn(async move {
            let snapshot = index_class_uncached_with_context(&load_context, &load_class_fqn).await;
            let _ = sender.send(Some(VendorClassLoadOutcome { snapshot }));
            remove_class_load_if_channel_matches(&coordinator, &task_key, &task_receiver);
            drop(epoch_guard);
        });
    } else {
        drop(epoch_guard);
    }

    let cleanup_receiver = receiver.clone();
    let result = wait_for_class_load(receiver).await;
    if result.is_none() {
        remove_class_load_if_channel_matches(&context.lazy_loads, &key, &cleanup_receiver);
    }
    result
}

pub(in crate::server) async fn lazy_index_class_with_context(
    context: &VendorLazyIndexContext,
    class_fqn: &str,
) -> bool {
    let was_present = context.index.get_committed_type(class_fqn).is_some();
    ensure_class_indexed_with_context(context, class_fqn)
        .await
        .is_some()
        && !was_present
}

fn push_unique_type_fqn(fqns: &mut Vec<String>, fqn: &str) {
    let normalized = fqn.trim_start_matches('\\');
    if !fqns
        .iter()
        .any(|existing| existing.eq_ignore_ascii_case(normalized))
    {
        fqns.push(normalized.to_string());
    }
}

fn hierarchy_dependencies(symbol: &php_lsp_types::SymbolInfo) -> Vec<String> {
    let mut dependencies = Vec::new();
    for dependency in &symbol.traits {
        push_unique_type_fqn(&mut dependencies, dependency);
    }
    for dependency in symbol
        .template_bindings
        .iter()
        .filter(|binding| binding.kind == php_lsp_types::TemplateBindingKind::Mixin)
        .map(|binding| binding.target.as_str())
    {
        push_unique_type_fqn(&mut dependencies, dependency);
    }
    for dependency in &symbol.extends {
        push_unique_type_fqn(&mut dependencies, dependency);
    }
    for dependency in &symbol.implements {
        push_unique_type_fqn(&mut dependencies, dependency);
    }
    dependencies
}

fn load_hierarchy_recursive<'a>(
    context: &'a VendorLazyIndexContext,
    class_fqn: &'a str,
    epoch: u64,
    depth: usize,
    visited: &'a mut HashSet<String>,
    generations: &'a mut Vec<php_lsp_index::workspace::TypeIndexGeneration>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
    Box::pin(async move {
        if depth >= MAX_VENDOR_HIERARCHY_DEPTH {
            return false;
        }
        let normalized = class_fqn.trim_start_matches('\\').to_ascii_lowercase();
        if !visited.insert(normalized) {
            return true;
        }

        let Some(snapshot) = ensure_class_indexed_at_epoch(context, class_fqn, epoch, None).await
        else {
            return false;
        };
        let dependencies = hierarchy_dependencies(&snapshot.symbol);
        generations.push(snapshot.generation);

        let mut complete = true;
        for dependency in dependencies {
            if !load_hierarchy_recursive(
                context,
                &dependency,
                epoch,
                depth + 1,
                visited,
                generations,
            )
            .await
            {
                complete = false;
            }
        }
        complete
    })
}

async fn load_hierarchy_uncached_with_context(
    context: &VendorLazyIndexContext,
    class_fqn: &str,
    epoch: u64,
) -> VendorHierarchySnapshot {
    let mut generations = Vec::new();
    let mut visited = HashSet::new();
    let complete =
        load_hierarchy_recursive(context, class_fqn, epoch, 0, &mut visited, &mut generations)
            .await;
    VendorHierarchySnapshot {
        generations,
        complete,
    }
}

async fn ensure_hierarchy_indexed_with_context(
    context: &VendorLazyIndexContext,
    class_fqn: &str,
) -> VendorHierarchySnapshot {
    for _ in 0..MAX_VENDOR_EPOCH_RETRIES {
        let epoch_guard = context.load_epoch.clone().read_owned().await;
        let epoch = *epoch_guard;
        let result =
            ensure_hierarchy_indexed_at_epoch(context, class_fqn, epoch, Some(epoch_guard)).await;
        if *context.load_epoch.read().await == epoch {
            return result;
        }
    }
    VendorHierarchySnapshot::default()
}

async fn ensure_hierarchy_indexed_at_epoch(
    context: &VendorLazyIndexContext,
    class_fqn: &str,
    epoch: u64,
    epoch_guard: Option<tokio::sync::OwnedRwLockReadGuard<u64>>,
) -> VendorHierarchySnapshot {
    let key = vendor_load_key(context, class_fqn, epoch);
    let (receiver, leader) = match context.lazy_loads.hierarchy_loads.entry(key.clone()) {
        dashmap::mapref::entry::Entry::Occupied(entry) => (entry.get().clone(), None),
        dashmap::mapref::entry::Entry::Vacant(entry) => {
            let (sender, receiver) = tokio::sync::watch::channel(None);
            entry.insert(receiver.clone());
            (receiver, Some(sender))
        }
    };

    if let Some(sender) = leader {
        let load_context = context.clone();
        let load_class_fqn = class_fqn.to_string();
        let coordinator = context.lazy_loads.clone();
        let task_key = key.clone();
        let task_receiver = sender.subscribe();
        tokio::spawn(async move {
            let snapshot =
                load_hierarchy_uncached_with_context(&load_context, &load_class_fqn, epoch).await;
            let _ = sender.send(Some(snapshot));
            remove_hierarchy_load_if_channel_matches(&coordinator, &task_key, &task_receiver);
            drop(epoch_guard);
        });
    } else {
        drop(epoch_guard);
    }

    let cleanup_receiver = receiver.clone();
    let result = wait_for_hierarchy_load(receiver).await;
    if result.generations.is_empty() {
        remove_hierarchy_load_if_channel_matches(&context.lazy_loads, &key, &cleanup_receiver);
    }
    result
}

pub(in crate::server) fn lazy_index_parents_with_context<'a>(
    context: &'a VendorLazyIndexContext,
    class_fqn: &'a str,
    depth: usize,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = VendorHierarchySnapshot> + Send + 'a>> {
    Box::pin(async move {
        if depth == 0 {
            return ensure_hierarchy_indexed_with_context(context, class_fqn).await;
        }
        let epoch_guard = context.load_epoch.clone().read_owned().await;
        let epoch = *epoch_guard;
        let mut generations = Vec::new();
        let mut visited = HashSet::new();
        let complete = load_hierarchy_recursive(
            context,
            class_fqn,
            epoch,
            depth,
            &mut visited,
            &mut generations,
        )
        .await;
        drop(epoch_guard);
        VendorHierarchySnapshot {
            generations,
            complete,
        }
    })
}

pub(in crate::server) async fn lazy_index_member_return_types_with_context(
    context: &VendorLazyIndexContext,
    class_fqn: &str,
) {
    let return_fqns: Vec<String> = context
        .index
        .get_members(class_fqn)
        .into_iter()
        .filter_map(|sym| {
            let owner_fqn = sym.parent_fqn.as_deref().unwrap_or(class_fqn);
            symbol_return_type_fqn(&context.index, owner_fqn, &sym)
        })
        .filter(|fqn| fqn.contains('\\') && context.index.get_committed_type(fqn).is_none())
        .collect();

    for return_fqn in return_fqns {
        ensure_hierarchy_indexed_with_context(context, &return_fqn).await;
    }
}

pub(in crate::server) async fn lazy_index_class_dependencies_with_context(
    context: &VendorLazyIndexContext,
    class_fqn: &str,
) -> VendorHierarchySnapshot {
    let hierarchy = ensure_hierarchy_indexed_with_context(context, class_fqn).await;
    lazy_index_member_return_types_with_context(context, class_fqn).await;
    hierarchy
}

pub(in crate::server) async fn resolve_member_stable_with_context(
    context: &VendorLazyIndexContext,
    fqn: &str,
    expected_kinds: Option<&[php_lsp_types::PhpSymbolKind]>,
) -> Option<std::sync::Arc<php_lsp_types::SymbolInfo>> {
    resolve_member_stable_with_context_and_hook(context, fqn, expected_kinds, |_| {}).await
}

async fn resolve_member_stable_with_context_and_hook(
    context: &VendorLazyIndexContext,
    fqn: &str,
    expected_kinds: Option<&[php_lsp_types::PhpSymbolKind]>,
    mut before_lookup: impl FnMut(usize),
) -> Option<std::sync::Arc<php_lsp_types::SymbolInfo>> {
    let (class_fqn, _) = fqn.rsplit_once("::")?;
    if class_fqn.trim_start_matches('\\').is_empty() {
        return None;
    }
    for attempt in 0..MAX_STABLE_MEMBER_RESOLUTION_RETRIES {
        let hierarchy = ensure_hierarchy_indexed_with_context(context, class_fqn).await;
        if hierarchy.generations.is_empty() {
            return None;
        }
        if !hierarchy.complete {
            tracing::debug!(
                "Lazy vendor hierarchy for {} is incomplete; resolving from committed nodes",
                class_fqn
            );
        }
        before_lookup(attempt);
        let resolved = match expected_kinds {
            Some(expected_kinds) => context
                .index
                .resolve_member_matching_kinds(fqn, expected_kinds),
            None => context.index.resolve_member(fqn),
        };
        if hierarchy.is_current(&context.index) {
            return resolved;
        }
    }
    None
}

#[cfg(test)]
pub(in crate::server) async fn resolve_member_stable_with_hook(
    context: &VendorLazyIndexContext,
    fqn: &str,
    expected_kinds: Option<&[php_lsp_types::PhpSymbolKind]>,
    before_lookup: impl FnMut(usize),
) -> Option<std::sync::Arc<php_lsp_types::SymbolInfo>> {
    resolve_member_stable_with_context_and_hook(context, fqn, expected_kinds, before_lookup).await
}

pub(in crate::server) fn append_vendor_autoload(
    map: &mut VendorAutoloadMap,
    pkg_dir: &Path,
    autoload: &serde_json::Value,
) {
    if let Some(psr4) = autoload.get("psr-4").and_then(|v| v.as_object()) {
        for (prefix, dirs) in psr4 {
            let mut directories = Vec::new();
            match dirs {
                serde_json::Value::String(dir) => {
                    directories.push(pkg_dir.join(dir));
                }
                serde_json::Value::Array(dir_list) => {
                    for dir in dir_list {
                        if let Some(dir_str) = dir.as_str() {
                            directories.push(pkg_dir.join(dir_str));
                        }
                    }
                }
                _ => {}
            }
            if !directories.is_empty() {
                map.psr4.push(VendorPsr4Mapping {
                    prefix: prefix.clone(),
                    directories,
                });
            }
        }
    }

    if let Some(files) = autoload.get("files").and_then(|value| value.as_array()) {
        for file in files {
            if let Some(file_path) = file.as_str() {
                push_unique_path(&mut map.files, pkg_dir.join(file_path));
            }
        }
    }

    if let Some(classmap) = autoload.get("classmap").and_then(|value| value.as_array()) {
        for path in classmap {
            if let Some(path) = path.as_str() {
                push_unique_path(&mut map.classmap, pkg_dir.join(path));
            }
        }
    }
}

pub(in crate::server) fn vendor_package_dir(vendor_dir: &Path, install_path: &str) -> PathBuf {
    if install_path.is_empty() {
        vendor_dir.to_path_buf()
    } else if install_path.starts_with("../") {
        vendor_dir.join("composer").join(install_path)
    } else {
        vendor_dir.join(install_path)
    }
}

pub(crate) fn resolve_vendor_paths_from_map(
    fqn: &str,
    map: &VendorAutoloadMap,
) -> Option<Vec<PathBuf>> {
    let psr4_candidates = vendor_psr4_candidate_paths(fqn, map, None, &[]);
    let mut paths = psr4_candidates
        .iter()
        .filter(|candidate| !candidate.is_file())
        .cloned()
        .collect::<Vec<_>>();
    let resolution = resolve_vendor_paths_from_map_with_limits(
        fqn,
        map,
        TraversalLimits {
            max_files: Some(DEFAULT_INDEXING_MAX_FILES),
            max_entries: Some(DEFAULT_INDEXING_MAX_ENTRIES),
        },
        None,
        &[],
    );
    if let Some(resolution) = resolution {
        for path in resolution.paths {
            push_unique_path(&mut paths, path);
        }
    }
    (!paths.is_empty()).then_some(paths)
}

struct VendorPathResolution {
    paths: Vec<PathBuf>,
    symlink_aliases: Vec<crate::util::fs_walk::SymlinkAlias>,
    physical_files: Vec<crate::util::fs_walk::PhysicalFileGroup>,
}

fn resolve_vendor_paths_from_map_with_limits(
    fqn: &str,
    map: &VendorAutoloadMap,
    traversal_limits: TraversalLimits,
    project_root: Option<&Path>,
    exclude_paths: &[PathBuf],
) -> Option<VendorPathResolution> {
    let normalized_fqn = fqn.trim_start_matches('\\');
    let psr4_candidates = vendor_psr4_candidate_paths(fqn, map, project_root, exclude_paths);

    let deadline = file_io_walk_deadline();
    let psr4 = walk_files(
        &psr4_candidates,
        traversal_limits,
        |path| project_root.is_some_and(|root| path_is_excluded(path, root, exclude_paths)),
        |_, _| false,
        is_php_file_path,
        || (Instant::now() >= deadline).then_some(TraversalStopReason::DeadlineExceeded),
    );
    if psr4.truncated() || psr4.stop_reason == Some(TraversalStopReason::DeadlineExceeded) {
        tracing::warn!(
            "Vendor PSR-4 candidate traversal was truncated after {} entries",
            psr4.stats.visited_entries
        );
    }

    let classmap = classmap_candidate_paths_for_fqn(
        normalized_fqn,
        map,
        traversal_limits,
        project_root,
        exclude_paths,
    );
    let mut priority_identities = ordered_physical_identities(&psr4.files, &psr4.physical_files);
    priority_identities.extend(ordered_physical_identities(
        &classmap.paths,
        &classmap.physical_files,
    ));

    let mut physical_files = psr4.physical_files;
    merge_physical_file_groups(&mut physical_files, classmap.physical_files);
    let representatives_by_identity = physical_files
        .iter()
        .map(|group| (group.identity.clone(), group.representative().to_path_buf()))
        .collect::<HashMap<_, _>>();
    let mut seen_identities = HashSet::new();
    let paths = priority_identities
        .into_iter()
        .filter(|identity| seen_identities.insert(identity.clone()))
        .filter_map(|identity| representatives_by_identity.get(&identity).cloned())
        .collect::<Vec<_>>();

    let mut symlink_aliases = psr4.symlink_aliases;
    symlink_aliases.extend(classmap.symlink_aliases);
    symlink_aliases.sort_by(|left, right| left.logical_path.cmp(&right.logical_path));
    symlink_aliases.dedup();

    if paths.is_empty() && symlink_aliases.is_empty() {
        None
    } else {
        Some(VendorPathResolution {
            paths,
            symlink_aliases,
            physical_files,
        })
    }
}

fn vendor_psr4_candidate_paths(
    fqn: &str,
    map: &VendorAutoloadMap,
    project_root: Option<&Path>,
    exclude_paths: &[PathBuf],
) -> Vec<PathBuf> {
    let normalized_fqn = fqn.trim_start_matches('\\');
    let mut candidates = Vec::new();
    for mapping in &map.psr4 {
        let Some(relative) = normalized_fqn.strip_prefix(mapping.prefix.as_str()) else {
            continue;
        };
        let relative_path = relative.replace('\\', "/") + ".php";
        for directory in &mapping.directories {
            let candidate = directory.join(&relative_path);
            if project_root.is_some_and(|root| path_is_excluded(&candidate, root, exclude_paths)) {
                continue;
            }
            push_unique_path(&mut candidates, candidate);
        }
    }
    candidates
}

fn ordered_physical_identities(
    paths: &[PathBuf],
    groups: &[crate::util::fs_walk::PhysicalFileGroup],
) -> Vec<crate::util::fs_walk::PhysicalIdentity> {
    let identities_by_path = groups
        .iter()
        .flat_map(|group| {
            group
                .paths
                .iter()
                .map(|path| (path.logical_path.clone(), group.identity.clone()))
        })
        .collect::<HashMap<_, _>>();
    paths
        .iter()
        .filter_map(|path| identities_by_path.get(path).cloned())
        .collect()
}

async fn resolve_vendor_paths_from_map_with_limits_blocking(
    fqn: &str,
    map: VendorAutoloadMap,
    traversal_limits: TraversalLimits,
    project_root: PathBuf,
    exclude_paths: Vec<PathBuf>,
) -> Option<VendorPathResolution> {
    let fqn = fqn.to_string();
    let path_label = project_root.display().to_string();
    match run_file_io_blocking("vendor path discovery", path_label.clone(), move || {
        resolve_vendor_paths_from_map_with_limits(
            &fqn,
            &map,
            traversal_limits,
            Some(&project_root),
            &exclude_paths,
        )
    })
    .await
    {
        Ok(resolution) => resolution,
        Err(message) => {
            tracing::warn!(
                "Vendor path discovery failed for {}: {}",
                path_label,
                message
            );
            None
        }
    }
}

pub(crate) fn vendor_autoload_file_paths_from_map(
    map: &VendorAutoloadMap,
    project_root: &Path,
    exclude_paths: &[PathBuf],
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for file_path in &map.files {
        push_vendor_autoload_file_and_static_includes(
            file_path,
            project_root,
            exclude_paths,
            &mut paths,
            0,
        );
    }
    paths
}

pub(in crate::server) async fn vendor_autoload_file_paths_from_map_blocking(
    map: VendorAutoloadMap,
    project_root: PathBuf,
    exclude_paths: Vec<PathBuf>,
) -> Vec<PathBuf> {
    let path_label = project_root.display().to_string();
    match run_file_io_blocking(
        "vendor autoload file discovery",
        path_label.clone(),
        move || vendor_autoload_file_paths_from_map(&map, &project_root, &exclude_paths),
    )
    .await
    {
        Ok(paths) => paths,
        Err(message) => {
            tracing::warn!(
                "Vendor autoload file discovery failed for {}: {}",
                path_label,
                message
            );
            Vec::new()
        }
    }
}

fn push_vendor_autoload_file_and_static_includes(
    file_path: &Path,
    project_root: &Path,
    exclude_paths: &[PathBuf],
    paths: &mut Vec<PathBuf>,
    depth: usize,
) {
    const MAX_STATIC_INCLUDE_DEPTH: usize = 8;

    if depth > MAX_STATIC_INCLUDE_DEPTH
        || !is_php_file_path(file_path)
        || path_is_excluded(file_path, project_root, exclude_paths)
    {
        return;
    }

    let already_seen = paths.iter().any(|path| path == file_path);
    push_unique_path(paths, file_path.to_path_buf());
    if already_seen || !file_path.is_file() {
        return;
    }

    for include_path in static_php_include_target_paths_for_file(file_path) {
        push_vendor_autoload_file_and_static_includes(
            &include_path,
            project_root,
            exclude_paths,
            paths,
            depth + 1,
        );
    }
}

fn static_php_include_target_paths_for_file(file_path: &Path) -> Vec<PathBuf> {
    let Ok(source) = std::fs::read_to_string(file_path) else {
        return Vec::new();
    };

    let mut parser = FileParser::new();
    parser.parse_full(&source);
    let Some(tree) = parser.tree() else {
        return Vec::new();
    };

    static_php_include_target_paths_for_source(&source, tree, file_path)
}

pub(crate) fn vendor_namespace_exists_from_map(fqn: &str, map: &VendorAutoloadMap) -> bool {
    let normalized_fqn = fqn.trim_matches('\\');
    if normalized_fqn.is_empty() {
        return false;
    }

    for mapping in &map.psr4 {
        let prefix = mapping.prefix.trim_matches('\\');
        if prefix.is_empty() {
            continue;
        }

        let relative = if normalized_fqn == prefix {
            ""
        } else if let Some(relative) = normalized_fqn.strip_prefix(prefix) {
            let Some(relative) = relative.strip_prefix('\\') else {
                continue;
            };
            relative
        } else {
            continue;
        };

        let relative_path = relative.replace('\\', "/");
        for directory in &mapping.directories {
            let namespace_dir = if relative_path.is_empty() {
                directory.clone()
            } else {
                directory.join(&relative_path)
            };
            if namespace_dir.is_dir() {
                return true;
            }
        }
    }

    false
}

fn classmap_candidate_paths_for_fqn(
    fqn: &str,
    map: &VendorAutoloadMap,
    traversal_limits: TraversalLimits,
    project_root: Option<&Path>,
    exclude_paths: &[PathBuf],
) -> VendorPathResolution {
    let class_basename = fqn.rsplit('\\').next().unwrap_or(fqn);
    let mut matching = Vec::new();
    let mut fallback = Vec::new();

    let deadline = file_io_walk_deadline();
    let outcome = walk_files(
        &map.classmap,
        traversal_limits,
        |path| project_root.is_some_and(|root| path_is_excluded(path, root, exclude_paths)),
        |_, _| true,
        is_php_file_path,
        || (Instant::now() >= deadline).then_some(TraversalStopReason::DeadlineExceeded),
    );
    if outcome.truncated() || outcome.stop_reason == Some(TraversalStopReason::DeadlineExceeded) {
        tracing::warn!(
            "Vendor classmap traversal was truncated after {} entries",
            outcome.stats.visited_entries
        );
    }
    for path in &outcome.files {
        push_classmap_candidate(path, class_basename, &mut matching, &mut fallback);
    }

    matching.extend(fallback);
    VendorPathResolution {
        paths: matching,
        symlink_aliases: outcome.symlink_aliases,
        physical_files: outcome.physical_files,
    }
}

fn push_classmap_candidate(
    path: &Path,
    class_basename: &str,
    matching: &mut Vec<PathBuf>,
    fallback: &mut Vec<PathBuf>,
) {
    if !is_php_file_path(path) {
        return;
    }

    let stem_matches = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.eq_ignore_ascii_case(class_basename));
    if stem_matches {
        push_unique_path(matching, path.to_path_buf());
    } else {
        push_unique_path(fallback, path.to_path_buf());
    }
}

fn is_php_file_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("php"))
}

pub(in crate::server) async fn cached_vendor_autoload_map_pinned(
    cache: &Arc<Mutex<VendorAutoloadCache>>,
    vendor_dir: &Path,
) -> Option<VendorAutoloadMap> {
    {
        let cache = cache.lock().await;
        if let Some(entry) = cache.by_vendor_dir.get(vendor_dir) {
            return Some(entry.map.clone());
        }
    }

    let Some(map) = parse_vendor_autoload_map_blocking(vendor_dir.to_path_buf()).await else {
        cache.lock().await.by_vendor_dir.remove(vendor_dir);
        return None;
    };

    #[cfg(test)]
    {
        let pause = cache.lock().await.before_insert_pause.take();
        if let Some(pause) = pause {
            let _ = pause.reached.send(());
            pause.release.notified().await;
        }
    }

    cache.lock().await.by_vendor_dir.insert(
        vendor_dir.to_path_buf(),
        VendorAutoloadCacheEntry { map: map.clone() },
    );
    Some(map)
}

#[cfg(test)]
pub(in crate::server) async fn cached_vendor_autoload_map_with_epoch(
    cache: &Arc<Mutex<VendorAutoloadCache>>,
    vendor_dir: &Path,
    load_epoch: &Arc<tokio::sync::RwLock<u64>>,
) -> Option<VendorAutoloadMap> {
    let epoch_guard = load_epoch.read().await;
    let map = cached_vendor_autoload_map_pinned(cache, vendor_dir).await;
    drop(epoch_guard);
    map
}

/// Try to resolve a FQN to file paths by scanning vendor/composer installed packages.
#[cfg(test)]
pub(in crate::server) fn resolve_vendor_paths(
    fqn: &str,
    vendor_dir: &Path,
) -> Option<Vec<PathBuf>> {
    let map = parse_vendor_autoload_map(vendor_dir)?;
    resolve_vendor_paths_from_map(fqn, &map)
}

impl PhpLspBackend {
    pub(in crate::server) fn vendor_lazy_index_context_from_request(
        &self,
        request: &WorkspaceRequestContext,
    ) -> VendorLazyIndexContext {
        if let Some(config) = request.workspace.as_ref() {
            let runtime = &config.runtime_config;
            return VendorLazyIndexContext {
                index: config.index.clone(),
                workspace_configs: vec![config.clone()],
                exclude_paths: runtime.exclude_paths.clone(),
                traversal_limits: runtime.traversal_limits,
                php_version: runtime.php_version,
                index_vendor: runtime.index_vendor,
                vendor_autoload_cache: self.vendor_autoload_cache.clone(),
                vendor_file_lru: config.vendor_file_lru.clone(),
                lazy_loads: self.vendor_lazy_loads.clone(),
                load_epoch: self.vendor_load_epoch.clone(),
                external_symlinks: Some(self.external_symlinks.clone()),
                runtime_generation: request.state.generation,
            };
        }
        VendorLazyIndexContext {
            index: self.index.clone(),
            workspace_configs: Vec::new(),
            exclude_paths: request.state.fallback.exclude_paths.clone(),
            traversal_limits: request.state.fallback.traversal_limits,
            php_version: request.state.fallback.php_version,
            index_vendor: false,
            vendor_autoload_cache: self.vendor_autoload_cache.clone(),
            vendor_file_lru: self.vendor_file_lru.clone(),
            lazy_loads: self.vendor_lazy_loads.clone(),
            load_epoch: self.vendor_load_epoch.clone(),
            external_symlinks: Some(self.external_symlinks.clone()),
            runtime_generation: request.state.generation,
        }
    }

    #[cfg(test)]
    pub(in crate::server) async fn vendor_lazy_index_context_for_uri(
        &self,
        uri_str: &str,
    ) -> VendorLazyIndexContext {
        let request = self.request_context_for_uri(uri_str).await;
        self.vendor_lazy_index_context_from_request(&request)
    }

    #[cfg(test)]
    pub(in crate::server) async fn vendor_lazy_index_context(&self) -> VendorLazyIndexContext {
        let state = self.runtime_state_snapshot().await;
        let workspace = state.configs.first().cloned();
        self.vendor_lazy_index_context_from_request(&WorkspaceRequestContext { state, workspace })
    }

    pub(in crate::server) async fn vendor_namespace_exists_lazy_in_request(
        &self,
        request: &WorkspaceRequestContext,
        fqn: &str,
    ) -> bool {
        let context = self.vendor_lazy_index_context_from_request(request);
        if !context.index_vendor {
            return false;
        }
        let _epoch_guard = context.load_epoch.read().await;
        for config in &context.workspace_configs {
            let vendor_dir = config.root.join("vendor");
            if !vendor_dir.is_dir() {
                continue;
            }
            if let Some(vendor_map) =
                cached_vendor_autoload_map_pinned(&context.vendor_autoload_cache, &vendor_dir).await
            {
                if vendor_namespace_exists_from_map(fqn, &vendor_map) {
                    return true;
                }
            }
        }
        false
    }

    #[cfg(test)]
    pub(in crate::server) async fn resolve_fqn_lazy_for_uri(
        &self,
        uri_str: &str,
        fqn: &str,
    ) -> Option<std::sync::Arc<php_lsp_types::SymbolInfo>> {
        let request = self.request_context_for_uri(uri_str).await;
        self.resolve_fqn_lazy_in_request(&request, fqn).await
    }

    pub(in crate::server) async fn resolve_fqn_lazy_in_request(
        &self,
        request: &WorkspaceRequestContext,
        fqn: &str,
    ) -> Option<std::sync::Arc<php_lsp_types::SymbolInfo>> {
        let index = request.index(&self.index);
        if fqn.contains("::") {
            let context = self.vendor_lazy_index_context_from_request(request);
            return resolve_member_stable_with_context(&context, fqn, None).await;
        }
        if let Some(sym) = index.resolve_fqn(fqn) {
            return Some(sym);
        }
        let class_fqn = fqn.rsplit_once("::").map_or(fqn, |(class, _)| class);
        let context = self.vendor_lazy_index_context_from_request(request);
        lazy_index_class_dependencies_with_context(&context, class_fqn).await;
        index.resolve_fqn(fqn)
    }

    #[cfg(test)]
    async fn resolve_fqn_lazy_matching_kinds(
        &self,
        fqn: &str,
        expected_kinds: &[php_lsp_types::PhpSymbolKind],
    ) -> Option<std::sync::Arc<php_lsp_types::SymbolInfo>> {
        if let Some(sym) = self.index.resolve_fqn_matching_kinds(fqn, expected_kinds) {
            return Some(sym);
        }

        let class_fqn = fqn.rsplit_once("::").map_or(fqn, |(class, _)| class);
        self.lazy_index_class_dependencies(class_fqn).await;

        self.index.resolve_fqn_matching_kinds(fqn, expected_kinds)
    }

    #[cfg(test)]
    async fn resolve_member_lazy_matching_kinds(
        &self,
        fqn: &str,
        expected_kinds: &[php_lsp_types::PhpSymbolKind],
    ) -> Option<std::sync::Arc<php_lsp_types::SymbolInfo>> {
        let context = self.vendor_lazy_index_context().await;
        resolve_member_stable_with_context(&context, fqn, Some(expected_kinds)).await
    }

    /// Lazy-index a single class FQN by finding its file via PSR-4/vendor mappings.
    /// Returns true only when the requested class is present in the index after loading.
    #[cfg(test)]
    pub(in crate::server) async fn lazy_index_class(&self, class_fqn: &str) -> bool {
        let context = self.vendor_lazy_index_context().await;
        lazy_index_class_with_context(&context, class_fqn).await
    }

    #[cfg(test)]
    pub(in crate::server) async fn lazy_index_class_dependencies(&self, class_fqn: &str) {
        let context = self.vendor_lazy_index_context().await;
        lazy_index_class_dependencies_with_context(&context, class_fqn).await;
    }

    pub(in crate::server) async fn lazy_index_class_dependencies_in_request(
        &self,
        request: &WorkspaceRequestContext,
        class_fqn: &str,
    ) {
        let context = self.vendor_lazy_index_context_from_request(request);
        lazy_index_class_dependencies_with_context(&context, class_fqn).await;
    }

    /// Resolve symbol from index with fallback for global built-ins.
    #[cfg(test)]
    pub(in crate::server) fn resolve_fqn_with_fallback(
        &self,
        fqn: &str,
        ref_kind: RefKind,
        allow_global_fallback: bool,
    ) -> Option<std::sync::Arc<php_lsp_types::SymbolInfo>> {
        resolve_fqn_with_ref_kind(&self.index, fqn, ref_kind, allow_global_fallback)
    }

    /// Fallback for `$this->prop->member()` when the declared property type
    /// doesn't have `member`. Scans the file for `$this->prop = <expr>`
    /// assignments, infers the RHS type, and tries to resolve the member on that
    /// type instead.
    pub(in crate::server) async fn try_property_assignment_type_fallback(
        &self,
        request: &WorkspaceRequestContext,
        uri_str: &str,
        prop_name: &str,
        member_name: &str,
    ) -> Option<GotoDefinitionResponse> {
        use php_lsp_parser::resolve::infer_property_type_from_assignments;
        let request_index = request.index(&self.index);

        let inferred_types = {
            let parser = match self.open_files.get(uri_str) {
                Some(p) => p,
                None => {
                    tracing::debug!("Property fallback: file not open: {}", uri_str);
                    return None;
                }
            };
            let tree = match parser.tree() {
                Some(t) => t,
                None => {
                    tracing::debug!("Property fallback: no tree for {}", uri_str);
                    return None;
                }
            };
            let source = parser.source();

            let file_symbols = request_index
                .file_symbols
                .get(uri_str)
                .map(|entry| entry.value().clone())
                .unwrap_or_default();

            let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
                resolve_member_type_from_index(&request_index, class_fqn, member_name)
            };

            let result = infer_property_type_from_assignments(
                tree,
                &source,
                prop_name,
                &file_symbols,
                Some(&resolver),
            );
            tracing::debug!(
                "Property fallback: infer_property_type_from_assignments('{}') = {:?}",
                prop_name,
                result
            );
            result
        };

        for assigned_type in &inferred_types {
            let fallback_fqn = format!("{}::{}", assigned_type, member_name);
            tracing::debug!(
                "Property assignment fallback: $this->{} assigned type '{}', trying '{}'",
                prop_name,
                assigned_type,
                fallback_fqn
            );

            if let Some(sym) = self
                .resolve_fqn_lazy_in_request(request, &fallback_fqn)
                .await
            {
                if let Some(location) = self
                    .location_for_symbol_selection_in_request(
                        request,
                        &sym,
                        "property assignment fallback target source read",
                    )
                    .await
                {
                    return Some(GotoDefinitionResponse::Scalar(location));
                }
            }
        }

        None
    }

    /// Resolve a symbol lazily, applying PHP's global function/constant
    /// fallback only when the original source name permits it.
    #[cfg(test)]
    pub(in crate::server) async fn resolve_fqn_lazy_with_fallback(
        &self,
        fqn: &str,
        ref_kind: RefKind,
        allow_global_fallback: bool,
    ) -> Option<std::sync::Arc<php_lsp_types::SymbolInfo>> {
        if let Some(expected_kinds) = member_kinds_for_ref_kind(ref_kind) {
            return self
                .resolve_member_lazy_matching_kinds(fqn, expected_kinds)
                .await;
        }

        let expected_kinds = top_level_kinds_for_ref_kind(ref_kind)?;
        if let Some(sym) = self
            .resolve_fqn_lazy_matching_kinds(fqn, expected_kinds)
            .await
        {
            return Some(sym);
        }
        if allow_global_fallback
            && (ref_kind == RefKind::FunctionCall || ref_kind == RefKind::GlobalConstant)
        {
            if let Some((_, short_name)) = fqn.rsplit_once('\\') {
                if let Some(sym) = self
                    .resolve_fqn_lazy_matching_kinds(short_name, expected_kinds)
                    .await
                {
                    return Some(sym);
                }
            }
        }
        None
    }

    pub(in crate::server) async fn resolve_fqn_lazy_with_fallback_in_request(
        &self,
        request: &WorkspaceRequestContext,
        fqn: &str,
        ref_kind: RefKind,
        allow_global_fallback: bool,
    ) -> Option<std::sync::Arc<php_lsp_types::SymbolInfo>> {
        let index = request.index(&self.index);
        if let Some(expected_kinds) = member_kinds_for_ref_kind(ref_kind) {
            let context = self.vendor_lazy_index_context_from_request(request);
            return resolve_member_stable_with_context(&context, fqn, Some(expected_kinds)).await;
        }

        let expected_kinds = top_level_kinds_for_ref_kind(ref_kind)?;
        if let Some(sym) = index.resolve_fqn_matching_kinds(fqn, expected_kinds) {
            return Some(sym);
        }

        let class_fqn = fqn.rsplit_once("::").map_or(fqn, |(class, _)| class);
        let context = self.vendor_lazy_index_context_from_request(request);
        lazy_index_class_dependencies_with_context(&context, class_fqn).await;
        let resolve = |candidate: &str| index.resolve_fqn_matching_kinds(candidate, expected_kinds);
        if let Some(sym) = resolve(fqn) {
            return Some(sym);
        }
        if allow_global_fallback
            && matches!(ref_kind, RefKind::FunctionCall | RefKind::GlobalConstant)
        {
            if let Some((_, short_name)) = fqn.rsplit_once('\\') {
                return resolve(short_name);
            }
        }
        None
    }
}

#[cfg(all(test, unix))]
#[path = "vendor_symlink_tests.rs"]
mod symlink_resolution_tests;
