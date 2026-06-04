//! Server-side stub orchestration helpers.
//!
//! This module discovers and validates configured phpstorm-stubs paths, clears
//! existing built-in symbols, and collects source files used by cache hashes.
//! The actual stub parsing and symbol insertion are implemented by
//! `php-lsp-index/src/stubs.rs`.

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
    let current_exe = std::env::current_exe().ok();
    candidate_stubs_paths_for_exe(root, client_stubs_path, current_exe.as_deref())
}

fn candidate_stubs_paths_for_exe(
    root: &Path,
    client_stubs_path: Option<PathBuf>,
    exe: Option<&Path>,
) -> Vec<PathBuf> {
    let mut candidate_paths: Vec<PathBuf> = Vec::new();

    if let Some(path) = client_stubs_path {
        push_candidate_path(&mut candidate_paths, path);
    }

    push_candidate_path(&mut candidate_paths, root.join("server/data/stubs"));

    if let Some(dir) = exe.and_then(Path::parent) {
        push_candidate_path(&mut candidate_paths, dir.join("data/stubs"));
        push_candidate_path(&mut candidate_paths, dir.join("../stubs"));
        push_candidate_path(&mut candidate_paths, dir.join("../../data/stubs"));
    }

    push_candidate_path(
        &mut candidate_paths,
        PathBuf::from("/usr/share/php-lsp/stubs"),
    );
    candidate_paths
}

fn push_candidate_path(candidate_paths: &mut Vec<PathBuf>, path: PathBuf) {
    let path = path
        .canonicalize()
        .unwrap_or_else(|_| lexically_normalize_path(&path));
    if !candidate_paths.iter().any(|candidate| candidate == &path) {
        candidate_paths.push(path);
    }
}

fn lexically_normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if matches!(
                    normalized.components().next_back(),
                    Some(std::path::Component::Normal(_))
                ) {
                    normalized.pop();
                } else {
                    normalized.push(component.as_os_str());
                }
            }
            std::path::Component::Prefix(_)
            | std::path::Component::RootDir
            | std::path::Component::Normal(_) => normalized.push(component.as_os_str()),
        }
    }
    normalized
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

    if stub_extensions
        .as_deref()
        .is_some_and(|extensions| extensions.is_empty())
    {
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
        let extensions =
            effective_stub_extensions_for_path(&stubs_path, stub_extensions.as_deref());
        if extensions.is_empty() {
            tracing::info!("phpstorm-stubs disabled by config: stub extensions list is empty");
            return 0;
        }
        let cache_sources = collect_stub_cache_sources(&stubs_path, &extensions);
        let cache_path = cache::cache_file_path_for_namespace(root, CacheNamespace::Stubs);
        let cache_config =
            stubs_index_cache_config_for_extensions(&stubs_path, php_version, extensions.clone());
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
                &stubs_path,
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
            let relative_path = path
                .strip_prefix(stubs_path)
                .map(|path| path.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| {
                    let file_name = path
                        .file_name()
                        .map(|name| name.to_string_lossy())
                        .unwrap_or_default();
                    format!("{}/{}", extension, file_name)
                });
            sources.push(CacheSourceFile::new(
                path.clone(),
                stubs::stub_file_uri(stubs_path, extension, &path),
                relative_path,
            ));
        }
    }
    sources.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    sources
}

#[cfg(test)]
mod tests {
    use super::*;
    use php_lsp_parser::parser::FileParser;
    use php_lsp_parser::semantic::{extract_semantic_diagnostics, SemanticDiagnosticKind};
    use php_lsp_parser::symbols::extract_file_symbols;

    fn source_stubs_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/stubs")
    }

    fn source_stubs_are_available(stubs_path: &Path) -> bool {
        stubs_path.join("Core/Core.php").is_file()
            && stubs_path.join("standard/standard_2.php").is_file()
            && stubs_path.join("standard/standard_8.php").is_file()
    }

    #[test]
    fn test_candidate_stubs_paths_include_source_checkout_stubs_from_target_binary() {
        let root = Path::new("/tmp/project");
        let exe = Path::new("/repo/php-lsp/server/target/debug/php-lsp");
        let paths = candidate_stubs_paths_for_exe(root, None, Some(exe));

        assert!(
            paths
                .iter()
                .any(|path| path == Path::new("/repo/php-lsp/server/data/stubs")),
            "expected source checkout stubs path in {paths:?}"
        );
    }

    #[test]
    fn test_load_configured_stubs_exposes_standard_builtin_functions_from_source_checkout() {
        let stubs_path = source_stubs_path();
        if !source_stubs_are_available(&stubs_path) {
            eprintln!(
                "Skipping server stubs smoke test: stubs not initialized at {}",
                stubs_path.display()
            );
            return;
        }

        let index = WorkspaceIndex::new();
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let loaded = load_configured_stubs(
            &index,
            &repo_root,
            None,
            Some(vec!["standard".to_string()]),
            PhpVersion::DEFAULT,
            true,
        );

        assert!(loaded > 0, "expected standard stubs to load");
        for fqn in ["in_array", "sprintf"] {
            let symbol = index
                .resolve_fqn(fqn)
                .unwrap_or_else(|| panic!("missing standard built-in function: {fqn}"));
            assert!(
                symbol.modifiers.is_builtin,
                "standard function should be marked built-in: {fqn}"
            );
        }
    }

    #[test]
    fn test_default_stubs_expose_global_extension_functions_in_namespaces() {
        let stubs_path = source_stubs_path();
        if !source_stubs_are_available(&stubs_path)
            || !stubs_path.join("libxml/libxml.php").is_file()
            || !stubs_path.join("posix/posix.php").is_file()
        {
            eprintln!(
                "Skipping server stubs smoke test: libxml/posix stubs not initialized at {}",
                stubs_path.display()
            );
            return;
        }

        let index = WorkspaceIndex::new();
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let loaded =
            load_configured_stubs(&index, &repo_root, None, None, PhpVersion::DEFAULT, true);

        assert!(loaded > 0, "expected default stubs to load");
        for fqn in ["libxml_clear_errors", "libxml_get_errors", "posix_geteuid"] {
            let symbol = index
                .resolve_fqn(fqn)
                .unwrap_or_else(|| panic!("missing default extension function: {fqn}"));
            assert!(
                symbol.modifiers.is_builtin,
                "extension function should be marked built-in: {fqn}"
            );
        }
        for fqn in ["ZipArchive", "ZipArchive::open", "ZipArchive::CREATE"] {
            let symbol = index
                .resolve_fqn(fqn)
                .unwrap_or_else(|| panic!("missing default extension symbol: {fqn}"));
            assert!(
                symbol.modifiers.is_builtin,
                "extension symbol should be marked built-in: {fqn}"
            );
        }

        let code = r#"<?php
namespace App\Controller;

libxml_clear_errors();
libxml_get_errors();
posix_geteuid();
"#;
        let mut parser = FileParser::new();
        parser.parse_full(code);
        let tree = parser.tree().expect("test PHP should parse");
        let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
        let diagnostics =
            extract_semantic_diagnostics(tree, code, &file_symbols, |fqn| index.resolve_fqn(fqn));
        let unknown_functions: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.kind == SemanticDiagnosticKind::UnknownFunction)
            .collect();

        assert!(
            unknown_functions.is_empty(),
            "default global extension stubs should satisfy namespaced unqualified calls, got: {:?}",
            unknown_functions
        );
    }
}
