//! phpstorm-stubs symbol loader.
//!
//! Loads PHP built-in function/class definitions from JetBrains/phpstorm-stubs.
//! Parsed symbols are added to the workspace index with the `is_builtin` modifier.
//! Server-side path discovery, validation, and cache-source collection live in
//! `php-lsp-server/src/indexing/stubs.rs`.

use crate::workspace::WorkspaceIndex;
use php_lsp_parser::parser::FileParser;
use php_lsp_parser::symbols::{
    extract_file_symbols, extract_file_symbols_for_php_version, PhpSymbolExtractionVersion,
};
use php_lsp_types::SymbolModifiers;
use std::path::{Path, PathBuf};

pub use php_lsp_parser::symbols::PhpSymbolExtractionVersion as StubPhpVersion;

/// Fallback extension list used when a stubs directory is not available to
/// inspect. Normal server loading discovers all available stub extension
/// directories with [`discover_stub_extensions`].
pub const DEFAULT_EXTENSIONS: &[&str] = &[
    "Core",
    "standard",
    "date",
    "json",
    "libxml",
    "pcre",
    "SPL",
    "mbstring",
    "curl",
    "dom",
    "SimpleXML",
    "xml",
    "filter",
    "hash",
    "session",
    "soap",
    "tokenizer",
    "ctype",
    "fileinfo",
    "PDO",
    "Reflection",
    "random",
    "intl",
    "openssl",
    "zlib",
    "bcmath",
    "gd",
    "iconv",
    "mysqli",
    "posix",
    "sodium",
    "exif",
];

const NON_EXTENSION_DIRS: &[&str] = &["meta", "tests", "vendor"];

/// Load phpstorm-stubs for the given extensions into the workspace index.
///
/// `stubs_path` is the path to the phpstorm-stubs directory (e.g., `server/data/stubs`).
/// `extensions` is a list of extension directory names to load.
///
/// Returns the number of files loaded.
pub fn load_stubs(index: &WorkspaceIndex, stubs_path: &Path, extensions: &[&str]) -> usize {
    load_stubs_for_php_version(index, stubs_path, extensions, None)
}

pub fn load_stubs_for_php_version(
    index: &WorkspaceIndex,
    stubs_path: &Path,
    extensions: &[&str],
    php_version: Option<PhpSymbolExtractionVersion>,
) -> usize {
    let mut loaded_files = 0;

    for ext_name in extensions {
        let php_files = collect_extension_stub_files(stubs_path, ext_name);
        if php_files.is_empty() && !stubs_path.join(ext_name).is_dir() {
            tracing::debug!(
                "Stubs extension directory not found: {}",
                stubs_path.join(ext_name).display()
            );
            continue;
        }

        for file_path in &php_files {
            if load_stub_file_for_php_version(index, stubs_path, ext_name, file_path, php_version)
                .is_some()
            {
                loaded_files += 1;
            }
        }
    }

    loaded_files
}

/// Build the stable pseudo-URI used for a phpstorm-stubs file.
pub fn stub_file_uri(stubs_path: &Path, ext_name: &str, file_path: &Path) -> String {
    let relative_path = relative_stub_file_path(stubs_path, ext_name, file_path);
    format!(
        "phpstub://{}/{}",
        ext_name,
        relative_path.to_string_lossy().replace('\\', "/")
    )
}

fn relative_stub_file_path(stubs_path: &Path, ext_name: &str, file_path: &Path) -> PathBuf {
    let extension_root = stubs_path.join(ext_name);
    if let Ok(relative) = file_path.strip_prefix(&extension_root) {
        if !relative.as_os_str().is_empty() {
            return relative.to_path_buf();
        }
    }

    file_path.file_name().map(PathBuf::from).unwrap_or_default()
}

/// Discover extension directory names available in a phpstorm-stubs root.
///
/// Only top-level directories containing PHP files are returned; repository
/// metadata, tests, vendor tooling, and phpstorm-meta folders are intentionally ignored.
pub fn discover_stub_extensions(stubs_path: &Path) -> Vec<String> {
    let mut extensions = Vec::new();
    let Ok(entries) = std::fs::read_dir(stubs_path) else {
        return extensions;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with('.') || NON_EXTENSION_DIRS.contains(&name) {
            continue;
        }
        if collect_stub_files(&path).is_empty() {
            continue;
        }

        extensions.push(name.to_string());
    }

    extensions.sort();
    extensions
}

/// Collect all .php files from a stubs extension directory recursively.
pub fn collect_extension_stub_files(stubs_path: &Path, ext_name: &str) -> Vec<PathBuf> {
    collect_stub_files(&stubs_path.join(ext_name))
}

/// Parse one stub file, mark its symbols as built-in and update the workspace index.
///
/// Returns the number of symbols in the parsed file, or `None` if the file could
/// not be read or parsed.
pub fn load_stub_file(
    index: &WorkspaceIndex,
    stubs_path: &Path,
    ext_name: &str,
    file_path: &Path,
) -> Option<usize> {
    load_stub_file_for_php_version(index, stubs_path, ext_name, file_path, None)
}

pub fn load_stub_file_for_php_version(
    index: &WorkspaceIndex,
    stubs_path: &Path,
    ext_name: &str,
    file_path: &Path,
    php_version: Option<PhpSymbolExtractionVersion>,
) -> Option<usize> {
    match std::fs::read_to_string(file_path) {
        Ok(source) => {
            let mut parser = FileParser::new();
            parser.parse_full(&source);

            let tree = parser.tree()?;
            let uri = stub_file_uri(stubs_path, ext_name, file_path);
            let mut file_symbols = if let Some(php_version) = php_version {
                extract_file_symbols_for_php_version(tree, &source, &uri, php_version)
            } else {
                extract_file_symbols(tree, &source, &uri)
            };

            for sym in &mut file_symbols.symbols {
                sym.modifiers = SymbolModifiers {
                    is_builtin: true,
                    ..sym.modifiers
                };
            }

            let sym_count = file_symbols.symbols.len();
            index.update_file(&uri, file_symbols);

            if sym_count > 0 {
                tracing::debug!(
                    "Loaded stubs {}/{}: {} symbols",
                    ext_name,
                    file_path.file_name().unwrap_or_default().to_string_lossy(),
                    sym_count
                );
            }

            Some(sym_count)
        }
        Err(e) => {
            tracing::warn!("Failed to read stub file {}: {}", file_path.display(), e);
            None
        }
    }
}

/// Collect all .php files from a directory recursively.
fn collect_stub_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_path_buf()];

    while let Some(current) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("php") {
                files.push(path);
            }
        }
    }

    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::len_zero)]
    fn test_default_extensions_not_empty() {
        assert!(DEFAULT_EXTENSIONS.len() > 0);
        assert!(DEFAULT_EXTENSIONS.contains(&"Core"));
        assert!(DEFAULT_EXTENSIONS.contains(&"standard"));
        assert!(DEFAULT_EXTENSIONS.contains(&"PDO"));
    }

    #[test]
    fn test_discover_stub_extensions_uses_available_php_stub_dirs() {
        let stubs_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/stubs");
        if !stubs_are_available(&stubs_path) {
            eprintln!(
                "Skipping stubs discovery test: stubs not initialized at {}",
                stubs_path.display()
            );
            return;
        }

        let extensions = discover_stub_extensions(&stubs_path);
        assert!(extensions.contains(&"Core".to_string()));
        assert!(extensions.contains(&"standard".to_string()));
        assert!(extensions.contains(&"libxml".to_string()));
        assert!(extensions.contains(&"posix".to_string()));
        assert!(extensions.contains(&"zip".to_string()));
        assert!(
            !extensions
                .iter()
                .any(|extension| extension.starts_with('.')),
            "metadata directories should not be treated as stub extensions: {extensions:?}"
        );
        for skipped in ["tests", "meta", "vendor"] {
            assert!(
                !extensions.iter().any(|extension| extension == skipped),
                "non-extension directory should not be treated as stub extension: {skipped}"
            );
        }
    }

    #[test]
    fn test_discover_stub_extensions_skips_vendor_even_with_php_files() {
        let root =
            std::env::temp_dir().join(format!("php-lsp-stub-discovery-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("Core")).expect("create Core stubs dir");
        std::fs::write(
            root.join("Core/Core.php"),
            "<?php function strlen(string $s): int;",
        )
        .expect("write Core stub");
        std::fs::create_dir_all(root.join("vendor/acme/package")).expect("create vendor dir");
        std::fs::write(
            root.join("vendor/acme/package/Helper.php"),
            "<?php function should_not_be_builtin(): void;",
        )
        .expect("write vendor PHP file");

        let extensions = discover_stub_extensions(&root);

        assert_eq!(extensions, vec!["Core".to_string()]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn test_collect_extension_stub_files_recurses_and_uri_preserves_relative_path() {
        let stubs_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/stubs");
        if !stubs_path.join("snappy/snappy/snappy.php").is_file() {
            eprintln!(
                "Skipping recursive stubs test: nested snappy stub not initialized at {}",
                stubs_path.display()
            );
            return;
        }

        let files = collect_extension_stub_files(&stubs_path, "snappy");
        let nested = stubs_path.join("snappy/snappy/snappy.php");

        assert!(
            files.iter().any(|file| file == &nested),
            "expected recursive stub collection to include {nested:?}, got {files:?}"
        );
        assert_eq!(
            stub_file_uri(&stubs_path, "snappy", &nested),
            "phpstub://snappy/snappy/snappy.php"
        );
    }

    #[test]
    fn test_stub_file_uri_uses_stubs_root_not_first_matching_path_component() {
        let stubs_path = std::env::temp_dir()
            .join("snappy")
            .join("project")
            .join("server/data/stubs");
        let nested = stubs_path.join("snappy/snappy/snappy.php");

        assert_eq!(
            stub_file_uri(&stubs_path, "snappy", &nested),
            "phpstub://snappy/snappy/snappy.php"
        );
    }

    fn stubs_are_available(stubs_path: &Path) -> bool {
        // Check that the submodule is actually initialized (not just an empty dir)
        stubs_path.join("Core/Core.php").is_file()
    }

    fn bundled_stubs_are_required() -> bool {
        std::env::var_os("CI").is_some()
            || std::env::var_os("PHP_LSP_REQUIRE_BUNDLED_STUBS").is_some()
    }

    fn bundled_stubs_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../client/stubs")
    }

    #[test]
    fn test_load_stubs_with_real_data() {
        // This test uses actual phpstorm-stubs if available
        let stubs_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/stubs");

        if !stubs_are_available(&stubs_path) {
            // Skip if stubs are not available (e.g., in CI without submodule)
            eprintln!(
                "Skipping stubs test: stubs not initialized at {}",
                stubs_path.display()
            );
            return;
        }

        let index = WorkspaceIndex::new();
        let loaded = load_stubs(&index, &stubs_path, &["Core"]);

        assert!(loaded > 0, "Should have loaded at least one stub file");

        // Core should define basic PHP classes like stdClass, Exception, etc.
        // Check that some known built-in class exists
        let has_builtin = index
            .types
            .iter()
            .any(|entry| entry.value().modifiers.is_builtin);
        assert!(has_builtin, "Should have at least one built-in type");
    }

    #[test]
    fn test_load_stubs_nonexistent_extension() {
        let stubs_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/stubs");

        if !stubs_are_available(&stubs_path) {
            return;
        }

        let index = WorkspaceIndex::new();
        let loaded = load_stubs(&index, &stubs_path, &["nonexistent_extension_xyz"]);
        assert_eq!(loaded, 0);
    }

    #[test]
    fn test_load_multiple_extensions() {
        let stubs_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/stubs");

        if !stubs_are_available(&stubs_path) {
            return;
        }

        let index = WorkspaceIndex::new();
        let loaded = load_stubs(&index, &stubs_path, &["Core", "standard", "date"]);

        assert!(
            loaded >= 3,
            "Should have loaded files from multiple extensions, got {}",
            loaded
        );
    }

    #[test]
    fn test_bundled_stubs_expose_core_builtin_symbols() {
        let stubs_path = bundled_stubs_path();
        if !stubs_are_available(&stubs_path) {
            let message = format!(
                "bundled stubs not initialized at {}; run scripts/bundle-stubs.sh",
                stubs_path.display()
            );
            if bundled_stubs_are_required() {
                panic!("{message}");
            }
            eprintln!("Skipping bundled stubs test: {message}");
            return;
        }

        let index = WorkspaceIndex::new();
        let loaded = load_stubs(
            &index,
            &stubs_path,
            &["Core", "standard", "SPL", "SimpleXML", "soap"],
        );

        assert!(
            loaded >= 20,
            "bundled stubs should load core/default files, got {loaded}"
        );

        for fqn in ["stdClass", "Exception", "ArrayObject", "SimpleXMLElement"] {
            let symbol = index
                .resolve_fqn(fqn)
                .unwrap_or_else(|| panic!("missing bundled built-in type: {fqn}"));
            assert!(
                symbol.modifiers.is_builtin,
                "bundled symbol should be marked built-in: {fqn}"
            );
        }

        for fqn in ["array_map", "strlen"] {
            let symbol = index
                .resolve_fqn(fqn)
                .unwrap_or_else(|| panic!("missing bundled built-in function: {fqn}"));
            assert!(
                symbol.modifiers.is_builtin,
                "bundled function should be marked built-in: {fqn}"
            );
        }
    }
}
