use super::*;

#[tokio::test(flavor = "current_thread")]
async fn p235_closed_controller_union_replaces_removed_and_unsaved_alternatives() {
    let f = Fixture::new();
    let controller = |ty: &str| {
        format!("<?php function page() {{ $this->render('page.twig', ['item'=>new {ty}()]); }}")
    };
    f.write("src/A.php", &controller("First"));
    f.write("src/B.php", &controller("Second"));
    f.write("templates/page.twig", "{% include 'partial.twig' %}");
    let old = f.context().await;
    assert_eq!(
        variable(&old, "partial.twig", "item").as_deref(),
        Some("First|Second")
    );
    let work = source_work(&f.root);
    f.context().await;
    assert_eq!(source_work(&f.root), work);
    f.write("src/B.php", &controller("Third"));
    f.invalidate("src/B.php").await;
    assert_eq!(
        variable(&f.context().await, "partial.twig", "item").as_deref(),
        Some("First|Third")
    );
    assert_eq!(source_work(&f.root), (work.0 + 1, work.1 + 1));
    f.index_php("src/A.php", &controller("Other"), true);
    f.invalidate("src/A.php").await;
    assert_eq!(
        variable(&f.context().await, "partial.twig", "item").as_deref(),
        Some("Other|Third")
    );
    f.files.remove(&f.uri("src/A.php"));
    f.index.remove_file(&f.uri("src/A.php"));
    f.invalidate("src/A.php").await;
    assert_eq!(
        variable(&f.context().await, "partial.twig", "item").as_deref(),
        Some("First|Third")
    );
    std::fs::remove_file(f.root.join("src/B.php")).unwrap();
    f.invalidate("src/B.php").await;
    assert_eq!(
        variable(&f.context().await, "partial.twig", "item").as_deref(),
        Some("First")
    );
    assert!(old.commit_if_current(|| true).is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn p235_metadata_eligibility_is_root_local_with_a_shared_coordinator() {
    let a = Fixture::new();
    let mut b = Fixture::new();
    b.cache = a.cache.clone();
    a.write(
        "composer.json",
        r#"{"require":{"symfony/twig-bundle":"*"}}"#,
    );
    let symfony = a.context().await;
    let plain = b.context().await;
    assert!(symfony.has_symfony_twig());
    assert!(!plain.has_symfony_twig());
    a.write("composer.json", "{}");
    a.invalidate("composer.json").await;
    assert!(!a.context().await.has_symfony_twig());
    assert_eq!(plain.commit_if_current(|| true), Some(true));
}

#[tokio::test(flavor = "current_thread")]
async fn p235_metadata_supersession_cancels_old_worker_and_preserves_single_worker() {
    let f = Fixture::new();
    f.write(
        "composer.json",
        r#"{"require":{"symfony/twig-bundle":"*"}}"#,
    );
    let old = f.context().await;
    assert!(old.has_symfony_twig());
    f.invalidate("composer.json").await;
    let (rx, release, hooks) = pause_next(&f).await;
    let stale = f.spawn();
    reached(rx).await;
    f.write("composer.json", "{}");
    f.invalidate("composer.json").await;
    let latest = f.spawn();
    release.send(()).unwrap();
    assert!(stale.await.unwrap().is_none());
    let view = latest.await.unwrap().unwrap();
    assert!(!view.has_symfony_twig());
    assert!(old.commit_if_current(|| true).is_none());
    assert_eq!(view.commit_if_current(|| true), Some(true));
    assert_eq!(hooks.peak.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn p235_excluded_composer_metadata_does_not_enable_globals() {
    let f = Fixture::new();
    f.write(
        "composer.json",
        r#"{"require":{"symfony/twig-bundle":"*"}}"#,
    );
    let view = twig_context_for_state(
        &f.root,
        &f.files,
        &f.templates,
        &f.index,
        &f.cache,
        TraversalLimits::default(),
        &[PathBuf::from("composer.json")],
        None,
        None,
    )
    .await
    .unwrap();
    assert!(!view.has_symfony_twig());
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn p235_external_metadata_symlink_uses_logical_root_and_invalidates_by_alias() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let external = Fixture::new();
    external.write(
        "metadata.json",
        r#"{"require":{"symfony/twig-bundle":"*"}}"#,
    );
    symlink(
        external.root.join("metadata.json"),
        f.root.join("composer.json"),
    )
    .unwrap();
    assert!(f.context().await.has_symfony_twig());
    let work = source_work(&f.root);
    f.context().await;
    assert_eq!(source_work(&f.root), work);
    external.write("metadata.json", "{}");
    f.invalidate("composer.json").await;
    assert!(!f.context().await.has_symfony_twig());
}
