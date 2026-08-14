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
fn test_resolve_class_psr0_keeps_namespace_underscores() {
    let json = r#"{
            "autoload": {
                "psr-0": {
                    "App\\": "src/"
                }
            }
        }"#;
    let map = parse_composer_json_str(json, Path::new("/project")).unwrap();
    let paths = map.resolve_class_to_paths("App\\Foo_Bar\\Baz");
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0], PathBuf::from("/project/src/Foo_Bar/Baz.php"));
}

#[test]
fn test_resolve_class_psr0_maps_only_class_name_underscores() {
    let json = r#"{
            "autoload": {
                "psr-0": {
                    "App\\": "src/"
                }
            }
        }"#;
    let map = parse_composer_json_str(json, Path::new("/project")).unwrap();
    let paths = map.resolve_class_to_paths("App\\Foo\\Legacy_Class");
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0], PathBuf::from("/project/src/Foo/Legacy/Class.php"));
}

#[test]
fn test_resolve_class_psr0_global_pear_style_class() {
    let json = r#"{
            "autoload": {
                "psr-0": {
                    "": "legacy/"
                }
            }
        }"#;
    let map = parse_composer_json_str(json, Path::new("/project")).unwrap();
    let paths = map.resolve_class_to_paths("Legacy_Class_Name");
    assert_eq!(paths.len(), 1);
    assert_eq!(
        paths[0],
        PathBuf::from("/project/legacy/Legacy/Class/Name.php")
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
