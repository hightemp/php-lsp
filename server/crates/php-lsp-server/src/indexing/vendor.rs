//! Vendor indexing helpers.

use super::super::*;

#[derive(Debug, Clone)]
pub(crate) struct VendorAutoloadCacheEntry {
    pub(crate) map: VendorAutoloadMap,
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
                map.files.push(pkg_dir.join(file_path));
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
    let mut paths = Vec::new();
    for mapping in &map.psr4 {
        let Some(relative) = fqn.strip_prefix(mapping.prefix.as_str()) else {
            continue;
        };
        let relative_path = relative.replace('\\', "/") + ".php";
        for directory in &mapping.directories {
            push_unique_path(&mut paths, directory.join(&relative_path));
        }
    }

    if paths.is_empty() {
        None
    } else {
        Some(paths)
    }
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

    /// Lazy-index a single class FQN by finding its file via PSR-4/vendor mappings.
    /// Returns true only when the requested class is present in the index after loading.
    pub(in crate::server) async fn lazy_index_class(&self, class_fqn: &str) -> bool {
        let requested_class_fqn = class_fqn.trim_start_matches('\\');
        // Skip if already in the index
        if self.index.types.contains_key(requested_class_fqn) {
            return false;
        }

        let index_vendor = *self.index_vendor.lock().await;
        let mut configs = self.workspace_configs.lock().await.clone();
        let exclude_paths = self.exclude_paths.lock().await.clone();
        let php_version = *self.php_version.lock().await;
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
            let mut all_paths = config
                .namespace_map
                .as_ref()
                .map(|ns_map| ns_map.resolve_class_to_paths(requested_class_fqn))
                .unwrap_or_default();

            let vendor_dir = config.root.join("vendor");
            if index_vendor && vendor_dir.is_dir() && all_paths.is_empty() {
                if let Some(vendor_map) =
                    cached_vendor_autoload_map(&self.vendor_autoload_cache, &vendor_dir).await
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

                if path_is_excluded(&abs, &config.root, &exclude_paths) {
                    continue;
                }

                let is_vendor_file = abs.starts_with(config.root.join("vendor"));
                let vendor_cache_config = is_vendor_file
                    .then(|| vendor_index_cache_config(&config.root, php_version, &exclude_paths));
                if let Some(cache_config) = vendor_cache_config.as_ref() {
                    if load_cached_vendor_file_blocking(
                        self.index.clone(),
                        config.root.clone(),
                        abs.clone(),
                        cache_config.clone(),
                    )
                    .await
                    {
                        self.touch_vendor_file_lru(&abs).await;
                        tracing::debug!("Lazy-indexed vendor file from cache: {}", abs.display());
                        if self.index.types.contains_key(requested_class_fqn) {
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
                    self.index.clone(),
                    abs.clone(),
                    "lazy PHP file index",
                )
                .await
                {
                    if is_vendor_file {
                        self.touch_vendor_file_lru(&abs).await;
                    }
                    tracing::debug!("Lazy-indexed file: {}", abs.display());
                    if self.index.types.contains_key(requested_class_fqn) {
                        if is_vendor_file {
                            if let Some(cache_config) = vendor_cache_config {
                                save_vendor_index_cache_blocking(
                                    self.index.clone(),
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

    /// Recursively lazy-index parent classes (extends + implements) up to a depth limit.
    pub(in crate::server) fn lazy_index_parents<'a>(
        &'a self,
        class_fqn: &'a str,
        depth: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            const MAX_DEPTH: usize = 10;
            if depth >= MAX_DEPTH {
                return;
            }

            // Get the class from the index to read its extends/implements
            let parent_fqns: Vec<String> = if let Some(sym) = self.index.types.get(class_fqn) {
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
                // Lazy-index the parent class file
                self.lazy_index_class(&parent_fqn).await;
                // Recurse into the parent's parents
                self.lazy_index_parents(&parent_fqn, depth + 1).await;
            }
        })
    }

    /// Lazy-index simple class return types from already-indexed members.
    pub(in crate::server) async fn lazy_index_member_return_types(&self, class_fqn: &str) {
        let return_fqns: Vec<String> = self
            .index
            .get_members(class_fqn)
            .into_iter()
            .filter_map(|sym| {
                let owner_fqn = sym.parent_fqn.as_deref().unwrap_or(class_fqn);
                symbol_return_type_fqn(&self.index, owner_fqn, &sym)
            })
            .filter(|fqn| fqn.contains('\\') && !self.index.types.contains_key(fqn.as_str()))
            .collect();

        for return_fqn in return_fqns {
            self.lazy_index_class(&return_fqn).await;
            self.lazy_index_parents(&return_fqn, 0).await;
        }
    }

    pub(in crate::server) async fn lazy_index_class_dependencies(&self, class_fqn: &str) {
        self.lazy_index_class(class_fqn).await;
        self.lazy_index_parents(class_fqn, 0).await;
        self.lazy_index_member_return_types(class_fqn).await;
    }

    /// Resolve symbol from index with fallback for global built-ins.
    pub(in crate::server) fn resolve_fqn_with_fallback(
        &self,
        fqn: &str,
        ref_kind: RefKind,
    ) -> Option<std::sync::Arc<php_lsp_types::SymbolInfo>> {
        if let Some(sym) = self.index.resolve_fqn(fqn) {
            return Some(sym);
        }
        if ref_kind == RefKind::FunctionCall || ref_kind == RefKind::GlobalConstant {
            if let Some((_, short_name)) = fqn.rsplit_once('\\') {
                if let Some(sym) = self.index.resolve_fqn(short_name) {
                    return Some(sym);
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
        if let Some(sym) = self.resolve_fqn_lazy(fqn).await {
            return Some(sym);
        }
        if ref_kind == RefKind::FunctionCall || ref_kind == RefKind::GlobalConstant {
            if let Some((_, short_name)) = fqn.rsplit_once('\\') {
                if let Some(sym) = self.resolve_fqn_lazy(short_name).await {
                    return Some(sym);
                }
            }
        }
        None
    }
}
