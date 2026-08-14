use super::*;

#[test]
fn normalizes_project_config_sections_to_runtime_settings() {
    let raw = serde_json::json!({
        "php": { "version": "8.3" },
        "diagnostics": {
            "mode": "syntax-only",
            "memberTypeNodeBudget": 128,
            "partialAnalysisDiagnostic": false,
            "unknown_symbols": "off",
            "severity": { "members": "error" }
        },
        "indexing": {
            "composer": false,
            "vendor": false,
            "include": ["src"],
            "exclude": ["vendor"]
        },
        "stubs": { "path": "/tmp/stubs", "extensions": ["Core"] },
        "security": { "allowProjectCommands": true },
        "formatting": { "provider": "custom", "command": "fmt {file}", "timeoutMs": 1000 },
        "phpstan": { "enabled": true, "memory_limit": "1G" }
    });

    let settings = normalize_project_config_settings(&raw);
    assert_eq!(settings["allowProjectCommands"], true);
    assert_eq!(settings["phpVersion"], "8.3");
    assert_eq!(settings["diagnostics"]["mode"], "syntax-only");
    assert_eq!(settings["diagnostics"]["memberTypeNodeBudget"], 128);
    assert_eq!(settings["diagnostics"]["partialAnalysisDiagnostic"], false);
    assert_eq!(
        settings["diagnostics"]["severity"]["unknown_symbols"],
        "off"
    );
    assert_eq!(settings["diagnostics"]["severity"]["members"], "error");
    assert_eq!(settings["composer"]["enabled"], false);
    assert_eq!(settings["indexVendor"], false);
    assert_eq!(settings["includePaths"][0], "src");
    assert_eq!(settings["excludePaths"][0], "vendor");
    assert_eq!(settings["stubs"]["path"], "/tmp/stubs");
    assert_eq!(settings["stubs"]["extensions"][0], "Core");
    assert_eq!(settings["formatting"]["provider"], "custom");
    assert_eq!(settings["phpstan"]["memory_limit"], "1G");
}

#[test]
fn recursive_merge_preserves_nested_settings() {
    let mut base = serde_json::json!({
        "diagnostics": {
            "mode": "basic-semantic",
            "severity": { "members": "warning" }
        }
    });
    let overlay = serde_json::json!({
        "diagnostics": {
            "severity": { "unused": "off" }
        }
    });

    merge_json_objects(&mut base, &overlay);

    assert_eq!(base["diagnostics"]["mode"], "basic-semantic");
    assert_eq!(base["diagnostics"]["severity"]["members"], "warning");
    assert_eq!(base["diagnostics"]["severity"]["unused"], "off");
}
