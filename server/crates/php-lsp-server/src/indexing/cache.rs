//! Server-side index cache orchestration helpers.
//!
//! This module translates runtime server configuration, stub paths, and vendor
//! metadata into `php_lsp_index::cache::IndexCacheConfig` values and stable
//! hash inputs. The persisted cache schema, bincode serialization, load/save
//! logic, and per-file validation live in `php-lsp-index/src/cache.rs`.

use super::super::*;
use std::time::UNIX_EPOCH;

pub(crate) fn workspace_index_cache_config(
    root: Option<&Path>,
    php_version: PhpVersion,
    include_paths: &[PathBuf],
    exclude_paths: &[PathBuf],
    stub_extensions: Option<&[String]>,
    client_stubs_path: Option<&Path>,
) -> IndexCacheConfig {
    let root = root.unwrap_or_else(|| Path::new(""));
    IndexCacheConfig {
        namespace: CacheNamespace::Workspace,
        php_lsp_version: env!("CARGO_PKG_VERSION").to_string(),
        php_version: php_version_label(php_version),
        include_paths: include_paths
            .iter()
            .map(|path| cache_path_label(path))
            .collect(),
        exclude_paths: exclude_paths
            .iter()
            .map(|path| cache_path_label(path))
            .collect(),
        stub_extensions: effective_stub_extensions(stub_extensions),
        stubs_hash: stubs_cache_hash(root, client_stubs_path, stub_extensions),
    }
}

pub(crate) fn stubs_index_cache_config(
    stubs_path: &Path,
    php_version: PhpVersion,
    stub_extensions: Option<&[String]>,
) -> IndexCacheConfig {
    IndexCacheConfig {
        namespace: CacheNamespace::Stubs,
        php_lsp_version: env!("CARGO_PKG_VERSION").to_string(),
        php_version: php_version_label(php_version),
        include_paths: vec![cache_path_label(stubs_path)],
        exclude_paths: Vec::new(),
        stub_extensions: effective_stub_extensions(stub_extensions),
        stubs_hash: stubs_cache_hash_for_path(stubs_path, stub_extensions),
    }
}

pub(crate) fn vendor_index_cache_config(
    root: &Path,
    php_version: PhpVersion,
    exclude_paths: &[PathBuf],
) -> IndexCacheConfig {
    IndexCacheConfig {
        namespace: CacheNamespace::Vendor,
        php_lsp_version: env!("CARGO_PKG_VERSION").to_string(),
        php_version: php_version_label(php_version),
        include_paths: vec![cache_path_label(&root.join("vendor"))],
        exclude_paths: exclude_paths
            .iter()
            .map(|path| cache_path_label(path))
            .collect(),
        stub_extensions: Vec::new(),
        stubs_hash: vendor_cache_hash(root),
    }
}

pub(crate) fn cache_path_label(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

pub(crate) fn effective_stub_extensions(stub_extensions: Option<&[String]>) -> Vec<String> {
    match stub_extensions {
        Some(extensions) => extensions.to_vec(),
        None => stubs::DEFAULT_EXTENSIONS
            .iter()
            .map(|ext| (*ext).to_string())
            .collect(),
    }
}

pub(crate) fn stubs_cache_hash(
    root: &Path,
    client_stubs_path: Option<&Path>,
    stub_extensions: Option<&[String]>,
) -> u64 {
    let extensions = effective_stub_extensions(stub_extensions);
    if extensions.is_empty() {
        return cache::stable_hash_strings(["stubs-cache-v1", "disabled"]);
    }

    let client_stubs_path = client_stubs_path.map(Path::to_path_buf);
    if let Some(stubs_root) = candidate_stubs_paths(root, client_stubs_path)
        .into_iter()
        .find(|path| path.is_dir())
    {
        return stubs_cache_hash_for_path(&stubs_root, stub_extensions);
    }

    let mut parts = vec!["stubs-cache-v1".to_string(), "root=missing".to_string()];
    for extension in extensions {
        parts.push(format!("extension={}:unknown", extension));
    }
    cache::stable_hash_strings(parts.iter().map(String::as_str))
}

pub(crate) fn stubs_cache_hash_for_path(
    stubs_root: &Path,
    stub_extensions: Option<&[String]>,
) -> u64 {
    let extensions = effective_stub_extensions(stub_extensions);
    if extensions.is_empty() {
        return cache::stable_hash_strings(["stubs-cache-v1", "disabled"]);
    }

    let mut parts = vec![
        "stubs-cache-v1".to_string(),
        format!("root={}", cache_path_label(stubs_root)),
    ];

    for file_name in ["composer.lock", "composer.json", "PhpStormStubsMap.php"] {
        push_metadata_hash_part(&mut parts, "file", file_name, &stubs_root.join(file_name));
    }

    for extension in extensions {
        let path = stubs_root.join(&extension);
        if path.exists() {
            push_metadata_hash_part(&mut parts, "extension", &extension, &path);
        } else {
            parts.push(format!("extension={}:missing", extension));
        }
    }

    cache::stable_hash_strings(parts.iter().map(String::as_str))
}

pub(crate) fn vendor_cache_hash(root: &Path) -> u64 {
    let mut parts = vec![
        "vendor-cache-v1".to_string(),
        format!("root={}", cache_path_label(root)),
    ];
    for relative in [
        "composer.json",
        "composer.lock",
        "vendor/composer/installed.json",
        "vendor/composer/autoload_psr4.php",
    ] {
        push_metadata_hash_part(&mut parts, "file", relative, &root.join(relative));
    }
    cache::stable_hash_strings(parts.iter().map(String::as_str))
}

pub(crate) fn push_metadata_hash_part(
    parts: &mut Vec<String>,
    kind: &str,
    label: &str,
    path: &Path,
) {
    match std::fs::metadata(path) {
        Ok(metadata) => {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| format!("{}.{:09}", duration.as_secs(), duration.subsec_nanos()))
                .unwrap_or_else(|| "unknown".to_string());
            parts.push(format!(
                "{}={}:{}:{}",
                kind,
                label,
                metadata.len(),
                modified
            ));
        }
        Err(_) => parts.push(format!("{}={}:missing", kind, label)),
    }
}
