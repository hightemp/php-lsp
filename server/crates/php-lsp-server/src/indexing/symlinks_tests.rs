use super::*;
use crate::util::fs_walk::{PhysicalIdentity, SymlinkAlias};

fn uri(path: &Path) -> Uri {
    path_to_uri(path)
        .expect("path URI")
        .parse::<Uri>()
        .expect("LSP URI")
}

#[test]
fn external_watcher_fallback_requires_both_lsp_capabilities() {
    assert!(!ExternalWatcherCapabilities::default().supported());
    assert!(!ExternalWatcherCapabilities {
        dynamic_registration: true,
        relative_pattern_support: false,
    }
    .supported());
    assert!(!ExternalWatcherCapabilities {
        dynamic_registration: false,
        relative_pattern_support: true,
    }
    .supported());
    assert!(ExternalWatcherCapabilities {
        dynamic_registration: true,
        relative_pattern_support: true,
    }
    .supported());
}

#[test]
fn desired_watch_specs_merge_duplicate_and_nested_targets() {
    let mut state = ExternalSymlinkState::default();
    state.workspaces.insert(
        PathBuf::from("/workspace"),
        WorkspaceSymlinkSnapshot {
            generation: 1,
            logical_root: PathBuf::from("/workspace"),
            aliases: vec![
                SymlinkAlias {
                    logical_path: PathBuf::from("/workspace/linked"),
                    physical_target: PathBuf::from("/external"),
                    target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from("/external")),
                    target_kind: SymlinkTargetKind::Directory,
                },
                SymlinkAlias {
                    logical_path: PathBuf::from("/workspace/linked/nested"),
                    physical_target: PathBuf::from("/external/nested"),
                    target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from(
                        "/external/nested",
                    )),
                    target_kind: SymlinkTargetKind::Directory,
                },
            ],
            physical_files: Vec::new(),
        },
    );

    let specs = state.desired_watch_specs();
    assert_eq!(specs.len(), EXTERNAL_WATCH_PATTERNS.len());
    assert!(specs.iter().all(|spec| spec.base == Path::new("/external")));
    let watcher = watch_spec_to_watcher(&specs[0]).expect("relative watcher");
    let GlobPattern::Relative(relative) = watcher.glob_pattern else {
        panic!("external watcher must use RelativePattern");
    };
    assert_eq!(relative.base_uri, OneOf::Right(uri(Path::new("/external"))));
}

#[test]
fn direct_external_installed_json_symlink_gets_a_file_watcher() {
    let mut state = ExternalSymlinkState::default();
    state.workspaces.insert(
        PathBuf::from("/workspace"),
        WorkspaceSymlinkSnapshot {
            generation: 1,
            logical_root: PathBuf::from("/workspace"),
            aliases: vec![SymlinkAlias {
                logical_path: PathBuf::from("/workspace/vendor/composer/installed.json"),
                physical_target: PathBuf::from("/external/installed.json"),
                target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from(
                    "/external/installed.json",
                )),
                target_kind: SymlinkTargetKind::File,
            }],
            physical_files: Vec::new(),
        },
    );

    assert_eq!(
        state.desired_watch_specs(),
        vec![WatchSpec {
            base: PathBuf::from("/external"),
            pattern: "installed.json".to_string(),
        }]
    );
}

#[test]
fn external_vendor_composer_directory_watches_root_installed_json() {
    let mut state = ExternalSymlinkState::default();
    state.workspaces.insert(
        PathBuf::from("/workspace"),
        WorkspaceSymlinkSnapshot {
            generation: 1,
            logical_root: PathBuf::from("/workspace"),
            aliases: vec![SymlinkAlias {
                logical_path: PathBuf::from("/workspace/vendor/composer"),
                physical_target: PathBuf::from("/external/composer"),
                target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from(
                    "/external/composer",
                )),
                target_kind: SymlinkTargetKind::Directory,
            }],
            physical_files: Vec::new(),
        },
    );

    assert!(state.desired_watch_specs().iter().any(|spec| {
        spec.base == Path::new("/external/composer") && spec.pattern == "**/installed.json"
    }));
}

#[test]
fn physical_events_map_to_the_lexicographically_first_logical_alias() {
    let mut snapshot = WorkspaceSymlinkSnapshot {
        generation: 1,
        logical_root: PathBuf::from("/workspace"),
        aliases: vec![
            SymlinkAlias {
                logical_path: PathBuf::from("/workspace/z-parent"),
                physical_target: PathBuf::from("/external"),
                target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from("/external")),
                target_kind: SymlinkTargetKind::Directory,
            },
            SymlinkAlias {
                logical_path: PathBuf::from("/workspace/a-nested"),
                physical_target: PathBuf::from("/external/nested"),
                target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from("/external/nested")),
                target_kind: SymlinkTargetKind::Directory,
            },
        ],
        physical_files: Vec::new(),
    };

    let events = translate_event_for_snapshot(
        &mut snapshot,
        Path::new("/external/nested/New.php"),
        FileChangeType::CREATED,
    );
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].uri, uri(Path::new("/workspace/a-nested/New.php")));
}

#[test]
fn deleting_representative_promotes_the_next_physical_alias() {
    let mut snapshot = WorkspaceSymlinkSnapshot {
        generation: 1,
        logical_root: PathBuf::from("/workspace"),
        aliases: Vec::new(),
        physical_files: vec![PhysicalFileGroup {
            identity: PhysicalIdentity::CanonicalPath(PathBuf::from("identity")),
            paths: vec![
                PhysicalFilePath {
                    logical_path: PathBuf::from("/workspace/a.php"),
                    physical_path: PathBuf::from("/external/a.php"),
                },
                PhysicalFilePath {
                    logical_path: PathBuf::from("/workspace/b.php"),
                    physical_path: PathBuf::from("/external/b.php"),
                },
            ],
        }],
    };

    let events = translate_event_for_snapshot(
        &mut snapshot,
        Path::new("/external/a.php"),
        FileChangeType::DELETED,
    );
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].uri, uri(Path::new("/workspace/a.php")));
    assert_eq!(events[0].typ, FileChangeType::DELETED);
    assert_eq!(events[1].uri, uri(Path::new("/workspace/b.php")));
    assert_eq!(events[1].typ, FileChangeType::CREATED);
}

#[test]
fn one_physical_event_maps_independently_into_multiple_workspaces() {
    let alias = |logical_root: &str| SymlinkAlias {
        logical_path: PathBuf::from(logical_root).join("linked"),
        physical_target: PathBuf::from("/external"),
        target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from("/external")),
        target_kind: SymlinkTargetKind::Directory,
    };
    let mut state = ExternalSymlinkState::default();
    for workspace in ["/workspace-a", "/workspace-b"] {
        state.workspaces.insert(
            PathBuf::from(workspace),
            WorkspaceSymlinkSnapshot {
                generation: 1,
                logical_root: PathBuf::from(workspace),
                aliases: vec![alias(workspace)],
                physical_files: Vec::new(),
            },
        );
    }

    let events = state.translate_events(vec![FileEvent {
        uri: uri(Path::new("/external/Changed.php")),
        typ: FileChangeType::CHANGED,
    }]);
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0].uri,
        uri(Path::new("/workspace-a/linked/Changed.php"))
    );
    assert_eq!(
        events[1].uri,
        uri(Path::new("/workspace-b/linked/Changed.php"))
    );
}

#[test]
fn stale_workspace_generation_cannot_replace_newer_alias_snapshot() {
    let workspace = PathBuf::from("/workspace");
    let snapshot = |generation, target: &str| WorkspaceSymlinkSnapshot {
        generation,
        logical_root: workspace.clone(),
        aliases: vec![SymlinkAlias {
            logical_path: workspace.join("linked"),
            physical_target: PathBuf::from(target),
            target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from(target)),
            target_kind: SymlinkTargetKind::Directory,
        }],
        physical_files: Vec::new(),
    };
    let mut state = ExternalSymlinkState::default();
    state.active_workspaces.insert(workspace.clone(), 8);
    assert!(state.publish_workspace_snapshot(workspace.clone(), snapshot(8, "/new")));
    assert!(!state.publish_workspace_snapshot(workspace.clone(), snapshot(7, "/stale")));
    assert_eq!(
        state.workspaces[&workspace].aliases[0].physical_target,
        PathBuf::from("/new")
    );
}

#[test]
fn removed_workspace_tombstone_rejects_delayed_index_and_vendor_publications() {
    let workspace = PathBuf::from("/workspace");
    let snapshot = || WorkspaceSymlinkSnapshot {
        generation: 5,
        logical_root: workspace.clone(),
        aliases: Vec::new(),
        physical_files: Vec::new(),
    };
    let mut state = ExternalSymlinkState::default();
    state.active_workspaces.insert(workspace.clone(), 5);
    assert!(state.publish_workspace_snapshot(workspace.clone(), snapshot()));

    state.active_workspaces.clear();
    state.workspaces.clear();
    assert!(!state.publish_workspace_snapshot(workspace.clone(), snapshot()));
    assert!(!state.publish_additional_snapshot(workspace.clone(), snapshot()));
    assert!(!state.workspaces.contains_key(&workspace));
}

#[test]
fn non_indexing_runtime_generation_preserves_aliases_but_rejects_old_publishers() {
    let workspace = PathBuf::from("/workspace");
    let snapshot = |generation| WorkspaceSymlinkSnapshot {
        generation,
        logical_root: workspace.clone(),
        aliases: vec![SymlinkAlias {
            logical_path: workspace.join("linked"),
            physical_target: PathBuf::from("/external"),
            target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from("/external")),
            target_kind: SymlinkTargetKind::Directory,
        }],
        physical_files: Vec::new(),
    };
    let mut state = ExternalSymlinkState::default();
    state.active_workspaces.insert(workspace.clone(), 1);
    assert!(state.publish_workspace_snapshot(workspace.clone(), snapshot(1)));

    state.set_active_generations(&[(workspace.clone(), 2)], &[]);
    assert_eq!(state.workspaces[&workspace].generation, 2);
    assert_eq!(state.workspaces[&workspace].aliases.len(), 1);
    assert!(!state.publish_workspace_snapshot(workspace.clone(), snapshot(1)));

    state.set_active_generations(&[(workspace.clone(), 3)], std::slice::from_ref(&workspace));
    assert!(!state.workspaces.contains_key(&workspace));
}

#[test]
fn failed_unregister_remains_pending_until_confirmation() {
    let old = RegisteredWatchers {
        id: "old".to_string(),
        specs: Vec::new(),
    };
    let new = RegisteredWatchers {
        id: "new".to_string(),
        specs: Vec::new(),
    };
    let mut state = ExternalSymlinkState {
        registered: Some(old.clone()),
        ..Default::default()
    };
    state.commit_registration(Some(new.clone()), Some(old));

    assert_eq!(
        state.registered.as_ref().map(|item| item.id.as_str()),
        Some("new")
    );
    assert_eq!(state.stale_registrations.len(), 1);
    state.record_unregistration_result("old", false);
    // Error/timeout keeps the old ID retryable.
    assert_eq!(state.stale_registrations[0].id, "old");
    state.record_unregistration_result("old", true);
    assert!(state.stale_registrations.is_empty());
}

#[test]
fn transient_alias_metadata_errors_do_not_remove_registry_state() {
    assert!(alias_missing_error_kind(std::io::ErrorKind::NotFound));
    assert!(alias_missing_error_kind(std::io::ErrorKind::NotADirectory));
    assert!(!alias_missing_error_kind(
        std::io::ErrorKind::PermissionDenied
    ));
    assert!(!alias_missing_error_kind(std::io::ErrorKind::Other));
}

#[test]
fn deleting_primary_directory_alias_promotes_the_same_physical_file() {
    let mut aliases = vec![
        SymlinkAlias {
            logical_path: PathBuf::from("/workspace/a-linked"),
            physical_target: PathBuf::from("/external"),
            target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from("/external")),
            target_kind: SymlinkTargetKind::Directory,
        },
        SymlinkAlias {
            logical_path: PathBuf::from("/workspace/b-linked"),
            physical_target: PathBuf::from("/external"),
            target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from("/external")),
            target_kind: SymlinkTargetKind::Directory,
        },
        SymlinkAlias {
            logical_path: PathBuf::from("/workspace/a-linked/nested"),
            physical_target: PathBuf::from("/external/nested"),
            target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from("/external/nested")),
            target_kind: SymlinkTargetKind::Directory,
        },
    ];
    let mut physical_files = vec![PhysicalFileGroup {
        identity: PhysicalIdentity::CanonicalPath(PathBuf::from("identity")),
        paths: vec![PhysicalFilePath {
            logical_path: PathBuf::from("/workspace/a-linked/Subject.php"),
            physical_path: PathBuf::from("/external/Subject.php"),
        }],
    }];
    normalize_alias_snapshot(&mut aliases, &mut physical_files);
    assert_eq!(physical_files[0].paths.len(), 2);

    let workspace = PathBuf::from("/workspace");
    let mut state = ExternalSymlinkState::default();
    state.workspaces.insert(
        workspace.clone(),
        WorkspaceSymlinkSnapshot {
            generation: 1,
            logical_root: workspace,
            aliases,
            physical_files,
        },
    );
    let events = state.translate_events(vec![FileEvent {
        uri: uri(Path::new("/workspace/a-linked/Subject.php")),
        typ: FileChangeType::DELETED,
    }]);
    let php_events = events
        .into_iter()
        .filter(|event| event.uri.as_str().ends_with("Subject.php"))
        .collect::<Vec<_>>();
    assert_eq!(php_events.len(), 2);
    assert_eq!(
        php_events[0].uri,
        uri(Path::new("/workspace/a-linked/Subject.php"))
    );
    assert_eq!(php_events[0].typ, FileChangeType::DELETED);
    assert_eq!(
        php_events[1].uri,
        uri(Path::new("/workspace/b-linked/Subject.php"))
    );
    assert_eq!(php_events[1].typ, FileChangeType::CREATED);
    assert_eq!(
        state.workspaces[&PathBuf::from("/workspace")]
            .aliases
            .iter()
            .map(|alias| alias.logical_path.clone())
            .collect::<Vec<_>>(),
        vec![PathBuf::from("/workspace/b-linked")]
    );
}

#[test]
fn event_inside_one_workspace_is_also_routed_to_another_workspace_alias() {
    let mut state = ExternalSymlinkState::default();
    state.workspaces.insert(
        PathBuf::from("/workspace-a"),
        WorkspaceSymlinkSnapshot {
            generation: 1,
            logical_root: PathBuf::from("/workspace-a"),
            aliases: vec![SymlinkAlias {
                logical_path: PathBuf::from("/workspace-a/linked"),
                physical_target: PathBuf::from("/workspace-b/shared"),
                target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from(
                    "/workspace-b/shared",
                )),
                target_kind: SymlinkTargetKind::Directory,
            }],
            physical_files: Vec::new(),
        },
    );
    state.workspaces.insert(
        PathBuf::from("/workspace-b"),
        WorkspaceSymlinkSnapshot {
            generation: 1,
            logical_root: PathBuf::from("/workspace-b"),
            aliases: Vec::new(),
            physical_files: Vec::new(),
        },
    );

    let events = state.translate_events(vec![FileEvent {
        uri: uri(Path::new("/workspace-b/shared/Changed.php")),
        typ: FileChangeType::CHANGED,
    }]);
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0].uri,
        uri(Path::new("/workspace-a/linked/Changed.php"))
    );
    assert_eq!(
        events[1].uri,
        uri(Path::new("/workspace-b/shared/Changed.php"))
    );
}

#[test]
fn physical_event_inside_workspace_also_updates_its_logical_alias() {
    let physical_file = PathBuf::from("/workspace/z-real/Changed.php");
    let logical_file = PathBuf::from("/workspace/a-linked/Changed.php");
    let mut state = ExternalSymlinkState::default();
    state.workspaces.insert(
        PathBuf::from("/workspace"),
        WorkspaceSymlinkSnapshot {
            generation: 1,
            logical_root: PathBuf::from("/workspace"),
            aliases: vec![SymlinkAlias {
                logical_path: PathBuf::from("/workspace/a-linked"),
                physical_target: PathBuf::from("/workspace/z-real"),
                target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from(
                    "/workspace/z-real",
                )),
                target_kind: SymlinkTargetKind::Directory,
            }],
            physical_files: vec![PhysicalFileGroup {
                identity: PhysicalIdentity::CanonicalPath(PathBuf::from("identity")),
                paths: vec![PhysicalFilePath {
                    logical_path: logical_file.clone(),
                    physical_path: physical_file.clone(),
                }],
            }],
        },
    );

    let events = state.translate_events(vec![FileEvent {
        uri: uri(&physical_file),
        typ: FileChangeType::CHANGED,
    }]);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].uri, uri(&logical_file));
}

#[test]
fn logical_candidates_keep_every_alias_in_lexical_order() {
    let snapshot = WorkspaceSymlinkSnapshot {
        generation: 1,
        logical_root: PathBuf::from("/workspace"),
        aliases: vec![
            SymlinkAlias {
                logical_path: PathBuf::from("/workspace/z-nested"),
                physical_target: PathBuf::from("/external/nested"),
                target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from("/external/nested")),
                target_kind: SymlinkTargetKind::Directory,
            },
            SymlinkAlias {
                logical_path: PathBuf::from("/workspace/a-parent"),
                physical_target: PathBuf::from("/external"),
                target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from("/external")),
                target_kind: SymlinkTargetKind::Directory,
            },
        ],
        physical_files: Vec::new(),
    };

    assert_eq!(
        logical_paths_for_physical(&snapshot, Path::new("/external/nested/New.php")),
        vec![
            PathBuf::from("/workspace/a-parent/nested/New.php"),
            PathBuf::from("/workspace/z-nested/New.php"),
        ]
    );
}
