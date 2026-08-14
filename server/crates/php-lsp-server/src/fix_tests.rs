use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn parse_fix_args_accepts_path_project_root_rules_and_format() {
    let args = parse_fix_args(vec![
        "src".to_string(),
        "--dry-run".to_string(),
        "--project-root".to_string(),
        "/tmp/project".to_string(),
        "--rule".to_string(),
        "organize-imports".to_string(),
        "--rule".to_string(),
        "add-return-type".to_string(),
        "--format".to_string(),
        "json".to_string(),
    ])
    .unwrap();

    assert_eq!(args.path, Some(PathBuf::from("src")));
    assert_eq!(args.project_root, Some(PathBuf::from("/tmp/project")));
    assert!(args.dry_run);
    assert_eq!(
        args.rules,
        vec![FixRule::OrganizeImports, FixRule::AddReturnType]
    );
    assert_eq!(args.format, FixFormat::Json);
}

#[test]
fn fix_json_output_has_stable_shape_and_dry_run_does_not_write() {
    let root = temp_dir("json-shape");
    let path = root.join("FixMe.php");
    let original = r#"<?php
namespace App;

use App\Unused;
use App\Used;

/** @return string */
function label($value) {
    return $value;
}

echo Used::class;
"#;
    std::fs::write(&path, original).unwrap();

    let result = run_fix_cli(vec![
        "--project-root".to_string(),
        root.display().to_string(),
        "--dry-run".to_string(),
        "--format".to_string(),
        "json".to_string(),
    ]);

    assert_eq!(result.exit_code, 2, "stderr: {}", result.stderr);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);

    let value: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert_eq!(value["schemaVersion"], 1);
    assert_eq!(value["dryRun"], true);
    assert_eq!(value["summary"]["filesAnalyzed"], 1);
    assert_eq!(value["summary"]["filesWithChanges"], 1);
    assert_eq!(value["files"][0]["path"], "FixMe.php");

    let rules = value["files"][0]["fixes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|fix| fix["rule"].as_str())
        .collect::<Vec<_>>();
    assert!(rules.contains(&"unused-imports"), "rules: {:?}", rules);
    assert!(rules.contains(&"add-return-type"), "rules: {:?}", rules);

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn fix_report_is_idempotent_after_applying_generated_edits() {
    let root = temp_dir("idempotent");
    let path = root.join("FixMe.php");
    std::fs::write(
        &path,
        r#"<?php
namespace App;

use App\Unused;
use App\Used;

/** @return string */
function label($value) {
    return $value;
}

echo Used::class;
"#,
    )
    .unwrap();

    let args = FixArgs {
        project_root: Some(root.clone()),
        dry_run: true,
        ..FixArgs::default()
    };
    let first = run_fix(&FixArgs {
        rules: DEFAULT_FIX_RULES.to_vec(),
        ..args.clone()
    })
    .unwrap();
    assert_eq!(first.total_fixes(), 2);
    assert_eq!(first.files.len(), 1);
    let original = std::fs::read_to_string(&path).unwrap();
    let edits = first.files[0]
        .actions
        .iter()
        .flat_map(|action| action.edits.iter().cloned())
        .collect::<Vec<_>>();
    let new_source = apply_fix_edits(&original, &edits).unwrap();
    std::fs::write(&path, new_source).unwrap();

    let second = run_fix(&FixArgs {
        rules: DEFAULT_FIX_RULES.to_vec(),
        ..args
    })
    .unwrap();
    assert_eq!(second.total_edits(), 0);

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn fix_cli_requires_dry_run() {
    let result = run_fix_cli(vec![]);
    assert_eq!(result.exit_code, 1);
    assert!(result.stderr.contains("requires --dry-run"));
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "php-lsp-fix-{label}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
