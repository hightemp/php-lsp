//! Vendor indexing helpers.

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

fn member_kinds_for_ref_kind(ref_kind: RefKind) -> Option<&'static [php_lsp_types::PhpSymbolKind]> {
    match ref_kind {
        RefKind::Constructor | RefKind::MethodCall => Some(METHOD_MEMBER_KINDS),
        RefKind::PropertyAccess | RefKind::StaticPropertyAccess => Some(PROPERTY_MEMBER_KINDS),
        RefKind::ClassConstant => Some(CLASS_CONSTANT_MEMBER_KINDS),
        _ => None,
    }
}

#[derive(Debug, Default)]
pub(crate) struct VendorAutoloadCache {
    pub(crate) by_vendor_dir: HashMap<PathBuf, VendorAutoloadCacheEntry>,
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
    pub(in crate::server) php_version: PhpVersion,
    pub(in crate::server) index_vendor: bool,
    pub(in crate::server) vendor_autoload_cache: Arc<Mutex<VendorAutoloadCache>>,
    pub(in crate::server) vendor_file_lru: Arc<Mutex<VendorFileLru>>,
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

pub(in crate::server) async fn lazy_index_class_with_context(
    context: &VendorLazyIndexContext,
    class_fqn: &str,
) -> bool {
    let requested_class_fqn = class_fqn.trim_start_matches('\\');
    if context.index.types.contains_key(requested_class_fqn) {
        return false;
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
                cached_vendor_autoload_map(&context.vendor_autoload_cache, &vendor_dir).await
            {
                if let Some(vendor_paths) =
                    resolve_vendor_paths_from_map(requested_class_fqn, &vendor_map)
                {
                    all_paths.extend(vendor_paths);
                }
            }
        }

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
                vendor_index_cache_config(&config.root, context.php_version, &context.exclude_paths)
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
                    if context.index.types.contains_key(requested_class_fqn) {
                        return true;
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
                if context.index.types.contains_key(requested_class_fqn) {
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
                    return true;
                }
                tracing::debug!(
                    "Lazy-indexed file {} did not contain requested class {}",
                    abs.display(),
                    requested_class_fqn
                );
            }
        }
    }

    false
}

pub(in crate::server) fn lazy_index_parents_with_context<'a>(
    context: &'a VendorLazyIndexContext,
    class_fqn: &'a str,
    depth: usize,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        const MAX_DEPTH: usize = 10;
        if depth >= MAX_DEPTH {
            return;
        }

        let parent_fqns: Vec<String> = if let Some(sym) = context.index.types.get(class_fqn) {
            sym.extends
                .iter()
                .chain(sym.implements.iter())
                .chain(sym.traits.iter())
                .cloned()
                .collect()
        } else {
            return;
        };

        for parent_fqn in parent_fqns {
            lazy_index_class_with_context(context, &parent_fqn).await;
            lazy_index_parents_with_context(context, &parent_fqn, depth + 1).await;
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
        .filter(|fqn| fqn.contains('\\') && !context.index.types.contains_key(fqn.as_str()))
        .collect();

    for return_fqn in return_fqns {
        lazy_index_class_with_context(context, &return_fqn).await;
        lazy_index_parents_with_context(context, &return_fqn, 0).await;
    }
}

pub(in crate::server) async fn lazy_index_class_dependencies_with_context(
    context: &VendorLazyIndexContext,
    class_fqn: &str,
) {
    lazy_index_class_with_context(context, class_fqn).await;
    lazy_index_parents_with_context(context, class_fqn, 0).await;
    lazy_index_member_return_types_with_context(context, class_fqn).await;
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
    let normalized_fqn = fqn.trim_start_matches('\\');
    let mut paths = Vec::new();
    for mapping in &map.psr4 {
        let Some(relative) = normalized_fqn.strip_prefix(mapping.prefix.as_str()) else {
            continue;
        };
        let relative_path = relative.replace('\\', "/") + ".php";
        for directory in &mapping.directories {
            push_unique_path(&mut paths, directory.join(&relative_path));
        }
    }
    for path in classmap_candidate_paths_for_fqn(normalized_fqn, map) {
        push_unique_path(&mut paths, path);
    }

    if paths.is_empty() {
        None
    } else {
        Some(paths)
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

fn classmap_candidate_paths_for_fqn(fqn: &str, map: &VendorAutoloadMap) -> Vec<PathBuf> {
    let class_basename = fqn.rsplit('\\').next().unwrap_or(fqn);
    let mut matching = Vec::new();
    let mut fallback = Vec::new();

    for path in &map.classmap {
        collect_classmap_php_files(path, class_basename, &mut matching, &mut fallback);
    }

    matching.extend(fallback);
    matching
}

fn collect_classmap_php_files(
    path: &Path,
    class_basename: &str,
    matching: &mut Vec<PathBuf>,
    fallback: &mut Vec<PathBuf>,
) {
    if path.is_file() {
        push_classmap_candidate(path, class_basename, matching, fallback);
        return;
    }

    if !path.is_dir() {
        return;
    }

    let mut entries = match std::fs::read_dir(path) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .collect::<Vec<_>>(),
        Err(_) => return,
    };
    entries.sort();

    for entry in entries {
        collect_classmap_php_files(&entry, class_basename, matching, fallback);
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

pub(in crate::server) async fn cached_vendor_autoload_map(
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

    cache.lock().await.by_vendor_dir.insert(
        vendor_dir.to_path_buf(),
        VendorAutoloadCacheEntry { map: map.clone() },
    );
    Some(map)
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
    pub(in crate::server) async fn vendor_lazy_index_context(&self) -> VendorLazyIndexContext {
        let mut workspace_configs = self.workspace_configs.lock().await.clone();
        let exclude_paths = self.exclude_paths.lock().await.clone();
        let php_version = *self.php_version.lock().await;
        let index_vendor = *self.index_vendor.lock().await;
        if workspace_configs.is_empty() {
            let root = self.workspace_root.lock().await.clone();
            let namespace_map = self.namespace_map.lock().await.clone();
            if let Some(root) = root {
                workspace_configs.push(WorkspaceRootConfig {
                    root,
                    namespace_map,
                });
            }
        }

        VendorLazyIndexContext {
            index: self.index.clone(),
            workspace_configs,
            exclude_paths,
            php_version,
            index_vendor,
            vendor_autoload_cache: self.vendor_autoload_cache.clone(),
            vendor_file_lru: self.vendor_file_lru.clone(),
        }
    }

    pub(in crate::server) async fn vendor_namespace_exists_lazy(&self, fqn: &str) -> bool {
        let index_vendor = *self.index_vendor.lock().await;
        if !index_vendor {
            return false;
        }

        let mut configs = self.workspace_configs.lock().await.clone();
        if configs.is_empty() {
            let root = self.workspace_root.lock().await.clone();
            let namespace_map = self.namespace_map.lock().await.clone();
            if let Some(root) = root {
                configs.push(WorkspaceRootConfig {
                    root,
                    namespace_map,
                });
            }
        }

        for config in configs {
            let vendor_dir = config.root.join("vendor");
            if !vendor_dir.is_dir() {
                continue;
            }
            if let Some(vendor_map) =
                cached_vendor_autoload_map(&self.vendor_autoload_cache, &vendor_dir).await
            {
                if vendor_namespace_exists_from_map(fqn, &vendor_map) {
                    return true;
                }
            }
        }

        false
    }

    pub(in crate::server) async fn resolve_fqn_lazy(
        &self,
        fqn: &str,
    ) -> Option<std::sync::Arc<php_lsp_types::SymbolInfo>> {
        // Try direct lookup first
        if let Some(sym) = self.index.resolve_fqn(fqn) {
            return Some(sym);
        }

        // For member FQNs like "Class::method", extract the class part
        // so PSR-4 resolution works (PSR-4 maps class names, not members).
        let class_fqn = if let Some((cls, _member)) = fqn.rsplit_once("::") {
            cls
        } else {
            fqn
        };

        self.lazy_index_class_dependencies(class_fqn).await;

        // Retry resolution with the full FQN
        if let Some(sym) = self.index.resolve_fqn(fqn) {
            return Some(sym);
        }

        None
    }

    async fn resolve_member_lazy_matching_kinds(
        &self,
        fqn: &str,
        expected_kinds: &[php_lsp_types::PhpSymbolKind],
    ) -> Option<std::sync::Arc<php_lsp_types::SymbolInfo>> {
        if let Some(sym) = self
            .index
            .resolve_member_matching_kinds(fqn, expected_kinds)
        {
            return Some(sym);
        }

        let (class_fqn, _) = fqn.rsplit_once("::")?;
        self.lazy_index_class_dependencies(class_fqn).await;

        self.index
            .resolve_member_matching_kinds(fqn, expected_kinds)
    }

    /// Lazy-index a single class FQN by finding its file via PSR-4/vendor mappings.
    /// Returns true only when the requested class is present in the index after loading.
    pub(in crate::server) async fn lazy_index_class(&self, class_fqn: &str) -> bool {
        let context = self.vendor_lazy_index_context().await;
        lazy_index_class_with_context(&context, class_fqn).await
    }

    pub(in crate::server) async fn lazy_index_class_dependencies(&self, class_fqn: &str) {
        let context = self.vendor_lazy_index_context().await;
        lazy_index_class_dependencies_with_context(&context, class_fqn).await;
    }

    /// Resolve symbol from index with fallback for global built-ins.
    pub(in crate::server) fn resolve_fqn_with_fallback(
        &self,
        fqn: &str,
        ref_kind: RefKind,
    ) -> Option<std::sync::Arc<php_lsp_types::SymbolInfo>> {
        if let Some(expected_kinds) = member_kinds_for_ref_kind(ref_kind) {
            return self
                .index
                .resolve_member_matching_kinds(fqn, expected_kinds);
        }

        if let Some(sym) = self.index.resolve_fqn(fqn) {
            if symbol_matches_ref_kind_for_lazy_resolution(&sym, ref_kind) {
                return Some(sym);
            }
        }
        if ref_kind == RefKind::FunctionCall || ref_kind == RefKind::GlobalConstant {
            if let Some((_, short_name)) = fqn.rsplit_once('\\') {
                if let Some(sym) = self.index.resolve_fqn(short_name) {
                    if symbol_matches_ref_kind_for_lazy_resolution(&sym, ref_kind) {
                        return Some(sym);
                    }
                }
            }
        }
        None
    }

    /// Fallback for `$this->prop->member()` when the declared property type
    /// doesn't have `member`. Scans the file for `$this->prop = <expr>`
    /// assignments, infers the RHS type, and tries to resolve the member on that
    /// type instead.
    pub(in crate::server) async fn try_property_assignment_type_fallback(
        &self,
        uri_str: &str,
        prop_name: &str,
        member_name: &str,
    ) -> Option<GotoDefinitionResponse> {
        use php_lsp_parser::resolve::infer_property_type_from_assignments;

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

            let file_symbols = self
                .index
                .file_symbols
                .get(uri_str)
                .map(|entry| entry.value().clone())
                .unwrap_or_default();

            let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
                self.resolve_member_type(class_fqn, member_name)
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

            if let Some(sym) = self.resolve_fqn_lazy(&fallback_fqn).await {
                if let Some(location) = self
                    .location_for_symbol_selection(
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

    /// Resolve symbol lazily with fallback for global built-ins.
    pub(in crate::server) async fn resolve_fqn_lazy_with_fallback(
        &self,
        fqn: &str,
        ref_kind: RefKind,
    ) -> Option<std::sync::Arc<php_lsp_types::SymbolInfo>> {
        if let Some(expected_kinds) = member_kinds_for_ref_kind(ref_kind) {
            return self
                .resolve_member_lazy_matching_kinds(fqn, expected_kinds)
                .await;
        }

        if let Some(sym) = self.resolve_fqn_lazy(fqn).await {
            if symbol_matches_ref_kind_for_lazy_resolution(&sym, ref_kind) {
                return Some(sym);
            }
        }
        if ref_kind == RefKind::FunctionCall || ref_kind == RefKind::GlobalConstant {
            if let Some((_, short_name)) = fqn.rsplit_once('\\') {
                if let Some(sym) = self.resolve_fqn_lazy(short_name).await {
                    if symbol_matches_ref_kind_for_lazy_resolution(&sym, ref_kind) {
                        return Some(sym);
                    }
                }
            }
        }
        None
    }
}

fn symbol_matches_ref_kind_for_lazy_resolution(
    sym: &php_lsp_types::SymbolInfo,
    ref_kind: RefKind,
) -> bool {
    matches!(
        (ref_kind, sym.kind),
        (RefKind::ClassName, php_lsp_types::PhpSymbolKind::Class)
            | (RefKind::ClassName, php_lsp_types::PhpSymbolKind::Interface)
            | (RefKind::ClassName, php_lsp_types::PhpSymbolKind::Trait)
            | (RefKind::ClassName, php_lsp_types::PhpSymbolKind::Enum)
            | (RefKind::Constructor, php_lsp_types::PhpSymbolKind::Method)
            | (
                RefKind::FunctionCall,
                php_lsp_types::PhpSymbolKind::Function
            )
            | (RefKind::MethodCall, php_lsp_types::PhpSymbolKind::Method)
            | (
                RefKind::PropertyAccess,
                php_lsp_types::PhpSymbolKind::Property
            )
            | (
                RefKind::StaticPropertyAccess,
                php_lsp_types::PhpSymbolKind::Property
            )
            | (
                RefKind::ClassConstant,
                php_lsp_types::PhpSymbolKind::ClassConstant
            )
            | (
                RefKind::ClassConstant,
                php_lsp_types::PhpSymbolKind::EnumCase
            )
            | (
                RefKind::GlobalConstant,
                php_lsp_types::PhpSymbolKind::GlobalConstant
            )
            | (
                RefKind::NamespaceName,
                php_lsp_types::PhpSymbolKind::Namespace
            )
    )
}
