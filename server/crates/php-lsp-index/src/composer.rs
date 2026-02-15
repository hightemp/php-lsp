//! Composer.json parsing and namespace mapping.
//!
//! Parses composer.json to extract PSR-4/PSR-0/classmap/files autoload
//! configuration and builds a namespace-to-directory mapping.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Namespace mapping extracted from composer.json autoload config.
#[derive(Debug, Clone, Default)]
pub struct NamespaceMap {
    /// PSR-4: namespace prefix → directories
    pub psr4: Vec<(String, Vec<PathBuf>)>,
    /// PSR-0: namespace prefix → directories
    pub psr0: Vec<(String, Vec<PathBuf>)>,
    /// classmap: directories/files to scan
    pub classmap: Vec<PathBuf>,
    /// files: specific files to always load (helpers, etc.)
    pub files: Vec<PathBuf>,
}

impl NamespaceMap {
    /// Resolve a fully qualified class name to possible file paths using PSR-4.
    ///
    /// E.g., with mapping `App\\` → `src/`, resolving `App\\Service\\Foo`
    /// returns `[src/Service/Foo.php]`.
    pub fn resolve_class_to_paths(&self, fqn: &str) -> Vec<PathBuf> {
        let mut results = Vec::new();

        for (prefix, dirs) in &self.psr4 {
            if let Some(relative) = fqn.strip_prefix(prefix.as_str()) {
                let relative_path = relative.replace('\\', "/") + ".php";
                for dir in dirs {
                    results.push(dir.join(&relative_path));
                }
            }
        }

        for (prefix, dirs) in &self.psr0 {
            if let Some(relative) = fqn.strip_prefix(prefix.as_str()) {
                // PSR-0: underscores in class name map to directory separators
                let relative_path = relative.replace(['\\', '_'], "/") + ".php";
                for dir in dirs {
                    results.push(dir.join(&relative_path));
                }
            }
        }

        results
    }

    /// Get all directories that should be scanned for PHP files.
    pub fn source_directories(&self) -> Vec<&Path> {
        let mut dirs: Vec<&Path> = Vec::new();
        for (_, paths) in &self.psr4 {
            for p in paths {
                dirs.push(p.as_path());
            }
        }
        for (_, paths) in &self.psr0 {
            for p in paths {
                dirs.push(p.as_path());
            }
        }
        for p in &self.classmap {
            dirs.push(p.as_path());
        }
        dirs
    }
}

/// Partial composer.json schema (only what we need).
#[derive(Debug, Deserialize, Default)]
struct ComposerJson {
    #[serde(default)]
    autoload: AutoloadSection,
    #[serde(default, rename = "autoload-dev")]
    autoload_dev: AutoloadSection,
}

#[derive(Debug, Deserialize, Default)]
struct AutoloadSection {
    #[serde(default, rename = "psr-4")]
    psr4: HashMap<String, Psr4Value>,
    #[serde(default, rename = "psr-0")]
    psr0: HashMap<String, Psr4Value>,
    #[serde(default)]
    classmap: Vec<String>,
    #[serde(default)]
    files: Vec<String>,
}

/// PSR-4 value can be a string or array of strings.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Psr4Value {
    Single(String),
    Multiple(Vec<String>),
}

impl Psr4Value {
    fn to_paths(&self, base_dir: &Path) -> Vec<PathBuf> {
        match self {
            Psr4Value::Single(s) => vec![base_dir.join(s)],
            Psr4Value::Multiple(v) => v.iter().map(|s| base_dir.join(s)).collect(),
        }
    }
}

/// Parse composer.json from the given path and return a NamespaceMap.
pub fn parse_composer_json(path: &Path) -> Result<NamespaceMap, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
    parse_composer_json_str(&content, path.parent().unwrap_or(Path::new(".")))
}

/// Parse composer.json content string with a base directory for resolving paths.
pub fn parse_composer_json_str(content: &str, base_dir: &Path) -> Result<NamespaceMap, String> {
    let composer: ComposerJson =
        serde_json::from_str(content).map_err(|e| format!("Invalid composer.json: {}", e))?;

    let mut map = NamespaceMap::default();

    // Process autoload
    process_autoload_section(&composer.autoload, base_dir, &mut map);
    // Process autoload-dev
    process_autoload_section(&composer.autoload_dev, base_dir, &mut map);

    Ok(map)
}

fn process_autoload_section(section: &AutoloadSection, base_dir: &Path, map: &mut NamespaceMap) {
    // PSR-4
    for (prefix, value) in &section.psr4 {
        let dirs = value.to_paths(base_dir);
        map.psr4.push((prefix.clone(), dirs));
    }

    // PSR-0
    for (prefix, value) in &section.psr0 {
        let dirs = value.to_paths(base_dir);
        map.psr0.push((prefix.clone(), dirs));
    }

    // classmap
    for path in &section.classmap {
        map.classmap.push(base_dir.join(path));
    }

    // files
    for path in &section.files {
        map.files.push(base_dir.join(path));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_basic_psr4() {
        let json = r#"{
            "autoload": {
                "psr-4": {
                    "App\\": "src/"
                }
            }
        }"#;
        let map = parse_composer_json_str(json, Path::new("/project")).unwrap();
        assert_eq!(map.psr4.len(), 1);
        assert_eq!(map.psr4[0].0, "App\\");
        assert_eq!(map.psr4[0].1, vec![PathBuf::from("/project/src/")]);
    }

    #[test]
    fn test_parse_psr4_with_dev() {
        let json = r#"{
            "autoload": {
                "psr-4": {
                    "App\\": "src/"
                }
            },
            "autoload-dev": {
                "psr-4": {
                    "App\\Tests\\": "tests/"
                }
            }
        }"#;
        let map = parse_composer_json_str(json, Path::new("/project")).unwrap();
        assert_eq!(map.psr4.len(), 2);
    }

    #[test]
    fn test_parse_multiple_dirs() {
        let json = r#"{
            "autoload": {
                "psr-4": {
                    "App\\": ["src/", "lib/"]
                }
            }
        }"#;
        let map = parse_composer_json_str(json, Path::new("/project")).unwrap();
        assert_eq!(map.psr4[0].1.len(), 2);
    }

    #[test]
    fn test_parse_classmap_and_files() {
        let json = r#"{
            "autoload": {
                "classmap": ["database/", "legacy/"],
                "files": ["helpers/functions.php"]
            }
        }"#;
        let map = parse_composer_json_str(json, Path::new("/project")).unwrap();
        assert_eq!(map.classmap.len(), 2);
        assert_eq!(map.files.len(), 1);
        assert_eq!(
            map.files[0],
            PathBuf::from("/project/helpers/functions.php")
        );
    }

    #[test]
    fn test_resolve_class_psr4() {
        let json = r#"{
            "autoload": {
                "psr-4": {
                    "App\\": "src/"
                }
            }
        }"#;
        let map = parse_composer_json_str(json, Path::new("/project")).unwrap();
        let paths = map.resolve_class_to_paths("App\\Service\\UserService");
        assert_eq!(paths.len(), 1);
        assert_eq!(
            paths[0],
            PathBuf::from("/project/src/Service/UserService.php")
        );
    }

    #[test]
    fn test_resolve_class_not_matching() {
        let json = r#"{
            "autoload": {
                "psr-4": {
                    "App\\": "src/"
                }
            }
        }"#;
        let map = parse_composer_json_str(json, Path::new("/project")).unwrap();
        let paths = map.resolve_class_to_paths("Vendor\\SomeClass");
        assert!(paths.is_empty());
    }

    #[test]
    fn test_source_directories() {
        let json = r#"{
            "autoload": {
                "psr-4": { "App\\": "src/" },
                "classmap": ["database/"]
            },
            "autoload-dev": {
                "psr-4": { "App\\Tests\\": "tests/" }
            }
        }"#;
        let map = parse_composer_json_str(json, Path::new("/project")).unwrap();
        let dirs = map.source_directories();
        assert!(dirs.len() >= 3); // src, tests, database
    }

    #[test]
    fn test_empty_composer_json() {
        let json = r#"{}"#;
        let map = parse_composer_json_str(json, Path::new("/project")).unwrap();
        assert!(map.psr4.is_empty());
        assert!(map.files.is_empty());
    }

    #[test]
    fn test_real_world_laravel() {
        let json = r#"{
            "autoload": {
                "psr-4": {
                    "App\\": "app/",
                    "Database\\Factories\\": "database/factories/",
                    "Database\\Seeders\\": "database/seeders/"
                }
            },
            "autoload-dev": {
                "psr-4": {
                    "Tests\\": "tests/"
                }
            }
        }"#;
        let map = parse_composer_json_str(json, Path::new("/var/www")).unwrap();
        assert_eq!(map.psr4.len(), 4);
        let paths = map.resolve_class_to_paths("App\\Http\\Controllers\\UserController");
        assert_eq!(
            paths[0],
            PathBuf::from("/var/www/app/Http/Controllers/UserController.php")
        );
    }
}
