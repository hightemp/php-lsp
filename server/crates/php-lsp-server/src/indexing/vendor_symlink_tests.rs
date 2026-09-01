use super::*;
use std::os::unix::fs::symlink;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn psr4_aliases_choose_one_physical_file_and_lexical_logical_uri() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "php-lsp-vendor-psr4-alias-{}-{nonce}",
        std::process::id()
    ));
    let external = std::env::temp_dir().join(format!(
        "php-lsp-vendor-psr4-external-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(root.join("vendor/a-package")).expect("create a package");
    std::fs::create_dir_all(root.join("vendor/z-package")).expect("create z package");
    std::fs::create_dir_all(&external).expect("create external source");
    std::fs::write(
        external.join("Subject.php"),
        "<?php namespace Vendor\\Package; class Subject {}",
    )
    .expect("write vendor class");
    symlink(&external, root.join("vendor/a-package/src")).expect("create a alias");
    symlink(&external, root.join("vendor/z-package/src")).expect("create z alias");

    let map = VendorAutoloadMap {
        psr4: vec![VendorPsr4Mapping {
            prefix: "Vendor\\Package\\".to_string(),
            directories: vec![
                root.join("vendor/z-package/src"),
                root.join("vendor/a-package/src"),
            ],
        }],
        ..VendorAutoloadMap::default()
    };
    let resolution = resolve_vendor_paths_from_map_with_limits(
        "Vendor\\Package\\Subject",
        &map,
        TraversalLimits::default(),
        Some(&root),
        &[],
    )
    .expect("vendor resolution");

    assert_eq!(
        resolution.paths,
        vec![root.join("vendor/a-package/src/Subject.php")]
    );
    assert_eq!(resolution.physical_files.len(), 1);
    assert_eq!(
        resolution.physical_files[0]
            .paths
            .iter()
            .map(|path| path.logical_path.clone())
            .collect::<Vec<_>>(),
        vec![
            root.join("vendor/a-package/src/Subject.php"),
            root.join("vendor/z-package/src/Subject.php"),
        ]
    );

    std::fs::remove_dir_all(root).expect("remove workspace");
    std::fs::remove_dir_all(external).expect("remove external source");
}

#[test]
fn missing_psr4_class_keeps_external_directory_alias_for_future_create_events() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "php-lsp-vendor-psr4-missing-{}-{nonce}",
        std::process::id()
    ));
    let external = std::env::temp_dir().join(format!(
        "php-lsp-vendor-psr4-missing-external-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(root.join("vendor/package")).expect("create package");
    std::fs::create_dir_all(&external).expect("create external source");
    symlink(&external, root.join("vendor/package/src")).expect("create source alias");

    let map = VendorAutoloadMap {
        psr4: vec![VendorPsr4Mapping {
            prefix: "Vendor\\Package\\".to_string(),
            directories: vec![root.join("vendor/package/src")],
        }],
        ..VendorAutoloadMap::default()
    };
    let resolution = resolve_vendor_paths_from_map_with_limits(
        "Vendor\\Package\\CreatedLater",
        &map,
        TraversalLimits::default(),
        Some(&root),
        &[],
    )
    .expect("alias-only vendor resolution");

    assert!(resolution.paths.is_empty());
    assert!(resolution
        .symlink_aliases
        .iter()
        .any(|alias| alias.logical_path == root.join("vendor/package/src")));

    std::fs::remove_dir_all(root).expect("remove workspace");
    std::fs::remove_dir_all(external).expect("remove external source");
}
