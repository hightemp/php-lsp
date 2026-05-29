//! Stub indexing helpers.

use super::super::*;

const REQUIRED_STUB_FILES: &[&str] = &[
    "PhpStormStubsMap.php",
    "Core/Core.php",
    "SPL/SPL.php",
    "standard/basic.php",
    "standard/standard_0.php",
    "date/date.php",
    "json/json.php",
    "pcre/pcre.php",
    "Reflection/Reflection.php",
    "SimpleXML/SimpleXML.php",
    "soap/soap.php",
];

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
    stub_extensions: Option<Vec<String>>,
    php_version: PhpVersion,
    clear_existing: bool,
) -> usize {
    if clear_existing {
        remove_stub_symbols(index);
    }

    let extensions = effective_stub_extensions(stub_extensions.as_deref());
    if extensions.is_empty() {
        tracing::info!("phpstorm-stubs disabled by config: stub extensions list is empty");
        return 0;
    }

    let mut missing_paths = Vec::new();
    let mut unusable_paths = Vec::new();

    for stubs_path in candidate_stubs_paths(root, client_stubs_path) {
        if !stubs_path.exists() {
            missing_paths.push(stubs_path.display().to_string());
            continue;
        }
        if !stubs_path.is_dir() {
            unusable_paths.push(format!("{} is not a directory", stubs_path.display()));
            continue;
        }
        if let Some(reason) = unusable_stubs_path_reason(&stubs_path) {
            unusable_paths.push(format!("{}: {}", stubs_path.display(), reason));
            continue;
        }

        tracing::info!("Loading phpstorm-stubs from {}", stubs_path.display());
        let cache_sources = collect_stub_cache_sources(&stubs_path, &extensions);
        let cache_path = cache::cache_file_path_for_namespace(root, CacheNamespace::Stubs);
        let cache_config =
            stubs_index_cache_config(&stubs_path, php_version, stub_extensions.as_deref());
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

    if !unusable_paths.is_empty() {
        tracing::warn!(
            "phpstorm-stubs path exists but is missing required files or is uninitialized: {}",
            unusable_paths.join("; ")
        );
    }
    if !missing_paths.is_empty() {
        tracing::warn!(
            "phpstorm-stubs not found in candidate paths: {}",
            missing_paths.join(", ")
        );
    }
    tracing::warn!("No usable phpstorm-stubs path found; built-in completions will be limited");
    0
}

fn unusable_stubs_path_reason(stubs_path: &Path) -> Option<String> {
    let php_file_count = count_php_stub_files(stubs_path);
    if php_file_count == 0 {
        return Some("contains no PHP stub files".to_string());
    }

    let missing: Vec<&str> = REQUIRED_STUB_FILES
        .iter()
        .copied()
        .filter(|relative| !stubs_path.join(relative).is_file())
        .collect();
    if missing.is_empty() {
        return None;
    }

    Some(format!(
        "missing required stub file(s): {}",
        missing.join(", ")
    ))
}

fn count_php_stub_files(stubs_path: &Path) -> usize {
    let mut count = 0;
    let mut pending = vec![stubs_path.to_path_buf()];

    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("php") {
                count += 1;
            }
        }
    }

    count
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
