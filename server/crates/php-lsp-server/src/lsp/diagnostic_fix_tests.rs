use super::*;
const URI: &str = "file:///qfm.php";
fn parser(source: &str) -> (FileParser, FileSymbols, WorkspaceIndex) {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    assert!(!parser.tree().unwrap().root_node().has_error(), "{source}");
    let file = extract_file_symbols(parser.tree().unwrap(), source, URI);
    let index = WorkspaceIndex::new();
    index.update_file_with_references(URI, file.clone(), vec![]);
    (parser, file, index)
}
fn diagnostic(line: u32, code: &str, message: &str) -> Diagnostic {
    Diagnostic {
        range: Range::new(Position::new(line, 0), Position::new(line, 1)),
        source: Some("phpstan".into()),
        code: Some(NumberOrString::String(code.into())),
        message: message.into(),
        ..Default::default()
    }
}
#[test]
fn qfm_batch_builds_one_position_index_for_all_callable_diagnostics() {
    let mut source = "<?php\n".to_string();
    for i in 0..12 {
        source += &format!("function f{i}():void {{throw new \\RuntimeException();}}\n");
    }
    let (parser, file, index) = parser(&source);
    RANGE_INDEX_BUILDS.with(|c| c.set(0));
    let cancel = OperationCancellationToken::new();
    let context = FixContext::new(
        URI,
        &source,
        parser.tree().unwrap(),
        &file,
        &index,
        None,
        0,
        &cancel,
    )
    .unwrap();
    for i in 0..12 {
        assert!(context.metadata(&diagnostic(i+1,"missingType.checkedException",&format!("Function f{i}() throws checked exception RuntimeException but it's missing from the PHPDoc @throws tag."))).is_some());
    }
    assert_eq!(
        RANGE_INDEX_BUILDS.with(|c| c.get()),
        1,
        "rebuilt a source-wide UTF-16 index per caller or diagnostic"
    );
}

#[test]
fn qfm_class_fix_candidates_must_be_legal_in_their_cst_position() {
    let mut unsafe_fixes = Vec::new();
    for (declaration, usage) in [
        ("interface Thing {}", "new V\\Thing();"),
        ("abstract class Thing {}", "new V\\Thing();"),
        ("enum Thing {}", "new V\\Thing();"),
        ("trait Thing {}", "new V\\Thing();"),
        ("class Thing {}", "class Child implements V\\Thing {}"),
        ("interface Thing {}", "class Child extends V\\Thing {}"),
        ("class Thing {}", "class Child {use V\\Thing;}"),
    ] {
        let source = format!("<?php namespace V {{{declaration}}} namespace App {{\n{usage}\n}}");
        let (parser, file, index) = parser(&source);
        let cancel = OperationCancellationToken::new();
        let context = FixContext::new(
            URI,
            &source,
            parser.tree().unwrap(),
            &file,
            &index,
            None,
            0,
            &cancel,
        )
        .unwrap();
        if context
            .metadata(&diagnostic(
                1,
                "class.notFound",
                "Class App\\V\\Thing not found.",
            ))
            .is_some()
        {
            unsafe_fixes.push(source);
        }
    }
    assert!(unsafe_fixes.is_empty(), "{}", unsafe_fixes.join("\n"));
}

#[test]
fn qfm_class_fix_keeps_valid_constructor_interface_and_type_candidates() {
    for (declaration, usage) in [
        ("class Thing {}", "new V\\Thing();"),
        ("interface Thing {}", "class Child implements V\\Thing {}"),
        ("class Thing {}", "class Child extends V\\Thing {}"),
        ("enum Thing {}", "function f(V\\Thing $x):void {}"),
    ] {
        let source = format!("<?php namespace V {{{declaration}}} namespace App {{\n{usage}\n}}");
        let (parser, file, index) = parser(&source);
        let cancel = OperationCancellationToken::new();
        let context = FixContext::new(
            URI,
            &source,
            parser.tree().unwrap(),
            &file,
            &index,
            None,
            0,
            &cancel,
        )
        .unwrap();
        assert!(
            context
                .metadata(&diagnostic(
                    1,
                    "class.notFound",
                    "Class App\\V\\Thing not found."
                ))
                .is_some(),
            "{source}"
        );
    }
}

#[test]
fn qfm_cancelled_preparation_does_not_build_source_facts() {
    let source = "<?php function run():void {}";
    let (parser, file, index) = parser(source);
    let cancel = OperationCancellationToken::new();
    cancel.cancel();
    RANGE_INDEX_BUILDS.with(|c| c.set(0));
    assert!(FixContext::new(
        URI,
        source,
        parser.tree().unwrap(),
        &file,
        &index,
        None,
        0,
        &cancel
    )
    .is_none());
    assert_eq!(RANGE_INDEX_BUILDS.with(|c| c.get()), 0);
}

struct Fixture {
    service: Arc<tower_lsp::LspService<PhpLspBackend>>,
    index: Arc<WorkspaceIndex>,
    uri: String,
    source: String,
    diagnostic: Diagnostic,
}
impl Fixture {
    async fn new() -> Self {
        use futures::StreamExt;
        use tower::{Service, ServiceExt};
        let (mut service, mut socket) = tower_lsp::LspService::new(PhpLspBackend::new);
        tokio::spawn(async move { while socket.next().await.is_some() {} });
        let request=tower_lsp::jsonrpc::Request::build("initialize").id(1).params(serde_json::json!({"capabilities":{},"initializationOptions":{"stubExtensions":[],"indexVendor":false,"diagnosticsMode":"off","analyzerCodeActions":{"enabled":true}}})).finish();
        let response = service
            .ready()
            .await
            .unwrap()
            .call(request)
            .await
            .unwrap()
            .unwrap();
        assert!(response.error().is_none());
        let service = Arc::new(service);
        let backend = service.inner();
        let uri = format!(
            "file:///qfm-race-{}.php",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let source = "<?php function Correct():void {}\ncorrect();\n".to_string();
        let (parser, mut file, _) = parser(&source);
        for symbol in &mut file.symbols {
            symbol.uri = uri.clone();
        }
        let request = backend.request_context_for_uri(&uri).await;
        let index = request.index(&backend.index);
        index.update_file_with_references(&uri, file.clone(), vec![]);
        let state = OpenDocumentState {
            version: 1,
            generation: 1,
        };
        let cancel = OperationCancellationToken::new();
        let context = FixContext::new(
            &uri,
            &source,
            parser.tree().unwrap(),
            &file,
            &index,
            Some(state),
            request.state.generation,
            &cancel,
        )
        .unwrap();
        let mut diagnostic = diagnostic(
            1,
            "function.nameCase",
            "Call to function Correct() with incorrect case: correct",
        );
        diagnostic.data = context.metadata(&diagnostic);
        assert!(diagnostic.data.is_some());
        drop(context);
        backend.open_files.insert(uri.clone(), parser);
        backend.document_versions.insert(uri.clone(), state);
        Self {
            service,
            index,
            uri,
            source,
            diagnostic,
        }
    }
    fn gate(
        &self,
    ) -> (
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::Sender<()>,
        std::sync::mpsc::Receiver<()>,
    ) {
        let (entered, wait) = std::sync::mpsc::channel();
        let (resume, release) = std::sync::mpsc::channel();
        let (cancelled, cancel_wait) = std::sync::mpsc::channel();
        METADATA_GATES
            .get_or_init(StdMutex::default)
            .lock()
            .unwrap()
            .insert(
                self.uri.clone(),
                MetadataGate {
                    entered,
                    resume: release,
                    cancelled,
                },
            );
        (wait, resume, cancel_wait)
    }
    fn params(&self) -> CodeActionParams {
        serde_json::from_value(serde_json::json!({"textDocument":{"uri":self.uri},"range":self.diagnostic.range,"context":{"diagnostics":[self.diagnostic],"only":["quickfix"]}})).unwrap()
    }
}
async fn wait_event(event: std::sync::mpsc::Receiver<()>) {
    tokio::task::spawn_blocking(move || event.recv_timeout(Duration::from_secs(5)).unwrap())
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn qfm_paused_validation_rejects_index_runtime_edit_and_reopen_changes() {
    for change in ["index", "runtime", "edit", "reopen"] {
        let fixture = Fixture::new().await;
        let params = fixture.params();
        let service = fixture.service.clone();
        let (wait, resume, _) = fixture.gate();
        let job =
            tokio::spawn(async move { service.inner().lsp_code_action(params).await.unwrap() });
        wait_event(wait).await;
        let backend = fixture.service.inner();
        match change {
            "index" => fixture.index.remove_file(&fixture.uri),
            "runtime" => {
                let mut state = backend.runtime_state.lock().await;
                *state = Arc::new(WorkspaceRuntimeState {
                    generation: state.generation + 1,
                    configs: state.configs.clone(),
                    fallback: state.fallback.clone(),
                    fallback_index: state.fallback_index.clone(),
                });
            }
            "edit" => {
                backend
                    .open_files
                    .get_mut(&fixture.uri)
                    .unwrap()
                    .parse_full(&format!("{}\n", fixture.source));
                backend.document_versions.insert(
                    fixture.uri.clone(),
                    OpenDocumentState {
                        version: 2,
                        generation: 1,
                    },
                );
            }
            "reopen" => {
                backend.document_versions.insert(
                    fixture.uri.clone(),
                    OpenDocumentState {
                        version: 1,
                        generation: 2,
                    },
                );
            }
            _ => unreachable!(),
        }
        resume.send(()).unwrap();
        assert!(
            job.await.unwrap().unwrap().is_empty(),
            "stale {change} edit"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn qfm_cancelled_validation_stops_its_worker_and_releases_the_permit() {
    let fixture = Fixture::new().await;
    let params = fixture.params();
    let service = fixture.service.clone();
    let permits = FILE_IO_BLOCKING_SEMAPHORE.available_permits();
    let (wait, _resume, cancelled) = fixture.gate();
    let job = tokio::spawn(async move { service.inner().lsp_code_action(params).await.unwrap() });
    wait_event(wait).await;
    assert_eq!(FILE_IO_BLOCKING_SEMAPHORE.available_permits(), permits - 1);
    job.abort();
    assert!(job.await.unwrap_err().is_cancelled());
    wait_event(cancelled).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while FILE_IO_BLOCKING_SEMAPHORE.available_permits() != permits {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
