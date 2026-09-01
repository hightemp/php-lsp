use super::*;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[test]
fn parse_analyze_args_accepts_path_project_root_severity_and_format() {
    let args = parse_analyze_args(vec![
        "src".to_string(),
        "--project-root".to_string(),
        "/tmp/project".to_string(),
        "--severity".to_string(),
        "warning".to_string(),
        "--format".to_string(),
        "json".to_string(),
    ])
    .unwrap();

    assert_eq!(args.path, Some(PathBuf::from("src")));
    assert_eq!(args.project_root, Some(PathBuf::from("/tmp/project")));
    assert_eq!(args.severity, AnalyzeSeverity::Warning);
    assert_eq!(args.format, AnalyzeFormat::Json);
}

#[test]
fn analyze_json_output_has_stable_shape() {
    let root = temp_dir("json-shape");
    std::fs::write(
        root.join("Broken.php"),
        "<?php\nnamespace App;\nfunction demo(): void { new MissingClass(); }\n",
    )
    .unwrap();

    let result = run_analyze_cli(vec![
        "--project-root".to_string(),
        root.display().to_string(),
        "--format".to_string(),
        "json".to_string(),
    ]);

    assert_eq!(result.exit_code, 2, "stderr: {}", result.stderr);
    let value: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert_eq!(value["schemaVersion"], 1);
    assert_eq!(value["summary"]["filesAnalyzed"], 1);
    assert_eq!(value["summary"]["diagnostics"], 1);
    assert_eq!(value["diagnostics"][0]["path"], "Broken.php");
    assert_eq!(value["diagnostics"][0]["severity"], "warning");
    assert!(value["diagnostics"][0]["message"]
        .as_str()
        .unwrap()
        .contains("Unknown class"));

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn analyze_exit_codes_report_clean_diagnostics_and_errors() {
    let clean_root = temp_dir("exit-clean");
    std::fs::write(clean_root.join("Clean.php"), "<?php\nclass Clean {}\n").unwrap();
    let clean = run_analyze_cli(vec![
        "--project-root".to_string(),
        clean_root.display().to_string(),
    ]);
    assert_eq!(clean.exit_code, 0, "stderr: {}", clean.stderr);
    assert!(clean.stdout.contains("No diagnostics found."));

    let broken_root = temp_dir("exit-broken");
    std::fs::write(
        broken_root.join("Broken.php"),
        "<?php\nnamespace App;\nfunction broken(): void { new MissingClass(); }\n",
    )
    .unwrap();
    let broken = run_analyze_cli(vec![
        "--project-root".to_string(),
        broken_root.display().to_string(),
        "--severity".to_string(),
        "warning".to_string(),
    ]);
    assert_eq!(broken.exit_code, 2, "stderr: {}", broken.stderr);

    let invalid = run_analyze_cli(vec!["/path/that/does/not/exist".to_string()]);
    assert_eq!(invalid.exit_code, 1);

    let _ = std::fs::remove_dir_all(clean_root);
    let _ = std::fs::remove_dir_all(broken_root);
}

#[test]
fn explicit_analyze_target_is_not_dropped_by_workspace_file_limit() {
    let root = temp_dir("explicit-target-limit");
    std::fs::write(
        root.join(".php-lsp.toml"),
        "[indexing]\ncomposer = false\nmaxFiles = 1\n",
    )
    .unwrap();
    std::fs::write(root.join("A.php"), "<?php class A {}\n").unwrap();
    std::fs::write(root.join("Z.php"), "<?php class Z {}\n").unwrap();

    let result = run_analyze_cli(vec![
        "Z.php".to_string(),
        "--project-root".to_string(),
        root.display().to_string(),
        "--format".to_string(),
        "json".to_string(),
    ]);
    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert!(result.stderr.contains("indexing.maxFiles=1"));
    let value: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert_eq!(value["summary"]["filesAnalyzed"], 1);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn analyze_directory_discovery_deadline_is_an_explicit_error() {
    let root = temp_dir("discovery-deadline");
    std::fs::write(root.join("Subject.php"), "<?php class Subject {}\n").unwrap();

    let outcome = collect_target_analyze_files(
        &root,
        &root,
        &[],
        TraversalLimits::default(),
        Instant::now() - Duration::from_millis(1),
    )
    .unwrap();
    assert_eq!(
        outcome.stop_reason,
        Some(TraversalStopReason::DeadlineExceeded)
    );
    let error = analyze_traversal_warning("Analyze target discovery", &outcome).unwrap_err();
    assert!(error.to_string().contains("traversal deadline"));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn analyze_resolves_vendor_psr4_symbols_from_composer_installed_metadata() {
    let root = temp_dir("vendor-psr4");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("vendor/composer")).unwrap();
    std::fs::create_dir_all(root.join("vendor/acme/library/src")).unwrap();

    std::fs::write(
        root.join("composer.json"),
        r#"{
                "autoload": {
                    "psr-4": {
                        "App\\": "src/"
                    }
                }
            }"#,
    )
    .unwrap();
    std::fs::write(
        root.join("vendor/composer/installed.json"),
        serde_json::json!({
            "packages": [
                {
                    "name": "acme/library",
                    "install-path": "../acme/library",
                    "autoload": {
                        "psr-4": {
                            "Vendor\\Package\\": "src/"
                        }
                    }
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        root.join("vendor/acme/library/src/ExternalThing.php"),
        "<?php\nnamespace Vendor\\Package;\nclass ExternalThing {}\n",
    )
    .unwrap();
    std::fs::write(
            root.join("src/Service.php"),
            "<?php\nnamespace App;\nuse Vendor\\Package\\ExternalThing;\nfinal class Service { public function build(): ExternalThing { return new ExternalThing(); } }\n",
        )
        .unwrap();

    let result = run_analyze_cli(vec![
        "src/Service.php".to_string(),
        "--project-root".to_string(),
        root.display().to_string(),
        "--severity".to_string(),
        "all".to_string(),
        "--format".to_string(),
        "json".to_string(),
    ]);

    assert_eq!(
        result.exit_code, 0,
        "stdout: {}\nstderr: {}",
        result.stdout, result.stderr
    );
    let value: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert_eq!(value["summary"]["diagnostics"], 0, "{}", result.stdout);

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn analyze_resolves_vendor_autoload_files_classmap_and_namespaces() {
    let root = temp_dir("vendor-autoload-files-classmap");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("vendor/composer")).unwrap();
    std::fs::create_dir_all(root.join("vendor/thecodingmachine/safe/generated")).unwrap();
    std::fs::create_dir_all(root.join("vendor/thecodingmachine/safe/generated/8.4")).unwrap();
    std::fs::create_dir_all(root.join("vendor/phpunit/phpunit/src/Metadata")).unwrap();
    std::fs::create_dir_all(root.join("vendor/laravel/pulse/src/Recorders")).unwrap();

    std::fs::write(
        root.join("composer.json"),
        r#"{
                "autoload": {
                    "psr-4": {
                        "App\\": "src/"
                    }
                }
            }"#,
    )
    .unwrap();

    let mut safe_files = Vec::new();
    for index in 0..16 {
        let relative = format!("generated/preload-{index}.php");
        std::fs::write(
            root.join("vendor/thecodingmachine/safe").join(&relative),
            "<?php\nnamespace Safe;\n",
        )
        .unwrap();
        safe_files.push(relative);
    }
    safe_files.push("generated/json.php".to_string());

    std::fs::write(
        root.join("vendor/thecodingmachine/safe/generated/json.php"),
        "<?php\nif (PHP_VERSION_ID >= 80400) { require_once __DIR__ . '/8.4/json.php'; }\n",
    )
    .unwrap();
    std::fs::write(
            root.join("vendor/thecodingmachine/safe/generated/8.4/json.php"),
            "<?php\nnamespace Safe;\nfunction json_decode(string $json) { return \\json_decode($json); }\nfunction json_encode($value): string { return \\json_encode($value); }\n",
        )
        .unwrap();
    std::fs::write(
        root.join("vendor/phpunit/phpunit/src/Metadata/Test.php"),
        "<?php\nnamespace PHPUnit\\Framework\\Attributes;\nfinal class Test {}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("vendor/laravel/pulse/src/Recorders/SlowRequests.php"),
        "<?php\nnamespace Laravel\\Pulse\\Recorders;\nfinal class SlowRequests {}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("vendor/composer/installed.json"),
        serde_json::json!({
            "packages": [
                {
                    "name": "thecodingmachine/safe",
                    "install-path": "../thecodingmachine/safe",
                    "autoload": {
                        "files": safe_files
                    }
                },
                {
                    "name": "phpunit/phpunit",
                    "install-path": "../phpunit/phpunit",
                    "autoload": {
                        "classmap": ["src/"]
                    }
                },
                {
                    "name": "laravel/pulse",
                    "install-path": "../laravel/pulse",
                    "autoload": {
                        "psr-4": {
                            "Laravel\\Pulse\\": "src/"
                        }
                    }
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
            root.join("src/Demo.php"),
            "<?php\nnamespace App;\nuse PHPUnit\\Framework\\Attributes\\Test;\nuse Laravel\\Pulse\\Recorders;\n#[Test]\nfinal class Demo { public function run(): void { \\Safe\\json_decode('{}'); \\Safe\\json_encode([]); Recorders\\SlowRequests::class; } }\n",
        )
        .unwrap();

    let result = run_analyze_cli(vec![
        "src/Demo.php".to_string(),
        "--project-root".to_string(),
        root.display().to_string(),
        "--severity".to_string(),
        "warning".to_string(),
        "--format".to_string(),
        "json".to_string(),
    ]);

    assert_eq!(
        result.exit_code, 0,
        "stdout: {}\nstderr: {}",
        result.stdout, result.stderr
    );
    let value: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert_eq!(value["summary"]["diagnostics"], 0, "{}", result.stdout);

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn analyze_resolves_carbon_static_trait_methods_and_laravel_now_helper_return() {
    let root = temp_dir("carbon-static-trait-methods");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("vendor/composer")).unwrap();
    std::fs::create_dir_all(root.join("vendor/nesbot/carbon/src/Carbon/Traits")).unwrap();
    std::fs::create_dir_all(root.join("vendor/laravel/framework/src/Illuminate/Foundation"))
        .unwrap();
    std::fs::create_dir_all(root.join("vendor/laravel/framework/src/Illuminate/Support")).unwrap();

    std::fs::write(
        root.join("composer.json"),
        r#"{
                "autoload": {
                    "psr-4": {
                        "App\\": "src/"
                    }
                }
            }"#,
    )
    .unwrap();
    std::fs::write(
        root.join("vendor/composer/installed.json"),
        serde_json::json!({
            "packages": [
                {
                    "name": "nesbot/carbon",
                    "install-path": "../nesbot/carbon",
                    "autoload": {
                        "psr-4": {
                            "Carbon\\": "src/Carbon/"
                        }
                    }
                },
                {
                    "name": "laravel/framework",
                    "install-path": "../laravel/framework",
                    "autoload": {
                        "psr-4": {
                            "Illuminate\\": "src/Illuminate/"
                        },
                        "files": ["src/Illuminate/Foundation/helpers.php"]
                    }
                }
            ]
        })
        .to_string(),
    )
    .unwrap();

    std::fs::write(
            root.join("vendor/nesbot/carbon/src/Carbon/CarbonInterface.php"),
            "<?php\nnamespace Carbon;\ninterface CarbonInterface { public static function now(): static; }\n",
        )
        .unwrap();
    std::fs::write(
            root.join("vendor/nesbot/carbon/src/Carbon/Traits/Creator.php"),
            "<?php\nnamespace Carbon\\Traits;\ntrait Creator { public static function now(): static { return new static(); } }\n",
        )
        .unwrap();
    std::fs::write(
            root.join("vendor/nesbot/carbon/src/Carbon/Traits/Date.php"),
            "<?php\nnamespace Carbon\\Traits;\nuse Carbon\\CarbonInterface;\n/**\n * @method CarbonInterface addMinutes(int|float $value = 1) Add minutes.\n */\ntrait Date { use Creator; }\n",
        )
        .unwrap();
    std::fs::write(
            root.join("vendor/nesbot/carbon/src/Carbon/Carbon.php"),
            "<?php\nnamespace Carbon;\nuse Carbon\\Traits\\Date;\n/**\n * @method bool isSameYear(\\DateTimeInterface|string $date) Checks if same year. If null passed, compare to now (with the same timezone).\n * @method $this addMinutes(int|float $value = 1) Add minutes.\n */\nclass Carbon extends \\DateTime implements CarbonInterface { use Date; }\n",
        )
        .unwrap();
    std::fs::write(
            root.join("vendor/nesbot/carbon/src/Carbon/CarbonImmutable.php"),
            "<?php\nnamespace Carbon;\nuse Carbon\\Traits\\Date;\n/**\n * @method CarbonImmutable addMinutes(int|float $value = 1) Add minutes.\n */\nclass CarbonImmutable extends \\DateTimeImmutable implements CarbonInterface { use Date; }\n",
        )
        .unwrap();
    std::fs::write(
            root.join("vendor/laravel/framework/src/Illuminate/Support/Carbon.php"),
            "<?php\nnamespace Illuminate\\Support;\nuse Carbon\\Carbon as BaseCarbon;\nclass Carbon extends BaseCarbon {}\n",
        )
        .unwrap();
    std::fs::write(
            root.join("vendor/laravel/framework/src/Illuminate/Foundation/helpers.php"),
            "<?php\nif (! function_exists('now')) { function now($tz = null): \\Illuminate\\Support\\Carbon { return new \\Illuminate\\Support\\Carbon(); } }\n",
        )
        .unwrap();
    std::fs::write(
            root.join("src/Demo.php"),
            "<?php\nnamespace App;\nuse Carbon\\Carbon;\nuse Carbon\\CarbonImmutable;\nfinal class Demo { public function run(): void { Carbon::now()->addMinutes(5); CarbonImmutable::now()->addMinutes(5); now()->addMinutes(5); } }\n",
        )
        .unwrap();

    let result = run_analyze_cli(vec![
        "src/Demo.php".to_string(),
        "--project-root".to_string(),
        root.display().to_string(),
        "--severity".to_string(),
        "warning".to_string(),
        "--format".to_string(),
        "json".to_string(),
    ]);

    assert_eq!(
        result.exit_code, 0,
        "stdout: {}\nstderr: {}",
        result.stdout, result.stderr
    );
    let value: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert_eq!(value["summary"]["diagnostics"], 0, "{}", result.stdout);

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn analyze_lazy_resolves_vendor_enum_property_diagnostics() {
    let root = temp_dir("vendor-enum-property-diagnostics");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("vendor/composer")).unwrap();
    std::fs::create_dir_all(root.join("vendor/monolog/monolog/src/Monolog")).unwrap();

    std::fs::write(
        root.join("composer.json"),
        r#"{
                "autoload": {
                    "psr-4": {
                        "App\\": "src/"
                    }
                }
            }"#,
    )
    .unwrap();
    std::fs::write(
        root.join("vendor/composer/installed.json"),
        serde_json::json!({
            "packages": [
                {
                    "name": "monolog/monolog",
                    "install-path": "../monolog/monolog",
                    "autoload": {
                        "psr-4": {
                            "Monolog\\": "src/Monolog/"
                        }
                    }
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        root.join("vendor/monolog/monolog/src/Monolog/LogRecord.php"),
        "<?php\nnamespace Monolog;\nclass LogRecord { public Level $level; }\n",
    )
    .unwrap();
    std::fs::write(
            root.join("vendor/monolog/monolog/src/Monolog/Level.php"),
            "<?php\nnamespace Monolog;\nenum Level: int { case Debug = 100; public function getName(): string { return 'debug'; } }\n",
        )
        .unwrap();
    std::fs::write(
            root.join("src/Demo.php"),
            "<?php\nnamespace App;\nuse Monolog\\LogRecord;\nfinal class Demo { public function run(LogRecord $record): int { return $record->level->value; } }\n",
        )
        .unwrap();

    let result = run_analyze_cli(vec![
        "src/Demo.php".to_string(),
        "--project-root".to_string(),
        root.display().to_string(),
        "--severity".to_string(),
        "warning".to_string(),
        "--format".to_string(),
        "json".to_string(),
    ]);

    assert_eq!(
        result.exit_code, 0,
        "stdout: {}\nstderr: {}",
        result.stdout, result.stderr
    );
    let value: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert_eq!(value["summary"]["diagnostics"], 0, "{}", result.stdout);

    let _ = std::fs::remove_dir_all(root);
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "php-lsp-analyze-{label}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
