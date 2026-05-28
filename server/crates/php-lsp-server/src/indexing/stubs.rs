//! Stub indexing helpers.

use super::super::*;

pub(crate) fn remove_stub_symbols(index: &WorkspaceIndex) {
    let stub_uris: Vec<String> = index
        .file_symbols
        .iter()
        .filter(|entry| entry.key().starts_with("phpstub://"))
        .map(|entry| entry.key().clone())
        .collect();

    for uri in stub_uris {
        index.remove_file(&uri);
    }
}

pub(crate) fn candidate_stubs_paths(
    root: &Path,
    client_stubs_path: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut candidate_paths: Vec<PathBuf> = Vec::new();

    if let Some(path) = client_stubs_path {
        candidate_paths.push(path);
    }

    candidate_paths.push(root.join("server/data/stubs"));

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidate_paths.push(dir.join("data/stubs"));
            candidate_paths.push(
                dir.join("../stubs")
                    .canonicalize()
                    .unwrap_or_else(|_| dir.join("../stubs")),
            );
        }
    }

    candidate_paths.push(PathBuf::from("/usr/share/php-lsp/stubs"));
    candidate_paths
}

pub(crate) fn load_configured_stubs(
    index: &WorkspaceIndex,
    root: &Path,
    client_stubs_path: Option<PathBuf>,
    stub_extensions: Vec<String>,
    php_version: PhpVersion,
    clear_existing: bool,
) -> usize {
    if clear_existing {
        remove_stub_symbols(index);
    }

    for stubs_path in candidate_stubs_paths(root, client_stubs_path) {
        if stubs_path.is_dir() {
            tracing::info!("Loading phpstorm-stubs from {}", stubs_path.display());
            let extensions = effective_stub_extensions(&stub_extensions);
            let cache_sources = collect_stub_cache_sources(&stubs_path, &extensions);
            let cache_path = cache::cache_file_path_for_namespace(root, CacheNamespace::Stubs);
            let cache_config = stubs_index_cache_config(&stubs_path, php_version, &stub_extensions);
            let stub_php_version = stubs::StubPhpVersion {
                major: php_version.major,
                minor: php_version.minor,
            };
            let cache_report = cache::load_valid_cached_sources(
                index,
                &cache_path,
                &stubs_path,
                &cache_sources,
                &cache_config,
            );
            if let Some(reason) = cache_report.miss_reason.as_deref() {
                tracing::debug!("Stubs index cache miss: {}", reason);
            }

            let mut parsed = 0;
            for source in &cache_report.parse_sources {
                let Some(ext_name) = source.relative_path.split('/').next() else {
                    continue;
                };
                if stubs::load_stub_file_for_php_version(
                    index,
                    ext_name,
                    &source.path,
                    Some(stub_php_version),
                )
                .is_some()
                {
                    parsed += 1;
                }
            }

            let cache_to_save =
                cache::build_cache_from_sources(index, &stubs_path, &cache_sources, &cache_config);
            if let Err(e) = cache::save_cache_atomic(&cache_path, &cache_to_save) {
                tracing::warn!(
                    "Failed to save stubs index cache at {}: {}",
                    cache_path.display(),
                    e
                );
            }

            let loaded = cache_report.loaded_files + parsed;
            tracing::info!(
                "Loaded {} stub files ({} from cache, {} parsed)",
                loaded,
                cache_report.loaded_files,
                parsed
            );
            return loaded;
        }
    }

    tracing::warn!("phpstorm-stubs not found, built-in completions will be limited");
    0
}

pub(crate) fn collect_stub_cache_sources(
    stubs_path: &Path,
    extensions: &[String],
) -> Vec<CacheSourceFile> {
    let mut sources = Vec::new();
    for extension in extensions {
        for path in stubs::collect_extension_stub_files(stubs_path, extension) {
            let file_name = path
                .file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_default();
            sources.push(CacheSourceFile::new(
                path.clone(),
                stubs::stub_file_uri(extension, &path),
                format!("{}/{}", extension, file_name),
            ));
        }
    }
    sources.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    sources
}
