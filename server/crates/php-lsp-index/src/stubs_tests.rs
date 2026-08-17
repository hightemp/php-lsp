use super::*;

#[test]
#[allow(clippy::len_zero)]
fn test_default_extensions_not_empty() {
    assert!(DEFAULT_EXTENSIONS.len() > 0);
    assert!(DEFAULT_EXTENSIONS.contains(&"Core"));
    assert!(DEFAULT_EXTENSIONS.contains(&"standard"));
    assert!(DEFAULT_EXTENSIONS.contains(&"PDO"));
}

#[test]
fn test_discover_stub_extensions_uses_available_php_stub_dirs() {
    let stubs_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/stubs");
    if !stubs_are_available(&stubs_path) {
        eprintln!(
            "Skipping stubs discovery test: stubs not initialized at {}",
            stubs_path.display()
        );
        return;
    }

    let extensions = discover_stub_extensions(&stubs_path);
    assert!(extensions.contains(&"Core".to_string()));
    assert!(extensions.contains(&"standard".to_string()));
    assert!(extensions.contains(&"libxml".to_string()));
    assert!(extensions.contains(&"posix".to_string()));
    assert!(extensions.contains(&"zip".to_string()));
    assert!(
        !extensions
            .iter()
            .any(|extension| extension.starts_with('.')),
        "metadata directories should not be treated as stub extensions: {extensions:?}"
    );
    for skipped in ["tests", "meta", "vendor"] {
        assert!(
            !extensions.iter().any(|extension| extension == skipped),
            "non-extension directory should not be treated as stub extension: {skipped}"
        );
    }
}

#[test]
fn test_discover_stub_extensions_skips_vendor_even_with_php_files() {
    let root = std::env::temp_dir().join(format!("php-lsp-stub-discovery-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("Core")).expect("create Core stubs dir");
    std::fs::write(
        root.join("Core/Core.php"),
        "<?php function strlen(string $s): int;",
    )
    .expect("write Core stub");
    std::fs::create_dir_all(root.join("vendor/acme/package")).expect("create vendor dir");
    std::fs::write(
        root.join("vendor/acme/package/Helper.php"),
        "<?php function should_not_be_builtin(): void;",
    )
    .expect("write vendor PHP file");

    let extensions = discover_stub_extensions(&root);

    assert_eq!(extensions, vec!["Core".to_string()]);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn test_stub_walkers_collect_and_count_real_nested_php_files() {
    let root = std::env::temp_dir().join(format!("php-lsp-real-stub-walk-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("Core/nested")).expect("create nested stubs dir");
    std::fs::write(
        root.join("Core/Core.php"),
        "<?php function core_fn(): void;",
    )
    .expect("write root stub");
    std::fs::write(
        root.join("Core/nested/Extra.php"),
        "<?php function extra_fn(): void;",
    )
    .expect("write nested stub");
    std::fs::write(root.join("Core/README.txt"), "not a stub").expect("write non-PHP file");

    let files = collect_extension_stub_files(&root, "Core");
    assert_eq!(
        files,
        vec![
            root.join("Core/Core.php"),
            root.join("Core/nested/Extra.php")
        ]
    );
    assert_eq!(count_php_stub_files(&root), 2);
    assert_eq!(discover_stub_extensions(&root), vec!["Core".to_string()]);
    assert!(is_real_stub_extension_directory(&root, "Core"));
    assert!(is_real_stub_file(&root, Path::new("Core/Core.php")));
    assert!(!is_real_stub_file(&root, Path::new("../outside.php")));

    std::fs::remove_dir_all(root).expect("remove nested stubs tree");
}

#[test]
fn test_collect_extension_stub_files_rejects_path_components() {
    let root = std::env::temp_dir().join(format!(
        "php-lsp-extension-component-{}",
        std::process::id()
    ));
    let external =
        std::env::temp_dir().join(format!("php-lsp-extension-external-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&external);
    std::fs::create_dir_all(root.join("Core/nested")).expect("create extension tree");
    std::fs::create_dir_all(&external).expect("create external extension tree");
    std::fs::write(
        root.join("Core/Core.php"),
        "<?php function core_fn(): void;",
    )
    .expect("write valid extension stub");
    std::fs::write(
        root.join("Core/nested/Nested.php"),
        "<?php function nested_fn(): void;",
    )
    .expect("write nested extension stub");
    std::fs::write(
        external.join("Outside.php"),
        "<?php function outside_fn(): void;",
    )
    .expect("write external extension stub");

    assert_eq!(collect_extension_stub_files(&root, "Core").len(), 2);
    for invalid in ["", ".", "..", "../Core", "Core/nested"] {
        assert!(
            collect_extension_stub_files(&root, invalid).is_empty(),
            "path-like extension name must be rejected: {invalid:?}"
        );
    }
    let external_extension = external.to_string_lossy();
    assert!(collect_extension_stub_files(&root, external_extension.as_ref()).is_empty());
    let index = WorkspaceIndex::new();
    assert_eq!(load_stubs(&index, &root, &[external_extension.as_ref()]), 0);
    assert!(load_stub_file(&index, &root, "../external", &external.join("Outside.php")).is_none());

    std::fs::remove_dir_all(root).expect("remove extension tree");
    std::fs::remove_dir_all(external).expect("remove external extension tree");
}

#[cfg(unix)]
#[test]
fn test_stub_walkers_skip_cycles_external_links_and_broken_links() {
    use std::os::unix::fs::symlink;

    let root =
        std::env::temp_dir().join(format!("php-lsp-symlink-stub-walk-{}", std::process::id()));
    let external =
        std::env::temp_dir().join(format!("php-lsp-external-stub-walk-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&external);
    std::fs::create_dir_all(root.join("Core/nested")).expect("create stubs tree");
    std::fs::create_dir_all(&external).expect("create external tree");
    std::fs::write(
        root.join("Core/Core.php"),
        "<?php function core_fn(): void;",
    )
    .expect("write real stub");
    std::fs::write(
        external.join("Outside.php"),
        "<?php function outside_fn(): void;",
    )
    .expect("write external stub");

    symlink(root.join("Core"), root.join("Core/nested/back-to-core"))
        .expect("create directory cycle");
    symlink(&external, root.join("Core/external-directory")).expect("link external directory");
    symlink(
        external.join("Outside.php"),
        root.join("Core/ExternalFile.php"),
    )
    .expect("link external file");
    symlink(root.join("missing"), root.join("Core/broken-link")).expect("create broken link");
    symlink(&external, root.join("LinkedExtension")).expect("link top-level extension");

    assert_eq!(
        collect_extension_stub_files(&root, "Core"),
        vec![root.join("Core/Core.php")]
    );
    assert!(collect_extension_stub_files(&root, "LinkedExtension").is_empty());
    assert!(!is_real_stub_extension_directory(&root, "LinkedExtension"));
    assert_eq!(count_php_stub_files(&root), 1);
    assert_eq!(discover_stub_extensions(&root), vec!["Core".to_string()]);
    assert!(!is_real_stub_file(
        &root,
        Path::new("Core/ExternalFile.php")
    ));
    assert!(!is_real_stub_file(
        &root,
        Path::new("Core/external-directory/Outside.php")
    ));
    assert!(!is_real_stub_file(
        &root,
        Path::new("LinkedExtension/Outside.php")
    ));

    std::fs::remove_dir_all(root).expect("remove symlink stubs tree");
    std::fs::remove_dir_all(external).expect("remove external stubs tree");
}

#[cfg(unix)]
#[test]
fn test_stub_walkers_accept_a_symlinked_configured_root() {
    use std::os::unix::fs::symlink;

    let actual =
        std::env::temp_dir().join(format!("php-lsp-actual-stub-root-{}", std::process::id()));
    let linked =
        std::env::temp_dir().join(format!("php-lsp-linked-stub-root-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&actual);
    let _ = std::fs::remove_file(&linked);
    std::fs::create_dir_all(actual.join("Core")).expect("create actual root");
    std::fs::write(
        actual.join("Core/Core.php"),
        "<?php function linked_root_fn(): void;",
    )
    .expect("write actual root stub");
    symlink(&actual, &linked).expect("link configured root");

    let files = collect_extension_stub_files(&linked, "Core");
    assert_eq!(files.len(), 1);
    assert_eq!(
        files[0].file_name().and_then(|name| name.to_str()),
        Some("Core.php")
    );
    assert_eq!(count_php_stub_files(&linked), 1);
    assert_eq!(discover_stub_extensions(&linked), vec!["Core".to_string()]);
    assert!(is_real_stub_extension_directory(&linked, "Core"));
    assert!(is_real_stub_file(&linked, Path::new("Core/Core.php")));

    std::fs::remove_file(linked).expect("remove linked root");
    std::fs::remove_dir_all(actual).expect("remove actual root");
}

#[test]
fn test_collect_extension_stub_files_recurses_and_uri_preserves_relative_path() {
    let stubs_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/stubs");
    if !stubs_path.join("snappy/snappy/snappy.php").is_file() {
        eprintln!(
            "Skipping recursive stubs test: nested snappy stub not initialized at {}",
            stubs_path.display()
        );
        return;
    }

    let files = collect_extension_stub_files(&stubs_path, "snappy");
    let nested = stubs_path.join("snappy/snappy/snappy.php");

    assert!(
        files.iter().any(|file| file == &nested),
        "expected recursive stub collection to include {nested:?}, got {files:?}"
    );
    assert_eq!(
        stub_file_uri(&stubs_path, "snappy", &nested),
        "phpstub://snappy/snappy/snappy.php"
    );
}

#[test]
fn test_stub_file_uri_uses_stubs_root_not_first_matching_path_component() {
    let stubs_path = std::env::temp_dir()
        .join("snappy")
        .join("project")
        .join("server/data/stubs");
    let nested = stubs_path.join("snappy/snappy/snappy.php");

    assert_eq!(
        stub_file_uri(&stubs_path, "snappy", &nested),
        "phpstub://snappy/snappy/snappy.php"
    );
}

fn stubs_are_available(stubs_path: &Path) -> bool {
    // Check that the submodule is actually initialized (not just an empty dir)
    stubs_path.join("Core/Core.php").is_file()
}

fn bundled_stubs_are_required() -> bool {
    std::env::var_os("CI").is_some() || std::env::var_os("PHP_LSP_REQUIRE_BUNDLED_STUBS").is_some()
}

fn bundled_stubs_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../client/stubs")
}

#[test]
fn test_load_stubs_with_real_data() {
    // This test uses actual phpstorm-stubs if available
    let stubs_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/stubs");

    if !stubs_are_available(&stubs_path) {
        // Skip if stubs are not available (e.g., in CI without submodule)
        eprintln!(
            "Skipping stubs test: stubs not initialized at {}",
            stubs_path.display()
        );
        return;
    }

    let index = WorkspaceIndex::new();
    let loaded = load_stubs(&index, &stubs_path, &["Core"]);

    assert!(loaded > 0, "Should have loaded at least one stub file");

    // Core should define basic PHP classes like stdClass, Exception, etc.
    // Check that some known built-in class exists
    let has_builtin = index
        .types
        .iter()
        .any(|entry| entry.value().modifiers.is_builtin);
    assert!(has_builtin, "Should have at least one built-in type");
}

#[test]
fn test_load_stubs_nonexistent_extension() {
    let stubs_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/stubs");

    if !stubs_are_available(&stubs_path) {
        return;
    }

    let index = WorkspaceIndex::new();
    let loaded = load_stubs(&index, &stubs_path, &["nonexistent_extension_xyz"]);
    assert_eq!(loaded, 0);
}

#[test]
fn test_load_multiple_extensions() {
    let stubs_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/stubs");

    if !stubs_are_available(&stubs_path) {
        return;
    }

    let index = WorkspaceIndex::new();
    let loaded = load_stubs(&index, &stubs_path, &["Core", "standard", "date"]);

    assert!(
        loaded >= 3,
        "Should have loaded files from multiple extensions, got {}",
        loaded
    );
}

#[test]
fn test_bundled_stubs_expose_core_builtin_symbols() {
    let stubs_path = bundled_stubs_path();
    if !stubs_are_available(&stubs_path) {
        let message = format!(
            "bundled stubs not initialized at {}; run scripts/bundle-stubs.sh",
            stubs_path.display()
        );
        if bundled_stubs_are_required() {
            panic!("{message}");
        }
        eprintln!("Skipping bundled stubs test: {message}");
        return;
    }

    let index = WorkspaceIndex::new();
    let loaded = load_stubs(
        &index,
        &stubs_path,
        &["Core", "standard", "SPL", "SimpleXML", "soap"],
    );

    assert!(
        loaded >= 20,
        "bundled stubs should load core/default files, got {loaded}"
    );

    for fqn in ["stdClass", "Exception", "ArrayObject", "SimpleXMLElement"] {
        let symbol = index
            .resolve_fqn(fqn)
            .unwrap_or_else(|| panic!("missing bundled built-in type: {fqn}"));
        assert!(
            symbol.modifiers.is_builtin,
            "bundled symbol should be marked built-in: {fqn}"
        );
    }

    for fqn in ["array_map", "strlen"] {
        let symbol = index
            .resolve_fqn(fqn)
            .unwrap_or_else(|| panic!("missing bundled built-in function: {fqn}"));
        assert!(
            symbol.modifiers.is_builtin,
            "bundled function should be marked built-in: {fqn}"
        );
    }
}
