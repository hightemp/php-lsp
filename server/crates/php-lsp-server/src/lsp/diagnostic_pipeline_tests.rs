use super::*;
use futures::StreamExt;
use php_lsp_parser::diagnostics::extract_syntax_errors;
use php_lsp_types::uri::path_to_uri;
use std::time::{SystemTime, UNIX_EPOCH};
use tower::{Service, ServiceExt};
use tower_lsp::{jsonrpc::Request, LspService};

#[test]
fn syntax_ranges_are_not_converted_from_utf16_a_second_time() {
    for source in [
        "<?php /* 😀 é 中文 */ function broken( {}",
        "<?php\r\n/* 😀😀 é */ function broken( {}\r\n",
        "<?php /* ascii */ function broken( {}",
    ] {
        let mut parser = FileParser::new();
        parser.parse_full(source);
        let expected = extract_syntax_errors(parser.tree().unwrap(), source);
        assert!(!expected.is_empty());
        let actual = compute_diagnostics(
            "file:///ranges.php",
            &parser,
            &WorkspaceIndex::new(),
            DiagnosticsMode::SyntaxOnly,
            PhpVersion::DEFAULT,
        );
        assert_eq!(actual.len(), expected.len());
        for (actual, expected) in actual.iter().zip(expected) {
            assert_eq!(actual.range, lsp_range_to_ls(expected.range), "{source}");
        }
    }
}

#[test]
fn syntax_diagnostics_have_a_stable_kind_independent_of_message() {
    let mut parser = FileParser::new();
    parser.parse_full("<?php function broken( {}");
    let actual = compute_diagnostics(
        "file:///kind.php",
        &parser,
        &WorkspaceIndex::new(),
        DiagnosticsMode::SyntaxOnly,
        PhpVersion::DEFAULT,
    );
    assert!(!actual.is_empty());
    assert!(
        actual.iter().all(
            |diagnostic| diagnostic.code == Some(NumberOrString::String("syntax-error".into()))
        ),
        "{actual:?}"
    );
}

struct Fixture {
    root: PathBuf,
    file: PathBuf,
    uri: Uri,
}

impl Fixture {
    fn new(source: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("php-lsp-p230-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("Subject.php");
        std::fs::write(&file, source).unwrap();
        let uri = path_to_uri(&file).unwrap().parse().unwrap();
        Self { root, file, uri }
    }
    fn script(&self, analyzer: &str) -> String {
        let output = if analyzer == "phpstan" {
            serde_json::json!({"files":{(self.file.to_string_lossy().to_string()):{"messages":[{"message":"disk result","line":1}]}}})
        } else {
            serde_json::json!([{"file_path":self.file,"message":"disk result","line_from":1}])
        };
        let script = self.root.join(format!("{analyzer}.sh"));
        std::fs::write(
            &script,
            format!(
                "printf 'ran\\n' >> {}\ncat <<'JSON'\n{output}\nJSON\n",
                shell_escape(&self.root.join("runs").to_string_lossy())
            ),
        )
        .unwrap();
        format!("sh {} {{file}}", shell_escape(&script.to_string_lossy()))
    }
    async fn backend(
        &self,
        semantic_errors: bool,
    ) -> (LspService<PhpLspBackend>, mpsc::UnboundedReceiver<Request>) {
        let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(request) = socket.next().await {
                let _ = tx.send(request);
            }
        });
        let options = serde_json::json!({"stubExtensions":[],"indexVendor":false,"diagnosticsMode":"basic-semantic","diagnostics":{"severity":if semantic_errors {"error"}else{"warning"}},"phpstanEnabled":true,"phpstanCommand":self.script("phpstan"),"phpstanMemoryLimit":"","psalmEnabled":true,"psalmCommand":self.script("psalm")});
        let response=service.ready().await.unwrap().call(Request::build("initialize").id(1).params(serde_json::json!({"processId":null,"capabilities":{},"initializationOptions":options})).finish()).await.unwrap().unwrap();
        assert!(response.error().is_none(), "{response:?}");
        (service, rx)
    }
    async fn install_source(&self, backend: &PhpLspBackend, source: &str, version: i32) {
        let uri = self.uri.as_str();
        let mut parser = FileParser::new();
        parser.parse_full(source);
        let symbols = extract_file_symbols(parser.tree().unwrap(), source, uri);
        let references =
            collect_symbol_references_in_file(parser.tree().unwrap(), source, &symbols);
        let request = backend.request_context_for_uri(uri).await;
        let root_index = request.index(&backend.index);
        update_aggregate_and_root_index(
            &backend.index,
            &root_index,
            uri,
            symbols,
            references,
            None,
        );
        backend
            .document_versions
            .insert(uri.into(), backend.next_document_state(version));
        backend.open_files.insert(uri.into(), parser);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn published(rx: &mut mpsc::UnboundedReceiver<Request>, uri: &Uri) -> serde_json::Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let request = rx.recv().await.expect("publisher closed");
            if request.method() == "textDocument/publishDiagnostics"
                && request
                    .params()
                    .is_some_and(|params| params["uri"] == uri.as_str())
            {
                return request.params().unwrap().clone();
            }
        }
    })
    .await
    .expect("diagnostics were not published")
}
fn external_count(params: &serde_json::Value) -> usize {
    params["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|diagnostic| matches!(diagnostic["source"].as_str(), Some("phpstan" | "psalm")))
        .count()
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn dirty_buffer_does_not_run_disk_analyzers_but_saved_buffer_does() {
    let disk = "<?php function disk():void {}";
    let buffer = "<?php function buffer():void {}";
    let fixture = Fixture::new(disk);
    let (service, mut rx) = fixture.backend(false).await;
    fixture.install_source(service.inner(), buffer, 2).await;
    service.inner().publish_diagnostics(&fixture.uri).await;
    let result = published(&mut rx, &fixture.uri).await;
    assert_eq!(result["version"], 2);
    assert_eq!(external_count(&result), 0, "{result}");
    assert!(
        !fixture.root.join("runs").exists(),
        "dirty buffer must skip both commands"
    );
    std::fs::write(&fixture.file, buffer).unwrap();
    service.inner().publish_diagnostics(&fixture.uri).await;
    assert_eq!(external_count(&published(&mut rx, &fixture.uri).await), 2);
    service.inner().shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn semantic_error_severity_does_not_masquerade_as_a_syntax_error() {
    let source = "<?php namespace App; new UnknownClass();";
    let fixture = Fixture::new(source);
    let (service, mut rx) = fixture.backend(true).await;
    fixture.install_source(service.inner(), source, 1).await;
    service.inner().publish_diagnostics(&fixture.uri).await;
    let result = published(&mut rx, &fixture.uri).await;
    assert!(
        result["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["source"] == "php-lsp" && diagnostic["severity"] == 1),
        "{result}"
    );
    assert_eq!(
        external_count(&result),
        2,
        "semantic errors must not suppress analyzers: {result}"
    );
    service.inner().shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn disk_replacement_during_analysis_discards_output_and_keeps_buffer_diagnostics() {
    let source = "<?php namespace App; new OriginalMissingClass();";
    let fixture = Fixture::new(source);
    let (service, mut rx) = fixture.backend(false).await;
    let script = fixture.root.join("phpstan.sh");
    let old = std::fs::read_to_string(&script).unwrap();
    std::fs::write(
        &script,
        format!("printf '%s' '<?php new ChangedDiskClass();' > \"$1\"\n{old}"),
    )
    .unwrap();
    fixture.install_source(service.inner(), source, 1).await;
    service.inner().publish_diagnostics(&fixture.uri).await;
    let result = published(&mut rx, &fixture.uri).await;
    assert_eq!(
        external_count(&result),
        0,
        "disk changed during analysis: {result}"
    );
    assert!(
        result["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["message"]
                .as_str()
                .is_some_and(|message| message.contains("OriginalMissingClass"))),
        "{result}"
    );
    service.inner().shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn queued_analyzer_result_revalidates_disk_and_preserves_builtin_results() {
    let source = "<?php namespace App; new OriginalMissingClass();";
    let fixture = Fixture::new(source);
    let (service, mut rx) = fixture.backend(false).await;
    let backend = service.inner();
    fixture.install_source(backend, source, 1).await;
    let context = backend.request_context_for_uri(fixture.uri.as_str()).await;
    let sequence = backend
        .diagnostics_publisher
        .start_computation(fixture.uri.as_str());
    let result = DiagnosticPublishRequest {
        uri: fixture.uri.clone(),
        diagnostics: vec![
            Diagnostic {
                source: Some("phpstan".into()),
                message: "stale disk".into(),
                ..Default::default()
            },
            Diagnostic {
                source: Some("php-lsp".into()),
                message: "current buffer".into(),
                ..Default::default()
            },
        ],
        version: Some(1),
        expected_state: backend.current_document_state(fixture.uri.as_str()),
        expected_template: None,
        expected_analyzer_source: Some(AnalyzerSourceSnapshot::new(
            fixture.file.clone(),
            Arc::from(source),
        )),
        require_idle_index: false,
        expected_runtime_generation: context.state.generation,
        indexing_workspace_folder: None,
        expected_indexing_run: None,
        expected_runtime_config: context.runtime_config().clone(),
        expected_index: context.index(&backend.index),
        computation_sequence: sequence,
    };
    // No yield between queuing and mutation: the current-thread publisher
    // can only examine the file after this deterministic replacement.
    backend.diagnostics_publisher.publish(result);
    std::fs::write(&fixture.file, "<?php new ChangedDiskClass();").unwrap();
    let result = published(&mut rx, &fixture.uri).await;
    assert_eq!(external_count(&result), 0, "{result}");
    assert_eq!(result["diagnostics"].as_array().unwrap().len(), 1);
    assert_eq!(result["diagnostics"][0]["message"], "current buffer");
    backend.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn actual_syntax_errors_block_both_analyzers() {
    let source = "<?php /* 😀 */ function broken( {}";
    let fixture = Fixture::new(source);
    let (service, mut rx) = fixture.backend(true).await;
    fixture.install_source(service.inner(), source, 1).await;
    service.inner().publish_diagnostics(&fixture.uri).await;
    let result = published(&mut rx, &fixture.uri).await;
    assert_eq!(external_count(&result), 0);
    assert!(!fixture.root.join("runs").exists());
    assert!(result["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .all(|diagnostic| diagnostic["code"] == "syntax-error"));
    service.inner().shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn source_validation_compares_all_bytes_and_rejects_missing_or_non_file_paths() {
    let source = format!("{}😀\r\nend", "x".repeat(130_000));
    let fixture = Fixture::new(&source);
    let proof = AnalyzerSourceSnapshot::new(fixture.file.clone(), Arc::from(source.as_str()));
    assert!(proof.matches_disk(None).await);
    let mut changed = source.clone();
    changed.replace_range(70_000..70_001, "y");
    std::fs::write(&fixture.file, &changed).unwrap();
    assert!(
        !proof.matches_disk(None).await,
        "same-size mutation after the first chunk"
    );
    std::fs::write(&fixture.file, format!("{source}extra")).unwrap();
    assert!(!proof.matches_disk(None).await);
    std::fs::remove_file(&fixture.file).unwrap();
    assert!(!proof.matches_disk(None).await);
    std::fs::create_dir(&fixture.file).unwrap();
    assert!(!proof.matches_disk(None).await);
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn external_symlinks_remain_eligible_and_retargeting_is_revalidated() {
    let source = "<?php namespace App; new MissingClass();";
    let fixture = Fixture::new(source);
    let external = Fixture::new(source);
    std::fs::remove_file(&fixture.file).unwrap();
    std::os::unix::fs::symlink(&external.file, &fixture.file).unwrap();
    let proof = AnalyzerSourceSnapshot::new(fixture.file.clone(), Arc::from(source));
    assert!(proof.matches_disk(None).await);
    let (service, mut rx) = fixture.backend(false).await;
    fixture.install_source(service.inner(), source, 1).await;
    service.inner().publish_diagnostics(&fixture.uri).await;
    assert_eq!(external_count(&published(&mut rx, &fixture.uri).await), 2);
    std::fs::write(&external.file, "<?php namespace Other;").unwrap();
    assert!(!proof.matches_disk(None).await);
    service.inner().shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_source_validation_does_not_wait_for_an_io_permit() {
    let fixture = Fixture::new("<?php");
    let proof = AnalyzerSourceSnapshot::new(fixture.file.clone(), Arc::from("<?php"));
    let permit = FILE_IO_BLOCKING_SEMAPHORE
        .clone()
        .acquire_many_owned(MAX_CONCURRENT_FILE_IO_TASKS as u32)
        .await
        .unwrap();
    let token = OperationCancellationToken::new();
    let checking = proof.matches_disk(Some(&token));
    let cancel = async {
        tokio::task::yield_now().await;
        token.cancel();
    };
    let (matched, ()) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(checking, cancel)
    })
    .await
    .expect("cancelled validation kept waiting for capacity");
    assert!(!matched);
    drop(permit);
}

#[tokio::test(flavor = "current_thread")]
async fn publisher_rechecks_document_runtime_indexing_and_sequence_after_source_validation() {
    for changed in ["document", "runtime", "indexing", "sequence"] {
        let source = "<?php namespace App; new OriginalMissingClass();";
        let fixture = Fixture::new(source);
        let (service, mut rx) = fixture.backend(false).await;
        let backend = service.inner();
        fixture.install_source(backend, source, 1).await;
        let reached = Arc::new(tokio::sync::Notify::new());
        let resume = Arc::new(tokio::sync::Notify::new());
        let sequence = backend
            .diagnostics_publisher
            .start_computation(fixture.uri.as_str());
        let mut old = queued_request(&fixture, backend, sequence, "old result").await;
        old.require_idle_index = true;
        old.indexing_workspace_folder = Some(fixture.root.clone());
        old.expected_analyzer_source = Some(
            AnalyzerSourceSnapshot::new(fixture.file.clone(), Arc::from(source))
                .with_validation_gate(reached.clone(), resume.clone()),
        );
        backend.diagnostics_publisher.publish(old);
        tokio::time::timeout(Duration::from_secs(5), reached.notified())
            .await
            .expect("publisher validation gate not reached");
        let mut run = None;
        match changed {
            "document" => {
                fixture
                    .install_source(backend, "<?php namespace App; new NewLifetimeClass();", 1)
                    .await
            }
            "runtime" => {
                let mut current = backend.runtime_state.lock().await;
                let mut fallback = current.fallback.clone();
                fallback.diagnostics_mode = DiagnosticsMode::SyntaxOnly;
                *current = Arc::new(WorkspaceRuntimeState {
                    fallback,
                    fallback_index: current.fallback_index.clone(),
                    configs: current.configs.clone(),
                    generation: current.generation + 1,
                });
            }
            "indexing" => run = Some(backend.start_indexing_run(&fixture.root)),
            "sequence" => {
                backend
                    .diagnostics_publisher
                    .start_computation(fixture.uri.as_str());
            }
            _ => unreachable!(),
        }
        // Except in the sequence case, retain the same computation identity
        // so it cannot mask missing document/runtime/indexing revalidation.
        let sequence = *backend
            .diagnostics_publisher
            .current_computation_sequences
            .get(fixture.uri.as_str())
            .unwrap();
        let fresh = queued_request(&fixture, backend, sequence, "fresh result").await;
        backend.diagnostics_publisher.publish(fresh);
        resume.notify_one();
        let result = published(&mut rx, &fixture.uri).await;
        assert_eq!(
            result["diagnostics"][0]["message"], "fresh result",
            "{changed}: {result}"
        );
        drop(run);
        backend.shutdown().await.unwrap();
    }
}

async fn queued_request(
    fixture: &Fixture,
    backend: &PhpLspBackend,
    sequence: u64,
    message: &str,
) -> DiagnosticPublishRequest {
    let context = backend.request_context_for_uri(fixture.uri.as_str()).await;
    DiagnosticPublishRequest {
        uri: fixture.uri.clone(),
        diagnostics: vec![Diagnostic {
            source: Some("php-lsp".into()),
            message: message.into(),
            ..Default::default()
        }],
        version: backend
            .current_document_state(fixture.uri.as_str())
            .map(|state| state.version),
        expected_state: backend.current_document_state(fixture.uri.as_str()),
        expected_template: None,
        expected_analyzer_source: None,
        require_idle_index: false,
        expected_runtime_generation: context.state.generation,
        indexing_workspace_folder: None,
        expected_indexing_run: None,
        expected_runtime_config: context.runtime_config().clone(),
        expected_index: context.index(&backend.index),
        computation_sequence: sequence,
    }
}

#[cfg(unix)]
async fn wait_for_starts(path: &Path, count: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if tokio::fs::read_to_string(path)
                .await
                .is_ok_and(|text| text.lines().count() >= count)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("analyzer did not reach the command barrier");
}

#[cfg(unix)]
fn block_phpstan(fixture: &Fixture) {
    let script = fixture.root.join("phpstan.sh");
    let old = std::fs::read_to_string(&script).unwrap();
    let started = shell_escape(&fixture.root.join("started").to_string_lossy());
    let release = shell_escape(&fixture.root.join("release").to_string_lossy());
    std::fs::write(script,format!("printf 'started\\n' >> {started}\nwhile [ ! -f {release} ]; do sleep 0.01; done\n{old}")).unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn editor_change_cancels_analyzer_and_only_publishes_the_new_buffer() {
    let source = "<?php namespace App; new OriginalMissingClass();";
    let changed = "<?php namespace App; new NewBufferClass();";
    let fixture = Fixture::new(source);
    let (service, mut rx) = fixture.backend(false).await;
    block_phpstan(&fixture);
    let backend = service.inner();
    fixture.install_source(backend, source, 1).await;
    tokio::time::timeout(Duration::from_secs(10),async {
        let publishing=backend.publish_diagnostics(&fixture.uri);
        let editing=async {
            wait_for_starts(&fixture.root.join("started"),1).await;
            backend.did_change(serde_json::from_value(serde_json::json!({"textDocument":{"uri":fixture.uri,"version":2},"contentChanges":[{"text":changed}]})).unwrap()).await;
            std::fs::write(fixture.root.join("release"),"").unwrap();
        };
        tokio::join!(publishing,editing);
        let result=published(&mut rx,&fixture.uri).await;
        assert_eq!(result["version"],2,"old editor version published: {result}");
        assert_eq!(external_count(&result),0);
        assert!(result["diagnostics"].as_array().unwrap().iter().any(|diagnostic|diagnostic["message"].as_str().is_some_and(|message|message.contains("NewBufferClass"))),"{result}");
    }).await.expect("edit cancellation stalled");
    backend.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_reopen_with_the_same_version_cannot_receive_an_old_analyzer_result() {
    let source = "<?php namespace App; new OldLifetimeClass();";
    let reopened = "<?php namespace App; new NewLifetimeClass();";
    let fixture = Fixture::new(source);
    let (service, mut rx) = fixture.backend(false).await;
    block_phpstan(&fixture);
    let backend = service.inner();
    fixture.install_source(backend, source, 1).await;
    tokio::time::timeout(Duration::from_secs(10),async {
        let publishing=backend.publish_diagnostics(&fixture.uri);
        let reopening=async {
            wait_for_starts(&fixture.root.join("started"),1).await;
            backend.did_close(serde_json::from_value(serde_json::json!({"textDocument":{"uri":fixture.uri}})).unwrap()).await;
            backend.did_open(serde_json::from_value(serde_json::json!({"textDocument":{"uri":fixture.uri,"languageId":"php","version":1,"text":reopened}})).unwrap()).await;
            std::fs::write(fixture.root.join("release"),"").unwrap();
        };
        tokio::join!(publishing,reopening);
        loop {
            let result=published(&mut rx,&fixture.uri).await;
            assert_eq!(external_count(&result),0,"old lifetime result: {result}");
            if result["version"]==1 {
                assert!(result["diagnostics"].as_array().unwrap().iter().any(|diagnostic|diagnostic["message"].as_str().is_some_and(|message|message.contains("NewLifetimeClass"))),"{result}");
                break;
            }
        }
    }).await.expect("close/reopen cancellation stalled");
    backend.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_analyzer_run_supersedes_an_old_run_without_changing_document_version() {
    let source = "<?php namespace App; new MissingClass();";
    let fixture = Fixture::new(source);
    let (service, mut rx) = fixture.backend(false).await;
    block_phpstan(&fixture);
    let backend = service.inner();
    fixture.install_source(backend, source, 1).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        let old = backend.publish_diagnostics(&fixture.uri);
        let replacement = async {
            wait_for_starts(&fixture.root.join("started"), 1).await;
            let latest = backend.publish_diagnostics(&fixture.uri);
            let release = async {
                wait_for_starts(&fixture.root.join("started"), 2).await;
                std::fs::write(fixture.root.join("release"), "").unwrap();
            };
            tokio::join!(latest, release);
        };
        tokio::join!(old, replacement);
        assert_eq!(external_count(&published(&mut rx, &fixture.uri).await), 2);
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("runs"))
                .unwrap()
                .lines()
                .count(),
            2,
            "cancelled run reached its output"
        );
    })
    .await
    .expect("replacement analyzer stalled");
    backend.shutdown().await.unwrap();
}
