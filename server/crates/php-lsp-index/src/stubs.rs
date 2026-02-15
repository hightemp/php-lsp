//! phpstorm-stubs loader.
//!
//! Loads PHP built-in function/class definitions from JetBrains/phpstorm-stubs.
//! Parsed symbols are added to the workspace index with the `is_builtin` modifier.

use crate::workspace::WorkspaceIndex;
use php_lsp_parser::parser::FileParser;
use php_lsp_parser::symbols::extract_file_symbols;
use php_lsp_types::SymbolModifiers;
use std::path::{Path, PathBuf};

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
    "tokenizer",
    "ctype",
    "fileinfo",
    "pdo",
    "Reflection",
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
pub fn load_stubs(
    index: &WorkspaceIndex,
    stubs_path: &Path,
    extensions: &[&str],
) -> usize {
    let mut loaded_files = 0;

    for ext_name in extensions {
        let ext_dir = stubs_path.join(ext_name);
        if !ext_dir.is_dir() {
            tracing::debug!("Stubs extension directory not found: {}", ext_dir.display());
            continue;
        }

        let php_files = collect_stub_files(&ext_dir);
        for file_path in &php_files {
            match std::fs::read_to_string(file_path) {
                Ok(source) => {
                    let mut parser = FileParser::new();
                    parser.parse_full(&source);

                    if let Some(tree) = parser.tree() {
                        let uri = format!("phpstub://{}/{}", ext_name, file_path.file_name().unwrap_or_default().to_string_lossy());
                        let mut file_symbols = extract_file_symbols(tree, &source, &uri);

                        // Mark all symbols as built-in
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

                        loaded_files += 1;
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to read stub file {}: {}", file_path.display(), e);
                }
            }
        }
    }

    loaded_files
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
    }

    #[test]
    fn test_load_stubs_with_real_data() {
        // This test uses actual phpstorm-stubs if available
        let stubs_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../data/stubs");

        if !stubs_path.is_dir() {
            // Skip if stubs are not available (e.g., in CI without submodule)
            eprintln!("Skipping stubs test: stubs directory not found at {}", stubs_path.display());
            return;
        }

        let index = WorkspaceIndex::new();
        let loaded = load_stubs(&index, &stubs_path, &["Core"]);

        assert!(loaded > 0, "Should have loaded at least one stub file");

        // Core should define basic PHP classes like stdClass, Exception, etc.
        // Check that some known built-in class exists
        let has_builtin = index.types.iter().any(|entry| entry.value().modifiers.is_builtin);
        assert!(has_builtin, "Should have at least one built-in type");
    }

    #[test]
    fn test_load_stubs_nonexistent_extension() {
        let stubs_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../data/stubs");

        if !stubs_path.is_dir() {
            return;
        }

        let index = WorkspaceIndex::new();
        let loaded = load_stubs(&index, &stubs_path, &["nonexistent_extension_xyz"]);
        assert_eq!(loaded, 0);
    }

    #[test]
    fn test_load_multiple_extensions() {
        let stubs_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../data/stubs");

        if !stubs_path.is_dir() {
            return;
        }

        let index = WorkspaceIndex::new();
        let loaded = load_stubs(&index, &stubs_path, &["Core", "standard", "date"]);

        assert!(loaded >= 3, "Should have loaded files from multiple extensions, got {}", loaded);
    }
}
