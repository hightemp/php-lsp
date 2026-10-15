use super::*;
use tower_lsp::LspService;

struct Fixture {
    service: Arc<LspService<PhpLspBackend>>,
    index: Arc<WorkspaceIndex>,
    root: PathBuf,
    target_uri: String,
    base_uri: String,
    trait_uri: String,
    target_source: String,
    base_source: String,
    trait_source: String,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn parsed(source: &str, uri: &str) -> (FileParser, FileSymbols) {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    assert!(!parser.tree().unwrap().root_node().has_error(), "{source}");
    let file = extract_file_symbols(parser.tree().unwrap(), source, uri);
    (parser, file)
}
impl Fixture {
    async fn new() -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("php-lsp-p240-{nonce}"));
        std::fs::create_dir_all(&root).unwrap();
        let path_uri = |name: &str| crate::util::uri::path_to_uri(&root.join(name)).unwrap();
        let target_uri = path_uri("Child.php");
        let base_uri = path_uri("Base.php");
        let trait_uri = path_uri("Trait.php");
        let target_source = "<?php namespace App; class Child extends \\Lib\\Base {}".to_string();
        let base_source =
            "<?php namespace Lib; abstract class Base extends First {use Needed;}".to_string();
        let trait_source="<?php namespace Lib; class First{} class Other{} trait Needed {abstract public function run(parent $value): void;}".to_string();
        let (service, _socket) = LspService::new(PhpLspBackend::new);
        let service = Arc::new(service);
        let backend = service.inner();
        let request = backend.request_context_for_uri(&target_uri).await;
        let index = request.index(&backend.index);
        for (uri, source) in [(&base_uri, &base_source), (&trait_uri, &trait_source)] {
            std::fs::write(uri_to_path(uri).unwrap(), source).unwrap();
            let (parser, file) = parsed(source, uri);
            let refs = collect_symbol_references_in_file(parser.tree().unwrap(), source, &file);
            index.update_file_with_references_from_source(
                uri,
                file,
                refs,
                SourceFingerprint::from_bytes(source.as_bytes()),
            );
        }
        let (parser, file) = parsed(&target_source, &target_uri);
        index.update_file_with_references(&target_uri, file, vec![]);
        backend.open_files.insert(target_uri.clone(), parser);
        backend.document_versions.insert(
            target_uri.clone(),
            OpenDocumentState {
                version: 1,
                generation: 1,
            },
        );
        Self {
            service,
            index,
            root,
            target_uri,
            base_uri,
            trait_uri,
            target_source,
            base_source,
            trait_source,
        }
    }
    async fn plan(&self) -> Option<ImplementationContracts> {
        let backend = self.service.inner();
        let request = backend.request_context_for_uri(&self.target_uri).await;
        let (_, file) = parsed(&self.target_source, &self.target_uri);
        let class = file.symbols.iter().find(|s| s.name == "Child").unwrap();
        let missing = missing_implementation_methods(&self.index, &file, class);
        backend
            .implementation_contracts(&request, &file, class, &missing)
            .await
    }
}

#[tokio::test(flavor = "current_thread")]
async fn p240_contract_plan_checks_consumer_source_even_when_trait_source_is_unchanged() {
    let fixture = Fixture::new().await;
    assert!(
        fixture.plan().await.is_some(),
        "valid trait contract should be supported"
    );
    let changed = fixture.base_source.replace("First", "Other");
    assert_eq!(changed.len(), fixture.base_source.len());
    std::fs::write(uri_to_path(&fixture.base_uri).unwrap(), changed).unwrap();
    assert!(
        fixture.plan().await.is_none(),
        "stale consumer source selected the wrong parent type"
    );
}

impl Fixture {
    fn action(&self) -> CodeAction {
        let (_, file) = parsed(&self.target_source, &self.target_uri);
        let class = file.symbols.iter().find(|s| s.name == "Child").unwrap();
        let methods = missing_implementation_methods(&self.index, &file, class);
        let CodeActionOrCommand::CodeAction(mut action) = build_implement_missing_methods_action(
            self.target_uri.parse().unwrap(),
            class,
            &methods,
            range_from_byte_range(&self.target_source, class.selection_range),
            Some(1),
        )
        .unwrap() else {
            panic!()
        };
        action.data.as_mut().unwrap()["documentGeneration"] = serde_json::json!(1);
        action
    }
    fn pause(
        &self,
        hold_cancel: bool,
    ) -> (
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::Sender<()>,
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (entered, wait) = std::sync::mpsc::channel();
        let (resume, release) = std::sync::mpsc::channel();
        let (cancelled, cancel_wait) = std::sync::mpsc::channel();
        let (finish, finish_wait) = std::sync::mpsc::channel();
        PLAN_PAUSES
            .get_or_init(StdMutex::default)
            .lock()
            .unwrap()
            .insert(
                self.target_uri.clone(),
                PlanPause {
                    entered,
                    resume: release,
                    cancelled,
                    finish: hold_cancel.then_some(finish_wait),
                },
            );
        (wait, resume, cancel_wait, finish)
    }
}
async fn wait_event(receiver: std::sync::mpsc::Receiver<()>) {
    tokio::task::spawn_blocking(move || receiver.recv_timeout(Duration::from_secs(5)).unwrap())
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn p240_resolve_rejects_mid_worker_edits_reopen_source_index_and_runtime_changes() {
    for change in [
        "target",
        "reopen",
        "parent-open",
        "donor-disk",
        "consumer-disk",
        "index",
        "runtime",
    ] {
        let fixture = Fixture::new().await;
        let action = fixture.action();
        let service = fixture.service.clone();
        let (wait, resume, _, _) = fixture.pause(false);
        let job = tokio::spawn(async move {
            service
                .inner()
                .lsp_code_action_resolve(action)
                .await
                .unwrap()
        });
        wait_event(wait).await;
        let backend = fixture.service.inner();
        match change {
            "target" => {
                let mut parser = backend.open_files.get_mut(&fixture.target_uri).unwrap();
                parser.parse_full(&format!("{}\n", fixture.target_source));
                backend.document_versions.insert(
                    fixture.target_uri.clone(),
                    OpenDocumentState {
                        version: 2,
                        generation: 2,
                    },
                );
            }
            "reopen" => {
                backend.open_files.remove(&fixture.target_uri);
                backend.document_versions.remove(&fixture.target_uri);
                let entry = backend.open_files.entry(fixture.target_uri.clone());
                backend.document_versions.insert(
                    fixture.target_uri.clone(),
                    OpenDocumentState {
                        version: 1,
                        generation: 2,
                    },
                );
                entry.insert(parsed(&fixture.target_source, &fixture.target_uri).0);
            }
            "parent-open" => {
                let entry = backend.open_files.entry(fixture.base_uri.clone());
                backend.document_versions.insert(
                    fixture.base_uri.clone(),
                    OpenDocumentState {
                        version: 1,
                        generation: 2,
                    },
                );
                entry.insert(parsed(&fixture.base_source, &fixture.base_uri).0);
            }
            "donor-disk" => {
                let changed = fixture.trait_source.replace("run(", "fun(");
                assert_eq!(changed.len(), fixture.trait_source.len());
                std::fs::write(uri_to_path(&fixture.trait_uri).unwrap(), changed).unwrap();
            }
            "consumer-disk" => {
                let changed = fixture.base_source.replace("First", "Other");
                assert_eq!(changed.len(), fixture.base_source.len());
                std::fs::write(uri_to_path(&fixture.base_uri).unwrap(), changed).unwrap();
            }
            "index" => fixture.index.remove_file(&fixture.trait_uri),
            "runtime" => {
                let mut state = backend.runtime_state.lock().await;
                *state = Arc::new(WorkspaceRuntimeState {
                    generation: state.generation + 1,
                    configs: state.configs.clone(),
                    fallback: state.fallback.clone(),
                    fallback_index: state.fallback_index.clone(),
                });
            }
            _ => unreachable!(),
        }
        resume.send(()).unwrap();
        let result = job.await.unwrap();
        assert!(
            result.edit.unwrap().changes.unwrap().is_empty(),
            "stale edit after {change}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn p240_cancelled_contract_worker_retains_its_permit_until_it_exits() {
    let fixture = Fixture::new().await;
    let action = fixture.action();
    let service = fixture.service.clone();
    let permits = FILE_IO_BLOCKING_SEMAPHORE.available_permits();
    let (wait, _resume, cancelled, finish) = fixture.pause(true);
    let job = tokio::spawn(async move {
        service
            .inner()
            .lsp_code_action_resolve(action)
            .await
            .unwrap()
    });
    wait_event(wait).await;
    assert_eq!(FILE_IO_BLOCKING_SEMAPHORE.available_permits(), permits - 1);
    job.abort();
    assert!(job.await.unwrap_err().is_cancelled());
    wait_event(cancelled).await;
    assert_eq!(
        FILE_IO_BLOCKING_SEMAPHORE.available_permits(),
        permits - 1,
        "permit returned before worker exit"
    );
    finish.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while FILE_IO_BLOCKING_SEMAPHORE.available_permits() != permits {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn p240_contract_publication_rechecks_dependencies_after_the_last_await() {
    for change in ["index", "donor-open"] {
        let fixture = Fixture::new().await;
        let action = fixture.action();
        let (entered, wait) = tokio::sync::oneshot::channel();
        let (resume, release) = tokio::sync::oneshot::channel();
        PUBLICATION_PAUSES
            .get_or_init(StdMutex::default)
            .lock()
            .unwrap()
            .insert(fixture.target_uri.clone(), (entered, release));
        let service = fixture.service.clone();
        let job = tokio::spawn(async move {
            service
                .inner()
                .lsp_code_action_resolve(action)
                .await
                .unwrap()
        });
        tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .unwrap()
            .unwrap();
        if change == "index" {
            fixture.index.remove_file(&fixture.trait_uri);
        } else {
            fixture.service.inner().document_versions.insert(
                fixture.trait_uri.clone(),
                OpenDocumentState {
                    version: 1,
                    generation: 9,
                },
            );
        }
        resume.send(()).unwrap();
        assert!(
            job.await.unwrap().edit.unwrap().changes.unwrap().is_empty(),
            "stale publication after {change}"
        );
    }
}
