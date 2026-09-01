use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_root(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "php-lsp-fs-walk-{label}-{}-{nanos}",
        std::process::id()
    ))
}

fn php_walk(roots: &[PathBuf], limits: TraversalLimits) -> FileWalkOutcome {
    walk_files(
        roots,
        limits,
        |_| false,
        |_, _| true,
        |path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("php"))
        },
        || None,
    )
}

#[test]
fn walk_is_deterministic_and_deduplicates_overlapping_roots() {
    let root = temp_root("deterministic");
    std::fs::create_dir_all(root.join("src/nested")).expect("create tree");
    for relative in ["src/Z.php", "src/A.php", "src/nested/B.php"] {
        std::fs::write(root.join(relative), "<?php").expect("write PHP file");
    }

    let outcome = php_walk(
        &[root.join("src/nested"), root.join("src")],
        TraversalLimits::default(),
    );
    assert_eq!(
        outcome.files,
        vec![
            root.join("src/A.php"),
            root.join("src/Z.php"),
            root.join("src/nested/B.php"),
        ]
    );
    assert_eq!(outcome.stats.visited_directories, 2);
    std::fs::remove_dir_all(root).expect("remove tree");
}

#[test]
fn max_entries_exactly_fits_a_complete_directory() {
    let root = temp_root("exact-entry-budget");
    std::fs::create_dir_all(&root).expect("create tree");
    std::fs::write(root.join("A.php"), "<?php").expect("write PHP file");

    let outcome = php_walk(
        std::slice::from_ref(&root),
        TraversalLimits {
            max_files: None,
            max_entries: Some(2),
        },
    );
    assert_eq!(outcome.files, vec![root.join("A.php")]);
    assert_eq!(outcome.stats.visited_entries, 2);
    assert_eq!(outcome.stop_reason, None);

    std::fs::remove_dir_all(root).expect("remove tree");
}

#[test]
fn walk_honors_file_entry_and_cancellation_limits() {
    let root = temp_root("limits");
    std::fs::create_dir_all(&root).expect("create tree");
    for name in ["A.php", "B.php", "C.php"] {
        std::fs::write(root.join(name), "<?php").expect("write PHP file");
    }

    let files = php_walk(
        std::slice::from_ref(&root),
        TraversalLimits {
            max_files: Some(2),
            max_entries: None,
        },
    );
    assert_eq!(files.files.len(), 2);
    assert_eq!(
        files.stop_reason,
        Some(TraversalStopReason::MaxFiles { limit: 2 })
    );

    let entries = php_walk(
        std::slice::from_ref(&root),
        TraversalLimits {
            max_files: None,
            max_entries: Some(2),
        },
    );
    assert_eq!(
        entries.stop_reason,
        Some(TraversalStopReason::MaxEntries { limit: 2 })
    );
    assert_eq!(entries.stats.visited_entries, 2);
    assert!(entries.files.is_empty());
    assert!(entries.stats.peak_pending_entries <= 2);

    let checks = AtomicUsize::new(0);
    let cancelled = walk_files(
        std::slice::from_ref(&root),
        TraversalLimits::default(),
        |_| false,
        |_, _| true,
        |_| true,
        || (checks.fetch_add(1, Ordering::SeqCst) >= 1).then_some(TraversalStopReason::Cancelled),
    );
    assert_eq!(cancelled.stop_reason, Some(TraversalStopReason::Cancelled));

    let deadline = walk_files(
        std::slice::from_ref(&root),
        TraversalLimits::default(),
        |_| false,
        |_, _| true,
        |_| true,
        || Some(TraversalStopReason::DeadlineExceeded),
    );
    assert_eq!(
        deadline.stop_reason,
        Some(TraversalStopReason::DeadlineExceeded)
    );

    std::fs::remove_dir_all(root).expect("remove tree");
}

#[test]
fn max_entries_stops_at_a_deterministic_directory_boundary() {
    let root = temp_root("wide-entry-budget");
    std::fs::create_dir_all(root.join("wide")).expect("create tree");
    std::fs::write(root.join("A.php"), "<?php").expect("write retained PHP file");
    for name in ["Z.php", "M.php", "B.php", "Q.php"] {
        std::fs::write(root.join("wide").join(name), "<?php").expect("write PHP file");
    }

    let limits = TraversalLimits {
        max_files: None,
        max_entries: Some(5),
    };
    let first = php_walk(std::slice::from_ref(&root), limits);
    let second = php_walk(std::slice::from_ref(&root), limits);
    assert_eq!(first.files, vec![root.join("A.php")]);
    assert_eq!(first.files, second.files);
    assert_eq!(
        first.stop_reason,
        Some(TraversalStopReason::MaxEntries { limit: 5 })
    );
    assert_eq!(first.stats.visited_entries, 5);
    assert!(first.stats.peak_pending_entries <= 5);

    std::fs::remove_dir_all(root).expect("remove tree");
}

#[cfg(unix)]
#[test]
fn walk_skips_special_files_without_stopping() {
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::net::UnixListener;

    let root = temp_root("special-file");
    std::fs::create_dir_all(&root).expect("create tree");
    std::fs::write(root.join("Subject.php"), "<?php").expect("write PHP file");
    let socket = root.join("events.sock");
    let _listener = UnixListener::bind(&socket).expect("bind Unix socket");
    assert!(std::fs::symlink_metadata(&socket)
        .expect("socket metadata")
        .file_type()
        .is_socket());

    let outcome = php_walk(std::slice::from_ref(&root), TraversalLimits::default());
    assert_eq!(outcome.files, vec![root.join("Subject.php")]);
    assert_eq!(outcome.stats.skipped_special_files, 1);

    std::fs::remove_dir_all(root).expect("remove tree");
}

#[test]
fn ten_thousand_files_require_only_linear_identity_lookups() {
    let root = temp_root("linear-10k");
    std::fs::create_dir_all(&root).expect("create scale tree");
    for index in 0..10_000usize {
        std::fs::write(root.join(format!("File{index:05}.php")), "<?php")
            .expect("write scale PHP file");
    }

    let outcome = php_walk(std::slice::from_ref(&root), TraversalLimits::default());
    assert_eq!(outcome.files.len(), 10_000);
    assert_eq!(outcome.stats.visited_entries, 10_001);
    assert_eq!(outcome.stats.identity_lookups, 10_001);
    assert_eq!(outcome.stats.duplicate_files, 0);

    std::fs::remove_dir_all(root).expect("remove scale tree");
}

#[test]
fn merging_ten_thousand_alias_paths_deduplicates_one_identity() {
    let identity = PhysicalIdentity::CanonicalPath(PathBuf::from("physical-identity"));
    let path = |index: usize| PhysicalFilePath {
        logical_path: PathBuf::from(format!("/logical/File{index:05}.php")),
        physical_path: PathBuf::from(format!("/physical/File{index:05}.php")),
    };
    let mut current = vec![PhysicalFileGroup {
        identity: identity.clone(),
        paths: (0..5_000).map(path).collect(),
    }];
    let incoming = vec![PhysicalFileGroup {
        identity,
        paths: (2_500..10_000).map(path).collect(),
    }];

    merge_physical_file_groups(&mut current, incoming);

    assert_eq!(current.len(), 1);
    assert_eq!(current[0].paths.len(), 10_000);
    assert_eq!(
        current[0].representative(),
        Path::new("/logical/File00000.php")
    );
}

#[cfg(unix)]
#[test]
fn walk_follows_external_links_without_cycles_and_deduplicates_aliases() {
    use std::os::unix::fs::symlink;

    let root = temp_root("symlink-root");
    let external = temp_root("symlink-external");
    std::fs::create_dir_all(root.join("nested")).expect("create workspace tree");
    std::fs::create_dir_all(&external).expect("create external tree");
    let external_file = external.join("Outside.php");
    std::fs::write(&external_file, "<?php").expect("write external PHP file");
    std::fs::hard_link(&external_file, external.join("HardAlias.php")).expect("hard-link PHP file");
    symlink(&external, root.join("a-linked")).expect("link external directory");
    symlink(&external, root.join("b-linked")).expect("link second external directory");
    symlink(&external_file, root.join("ExternalFile.php")).expect("link external PHP file");
    symlink(&root, root.join("nested/back")).expect("create cycle");
    symlink(root.join("missing"), root.join("broken")).expect("create broken link");

    let outcome = php_walk(std::slice::from_ref(&root), TraversalLimits::default());
    assert_eq!(outcome.files.len(), 1);
    assert_eq!(outcome.files[0], root.join("ExternalFile.php"));
    assert!(outcome.stats.duplicate_directories >= 2);
    assert!(outcome.stats.duplicate_files >= 1);
    assert!(outcome.stats.skipped_errors >= 1);
    assert!(outcome
        .symlink_aliases
        .iter()
        .any(|alias| alias.logical_path == root.join("a-linked")));
    assert!(outcome.symlink_aliases.iter().any(|alias| {
        alias.logical_path == root.join("ExternalFile.php")
            && alias.target_kind == SymlinkTargetKind::File
    }));
    assert!(symlink_aliases_on_path(&root.join("a-linked/Outside.php"))
        .iter()
        .any(|alias| alias.logical_path == root.join("a-linked")));

    std::fs::remove_dir_all(root).expect("remove workspace tree");
    std::fs::remove_dir_all(external).expect("remove external tree");
}

#[cfg(unix)]
#[test]
fn walk_records_symlink_ancestors_of_a_nested_explicit_root() {
    use std::os::unix::fs::symlink;

    let root = temp_root("nested-root-symlink");
    let external = temp_root("nested-root-external");
    std::fs::create_dir_all(&root).expect("create workspace tree");
    std::fs::create_dir_all(external.join("package/src")).expect("create external tree");
    std::fs::write(external.join("package/src/Subject.php"), "<?php")
        .expect("write external PHP file");
    symlink(&external, root.join("linked")).expect("link external ancestor");

    let nested_root = root.join("linked/package/src");
    let outcome = php_walk(
        std::slice::from_ref(&nested_root),
        TraversalLimits::default(),
    );
    assert_eq!(outcome.files, vec![nested_root.join("Subject.php")]);
    assert!(outcome.symlink_aliases.iter().any(|alias| {
        alias.logical_path == root.join("linked")
            && alias.physical_target == external
            && alias.target_kind == SymlinkTargetKind::Directory
    }));

    std::fs::remove_dir_all(root).expect("remove workspace tree");
    std::fs::remove_dir_all(external).expect("remove external tree");
}

#[cfg(unix)]
#[test]
fn logical_exclusion_skips_only_the_selected_symlink_branch() {
    use std::os::unix::fs::symlink;

    let root = temp_root("logical-exclude-root");
    let external = temp_root("logical-exclude-external");
    std::fs::create_dir_all(&root).expect("create workspace tree");
    std::fs::create_dir_all(&external).expect("create external tree");
    std::fs::write(external.join("Shared.php"), "<?php").expect("write PHP file");
    symlink(&external, root.join("excluded")).expect("link excluded alias");
    symlink(&external, root.join("included")).expect("link included alias");

    let excluded = root.join("excluded");
    let outcome = walk_files(
        std::slice::from_ref(&root),
        TraversalLimits::default(),
        |path| path.starts_with(&excluded),
        |_, _| true,
        |path| path.extension().is_some_and(|extension| extension == "php"),
        || None,
    );
    assert_eq!(outcome.files, vec![root.join("included/Shared.php")]);

    std::fs::remove_dir_all(root).expect("remove workspace tree");
    std::fs::remove_dir_all(external).expect("remove external tree");
}
