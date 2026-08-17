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
        if !is_valid_stub_extension_name(ext_name) {
            tracing::warn!("Ignoring invalid stubs extension name: {:?}", ext_name);
            continue;
        }
        let php_files = collect_extension_stub_files(stubs_path, ext_name);
        if php_files.is_empty() && !is_real_stub_extension_directory(stubs_path, ext_name) {
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
    if !is_valid_stub_extension_name(ext_name) {
        return file_path.file_name().map(PathBuf::from).unwrap_or_default();
    }
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
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let path = entry.path();

        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
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
    if !is_real_stub_extension_directory(stubs_path, ext_name) {
        return Vec::new();
    }
    let extension_path = stubs_path.join(ext_name);
    collect_stub_files(&extension_path)
}

/// Return whether an extension name is one normal path component.
pub fn is_valid_stub_extension_name(ext_name: &str) -> bool {
    let mut components = Path::new(ext_name).components();
    matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none()
}

/// Return whether an extension is a real directory directly below the configured root.
pub fn is_real_stub_extension_directory(stubs_path: &Path, ext_name: &str) -> bool {
    if !is_valid_stub_extension_name(ext_name) {
        return false;
    }
    std::fs::symlink_metadata(stubs_path.join(ext_name))
        .is_ok_and(|metadata| metadata.file_type().is_dir())
}

/// Count real PHP files below a configured stubs root without following symlink entries.
pub fn count_php_stub_files(stubs_path: &Path) -> usize {
    let mut count = 0;
    visit_php_stub_files(stubs_path, |_| count += 1);
    count
}

/// Return whether a relative stub file is composed only of real path entries below the root.
///
/// The configured root itself may be a symlink, but every relative component must be a real
/// directory or final regular file. Absolute paths and parent traversal are rejected.
pub fn is_real_stub_file(stubs_path: &Path, relative_path: &Path) -> bool {
    let mut current = stubs_path.to_path_buf();
    let mut components = relative_path.components().peekable();

    while let Some(component) = components.next() {
        match component {
            std::path::Component::CurDir => continue,
            std::path::Component::Normal(part) => current.push(part),
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => return false,
        }

        let Ok(metadata) = std::fs::symlink_metadata(&current) else {
            return false;
        };
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            return false;
        }
        if components.peek().is_some() {
            if !file_type.is_dir() {
                return false;
            }
        } else {
            return file_type.is_file();
        }
    }

    false
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
    if !is_valid_stub_extension_name(ext_name) {
        tracing::warn!("Ignoring invalid stubs extension name: {:?}", ext_name);
        return None;
    }
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
    visit_php_stub_files(dir, |path| files.push(path));
    files.sort();
    files
}

fn visit_php_stub_files(dir: &Path, mut visitor: impl FnMut(PathBuf)) {
    let mut pending = vec![dir.to_path_buf()];

    while let Some(current) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };

        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file()
                && path.extension().and_then(|e| e.to_str()) == Some("php")
            {
                visitor(path);
            }
        }
    }
}

#[cfg(test)]
#[path = "stubs_tests.rs"]
mod tests;
