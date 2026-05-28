//! phpstorm-stubs loader.
//!
//! Loads PHP built-in function/class definitions from JetBrains/phpstorm-stubs.
//! Parsed symbols are added to the workspace index with the `is_builtin` modifier.

use crate::workspace::WorkspaceIndex;
use php_lsp_parser::parser::FileParser;
use php_lsp_parser::symbols::{
    extract_file_symbols, extract_file_symbols_for_php_version, PhpSymbolExtractionVersion,
};
use php_lsp_types::SymbolModifiers;
use std::path::{Path, PathBuf};

pub use php_lsp_parser::symbols::PhpSymbolExtractionVersion as StubPhpVersion;

/// Default extensions that are always loaded (common PHP extensions).
pub const DEFAULT_EXTENSIONS: &[&str] = &[
    "Core",
    "standard",
    "date",
    "json",
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
    "sodium",
    "exif",
];

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
            if load_stub_file_for_php_version(index, ext_name, file_path, php_version).is_some() {
                loaded_files += 1;
            }
        }
    }

    loaded_files
}

/// Build the stable pseudo-URI used for a phpstorm-stubs file.
pub fn stub_file_uri(ext_name: &str, file_path: &Path) -> String {
    format!(
        "phpstub://{}/{}",
        ext_name,
        file_path.file_name().unwrap_or_default().to_string_lossy()
    )
}

/// Collect all .php files from a stubs extension directory (non-recursive).
pub fn collect_extension_stub_files(stubs_path: &Path, ext_name: &str) -> Vec<PathBuf> {
    collect_stub_files(&stubs_path.join(ext_name))
}

/// Parse one stub file, mark its symbols as built-in and update the workspace index.
///
/// Returns the number of symbols in the parsed file, or `None` if the file could
/// not be read or parsed.
pub fn load_stub_file(index: &WorkspaceIndex, ext_name: &str, file_path: &Path) -> Option<usize> {
    load_stub_file_for_php_version(index, ext_name, file_path, None)
}

pub fn load_stub_file_for_php_version(
    index: &WorkspaceIndex,
    ext_name: &str,
    file_path: &Path,
    php_version: Option<PhpSymbolExtractionVersion>,
) -> Option<usize> {
    match std::fs::read_to_string(file_path) {
        Ok(source) => {
            let mut parser = FileParser::new();
            parser.parse_full(&source);

            let tree = parser.tree()?;
            let uri = stub_file_uri(ext_name, file_path);
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

/// Collect all .php files from a stubs extension directory (non-recursive).
fn collect_stub_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("php") {
                files.push(path);
            }
        }
    }
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

    fn stubs_are_available(stubs_path: &Path) -> bool {
        // Check that the submodule is actually initialized (not just an empty dir)
        stubs_path.join("Core/Core.php").is_file()
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
}
