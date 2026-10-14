use super::*;

#[tokio::test(flavor = "current_thread")]
async fn p234_plain_include_updates_from_one_cached_caller_without_extra_source_work() {
    let fixture = Fixture::new();
    fixture.controller(0, "User");
    fixture.write("templates/caller0.twig", "{% include 'partial.twig' %}");
    fixture.write("templates/partial.twig", "{{ item }}");
    let context = fixture.context().await;
    assert_eq!(
        variable(&context, "partial.twig", "item").as_deref(),
        Some("User")
    );
    let before = source_work(&fixture.root);
    assert_eq!(before, (3, 1));
    let warm = fixture.context().await;
    assert_eq!(
        variable(&warm, "partial.twig", "item").as_deref(),
        Some("User")
    );
    assert_eq!(source_work(&fixture.root), before);
    fixture.templates.insert(
        fixture.uri("templates/caller0.twig"),
        preprocess_twig_template("{% include 'partial.twig' only %}", &[]),
    );
    fixture.invalidate("templates/caller0.twig").await;
    let latest = fixture.context().await;
    assert!(latest.variables("partial.twig").is_empty());
    assert_eq!(source_work(&fixture.root), before);
    assert!(context.commit_if_current(|| true).is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn p234_open_template_without_include_already_replaces_disk_and_close_restores_it() {
    let fixture = Fixture::new();
    fixture.controller(0, "DiskUser");
    fixture.caller(0);
    fixture.templates.insert(
        fixture.uri("templates/caller0.twig"),
        preprocess_twig_template("unsaved: removed include", &[]),
    );
    assert!(fixture.context().await.variables("partial.twig").is_empty());
    fixture
        .templates
        .remove(&fixture.uri("templates/caller0.twig"));
    fixture.invalidate("templates/caller0.twig").await;
    assert_eq!(
        variable(&fixture.context().await, "partial.twig", "value0").as_deref(),
        Some("DiskUser")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn p234_one_level_scope_does_not_recursively_propagate_include_cycles() {
    let fixture = Fixture::new();
    fixture.controller(0, "User");
    fixture.write("templates/caller0.twig", "{% include 'child.twig' %}");
    fixture.write(
        "templates/child.twig",
        "{% include 'leaf.twig' %}{% include 'caller0.twig' %}",
    );
    fixture.write("templates/leaf.twig", "{{ item }}");
    let context = fixture.context().await;
    assert_eq!(
        variable(&context, "child.twig", "item").as_deref(),
        Some("User")
    );
    assert!(context.variables("leaf.twig").is_empty());
    assert_eq!(source_work(&fixture.root), (4, 1));
}

#[tokio::test(flavor = "current_thread")]
async fn p234_unsaved_plain_include_removal_and_close_restore_without_rescanning_php() {
    let fixture = Fixture::new();
    fixture.controller(0, "User");
    fixture.write("templates/caller0.twig", "{% include 'partial.twig' %}");
    let previous = fixture.context().await;
    assert_eq!(
        variable(&previous, "partial.twig", "item").as_deref(),
        Some("User")
    );
    let work = source_work(&fixture.root);
    let uri = fixture.uri("templates/caller0.twig");
    fixture.templates.insert(
        uri.clone(),
        preprocess_twig_template("removed last include", &[]),
    );
    fixture.invalidate("templates/caller0.twig").await;
    let removed = fixture.context().await;
    assert!(removed.variables("partial.twig").is_empty());
    assert!(!removed
        .snapshot
        .include_callers
        .contains_key("partial.twig"));
    assert!(previous.commit_if_current(|| true).is_none());
    assert_eq!(source_work(&fixture.root), work);
    fixture.templates.remove(&uri);
    fixture.invalidate("templates/caller0.twig").await;
    assert_eq!(
        variable(&fixture.context().await, "partial.twig", "item").as_deref(),
        Some("User")
    );
    assert!(removed.commit_if_current(|| true).is_none());
    assert_eq!(source_work(&fixture.root), (work.0 + 1, work.1));
}

#[tokio::test(flavor = "current_thread")]
async fn p234_inherited_contexts_stay_isolated_between_roots() {
    let a = Fixture::new();
    let mut b = Fixture::new();
    b.cache = a.cache.clone();
    for (fixture, ty) in [(&a, "First"), (&b, "Second")] {
        fixture.controller(0, ty);
        fixture.write("templates/caller0.twig", "{% include 'partial.twig' %}");
    }
    a.context().await;
    let b_context = b.context().await;
    let before = source_work(&b.root);
    a.templates.insert(
        a.uri("templates/caller0.twig"),
        preprocess_twig_template("{% include 'partial.twig' only %}", &[]),
    );
    a.invalidate("templates/caller0.twig").await;
    assert!(a.context().await.variables("partial.twig").is_empty());
    assert_eq!(
        variable(&b.context().await, "partial.twig", "item").as_deref(),
        Some("Second")
    );
    assert_eq!(b_context.commit_if_current(|| true), Some(true));
    assert_eq!(source_work(&b.root), before);
}

#[tokio::test(flavor = "current_thread")]
async fn p234_superseded_include_workers_cannot_publish_old_inherited_context() {
    let fixture = Fixture::new();
    fixture.controller(0, "User");
    fixture.write("templates/caller0.twig", "{% include 'partial.twig' %}");
    let old_view = fixture.context().await;
    let before = source_work(&fixture.root);
    fixture.invalidate("templates/caller0.twig").await;
    let (rx, release, hooks) = pause_next(&fixture).await;
    let old_job = fixture.spawn();
    reached(rx).await;
    fixture.templates.insert(
        fixture.uri("templates/caller0.twig"),
        preprocess_twig_template("{% include 'partial.twig' only %}", &[]),
    );
    fixture.invalidate("templates/caller0.twig").await;
    let latest = fixture.spawn();
    assert!(old_view.commit_if_current(|| true).is_none());
    release.send(()).unwrap();
    assert!(old_job.await.unwrap().is_none());
    let latest = latest.await.unwrap().unwrap();
    assert!(latest.variables("partial.twig").is_empty());
    assert_eq!(latest.commit_if_current(|| true), Some(true));
    assert_eq!(hooks.peak.load(Ordering::SeqCst), 1);
    assert_eq!(source_work(&fixture.root), before);
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn p234_external_template_alias_overlay_suppresses_old_physical_disk_context() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let external = Fixture::new();
    fixture.controller(0, "User");
    external.write("caller.twig", "{% include 'partial.twig' %}");
    symlink(
        external.root.join("caller.twig"),
        fixture.root.join("templates/caller0.twig"),
    )
    .unwrap();
    let first = fixture.context().await;
    assert_eq!(
        variable(&first, "partial.twig", "item").as_deref(),
        Some("User")
    );
    let work = source_work(&fixture.root);
    symlink(
        external.root.join("caller.twig"),
        fixture.root.join("templates/z-Ж alias.twig"),
    )
    .unwrap();
    fixture.templates.insert(
        fixture.uri("templates/z-Ж alias.twig"),
        preprocess_twig_template("removed include in unsaved alias", &[]),
    );
    fixture.invalidate("templates/z-Ж alias.twig").await;
    assert!(fixture.context().await.variables("partial.twig").is_empty());
    assert_eq!(source_work(&fixture.root), work);
    fixture
        .templates
        .remove(&fixture.uri("templates/z-Ж alias.twig"));
    fixture.invalidate("templates/z-Ж alias.twig").await;
    assert_eq!(
        variable(&fixture.context().await, "partial.twig", "item").as_deref(),
        Some("User")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn p234_inherited_context_respects_logical_exclusions_and_scan_limits() {
    let fixture = Fixture::new();
    fixture.controller(0, "User");
    fixture.write("templates/caller0.twig", "{% include 'partial.twig' %}");
    fixture.write("templates/zz.twig", "unrelated");
    let excluded = twig_context_for_state(
        &fixture.root,
        &fixture.files,
        &fixture.templates,
        &fixture.index,
        &fixture.cache,
        TraversalLimits::default(),
        &[PathBuf::from("templates/caller0.twig")],
        None,
        None,
    )
    .await
    .unwrap();
    assert!(excluded.variables("partial.twig").is_empty());
    let limits = TraversalLimits {
        max_files: Some(1),
        max_entries: None,
    };
    let limited = fixture.context_with(limits).await.unwrap();
    assert!(limited.snapshot.partial);
    assert_eq!(
        variable(&limited, "partial.twig", "item").as_deref(),
        Some("User")
    );
    let work = source_work(&fixture.root);
    fixture.context_with(limits).await.unwrap();
    assert_eq!(source_work(&fixture.root), work);
}
