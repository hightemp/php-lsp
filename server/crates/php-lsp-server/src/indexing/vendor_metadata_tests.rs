use super::*;
use serde_json::json;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "php-lsp-vendor-metadata-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("vendor/composer")).unwrap();
        Self(root)
    }

    fn write(&self, path: &str, source: &str) {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }

    fn map(&self, packages: serde_json::Value) -> VendorAutoloadMap {
        self.write("vendor/composer/installed.json", &packages.to_string());
        parse_vendor_autoload_map(&self.0.join("vendor")).expect("metadata map")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let cache_path = cache::cache_file_path(&self.0);
        if let Some(dir) = cache_path.parent().and_then(Path::parent) {
            let _ = std::fs::remove_dir_all(dir);
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn vendor_metadata_rejects_hostile_install_paths_without_dropping_good_packages() {
    let fixture = Fixture::new();
    let mut packages = vec![
        json!({"name":"acme/good", "install-path":"../acme/good", "autoload":{"psr-4":{"Good\\":"src/"}}}),
    ];
    for path in [
        fixture.0.join("outside").to_string_lossy().into_owned(),
        "../../outside".into(),
        "..\\..\\outside".into(),
        "C:\\outside".into(),
        "C:outside".into(),
        "\\\\host\\share".into(),
        "/outside".into(),
        "\\outside".into(),
        "".into(),
    ] {
        packages.push(json!({"name":"acme/bad", "install-path":path, "autoload":{"psr-4":{"Bad\\":"src/"},"files":["bootstrap.php"],"classmap":["."]}}));
    }
    packages
        .push(json!({"name":"acme/null", "install-path":null,"autoload":{"files":["bad.php"]}}));
    packages.push(json!({"autoload":{"files":["bad.php"]}}));
    let map = fixture.map(json!({"packages":packages}));
    assert_eq!(map.psr4.len(), 1, "hostile packages must be skipped");
    assert_eq!(
        map.psr4[0].directories,
        vec![fixture.0.join("vendor/acme/good/src")]
    );
    assert!(map.files.is_empty() && map.classmap.is_empty());
}

#[test]
fn vendor_metadata_uses_composer_relative_install_path_and_legacy_package_name() {
    let fixture = Fixture::new();
    let map = fixture.map(json!([
        {"name":"acme/explicit","install-path":"local/pkg","autoload":{"files":["init.php"]}},
        {"name":"acme/legacy","autoload":{"files":["init.php"]}},
        {"name":"../../outside","autoload":{"files":["bad.php"]}},
        {"name":"acme/meta","type":"metapackage","autoload":{"files":["bad.php"]}}
    ]));
    assert_eq!(
        map.files,
        vec![
            fixture.0.join("vendor/composer/local/pkg/init.php"),
            fixture.0.join("vendor/acme/legacy/init.php")
        ]
    );
}

#[test]
fn vendor_metadata_normalizes_and_checks_every_autoload_path() {
    let fixture = Fixture::new();
    let paths = json!([
        "src/./nested/../",
        "src\\Legacy",
        "../../../outside",
        "..\\..\\..\\outside",
        "/outside",
        "C:\\outside",
        "\\\\host\\share",
        "bad\u{0}path"
    ]);
    let map = fixture.map(
        json!({"packages":[{"name":"acme/pkg","install-path":"../acme/pkg","autoload":{
            "psr-4":{"Good\\":paths},"psr-0":{"Legacy_":paths},"files":paths,"classmap":paths
        }}]}),
    );
    let expected = vec![
        fixture.0.join("vendor/acme/pkg/src"),
        fixture.0.join("vendor/acme/pkg/src/Legacy"),
    ];
    assert_eq!(map.psr4[0].directories, expected);
    assert_eq!(map.psr0[0].directories, expected);
    assert_eq!(map.files, expected);
    assert_eq!(map.classmap, expected);
}

#[test]
fn vendor_metadata_checks_final_candidates_and_namespace_probes() {
    let fixture = Fixture::new();
    fixture.write("outside/Secret.php", "<?php class Secret {}");
    let outside = fixture.0.join("outside");
    let mut map = fixture.map(json!({"packages":[]}));
    map.psr4.push(VendorNamespaceMapping {
        prefix: "".into(),
        directories: vec![outside.clone(), fixture.0.join("vendor/../outside")],
    });
    map.psr0 = map.psr4.clone();
    map.classmap = vec![outside.clone()];
    map.files = vec![outside.join("Secret.php")];
    assert!(resolve_vendor_paths_from_map("Secret", &map).is_none());
    assert!(!vendor_namespace_exists_from_map("Secret", &map));
    assert!(vendor_autoload_file_paths_from_map(&map, &fixture.0, &[]).is_empty());

    map.psr4[0].directories = vec![fixture.0.join("vendor")];
    for fqn in [
        "../outside/Secret",
        "..\\outside\\Secret",
        "C:\\outside\\Secret",
        "A\\\\B",
        "",
        "Acme/Secret",
    ] {
        assert!(
            resolve_vendor_paths_from_map(fqn, &map).is_none(),
            "invalid FQN: {fqn}"
        );
        assert!(
            !vendor_namespace_exists_from_map(fqn, &map),
            "invalid namespace: {fqn}"
        );
    }
}

#[cfg(unix)]
#[test]
fn vendor_metadata_preserves_external_package_directory_and_file_symlinks() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    fixture.write(
        "library/src/Subject.php",
        "<?php namespace Acme; class Subject {}",
    );
    fixture.write(
        "library/legacy/Legacy/Subject.php",
        "<?php class Legacy_Subject {}",
    );
    fixture.write(
        "library/bootstrap.php",
        "<?php require __DIR__ . '/src/../bootstrap.php'; require __DIR__ . '/helper.php';",
    );
    fixture.write("helper.php", "<?php function linked_helper() {}");
    std::fs::create_dir_all(fixture.0.join("vendor/acme")).unwrap();
    symlink(fixture.0.join("library"), fixture.0.join("vendor/acme/a")).unwrap();
    symlink(fixture.0.join("library"), fixture.0.join("vendor/acme/z")).unwrap();
    symlink(
        fixture.0.join("helper.php"),
        fixture.0.join("library/helper.php"),
    )
    .unwrap();
    symlink(
        fixture.0.join("library/src"),
        fixture.0.join("library/mapped"),
    )
    .unwrap();
    symlink(fixture.0.join("library"), fixture.0.join("library/cycle")).unwrap();
    let map = fixture.map(json!({"packages":[
        {"name":"acme/a","install-path":"../acme/a","autoload":{"psr-4":{"Acme\\":"mapped/"},"psr-0":{"Legacy_":"legacy/"},"files":["bootstrap.php"],"classmap":["src/","cycle/src/"]}},
        {"name":"acme/z","install-path":"../acme/z","autoload":{"psr-4":{"Acme\\":"mapped/"}}}
    ]}));
    let resolved = resolve_vendor_paths_from_map_with_limits(
        "Acme\\Subject",
        &map,
        TraversalLimits {
            max_entries: Some(100),
            max_files: Some(10),
        },
        Some(&fixture.0),
        &[],
    )
    .unwrap();
    assert_eq!(
        resolved.paths,
        vec![fixture.0.join("vendor/acme/a/cycle/src/Subject.php")]
    );
    assert_eq!(resolved.physical_files.len(), 1);
    assert!(resolved.physical_files[0]
        .paths
        .iter()
        .any(|path| path.logical_path == fixture.0.join("vendor/acme/z/mapped/Subject.php")));
    assert!(resolved
        .symlink_aliases
        .iter()
        .any(|alias| alias.logical_path == fixture.0.join("vendor/acme/a")));
    let entries = vendor_autoload_file_paths_from_map(&map, &fixture.0, &[]);
    assert_eq!(
        entries,
        vec![
            fixture.0.join("vendor/acme/a/bootstrap.php"),
            fixture.0.join("vendor/acme/a/helper.php")
        ]
    );
    let excluded = vec![
        PathBuf::from("vendor/acme/a"),
        PathBuf::from("vendor/acme/z"),
    ];
    assert!(resolve_vendor_paths_from_map_with_limits(
        "Acme\\Subject",
        &map,
        TraversalLimits::default(),
        Some(&fixture.0),
        &excluded
    )
    .is_none());
    assert!(vendor_autoload_file_paths_from_map(&map, &fixture.0, &excluded).is_empty());
    let capped = resolve_vendor_paths_from_map_with_limits(
        "Acme\\Subject",
        &map,
        TraversalLimits {
            max_entries: Some(0),
            max_files: Some(0),
        },
        Some(&fixture.0),
        &[],
    );
    assert!(capped.is_none());
}

#[test]
fn vendor_metadata_ignores_dependency_autoload_dev_including_dev_packages() {
    let fixture = Fixture::new();
    let map = fixture.map(json!({"dev":true,"dev-package-names":["acme/pkg"],"packages":[{
        "name":"acme/pkg","install-path":"../acme/pkg",
        "autoload":{"psr-4":{"Runtime\\":"src/"},"files":["bootstrap.php"]},
        "autoload-dev":{"psr-4":{"Tests\\":"tests/"},"files":["dev-bootstrap.php"],"classmap":["tests/"]}
    }]}));
    assert_eq!(map.psr4.len(), 1);
    assert_eq!(
        map.files,
        vec![fixture.0.join("vendor/acme/pkg/bootstrap.php")]
    );
    assert!(map.classmap.is_empty());
}

#[test]
fn vendor_metadata_resolves_psr0_full_prefix_and_class_name_underscores() {
    let fixture = Fixture::new();
    for (fqn, relative) in [
        (
            "Acme\\Name_Space\\Legacy_Class",
            "src/Acme/Name_Space/Legacy/Class.php",
        ),
        ("PEAR_Legacy_Class", "legacy/PEAR/Legacy/Class.php"),
        ("Fallback\\Subject", "fallback/Fallback/Subject.php"),
    ] {
        fixture.write(
            &format!("vendor/acme/pkg/{relative}"),
            &format!("<?php // {fqn}"),
        );
    }
    let map = fixture.map(
        json!({"packages":[{"name":"acme/pkg","install-path":"../acme/pkg","autoload":{
            "psr-0":{"Acme\\":["missing/","src/"],"PEAR_":"legacy/","":"fallback/"}
        }}]}),
    );
    for (fqn, relative) in [
        (
            "\\Acme\\Name_Space\\Legacy_Class",
            "src/Acme/Name_Space/Legacy/Class.php",
        ),
        ("PEAR_Legacy_Class", "legacy/PEAR/Legacy/Class.php"),
        ("Fallback\\Subject", "fallback/Fallback/Subject.php"),
    ] {
        let resolution = resolve_vendor_paths_from_map_with_limits(
            fqn,
            &map,
            TraversalLimits::default(),
            Some(&fixture.0),
            &[],
        )
        .expect("PSR-0 resolution");
        assert_eq!(
            resolution.paths,
            vec![fixture.0.join("vendor/acme/pkg").join(relative)]
        );
    }
    assert!(vendor_namespace_exists_from_map("Acme\\Name_Space", &map));
}

#[test]
fn vendor_metadata_entrypoint_include_chain_stays_inside_logical_vendor() {
    let fixture = Fixture::new();
    fixture.write(
        "vendor/acme/pkg/bootstrap.php",
        "<?php require __DIR__ . '/./inside.php'; require __DIR__ . '/../../../outside.php';",
    );
    fixture.write("vendor/acme/pkg/inside.php", "<?php function inside() {} ");
    fixture.write("outside.php", "<?php function outside() {} ");
    let map = fixture.map(json!({"packages":[{"name":"acme/pkg","install-path":"../acme/pkg","autoload":{"files":["bootstrap.php"]}}]}));
    assert_eq!(
        vendor_autoload_file_paths_from_map(&map, &fixture.0, &[]),
        vec![
            fixture.0.join("vendor/acme/pkg/bootstrap.php"),
            fixture.0.join("vendor/acme/pkg/inside.php")
        ]
    );
}

#[test]
fn vendor_metadata_psr4_precedes_psr0_and_directory_order_is_preserved() {
    let fixture = Fixture::new();
    for path in [
        "vendor/acme/pkg/z-psr4/Subject.php",
        "vendor/acme/pkg/m-psr4/Subject.php",
        "vendor/acme/pkg/a-psr0/Acme/Subject.php",
    ] {
        fixture.write(path, "<?php namespace Acme; class Subject {}");
    }
    let map = fixture.map(
        json!({"packages":[{"name":"acme/pkg","install-path":"../acme/pkg","autoload":{
            "psr-4":{"Acme\\":["z-psr4/", "m-psr4/"]}, "psr-0":{"Acme\\":"a-psr0/"}
        }}]}),
    );
    let resolved = resolve_vendor_paths_from_map_with_limits(
        "Acme\\Subject",
        &map,
        TraversalLimits::default(),
        Some(&fixture.0),
        &[],
    )
    .unwrap();
    assert_eq!(
        resolved.paths,
        vec![
            fixture.0.join("vendor/acme/pkg/z-psr4/Subject.php"),
            fixture.0.join("vendor/acme/pkg/m-psr4/Subject.php"),
            fixture.0.join("vendor/acme/pkg/a-psr0/Acme/Subject.php")
        ]
    );
    for limits in [
        TraversalLimits {
            max_files: Some(1),
            max_entries: None,
        },
        TraversalLimits {
            max_files: None,
            max_entries: Some(1),
        },
    ] {
        assert!(
            resolve_vendor_paths_from_map_with_limits(
                "Acme\\Subject",
                &map,
                limits,
                Some(&fixture.0),
                &[]
            )
            .is_none(),
            "a truncated walk must not select a lower-priority definition"
        );
    }
}

#[test]
fn vendor_metadata_psr0_accepts_partial_prefixes_and_leading_underscores() {
    let fixture = Fixture::new();
    fixture.write(
        "vendor/acme/pkg/src/Foo.php",
        "<?php class _Foo {} class __Foo {}",
    );
    fixture.write(
        "vendor/acme/pkg/src/Acme/Foo.php",
        "<?php namespace Acme; class _Foo {}",
    );
    let map = fixture.map(json!({"packages":[{"name":"acme/pkg","install-path":"../acme/pkg","autoload":{"psr-0":{"_":"src/","Ac":"src/"}}}]}));
    for (fqn, relative) in [
        ("_Foo", "Foo.php"),
        ("__Foo", "Foo.php"),
        ("Acme\\_Foo", "Acme/Foo.php"),
    ] {
        let resolution = resolve_vendor_paths_from_map_with_limits(
            fqn,
            &map,
            TraversalLimits::default(),
            Some(&fixture.0),
            &[],
        )
        .expect("PSR-0 lookup");
        assert_eq!(
            resolution.paths,
            vec![fixture.0.join("vendor/acme/pkg/src").join(relative)]
        );
    }
    assert!(vendor_namespace_exists_from_map("Acme", &map));
}

#[tokio::test(flavor = "current_thread")]
async fn vendor_metadata_preload_filters_forbidden_sources_even_with_valid_disk_cache() {
    let fixture = Fixture::new();
    fixture.write(
        "vendor/acme/pkg/bootstrap.php",
        "<?php function allowed_bootstrap() {}",
    );
    fixture.write("outside.php", "<?php class CachedOutside {}");
    fixture.write("vendor/acme/pkg/dev.php", "<?php class CachedDevOnly {}");
    fixture.map(json!({"packages":[{
        "name":"acme/pkg","install-path":"../acme/pkg",
        "autoload":{"files":["bootstrap.php", "../../../outside.php"]},
        "autoload-dev":{"files":["dev.php"]}
    }]}));
    let config = vendor_index_cache_config(
        &fixture.0,
        PhpVersion::DEFAULT,
        &[],
        TraversalLimits::default(),
    );
    let cached_index = WorkspaceIndex::new();
    let sources = [
        "vendor/acme/pkg/bootstrap.php",
        "outside.php",
        "vendor/acme/pkg/dev.php",
    ]
    .into_iter()
    .map(|relative| {
        let path = fixture.0.join(relative);
        assert!(parse_and_index_php_file(&cached_index, &path));
        CacheSourceFile::workspace(&fixture.0, &path).unwrap()
    })
    .collect::<Vec<_>>();
    let snapshot = cache::build_cache_from_sources(&cached_index, &fixture.0, &sources, &config);
    let cache_path = cache::cache_file_path_for_namespace(&fixture.0, CacheNamespace::Vendor);
    cache::save_cache_atomic(&cache_path, &snapshot).unwrap();
    let restored = WorkspaceIndex::new();
    assert!(
        load_cached_vendor_file(&restored, &fixture.0, &sources[0].path, &config),
        "positive source must actually load from disk cache"
    );
    assert!(restored.resolve_fqn("allowed_bootstrap").is_some());
    assert!(restored.resolve_fqn("CachedOutside").is_none());
    assert!(restored.resolve_fqn("CachedDevOnly").is_none());

    let target = Arc::new(WorkspaceIndex::new());
    let loaded = preload_vendor_entrypoints(
        target.clone(),
        &fixture.0,
        &[],
        TraversalLimits::default(),
        PhpVersion::DEFAULT,
        &Arc::new(Mutex::new(VendorAutoloadCache::default())),
        &Arc::new(Mutex::new(VendorFileLru::default())),
        &Arc::new(tokio::sync::RwLock::new(0)),
        None,
    )
    .await;
    assert_eq!(loaded, 1);
    assert!(target.resolve_fqn("allowed_bootstrap").is_some());
    assert!(target.resolve_fqn("CachedOutside").is_none());
    assert!(target.resolve_fqn("CachedDevOnly").is_none());
}
