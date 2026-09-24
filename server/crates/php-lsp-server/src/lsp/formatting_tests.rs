use super::*;

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn aborted_formatter_cleans_its_temp_directory() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("php-lsp-formatter-abort-{nanos}"));
    std::fs::create_dir_all(&root).unwrap();
    let ready = root.join("ready");
    let script = root.join("formatter.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s' \"$1\" > {}\nsleep 5\n",
            shell_escape(&ready.to_string_lossy())
        ),
    )
    .unwrap();
    let command = format!("sh {} {{file}}", shell_escape(&script.to_string_lossy()));
    let config = FormattingConfig::from_options(Some("custom"), Some(&command), Some(5_000));
    let run = tokio::spawn(run_external_formatter(
        "<?php echo 1;".into(),
        config,
        None,
        None,
    ));
    tokio::time::timeout(Duration::from_secs(2), async {
        while !ready.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("formatter did not start");
    let input = std::fs::read_to_string(&ready).unwrap();
    let temp_dir = PathBuf::from(input).parent().unwrap().to_path_buf();
    assert!(temp_dir.join("input.php").exists());
    run.abort();
    let _ = run.await;
    let cleaned = tokio::time::timeout(Duration::from_millis(500), async {
        while temp_dir.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .is_ok();
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::remove_dir_all(root).unwrap();
    assert!(cleaned, "formatter temp dir survived request cancellation");
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn queued_formatter_cleanup_survives_caller_abort_with_capacity_exhausted() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("php-lsp-formatter-queued-{nanos}"));
    std::fs::create_dir_all(&root).unwrap();
    let ready = root.join("ready");
    let proceed = root.join("proceed");
    let exited = root.join("exited");
    let script = root.join("formatter.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s' \"$1\" > {}\nwhile [ ! -e {} ]; do sleep 0.005; done\ntouch {}\n",
            shell_escape(&ready.to_string_lossy()),
            shell_escape(&proceed.to_string_lossy()),
            shell_escape(&exited.to_string_lossy()),
        ),
    )
    .unwrap();
    let command = format!("sh {} {{file}}", shell_escape(&script.to_string_lossy()));
    let config = FormattingConfig::from_options(Some("custom"), Some(&command), Some(5_000));
    let run = tokio::spawn(run_external_formatter(
        "<?php echo 1;".into(),
        config,
        None,
        None,
    ));
    tokio::time::timeout(Duration::from_secs(2), async {
        while !ready.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("formatter did not start");
    let input = std::fs::read_to_string(&ready).unwrap();
    let temp_dir = PathBuf::from(input).parent().unwrap().to_path_buf();
    let held = FILE_IO_BLOCKING_SEMAPHORE
        .clone()
        .acquire_many_owned(MAX_CONCURRENT_FILE_IO_TASKS as u32)
        .await
        .unwrap();
    std::fs::write(&proceed, "go").unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !exited.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("formatter did not exit");
    tokio::time::sleep(Duration::from_millis(50)).await;
    run.abort();
    let _ = run.await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let waited_for_capacity = temp_dir.exists();
    drop(held);
    let cleaned = tokio::time::timeout(Duration::from_millis(500), async {
        while temp_dir.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .is_ok();
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::remove_dir_all(root).unwrap();
    assert!(
        waited_for_capacity,
        "cleanup bypassed the shared capacity limit"
    );
    assert!(
        cleaned,
        "queued formatter cleanup left temp files after abort"
    );
}

#[test]
fn strip_range_formatter_wrapper_preserves_unwrapped_output() {
    let formatted = "<?php\necho 'selected with tag';\n".to_string();

    assert_eq!(
        strip_range_formatter_wrapper(formatted.clone(), false),
        Some(formatted)
    );
}

#[test]
fn strip_range_formatter_wrapper_accepts_lf_and_crlf_prefixes() {
    for (formatted, expected) in [
        ("<?php\necho 'lf';\n", "echo 'lf';\n"),
        ("<?php\r\necho 'crlf';\r\n", "echo 'crlf';\r\n"),
    ] {
        assert_eq!(
            strip_range_formatter_wrapper(formatted.to_string(), true).as_deref(),
            Some(expected)
        );
    }
}

#[test]
fn strip_range_formatter_wrapper_rejects_missing_or_changed_prefix() {
    for formatted in [
        "echo 'missing';\n",
        "\n<?php\necho 'shifted';\n",
        "<?phpecho 'changed';\n",
        "",
    ] {
        assert_eq!(
            strip_range_formatter_wrapper(formatted.to_string(), true),
            None,
            "unexpectedly accepted formatter output: {formatted:?}"
        );
    }
}
