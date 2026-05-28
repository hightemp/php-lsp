//! Disk cache for workspace index snapshots.

use crate::workspace::WorkspaceIndex;
use php_lsp_types::{FileSymbols, PhpSymbolKind, SymbolInfo, SymbolReference};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const CACHE_SCHEMA_VERSION: u32 = 11;
pub const CACHE_FILE_NAME: &str = "index.bin";
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

    pub fn workspace(root: &Path, path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            uri: path_to_uri(path),
            relative_path: relative_cache_path(root, path),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedFileMetadata {
    pub modified_secs: u64,
    pub modified_nanos: u32,
    pub size: u64,
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
    fs::write(&tmp_path, bytes)?;
    fs::rename(tmp_path, path)?;
    Ok(())
}

pub fn load_valid_cached_files(
    index: &WorkspaceIndex,
    cache_path: &Path,
    workspace_root: &Path,
    current_files: &[PathBuf],
    config: &IndexCacheConfig,
) -> CacheLoadReport {
    let sources: Vec<CacheSourceFile> = current_files
        .iter()
        .map(|path| CacheSourceFile::workspace(workspace_root, path))
        .collect();
    load_valid_cached_sources(index, cache_path, workspace_root, &sources, config)
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
    let sources: Vec<CacheSourceFile> = current_files
        .iter()
        .map(|path| CacheSourceFile::workspace(workspace_root, path))
        .collect();
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
            .map(|entry| entry.value().clone())
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
    let mut hash = 0xcbf29ce484222325u64;
    for part in parts {
        for byte in part.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

pub fn file_metadata(path: &Path) -> io::Result<CachedFileMetadata> {
    let metadata = fs::metadata(path)?;
    let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
    let duration = modified.duration_since(UNIX_EPOCH).unwrap_or_default();
    Ok(CachedFileMetadata {
        modified_secs: duration.as_secs(),
        modified_nanos: duration.subsec_nanos(),
        size: metadata.len(),
    })
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

fn path_to_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

fn unix_ms(time: SystemTime) -> u64 {
    let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    duration
        .as_secs()
        .saturating_mul(1000)
        .saturating_add(u64::from(duration.subsec_millis()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use php_lsp_types::{SymbolModifiers, Visibility};
    use std::io::Write;

    fn unique_temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "php-lsp-cache-test-{}-{}",
            name,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_symbol(uri: &str) -> SymbolInfo {
        SymbolInfo {
            name: "Foo".to_string(),
            fqn: "App\\Foo".to_string(),
            kind: PhpSymbolKind::Class,
            uri: uri.to_string(),
            range: (0, 0, 1, 0),
            selection_range: (0, 6, 0, 9),
            visibility: Visibility::Public,
            modifiers: SymbolModifiers::default(),
            doc_comment: None,
            signature: None,
            parent_fqn: None,
            extends: vec![],
            implements: vec![],
            traits: vec![],
            templates: vec![],
            template_bindings: vec![],
        }
    }

    fn test_config() -> IndexCacheConfig {
        IndexCacheConfig {
            namespace: CacheNamespace::Workspace,
            php_lsp_version: "0.4.1".to_string(),
            php_version: "8.2".to_string(),
            include_paths: vec!["src".to_string()],
            exclude_paths: vec!["vendor".to_string()],
            stub_extensions: vec!["Core".to_string()],
            stubs_hash: 42,
        }
    }

    #[test]
    fn cache_roundtrip_loads_valid_file_symbols() {
        let root = unique_temp_dir("roundtrip");
        let src = root.join("src");
        fs::create_dir_all(&src).unwrap();
        let file = src.join("Foo.php");
        fs::write(&file, "<?php class Foo {}").unwrap();
        let uri = path_to_uri(&file);

        let index = WorkspaceIndex::new();
        index.update_file(
            &uri,
            FileSymbols {
                namespace: Some("App".to_string()),
                use_statements: vec![],
                symbols: vec![make_symbol(&uri)],
                ..Default::default()
            },
        );

        let config = test_config();
        let cache = build_cache_from_index(&index, &root, std::slice::from_ref(&file), &config);
        assert_eq!(cache.files.len(), 1);
        assert_eq!(cache.top_level.types.len(), 1);

        let cache_path = root.join("index.bin");
        save_cache_atomic(&cache_path, &cache).unwrap();

        let loaded = WorkspaceIndex::new();
        let report = load_valid_cached_files(
            &loaded,
            &cache_path,
            &root,
            std::slice::from_ref(&file),
            &config,
        );
        assert_eq!(report.loaded_files, 1);
        assert!(report.parse_files.is_empty());
        assert!(loaded.resolve_fqn("App\\Foo").is_some());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cache_roundtrip_loads_file_references() {
        let root = unique_temp_dir("references");
        let file = root.join("Foo.php");
        fs::write(&file, "<?php class Foo {}").unwrap();
        let uri = path_to_uri(&file);
        let references = vec![SymbolReference {
            target_fqn: "App\\Foo".to_string(),
            target_kind: PhpSymbolKind::Class,
            range: (3, 12, 3, 15),
            is_declaration: false,
            starts_with_dollar: false,
        }];

        let index = WorkspaceIndex::new();
        index.update_file_with_references(
            &uri,
            FileSymbols {
                namespace: None,
                use_statements: vec![],
                symbols: vec![make_symbol(&uri)],
                ..Default::default()
            },
            references.clone(),
        );

        let config = test_config();
        let cache = build_cache_from_index(&index, &root, std::slice::from_ref(&file), &config);
        assert_eq!(cache.files[0].references, references);

        let cache_path = root.join("index.bin");
        save_cache_atomic(&cache_path, &cache).unwrap();

        let loaded = WorkspaceIndex::new();
        let report = load_valid_cached_files(
            &loaded,
            &cache_path,
            &root,
            std::slice::from_ref(&file),
            &config,
        );
        assert_eq!(report.loaded_files, 1);
        assert_eq!(
            loaded
                .file_references
                .get(&uri)
                .map(|entry| entry.value().clone())
                .unwrap_or_default(),
            references
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cache_invalidates_changed_file_metadata() {
        let root = unique_temp_dir("changed");
        let file = root.join("Foo.php");
        fs::write(&file, "<?php class Foo {}").unwrap();
        let uri = path_to_uri(&file);

        let index = WorkspaceIndex::new();
        index.update_file(
            &uri,
            FileSymbols {
                namespace: None,
                use_statements: vec![],
                symbols: vec![make_symbol(&uri)],
                ..Default::default()
            },
        );

        let config = test_config();
        let cache = build_cache_from_index(&index, &root, std::slice::from_ref(&file), &config);
        let cache_path = root.join("index.bin");
        save_cache_atomic(&cache_path, &cache).unwrap();

        let mut handle = fs::OpenOptions::new().append(true).open(&file).unwrap();
        writeln!(handle, "\n// changed").unwrap();

        let loaded = WorkspaceIndex::new();
        let report = load_valid_cached_files(
            &loaded,
            &cache_path,
            &root,
            std::slice::from_ref(&file),
            &config,
        );
        assert_eq!(report.loaded_files, 0);
        assert_eq!(report.stale_files, 1);
        assert_eq!(report.parse_files, vec![file.clone()]);
        assert!(loaded.resolve_fqn("App\\Foo").is_none());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cache_path_uses_workspace_hash_under_php_lsp_dir() {
        let base = PathBuf::from("/tmp/php-lsp-cache-base");
        let path = cache_file_path_with_base(base.clone(), Path::new("/tmp/project"));
        assert_eq!(
            path.file_name().and_then(|p| p.to_str()),
            Some(CACHE_FILE_NAME)
        );
        assert!(path.starts_with(base.join("php-lsp")));
        assert!(path.ends_with(Path::new("workspace").join(CACHE_FILE_NAME)));
    }

    #[test]
    fn concurrent_saves_to_same_cache_path_do_not_share_temp_file() {
        let root = unique_temp_dir("concurrent-save");
        let file = root.join("src").join("Foo.php");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "<?php class Foo {}").unwrap();
        let uri = path_to_uri(&file);
        let cache_path = root.join("cache").join(CACHE_FILE_NAME);
        let config = test_config();

        let mut handles = Vec::new();
        for _ in 0..8 {
            let root = root.clone();
            let file = file.clone();
            let uri = uri.clone();
            let cache_path = cache_path.clone();
            let config = config.clone();
            handles.push(std::thread::spawn(move || {
                let index = WorkspaceIndex::new();
                index.update_file(
                    &uri,
                    FileSymbols {
                        namespace: Some("App".to_string()),
                        use_statements: vec![],
                        symbols: vec![make_symbol(&uri)],
                        ..Default::default()
                    },
                );
                let cache = build_cache_from_index(&index, &root, &[file], &config);
                save_cache_atomic(&cache_path, &cache)
            }));
        }

        for handle in handles {
            handle.join().unwrap().unwrap();
        }

        let loaded = load_cache(&cache_path).unwrap();
        assert_eq!(loaded.files.len(), 1);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cache_path_uses_separate_namespace_directories() {
        let base = PathBuf::from("/tmp/php-lsp-cache-base");
        let root = Path::new("/tmp/project");
        let workspace =
            cache_file_path_with_base_for_namespace(base.clone(), root, CacheNamespace::Workspace);
        let stubs =
            cache_file_path_with_base_for_namespace(base.clone(), root, CacheNamespace::Stubs);
        let vendor = cache_file_path_with_base_for_namespace(base, root, CacheNamespace::Vendor);

        assert_ne!(workspace, stubs);
        assert_ne!(workspace, vendor);
        assert_ne!(stubs, vendor);
        assert!(workspace.ends_with(Path::new("workspace").join(CACHE_FILE_NAME)));
        assert!(stubs.ends_with(Path::new("stubs").join(CACHE_FILE_NAME)));
        assert!(vendor.ends_with(Path::new("vendor").join(CACHE_FILE_NAME)));
    }
}
