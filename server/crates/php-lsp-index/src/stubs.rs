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
#[path = "stubs_tests.rs"]
mod tests;
