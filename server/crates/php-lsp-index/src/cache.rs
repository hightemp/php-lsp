//! Disk cache storage for workspace index snapshots.
//!
//! This module owns the cache schema version, serialized snapshot data model,
//! namespace path layout, atomic load/save operations, and per-file freshness
//! validation. Server runtime code should build `IndexCacheConfig` inputs in
//! `php-lsp-server/src/indexing/cache.rs` and call this module for persistence.

use crate::workspace::WorkspaceIndex;
use php_lsp_types::uri::{path_to_uri, FileUriError};
use php_lsp_types::{FileSymbols, PhpSymbolKind, SymbolInfo, SymbolReference};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Serialized `IndexCache` schema version.
///
/// `bincode` is not self-describing. Bump this whenever `IndexCache`,
/// `CachedFile`, `CachedFileMetadata`, `CachedTopLevelSymbols`, or nested
/// serialized `php-lsp-types` fields change in a way that can affect persisted
/// bytes. The cache schema fixture test below guards the representative binary
/// shape so CI fails until this version and its fingerprint are updated
/// together.
pub const CACHE_SCHEMA_VERSION: u32 = 23;
pub const CACHE_FILE_NAME: &str = "index.bin";
const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;
const HASH_SEPARATOR_BYTE: u8 = 0xff;
static CACHE_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheNamespace {
    Workspace,
    Stubs,
    Vendor,
}

impl CacheNamespace {
    pub fn as_str(self) -> &'static str {
        match self {
            CacheNamespace::Workspace => "workspace",
            CacheNamespace::Stubs => "stubs",
            CacheNamespace::Vendor => "vendor",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexCacheConfig {
    pub namespace: CacheNamespace,
    pub php_lsp_version: String,
    pub php_version: String,
    pub include_paths: Vec<String>,
    pub exclude_paths: Vec<String>,
    pub traversal_max_files: Option<usize>,
    pub traversal_max_entries: Option<usize>,
    pub stub_extensions: Vec<String>,
    pub stubs_hash: u64,
}

impl IndexCacheConfig {
    pub fn config_hash(&self) -> u64 {
        let mut parts = vec![
            format!("namespace={}", self.namespace.as_str()),
            format!("php-lsp-version={}", self.php_lsp_version),
            format!("php-version={}", self.php_version),
            format!("stubs-hash={:016x}", self.stubs_hash),
            format!(
                "traversal-max-files={}",
                self.traversal_max_files
                    .map_or_else(|| "unlimited".to_string(), |limit| limit.to_string())
            ),
            format!(
                "traversal-max-entries={}",
                self.traversal_max_entries
                    .map_or_else(|| "unlimited".to_string(), |limit| limit.to_string())
            ),
        ];
        extend_sorted(&mut parts, "include", &self.include_paths);
        extend_sorted(&mut parts, "exclude", &self.exclude_paths);
        extend_sorted(&mut parts, "stub-extension", &self.stub_extensions);
        stable_hash_strings(parts.iter().map(String::as_str))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexCache {
    pub schema_version: u32,
    pub namespace: String,
    pub php_lsp_version: String,
    pub workspace_root: String,
    pub config_hash: u64,
    pub stubs_hash: u64,
    pub created_at_unix_ms: u64,
    pub files: Vec<CachedFile>,
    pub top_level: CachedTopLevelSymbols,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedFile {
    pub uri: String,
    pub relative_path: String,
    pub metadata: CachedFileMetadata,
    pub file_symbols: FileSymbols,
    pub references: Vec<SymbolReference>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheSourceFile {
    pub path: PathBuf,
    pub uri: String,
    pub relative_path: String,
}

impl CacheSourceFile {
    pub fn new(path: PathBuf, uri: String, relative_path: String) -> Self {
        Self {
            path,
            uri,
            relative_path,
        }
    }

    pub fn workspace(root: &Path, path: &Path) -> Result<Self, FileUriError> {
        Ok(Self {
            path: path.to_path_buf(),
            uri: path_to_uri(path)?,
            relative_path: relative_cache_path(root, path),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedFileMetadata {
    pub modified_secs: u64,
    pub modified_nanos: u32,
    pub modified_status: ModifiedTimeStatus,
    pub size: u64,
    pub content_hash: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModifiedTimeStatus {
    Available,
    Unavailable,
    BeforeUnixEpoch,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CachedTopLevelSymbols {
    pub types: Vec<SymbolInfo>,
    pub functions: Vec<SymbolInfo>,
    pub constants: Vec<SymbolInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheLoadReport {
    pub cache_path: PathBuf,
    pub loaded_files: usize,
    pub stale_files: usize,
    pub missing_files: usize,
    pub extra_files: usize,
    pub indexed_symbols: usize,
    pub parse_files: Vec<PathBuf>,
    pub parse_sources: Vec<CacheSourceFile>,
    pub miss_reason: Option<String>,
}

#[derive(Debug)]
pub enum CacheError {
    Io(io::Error),
    Bincode(Box<bincode::ErrorKind>),
}

impl From<io::Error> for CacheError {
    fn from(value: io::Error) -> Self {
        CacheError::Io(value)
    }
}

impl From<Box<bincode::ErrorKind>> for CacheError {
    fn from(value: Box<bincode::ErrorKind>) -> Self {
        CacheError::Bincode(value)
    }
}

impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CacheError::Io(err) => write!(f, "{}", err),
            CacheError::Bincode(err) => write!(f, "{}", err),
        }
    }
}

impl std::error::Error for CacheError {}

pub fn cache_file_path(workspace_root: &Path) -> PathBuf {
    cache_file_path_for_namespace(workspace_root, CacheNamespace::Workspace)
}

pub fn cache_file_path_with_base(base_dir: PathBuf, workspace_root: &Path) -> PathBuf {
    cache_file_path_with_base_for_namespace(base_dir, workspace_root, CacheNamespace::Workspace)
}

pub fn cache_file_path_for_namespace(workspace_root: &Path, namespace: CacheNamespace) -> PathBuf {
    cache_file_path_with_base_for_namespace(default_cache_base_dir(), workspace_root, namespace)
}

pub fn cache_file_path_with_base_for_namespace(
    base_dir: PathBuf,
    workspace_root: &Path,
    namespace: CacheNamespace,
) -> PathBuf {
    base_dir
        .join("php-lsp")
        .join(workspace_hash(workspace_root))
        .join(namespace.as_str())
        .join(CACHE_FILE_NAME)
}

pub fn load_cache(path: &Path) -> Result<IndexCache, CacheError> {
    let bytes = fs::read(path)?;
    Ok(bincode::deserialize(&bytes)?)
}

pub fn save_cache_atomic(path: &Path, cache: &IndexCache) -> Result<(), CacheError> {
    prepare_cache_write(path, cache)?.commit()
}

pub struct PreparedCacheWrite {
    tmp_path: PathBuf,
    destination: PathBuf,
    committed: bool,
}

impl PreparedCacheWrite {
    pub fn commit(mut self) -> Result<(), CacheError> {
        replace_cache_file(&self.tmp_path, &self.destination)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for PreparedCacheWrite {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.tmp_path);
        }
    }
}

pub fn prepare_cache_write(
    path: &Path,
    cache: &IndexCache,
) -> Result<PreparedCacheWrite, CacheError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let counter = CACHE_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_path = path.with_file_name(format!(
        "{}.{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(CACHE_FILE_NAME),
        std::process::id(),
        counter
    ));
    let bytes = bincode::serialize(cache)?;
    write_cache_temp_file(&tmp_path, &bytes)?;
    Ok(PreparedCacheWrite {
        tmp_path,
        destination: path.to_path_buf(),
        committed: false,
    })
}

fn write_cache_temp_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn replace_cache_file(tmp_path: &Path, path: &Path) -> io::Result<()> {
    match fs::rename(tmp_path, path) {
        Ok(()) => Ok(()),
        Err(rename_error) => {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(remove_error) if remove_error.kind() == io::ErrorKind::NotFound => {}
                Err(remove_error) => {
                    let _ = fs::remove_file(tmp_path);
                    return Err(io::Error::new(
                        remove_error.kind(),
                        format!(
                            "failed to replace existing cache after rename failed ({rename_error}): {remove_error}"
                        ),
                    ));
                }
            }

            if let Err(retry_error) = fs::rename(tmp_path, path) {
                let _ = fs::remove_file(tmp_path);
                return Err(retry_error);
            }
            Ok(())
        }
    }
}

pub fn load_valid_cached_files(
    index: &WorkspaceIndex,
    cache_path: &Path,
    workspace_root: &Path,
    current_files: &[PathBuf],
    config: &IndexCacheConfig,
) -> CacheLoadReport {
    let (sources, uri_failures) = workspace_cache_sources(workspace_root, current_files);
    let mut report = load_valid_cached_sources(index, cache_path, workspace_root, &sources, config);
    if !uri_failures.is_empty() {
        if report.miss_reason.is_none() {
            report.miss_reason = Some(format!(
                "failed to convert {} path(s) to file URIs",
                uri_failures.len()
            ));
        }
        report.missing_files = report.missing_files.saturating_add(uri_failures.len());
        report.parse_files.extend(uri_failures);
        report.parse_files.sort();
        report.parse_files.dedup();
    }
    report
}

pub fn load_valid_cached_sources(
    index: &WorkspaceIndex,
    cache_path: &Path,
    workspace_root: &Path,
    current_sources: &[CacheSourceFile],
    config: &IndexCacheConfig,
) -> CacheLoadReport {
    let mut report = CacheLoadReport {
        cache_path: cache_path.to_path_buf(),
        loaded_files: 0,
        stale_files: 0,
        missing_files: 0,
        extra_files: 0,
        indexed_symbols: 0,
        parse_files: Vec::new(),
        parse_sources: Vec::new(),
        miss_reason: None,
    };

    let cache = match load_cache(cache_path) {
        Ok(cache) => cache,
        Err(CacheError::Io(err)) if err.kind() == io::ErrorKind::NotFound => {
            report.miss_reason = Some("cache file not found".to_string());
            report.parse_sources = current_sources.to_vec();
            report.parse_files = report
                .parse_sources
                .iter()
                .map(|source| source.path.clone())
                .collect();
            report.missing_files = report.parse_files.len();
            return report;
        }
        Err(err) => {
            report.miss_reason = Some(format!("failed to load cache: {}", err));
            report.parse_sources = current_sources.to_vec();
            report.parse_files = report
                .parse_sources
                .iter()
                .map(|source| source.path.clone())
                .collect();
            report.missing_files = report.parse_files.len();
            return report;
        }
    };

    if let Some(reason) = cache_miss_reason(&cache, workspace_root, config) {
        report.miss_reason = Some(reason);
        report.parse_sources = current_sources.to_vec();
        report.parse_files = report
            .parse_sources
            .iter()
            .map(|source| source.path.clone())
            .collect();
        report.missing_files = report.parse_files.len();
        return report;
    }

    let mut current_by_relative = HashMap::new();
    for source in current_sources {
        current_by_relative.insert(source.relative_path.clone(), source.clone());
    }

    let mut loaded_relatives = HashSet::new();
    for cached_file in cache.files {
        let Some(current_source) = current_by_relative.get(&cached_file.relative_path) else {
            report.extra_files += 1;
            continue;
        };

        match file_metadata(&current_source.path) {
            Ok(metadata)
                if metadata == cached_file.metadata && cached_file.uri == current_source.uri =>
            {
                report.indexed_symbols += cached_file.file_symbols.symbols.len();
                index.update_file_with_references(
                    &cached_file.uri,
                    cached_file.file_symbols,
                    cached_file.references,
                );
                loaded_relatives.insert(cached_file.relative_path);
                report.loaded_files += 1;
            }
            Ok(_) | Err(_) => {
                report.stale_files += 1;
            }
        }
    }

    for (relative, source) in current_by_relative {
        if !loaded_relatives.contains(&relative) {
            report.parse_sources.push(source);
        }
    }
    report.parse_files = report
        .parse_sources
        .iter()
        .map(|source| source.path.clone())
        .collect();
    report.parse_files.sort();
    report
        .parse_sources
        .sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    report.missing_files = report.parse_files.len().saturating_sub(report.stale_files);
    report
}

pub fn build_cache_from_index(
    index: &WorkspaceIndex,
    workspace_root: &Path,
    current_files: &[PathBuf],
    config: &IndexCacheConfig,
) -> IndexCache {
    let (sources, _) = workspace_cache_sources(workspace_root, current_files);
    build_cache_from_sources(index, workspace_root, &sources, config)
}

pub fn build_cache_from_sources(
    index: &WorkspaceIndex,
    workspace_root: &Path,
    current_sources: &[CacheSourceFile],
    config: &IndexCacheConfig,
) -> IndexCache {
    let mut files = Vec::new();

    for source in current_sources {
        let Some(file_symbols) = index
            .file_symbols
            .get(&source.uri)
            .map(|entry| entry.value().as_ref().clone())
        else {
            continue;
        };
        let Ok(metadata) = file_metadata(&source.path) else {
            continue;
        };

        files.push(CachedFile {
            uri: source.uri.clone(),
            relative_path: source.relative_path.clone(),
            metadata,
            file_symbols,
            references: index
                .file_references
                .get(&source.uri)
                .map(|entry| entry.value().clone())
                .unwrap_or_default(),
        });
    }

    files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    let top_level = top_level_symbols(&files);

    IndexCache {
        schema_version: CACHE_SCHEMA_VERSION,
        namespace: config.namespace.as_str().to_string(),
        php_lsp_version: config.php_lsp_version.clone(),
        workspace_root: normalized_path_string(workspace_root),
        config_hash: config.config_hash(),
        stubs_hash: config.stubs_hash,
        created_at_unix_ms: unix_ms(SystemTime::now()),
        files,
        top_level,
    }
}

pub fn stable_hash_strings<'a>(parts: impl IntoIterator<Item = &'a str>) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for part in parts {
        for byte in part.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash ^= u64::from(HASH_SEPARATOR_BYTE);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

pub fn stable_hash_bytes(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

pub fn file_metadata(path: &Path) -> io::Result<CachedFileMetadata> {
    let bytes = fs::read(path)?;
    let metadata = fs::metadata(path)?;
    Ok(file_metadata_from_parts(
        &bytes,
        metadata.len(),
        metadata.modified(),
    ))
}

fn file_metadata_from_parts(
    bytes: &[u8],
    size: u64,
    modified: io::Result<SystemTime>,
) -> CachedFileMetadata {
    let (modified_secs, modified_nanos, modified_status) = modified_time_parts(modified);
    CachedFileMetadata {
        modified_secs,
        modified_nanos,
        modified_status,
        size,
        content_hash: stable_hash_bytes(bytes),
    }
}

fn modified_time_parts(modified: io::Result<SystemTime>) -> (u64, u32, ModifiedTimeStatus) {
    match modified {
        Ok(time) => match time.duration_since(UNIX_EPOCH) {
            Ok(duration) => (
                duration.as_secs(),
                duration.subsec_nanos(),
                ModifiedTimeStatus::Available,
            ),
            Err(_) => (0, 0, ModifiedTimeStatus::BeforeUnixEpoch),
        },
        Err(_) => (0, 0, ModifiedTimeStatus::Unavailable),
    }
}

fn cache_miss_reason(
    cache: &IndexCache,
    workspace_root: &Path,
    config: &IndexCacheConfig,
) -> Option<String> {
    if cache.schema_version != CACHE_SCHEMA_VERSION {
        return Some(format!(
            "schema version mismatch: cache={}, expected={}",
            cache.schema_version, CACHE_SCHEMA_VERSION
        ));
    }
    if cache.namespace != config.namespace.as_str() {
        return Some(format!(
            "namespace mismatch: cache={}, expected={}",
            cache.namespace,
            config.namespace.as_str()
        ));
    }
    if cache.php_lsp_version != config.php_lsp_version {
        return Some(format!(
            "php-lsp version mismatch: cache={}, expected={}",
            cache.php_lsp_version, config.php_lsp_version
        ));
    }
    if cache.workspace_root != normalized_path_string(workspace_root) {
        return Some("workspace root mismatch".to_string());
    }
    if cache.config_hash != config.config_hash() {
        return Some("configuration hash mismatch".to_string());
    }
    if cache.stubs_hash != config.stubs_hash {
        return Some("stubs hash mismatch".to_string());
    }
    None
}

fn top_level_symbols(files: &[CachedFile]) -> CachedTopLevelSymbols {
    let mut top_level = CachedTopLevelSymbols::default();
    for file in files {
        for symbol in &file.file_symbols.symbols {
            match symbol.kind {
                PhpSymbolKind::Class
                | PhpSymbolKind::Interface
                | PhpSymbolKind::Trait
                | PhpSymbolKind::Enum => top_level.types.push(symbol.clone()),
                PhpSymbolKind::Function => top_level.functions.push(symbol.clone()),
                PhpSymbolKind::GlobalConstant => top_level.constants.push(symbol.clone()),
                _ => {}
            }
        }
    }
    top_level
}

fn extend_sorted(parts: &mut Vec<String>, prefix: &str, values: &[String]) {
    let mut sorted = values.to_vec();
    sorted.sort();
    for value in sorted {
        parts.push(format!("{}={}", prefix, value));
    }
}

fn default_cache_base_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(path);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".cache");
    }
    std::env::temp_dir()
}

fn workspace_hash(workspace_root: &Path) -> String {
    let normalized = normalized_path_string(workspace_root);
    format!("{:016x}", stable_hash_strings([normalized.as_str()]))
}

fn relative_cache_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn normalized_path_string(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .replace('\\', "/")
}

fn workspace_cache_sources(root: &Path, paths: &[PathBuf]) -> (Vec<CacheSourceFile>, Vec<PathBuf>) {
    let mut sources = Vec::new();
    let mut uri_failures = Vec::new();
    for path in paths {
        match CacheSourceFile::workspace(root, path) {
            Ok(source) => sources.push(source),
            Err(_) => uri_failures.push(path.clone()),
        }
    }
    (sources, uri_failures)
}

fn unix_ms(time: SystemTime) -> u64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration
            .as_secs()
            .saturating_mul(1000)
            .saturating_add(u64::from(duration.subsec_millis())),
        Err(_) => 0,
    }
}

#[cfg(test)]
#[path = "cache_tests.rs"]
mod tests;
