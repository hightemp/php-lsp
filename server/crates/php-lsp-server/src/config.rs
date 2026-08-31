use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

pub const PROJECT_CONFIG_FILE_NAME: &str = ".php-lsp.toml";
pub const DEFAULT_INDEXING_MAX_FILES: usize = 100_000;
pub const DEFAULT_INDEXING_MAX_ENTRIES: usize = 1_000_000;

pub const DEFAULT_PROJECT_CONFIG: &str = r#"# PHP Language Server project configuration.
# VS Code settings override these shared defaults when explicitly configured.

[php]
version = "8.2"

[diagnostics]
mode = "basic-semantic"
# Maximum relevant AST nodes before member/type diagnostics are skipped.
# Set to 0 to disable the budget cap for this project.
memberTypeNodeBudget = 512
partialAnalysisDiagnostic = true

[diagnostics.severity]
unknownSymbols = "warning"
unused = "warning"
duplicateSymbols = "warning"
members = "warning"
typeCompatibility = "warning"
overrideSignatures = "warning"
phpVersion = "warning"

[indexing]
composer = true
vendor = true
include = []
exclude = []
# Maximum unique matching files and inspected filesystem entries per traversal.
# Project config may lower these defaults; trusted global/client config may raise
# them or set either value to 0 to disable that limit.
# maxFiles = 100000
# maxEntries = 1000000

[stubs]
# Omit `extensions` to discover all available stub extension directories. Set `extensions = []` to disable stubs.
# extensions = ["Core", "standard", "SPL"]
# path = "/absolute/path/to/phpstorm-stubs"

[formatting]
provider = "auto"
# Project formatter commands/providers that execute tools are ignored unless
# trusted from VS Code or global config with allowProjectCommands = true.
command = ""
timeoutMs = 30000

[phpstan]
enabled = false
command = "vendor/bin/phpstan analyse --error-format=json --no-progress --no-interaction {file}"
timeoutMs = 30000
memory_limit = ""

[psalm]
enabled = false
command = "vendor/bin/psalm --output-format=json --no-progress {file}"
timeoutMs = 30000

[analyzerCodeActions]
enabled = false
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitConfigResult {
    Created(PathBuf),
    AlreadyExists(PathBuf),
}

pub fn write_default_project_config(path: &Path) -> std::io::Result<InitConfigResult> {
    if path.exists() {
        return Ok(InitConfigResult::AlreadyExists(path.to_path_buf()));
    }

    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, DEFAULT_PROJECT_CONFIG)?;
    Ok(InitConfigResult::Created(path.to_path_buf()))
}

pub fn global_config_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Ok(path) = std::env::var("PHP_LSP_CONFIG") {
        let path = path.trim();
        if !path.is_empty() {
            candidates.push(PathBuf::from(path));
        }
    }

    if let Ok(xdg_config_home) = std::env::var("XDG_CONFIG_HOME") {
        let path = xdg_config_home.trim();
        if !path.is_empty() {
            candidates.push(PathBuf::from(path).join("php-lsp").join("config.toml"));
        }
    }

    if let Some(home) = home_dir() {
        candidates.push(home.join(".config").join("php-lsp").join("config.toml"));
        candidates.push(home.join(".php-lsp.toml"));
    }

    dedup_paths(candidates)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

fn dedup_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut unique = Vec::new();
    for path in paths {
        if !unique.iter().any(|existing| existing == &path) {
            unique.push(path);
        }
    }
    unique
}

pub fn load_toml_settings(path: &Path) -> std::result::Result<Value, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {}", path.display(), err))?;
    let value = content
        .parse::<toml::Value>()
        .map_err(|err| format!("failed to parse {}: {}", path.display(), err))?;
    let json = serde_json::to_value(value)
        .map_err(|err| format!("failed to normalize {}: {}", path.display(), err))?;
    Ok(normalize_project_config_settings(&json))
}

pub fn normalize_client_settings(settings: &Value) -> Value {
    settings
        .get("phpLsp")
        .cloned()
        .unwrap_or_else(|| settings.clone())
}

pub fn normalize_project_config_settings(raw: &Value) -> Value {
    let raw = raw.get("phpLsp").unwrap_or(raw);
    let mut settings = Map::new();

    if let Some(allow_project_commands) = raw
        .get("allowProjectCommands")
        .or_else(|| {
            raw.get("security")
                .and_then(|security| security.get("allowProjectCommands"))
        })
        .and_then(Value::as_bool)
    {
        settings.insert(
            "allowProjectCommands".to_string(),
            Value::Bool(allow_project_commands),
        );
    }

    if let Some(version) = string_at(raw, &["php", "version"]) {
        settings.insert("phpVersion".to_string(), Value::String(version.to_string()));
    }

    if let Some(diagnostics) = raw.get("diagnostics").and_then(Value::as_object) {
        let mut diagnostics_settings = Map::new();
        if let Some(mode) = diagnostics.get("mode").and_then(Value::as_str) {
            diagnostics_settings.insert("mode".to_string(), Value::String(mode.to_string()));
        }
        if let Some(budget) = diagnostics
            .get("memberTypeNodeBudget")
            .or_else(|| diagnostics.get("memberTypeBudget"))
            .and_then(Value::as_u64)
        {
            diagnostics_settings.insert("memberTypeNodeBudget".to_string(), Value::from(budget));
        }
        if let Some(enabled) = diagnostics
            .get("partialAnalysisDiagnostic")
            .and_then(Value::as_bool)
        {
            diagnostics_settings.insert(
                "partialAnalysisDiagnostic".to_string(),
                Value::Bool(enabled),
            );
        }

        let mut severity = Map::new();
        if let Some(severity_object) = diagnostics.get("severity").and_then(Value::as_object) {
            for (key, value) in severity_object {
                if let Some(level) = value.as_str() {
                    severity.insert(key.clone(), Value::String(level.to_string()));
                }
            }
        }
        for (key, value) in diagnostics {
            if key == "mode" || key == "severity" {
                continue;
            }
            if is_diagnostic_category_key(key) {
                if let Some(level) = value.as_str() {
                    severity.insert(key.clone(), Value::String(level.to_string()));
                }
            }
        }
        if !severity.is_empty() {
            diagnostics_settings.insert("severity".to_string(), Value::Object(severity));
        }
        if !diagnostics_settings.is_empty() {
            settings.insert(
                "diagnostics".to_string(),
                Value::Object(diagnostics_settings),
            );
        }
    }

    if let Some(indexing) = raw.get("indexing").and_then(Value::as_object) {
        if let Some(include) = string_array_value(indexing.get("include")) {
            settings.insert("includePaths".to_string(), include);
        }
        if let Some(exclude) = string_array_value(indexing.get("exclude")) {
            settings.insert("excludePaths".to_string(), exclude);
        }
        if let Some(max_files) = indexing.get("maxFiles").and_then(Value::as_u64) {
            settings.insert("indexingMaxFiles".to_string(), Value::from(max_files));
        }
        if let Some(max_entries) = indexing.get("maxEntries").and_then(Value::as_u64) {
            settings.insert("indexingMaxEntries".to_string(), Value::from(max_entries));
        }
        if let Some(vendor) = indexing.get("vendor").and_then(Value::as_bool) {
            settings.insert("indexVendor".to_string(), Value::Bool(vendor));
        }
        if let Some(composer) = indexing.get("composer").and_then(Value::as_bool) {
            settings.insert(
                "composer".to_string(),
                Value::Object(Map::from_iter([(
                    "enabled".to_string(),
                    Value::Bool(composer),
                )])),
            );
        }
        if let Some(stubs) = string_array_value(indexing.get("stubs")) {
            settings.insert(
                "stubs".to_string(),
                Value::Object(Map::from_iter([("extensions".to_string(), stubs)])),
            );
        }
    }

    if let Some(stubs) = raw.get("stubs").and_then(Value::as_object) {
        let mut stubs_settings = Map::new();
        if let Some(extensions) = string_array_value(stubs.get("extensions")) {
            stubs_settings.insert("extensions".to_string(), extensions);
        }
        if let Some(path) = stubs.get("path").and_then(Value::as_str) {
            stubs_settings.insert("path".to_string(), Value::String(path.to_string()));
        }
        if !stubs_settings.is_empty() {
            merge_json_objects(
                settings
                    .entry("stubs".to_string())
                    .or_insert_with(|| Value::Object(Map::new())),
                &Value::Object(stubs_settings),
            );
        }
    }

    copy_section(
        raw,
        &mut settings,
        "formatting",
        &["provider", "command", "timeoutMs", "timeout"],
    );
    copy_section(
        raw,
        &mut settings,
        "phpstan",
        &[
            "enabled",
            "command",
            "timeoutMs",
            "timeout",
            "memory_limit",
            "memoryLimit",
        ],
    );
    copy_section(
        raw,
        &mut settings,
        "psalm",
        &["enabled", "command", "timeoutMs", "timeout"],
    );
    copy_section(raw, &mut settings, "analyzerCodeActions", &["enabled"]);

    Value::Object(settings)
}

pub fn merge_json_objects(base: &mut Value, overlay: &Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(key) {
                    Some(existing) => merge_json_objects(existing, value),
                    None => {
                        base.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (base, overlay) => {
            *base = overlay.clone();
        }
    }
}

fn copy_section(raw: &Value, settings: &mut Map<String, Value>, section: &str, keys: &[&str]) {
    let Some(source) = raw.get(section).and_then(Value::as_object) else {
        return;
    };

    let mut target = Map::new();
    for key in keys {
        if let Some(value) = source.get(*key) {
            target.insert((*key).to_string(), value.clone());
        }
    }
    if !target.is_empty() {
        settings.insert(section.to_string(), Value::Object(target));
    }
}

fn string_at<'a>(raw: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut current = raw;
    for segment in path {
        current = current.get(*segment)?;
    }
    current.as_str()
}

fn string_array_value(value: Option<&Value>) -> Option<Value> {
    let values = value?.as_array()?;
    Some(Value::Array(
        values
            .iter()
            .filter_map(Value::as_str)
            .map(|value| Value::String(value.to_string()))
            .collect(),
    ))
}

fn is_diagnostic_category_key(key: &str) -> bool {
    matches!(
        key,
        "unknownSymbols"
            | "unknown_symbols"
            | "unused"
            | "duplicateSymbols"
            | "duplicate_symbols"
            | "members"
            | "typeCompatibility"
            | "type_compatibility"
            | "overrideSignatures"
            | "override_signatures"
            | "phpVersion"
            | "php_version"
    )
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
