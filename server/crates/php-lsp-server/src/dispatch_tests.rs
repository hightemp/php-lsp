use super::*;
use serde_json::{json, Value};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tower_lsp::ls_types::*;
use tower_lsp::{LanguageServer, LspService};

#[derive(Default)]
struct Gate {
    entered: tokio::sync::Notify,
    polls: std::sync::atomic::AtomicUsize,
    released: Mutex<bool>,
    cv: Condvar,
}
impl Gate {
    fn block(&self) {
        self.polls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        let (released, timeout) = self
            .cv
            .wait_timeout_while(
                self.released.lock().unwrap(),
                Duration::from_secs(3),
                |release| !*release,
            )
            .unwrap();
        assert!(
            *released || !timeout.timed_out(),
            "test gate watchdog expired"
        );
    }
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.cv.notify_all();
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct Probe {
    client: tower_lsp::Client,
    block_shutdown: Arc<std::sync::atomic::AtomicBool>,
    shutdown_entered: Arc<Notify>,
    gate: Arc<Gate>,
    changes: Arc<Mutex<Vec<i32>>>,
    change_entered: Arc<tokio::sync::Notify>,
    change_resume: Arc<tokio::sync::Notify>,
}
impl LanguageServer for Probe {
    async fn initialize(
        &self,
        _: InitializeParams,
    ) -> tower_lsp::jsonrpc::Result<InitializeResult> {
        Ok(InitializeResult::default())
    }
    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        if params.text_document.version == 13 {
            panic!("expected notification panic");
        }
        if params.text_document.version == 2 {
            self.change_entered.notify_one();
            self.change_resume.notified().await;
        }
        self.changes
            .lock()
            .unwrap()
            .push(params.text_document.version);
        super::document_committed();
    }
    async fn completion_resolve(
        &self,
        mut item: CompletionItem,
    ) -> tower_lsp::jsonrpc::Result<CompletionItem> {
        item.detail = Some(format!("{:?}", *self.changes.lock().unwrap()));
        Ok(item)
    }
    async fn code_action_resolve(
        &self,
        mut action: CodeAction,
    ) -> tower_lsp::jsonrpc::Result<CodeAction> {
        action.title = format!("{:?}", *self.changes.lock().unwrap());
        Ok(action)
    }
    async fn shutdown(&self) -> tower_lsp::jsonrpc::Result<()> {
        self.shutdown_entered.notify_one();
        if self.block_shutdown.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        Ok(())
    }
}
impl Probe {
    async fn panic_request(&self) -> tower_lsp::jsonrpc::Result<Value> {
        panic!("expected worker panic")
    }
    async fn wake_during_poll(&self) -> tower_lsp::jsonrpc::Result<Value> {
        let mut polls = 0;
        poll_fn(move |cx| {
            polls += 1;
            if polls < 64 {
                cx.waker().wake_by_ref();
                Poll::Pending
            } else {
                Poll::Ready(Ok(json!(polls)))
            }
        })
        .await
    }

    async fn notification(&self) {
        let _ = self.client.workspace_folders().await;
    }
    async fn heavy(&self) -> tower_lsp::jsonrpc::Result<Value> {
        self.gate.block();
        Ok(json!("heavy finished"))
    }
    async fn state(&self, _: serde_json::Value) -> tower_lsp::jsonrpc::Result<Value> {
        Ok(json!(*self.changes.lock().unwrap()))
    }
    async fn ping(&self) -> tower_lsp::jsonrpc::Result<Value> {
        Ok(json!("pong"))
    }
}
struct Wire {
    read: BufReader<tokio::io::ReadHalf<DuplexStream>>,
    write: tokio::io::WriteHalf<DuplexStream>,
    job: tokio::task::JoinHandle<()>,
    state: Arc<State>,
    block_shutdown: Arc<std::sync::atomic::AtomicBool>,
    shutdown_entered: Arc<Notify>,
    change_entered: Arc<tokio::sync::Notify>,
    change_resume: Arc<tokio::sync::Notify>,
}
impl Wire {
    async fn start(gate: Arc<Gate>) -> Self {
        let change_entered = Arc::new(tokio::sync::Notify::new());
        let change_resume = Arc::new(tokio::sync::Notify::new());
        let entered = change_entered.clone();
        let resume = change_resume.clone();
        let block_shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shutdown_flag = block_shutdown.clone();
        let shutdown_entered = Arc::new(Notify::new());
        let shutdown_notice = shutdown_entered.clone();
        let (service, socket) = LspService::build(move |client| Probe {
            client,
            block_shutdown: shutdown_flag,
            shutdown_entered: shutdown_notice,
            gate,
            changes: Arc::default(),
            change_entered: entered,
            change_resume: resume,
        })
        .custom_method("test/heavy", Probe::heavy)
        .custom_method("test/ping", Probe::ping)
        .custom_method("test/state", Probe::state)
        .custom_method("test/notification", Probe::notification)
        .custom_method("test/panic", Probe::panic_request)
        .custom_method("test/wake", Probe::wake_during_poll)
        .finish();
        let mut wire = Self::with_service(service, socket).await;
        wire.block_shutdown = block_shutdown;
        wire.shutdown_entered = shutdown_entered;
        wire.change_entered = change_entered;
        wire.change_resume = change_resume;
        wire
    }
    async fn with_service<S>(service: S, socket: ClientSocket) -> Self
    where
        S: Service<Request, Response = Option<Response>> + Send + 'static,
        S::Error: Into<Box<dyn std::error::Error + Send + Sync>> + Send + 'static,
        S::Future: Send + 'static,
    {
        let (client, server) = tokio::io::duplex(65536);
        let (input, output) = tokio::io::split(server);
        let dispatcher = Dispatcher::new(service);
        let state = dispatcher.state.clone();
        let job = tokio::spawn(serve_dispatcher(input, output, dispatcher, socket));
        let (read, write) = tokio::io::split(client);
        let mut wire = Self {
            read: BufReader::new(read),
            write,
            job,
            state,
            block_shutdown: Arc::default(),
            shutdown_entered: Arc::default(),
            change_entered: Arc::default(),
            change_resume: Arc::default(),
        };
        wire.send(json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"capabilities":{},"initializationOptions":{"stubExtensions":[],"indexVendor":false,"diagnosticsMode":"off"}}})).await;
        assert!(wire.response().await.get("result").is_some());
        wire
    }
    async fn reply(&mut self, id: i64) -> Value {
        loop {
            let reply = self.response().await;
            if reply.get("id") == Some(&json!(id)) {
                return reply;
            }
        }
    }
    async fn send(&mut self, value: Value) {
        let body = serde_json::to_vec(&value).unwrap();
        self.write
            .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
            .await
            .unwrap();
        self.write.write_all(&body).await.unwrap();
    }
    async fn response(&mut self) -> Value {
        tokio::time::timeout(Duration::from_millis(800), async {
            let mut len = None;
            loop {
                let mut line = String::new();
                assert!(self.read.read_line(&mut line).await.unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.strip_prefix("Content-Length: ") {
                    len = Some(value.trim().parse::<usize>().unwrap());
                }
            }
            let mut body = vec![0; len.unwrap()];
            self.read.read_exact(&mut body).await.unwrap();
            serde_json::from_slice(&body).unwrap()
        })
        .await
        .expect("transport stalled behind handler computation")
    }
    async fn finish(mut self) {
        self.send(json!({"jsonrpc":"2.0","method":"exit"})).await;
        tokio::time::timeout(Duration::from_secs(2), &mut self.job)
            .await
            .unwrap()
            .unwrap();
    }
}
impl Drop for Wire {
    fn drop(&mut self) {
        self.change_resume.notify_one();
        self.job.abort();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_transport_polls_a_light_request_while_a_heavy_poll_is_blocked() {
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let mut wire = Wire::start(gate.clone()).await;
    wire.send(json!({"jsonrpc":"2.0","id":1,"method":"test/heavy"}))
        .await;
    tokio::time::timeout(Duration::from_secs(1), gate.entered.notified())
        .await
        .unwrap();
    wire.send(json!({"jsonrpc":"2.0","id":2,"method":"test/ping"}))
        .await;
    let response = wire.response().await;
    assert_eq!(response["id"], 2);
    assert_eq!(response["result"], "pong");
    gate.release();
    assert_eq!(wire.response().await["id"], 1);
    wire.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_transport_cancels_a_running_poll_before_it_finishes() {
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let mut wire = Wire::start(gate.clone()).await;
    wire.send(json!({"jsonrpc":"2.0","id":1,"method":"test/heavy"}))
        .await;
    tokio::time::timeout(Duration::from_secs(1), gate.entered.notified())
        .await
        .unwrap();
    wire.send(json!({"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":1}}))
        .await;
    let response = wire.response().await;
    assert_eq!(response["id"], 1);
    assert_eq!(response["error"]["code"], -32800);
    gate.release();
    wire.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_transport_preserves_notification_order_and_read_fences_with_version_gaps() {
    let mut wire = Wire::start(Arc::default()).await;
    wire.send(json!({"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":"file:///ordered.php","version":2},"contentChanges":[{"text":"first"}]}})).await;
    tokio::time::timeout(Duration::from_secs(1), wire.change_entered.notified())
        .await
        .unwrap();
    wire.send(json!({"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":"file:///ordered.php","version":7},"contentChanges":[{"text":"second"}]}})).await;
    wire.send(json!({"jsonrpc":"2.0","id":10,"method":"test/state","params":{"textDocument":{"uri":"file:///ordered.php"}}})).await;
    wire.send(json!({"jsonrpc":"2.0","id":11,"method":"test/ping"}))
        .await;
    assert_eq!(
        wire.response().await["id"],
        11,
        "a read overtook its preceding document mutation"
    );
    wire.change_resume.notify_one();
    let response = wire.response().await;
    assert_eq!(response["id"], 10);
    assert_eq!(response["result"], json!([2, 7]));
    wire.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_transport_cancellation_survives_more_than_the_transport_queue_of_requests() {
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let mut wire = Wire::start(gate.clone()).await;
    for id in 1..=4 {
        wire.send(json!({"jsonrpc":"2.0","id":id,"method":"test/heavy"}))
            .await;
    }
    tokio::time::timeout(Duration::from_secs(1), gate.entered.notified())
        .await
        .unwrap();
    for id in 100..228 {
        wire.send(json!({"jsonrpc":"2.0","id":id,"method":"test/ping"}))
            .await;
    }
    for id in 1..=4 {
        wire.send(json!({"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":id}}))
            .await;
    }
    let mut seen = std::collections::HashSet::new();
    let mut cancelled = std::collections::HashSet::new();
    while cancelled.len() < 4 {
        let response = wire.response().await;
        let id = response["id"].as_i64().unwrap();
        assert!(seen.insert(id), "duplicate response: {response}");
        if (1..=4).contains(&id) {
            assert_eq!(response["error"]["code"], -32800);
            cancelled.insert(id);
        }
    }
    gate.release();
    while seen.len() < 132 {
        let response = wire.response().await;
        assert!(
            seen.insert(response["id"].as_i64().unwrap()),
            "duplicate response: {response}"
        );
    }
    wire.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_transport_notification_capacity_does_not_block_client_responses() {
    let mut wire = Wire::start(Arc::default()).await;
    for _ in 0..MAX_NOTIFICATIONS {
        wire.send(json!({"jsonrpc":"2.0","method":"test/notification"}))
            .await;
    }
    let mut replies = Vec::new();
    for _ in 0..MAX_NOTIFICATIONS {
        let request = wire.response().await;
        assert_eq!(request["method"], "workspace/workspaceFolders");
        replies.push(request["id"].clone());
    }
    wire.send(json!({"jsonrpc":"2.0","id":900,"method":"test/ping"}))
        .await;
    for id in replies {
        wire.send(json!({"jsonrpc":"2.0","id":id,"result":[]}))
            .await;
    }
    assert_eq!(wire.response().await["id"], 900);
    wire.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_transport_exit_cancels_pending_shutdown_without_eof() {
    let mut wire = Wire::start(Arc::default()).await;
    wire.block_shutdown.store(true, Ordering::SeqCst);
    wire.send(json!({"jsonrpc":"2.0","id":99,"method":"shutdown"}))
        .await;
    tokio::time::timeout(Duration::from_secs(1), wire.shutdown_entered.notified())
        .await
        .unwrap();
    wire.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_transport_resolve_waits_for_prior_document_mutation() {
    let mut wire = Wire::start(Arc::default()).await;
    wire.send(json!({"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":"file:///resolve.php","version":2},"contentChanges":[{"text":"first"}]}})).await;
    tokio::time::timeout(Duration::from_secs(1), wire.change_entered.notified())
        .await
        .unwrap();
    wire.send(json!({"jsonrpc":"2.0","id":10,"method":"codeAction/resolve","params":{"title":"action","data":{"uri":"file:///resolve.php"}}})).await;
    wire.send(json!({"jsonrpc":"2.0","id":11,"method":"test/ping"}))
        .await;
    assert_eq!(wire.response().await["id"], 11);
    wire.change_resume.notify_one();
    let response = wire.response().await;
    assert_eq!(response["id"], 10);
    assert_eq!(response["result"]["title"], "[2]");
    wire.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_transport_duplicate_shutdown_does_not_change_session_state() {
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let mut wire = Wire::start(gate.clone()).await;
    wire.send(json!({"jsonrpc":"2.0","id":1,"method":"test/heavy"}))
        .await;
    tokio::time::timeout(Duration::from_secs(1), gate.entered.notified())
        .await
        .unwrap();
    wire.send(json!({"jsonrpc":"2.0","id":1,"method":"shutdown"}))
        .await;
    assert_eq!(wire.response().await["error"]["code"], -32600);
    wire.send(json!({"jsonrpc":"2.0","id":2,"method":"test/ping"}))
        .await;
    let response = wire.response().await;
    assert_eq!(response["id"], 2);
    assert_eq!(response["result"], "pong");
    gate.release();
    assert_eq!(wire.response().await["result"], "heavy finished");
    wire.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_transport_notification_overflow_ends_session_without_losing_edits_silently() {
    let mut wire = Wire::start(Arc::default()).await;
    wire.send(json!({"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":"file:///overflow.php","version":2},"contentChanges":[{"text":"first"}]}})).await;
    tokio::time::timeout(Duration::from_secs(1), wire.change_entered.notified())
        .await
        .unwrap();
    for version in 3..MAX_NOTIFICATIONS + 3 {
        wire.send(json!({"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":"file:///overflow.php","version":version},"contentChanges":[{"text":"next"}]}})).await;
    }
    tokio::time::timeout(Duration::from_secs(1), &mut wire.job)
        .await
        .expect("overloaded document session stayed open after an unaccepted edit")
        .unwrap();
}

type DocumentPause = (Arc<Notify>, Arc<Notify>);
static DOCUMENT_PAUSES: std::sync::LazyLock<Mutex<HashMap<(String, &'static str), DocumentPause>>> =
    std::sync::LazyLock::new(Mutex::default);
pub(super) async fn pause_document_post(uri: &str, method: &'static str) {
    let pause = DOCUMENT_PAUSES
        .lock()
        .unwrap()
        .remove(&(uri.to_string(), method));
    if let Some((entered, resume)) = pause {
        entered.notify_one();
        resume.notified().await;
    }
}
struct Resume(Arc<Notify>);
impl Drop for Resume {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

#[path = "dispatch_backend_tests.rs"]
mod backend;

async fn eventually(check: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !check() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("worker ownership was not released");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_running_cancellation_keeps_permits_and_rejected_id_can_be_reused() {
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let mut wire = Wire::start(gate.clone()).await;
    for id in 1..=MAX_REQUESTS {
        wire.send(json!({"jsonrpc":"2.0","id":id,"method":"test/heavy"}))
            .await;
    }
    eventually(|| {
        wire.state.requests.available_permits() == 0
            && gate.polls.load(Ordering::SeqCst) == QUERY_POLLS
    })
    .await;
    assert_eq!(wire.state.queries.available_permits(), 0);
    wire.send(json!({"jsonrpc":"2.0","id":999,"method":"test/ping"}))
        .await;
    assert_eq!(wire.response().await["error"]["code"], -32000);
    for id in 1..=MAX_REQUESTS {
        wire.send(json!({"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":id}}))
            .await;
    }
    let mut cancelled = std::collections::HashSet::new();
    for _ in 0..MAX_REQUESTS {
        let response = wire.response().await;
        assert_eq!(response["error"]["code"], -32800);
        assert!(cancelled.insert(response["id"].clone().to_string()));
    }
    assert_eq!(
        wire.state.queries.available_permits(),
        0,
        "running polls released capacity on logical cancellation"
    );
    assert!(wire.state.requests.available_permits() <= MAX_REQUESTS - QUERY_POLLS);
    gate.release();
    eventually(|| wire.state.requests.available_permits() == MAX_REQUESTS).await;
    wire.send(json!({"jsonrpc":"2.0","id":999,"method":"test/ping"}))
        .await;
    assert_eq!(wire.response().await["result"], "pong");
    wire.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_cancel_running_reuse_id_cancel_again_cannot_remove_a_new_request() {
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let mut wire = Wire::start(gate.clone()).await;
    wire.send(json!({"jsonrpc":"2.0","id":1,"method":"test/heavy"}))
        .await;
    gate.entered.notified().await;
    wire.send(json!({"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":1}}))
        .await;
    assert_eq!(wire.response().await["error"]["code"], -32800);
    wire.send(json!({"jsonrpc":"2.0","id":1,"method":"test/heavy"}))
        .await;
    wire.send(json!({"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":1}}))
        .await;
    assert_eq!(wire.response().await["error"]["code"], -32800);
    assert_eq!(
        gate.polls.load(Ordering::SeqCst),
        1,
        "new ID incarnation ran before old physical completion"
    );
    gate.release();
    wire.send(json!({"jsonrpc":"2.0","id":1,"method":"test/ping"}))
        .await;
    assert_eq!(wire.response().await["result"], "pong");
    wire.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_worker_panic_cleans_up_id_capacity_and_next_request() {
    let mut wire = Wire::start(Arc::default()).await;
    wire.send(json!({"jsonrpc":"2.0","id":1,"method":"test/panic"}))
        .await;
    assert_eq!(wire.response().await["error"]["code"], -32603);
    wire.send(json!({"jsonrpc":"2.0","id":1,"method":"test/ping"}))
        .await;
    assert_eq!(wire.response().await["result"], "pong");
    eventually(|| {
        wire.state.requests.available_permits() == MAX_REQUESTS
            && wire.state.payloads.available_permits() == MAX_PAYLOAD_WEIGHT as usize
    })
    .await;
    wire.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_self_wakes_during_poll_are_not_lost() {
    let mut wire = Wire::start(Arc::default()).await;
    wire.send(json!({"jsonrpc":"2.0","id":1,"method":"test/wake"}))
        .await;
    assert_eq!(wire.response().await["result"], 64);
    wire.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_dropped_transport_retains_running_poll_then_releases_every_resource() {
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let mut wire = Wire::start(gate.clone()).await;
    let state = wire.state.clone();
    wire.send(json!({"jsonrpc":"2.0","id":1,"method":"test/heavy"}))
        .await;
    gate.entered.notified().await;
    drop(wire);
    eventually(|| state.terminal.is_cancelled()).await;
    assert_eq!(state.queries.available_permits(), QUERY_POLLS - 1);
    gate.release();
    eventually(|| {
        state.queries.available_permits() == QUERY_POLLS
            && state.requests.available_permits() == MAX_REQUESTS
            && state.payloads.available_permits() == MAX_PAYLOAD_WEIGHT as usize
            && state.jobs.lock().unwrap().is_empty()
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_payload_admission_rejects_before_registering_the_request_id() {
    let mut wire = Wire::start(Arc::default()).await;
    let reserve = wire
        .state
        .payloads
        .clone()
        .acquire_many_owned(MAX_PAYLOAD_WEIGHT)
        .await
        .unwrap();
    wire.send(json!({"jsonrpc":"2.0","id":21,"method":"test/ping"}))
        .await;
    assert_eq!(wire.response().await["error"]["code"], -32000);
    drop(reserve);
    wire.send(json!({"jsonrpc":"2.0","id":21,"method":"test/ping"}))
        .await;
    assert_eq!(wire.response().await["result"], "pong");
    wire.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_rejected_document_request_does_not_replace_the_notification_fence() {
    let mut wire = Wire::start(Arc::default()).await;
    wire.send(json!({"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":"file:///fence.php","version":2},"contentChanges":[{"text":"first"}]}})).await;
    wire.change_entered.notified().await;
    wire.send(json!({"jsonrpc":"2.0","id":15,"method":"textDocument/didChange","params":{"textDocument":{"uri":"file:///fence.php","version":3},"contentChanges":[{"text":"invalid"}]}})).await;
    assert_eq!(wire.response().await["error"]["code"], -32600);
    wire.send(json!({"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":"file:///fence.php","version":7},"contentChanges":[{"text":"second"}]}})).await;
    wire.send(json!({"jsonrpc":"2.0","id":10,"method":"test/state","params":{"textDocument":{"uri":"file:///fence.php"}}})).await;
    wire.send(json!({"jsonrpc":"2.0","id":11,"method":"test/ping"}))
        .await;
    assert_eq!(wire.response().await["id"], 11);
    wire.change_resume.notify_one();
    assert_eq!(wire.response().await["result"], json!([2, 7]));
    wire.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_cancelled_read_waiting_for_document_commit_releases_admission() {
    let mut wire = Wire::start(Arc::default()).await;
    wire.send(json!({"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":"file:///queued.php","version":2},"contentChanges":[{"text":"first"}]}})).await;
    wire.change_entered.notified().await;
    wire.send(json!({"jsonrpc":"2.0","id":10,"method":"test/state","params":{"textDocument":{"uri":"file:///queued.php"}}})).await;
    eventually(|| wire.state.requests.available_permits() == MAX_REQUESTS - 1).await;
    wire.send(json!({"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":10}}))
        .await;
    assert_eq!(wire.response().await["error"]["code"], -32800);
    eventually(|| wire.state.requests.available_permits() == MAX_REQUESTS).await;
    wire.change_resume.notify_one();
    wire.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_shutdown_does_not_need_ordinary_payload_capacity() {
    let mut wire = Wire::start(Arc::default()).await;
    let _reserve = wire
        .state
        .payloads
        .clone()
        .acquire_many_owned(MAX_PAYLOAD_WEIGHT)
        .await
        .unwrap();
    wire.send(json!({"jsonrpc":"2.0","id":99,"method":"shutdown"}))
        .await;
    let response = wire.response().await;
    assert!(response.get("result").is_some(), "{response}");
    wire.finish().await;
}

#[test]
fn dispatch_payload_weight_accounts_for_retained_string_request_ids() {
    let id = "x".repeat(4096);
    let request = Request::build("test/ping").id(id.clone()).finish();
    assert!(
        payload_weight(&request) >= id.len() as u32,
        "ID payload bypassed accounting"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_oversized_request_id_is_rejected_before_registration() {
    let mut wire = Wire::start(Arc::default()).await;
    let id = "x".repeat(65537);
    wire.send(json!({"jsonrpc":"2.0","id":id,"method":"test/ping"}))
        .await;
    let response = wire.response().await;
    assert_eq!(response["error"]["code"], -32600);
    assert!(
        response["id"].is_null(),
        "oversized ID retained in rejection response"
    );
    assert!(wire.state.jobs.lock().unwrap().is_empty());
    wire.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_notification_panic_ends_session_before_following_edits() {
    let mut wire = Wire::start(Arc::default()).await;
    for version in [13, 14] {
        wire.send(json!({"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":"file:///panic.php","version":version},"contentChanges":[{"text":"next"}]}})).await;
    }
    tokio::time::timeout(Duration::from_secs(1), &mut wire.job)
        .await
        .expect("continued a session after a failed mandatory mutation")
        .unwrap();
    assert!(wire.state.terminal.is_cancelled());
}

async fn direct_probe(gate: Arc<Gate>) -> (Dispatcher<LspService<Probe>>, ClientSocket) {
    use tower::ServiceExt;
    let (service, socket) = LspService::build(move |client| Probe {
        client,
        gate,
        changes: Arc::default(),
        change_entered: Arc::default(),
        change_resume: Arc::default(),
        block_shutdown: Arc::default(),
        shutdown_entered: Arc::default(),
    })
    .custom_method("test/heavy", Probe::heavy)
    .custom_method("test/ping", Probe::ping)
    .finish();
    let mut dispatcher = Dispatcher::new(service);
    let initialized = dispatcher
        .ready()
        .await
        .unwrap()
        .call(
            Request::build("initialize")
                .id(0)
                .params(json!({"capabilities":{}}))
                .finish(),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(initialized.error().is_none());
    (dispatcher, socket)
}

#[tokio::test(flavor = "current_thread")]
async fn dispatch_drop_receipt_before_first_poll_cancels_eager_work_and_releases_id() {
    let (mut dispatcher, _socket) = direct_probe(Arc::default()).await;
    let receipt = dispatcher.call(Request::build("test/ping").id(1).finish());
    let job = dispatcher
        .state
        .jobs
        .lock()
        .unwrap()
        .get(&Id::Number(1))
        .unwrap()
        .clone();
    drop(receipt);
    assert!(
        job.cancel.is_cancelled(),
        "unpolled receipt had no cancellation owner"
    );
    assert!(job.replied.load(Ordering::SeqCst));
    tokio::time::timeout(Duration::from_secs(1), job.finished.wait())
        .await
        .unwrap();
    assert!(dispatcher.state.jobs.lock().unwrap().is_empty());
    let response = dispatcher
        .call(Request::build("test/ping").id(1).finish())
        .await
        .unwrap()
        .unwrap();
    assert!(response.error().is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn dispatch_prestart_cancelled_id_reuse_keeps_the_physical_predecessor_fence() {
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let (mut dispatcher, _socket) = direct_probe(gate.clone()).await;
    let first = dispatcher.call(Request::build("test/heavy").id(1).finish());
    gate.entered.notified().await;
    dispatcher
        .call(
            Request::build("$/cancelRequest")
                .params(json!({"id":1}))
                .finish(),
        )
        .await
        .unwrap();
    assert!(first.await.unwrap().unwrap().error().is_some());
    let second = dispatcher.call(Request::build("test/heavy").id(1).finish());
    let queued = dispatcher
        .state
        .jobs
        .lock()
        .unwrap()
        .get(&Id::Number(1))
        .unwrap()
        .clone();
    dispatcher
        .call(
            Request::build("$/cancelRequest")
                .params(json!({"id":1}))
                .finish(),
        )
        .await
        .unwrap();
    assert!(second.await.unwrap().unwrap().error().is_some());
    // current_thread + ready cancellation above guarantees the second driver
    // has not run until this explicit notification wait yields to it.
    queued.entered.wait().await;
    assert!(
        !queued.finished.done(),
        "pre-start cancellation skipped the old running poll"
    );
    let third = dispatcher.call(Request::build("test/ping").id(1).finish());
    gate.release();
    let response = third.await.unwrap().unwrap();
    assert!(response.error().is_none(), "{response:?}");
}

#[test]
fn dispatch_cancel_ids_are_bounded_before_clone_and_match_scalar_forms() {
    for (value, expected) in [
        (json!(7), Some(Id::Number(7))),
        (json!("known"), Some(Id::String("known".into()))),
        (Value::Null, None),
        (json!({"nested":"unknown"}), None),
        (json!("x".repeat(MAX_ID_BYTES + 1)), None),
    ] {
        let request = Request::build("$/cancelRequest")
            .params(json!({"id":value}))
            .finish();
        assert_eq!(cancellation_id(&request), expected);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_legacy_completion_resolve_waits_for_document_commits_without_a_uri() {
    let mut wire = Wire::start(Arc::default()).await;
    wire.send(json!({"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":"file:///legacy.php","version":2},"contentChanges":[{"text":"first"}]}})).await;
    wire.change_entered.notified().await;
    wire.send(json!({"jsonrpc":"2.0","id":10,"method":"completionItem/resolve","params":{"label":"Legacy","data":"Legacy"}})).await;
    wire.send(json!({"jsonrpc":"2.0","id":11,"method":"test/ping"}))
        .await;
    assert_eq!(wire.response().await["id"], 11);
    wire.change_resume.notify_one();
    assert_eq!(wire.response().await["result"]["detail"], "[2]");
    wire.finish().await;
}

#[test]
fn dispatch_document_uris_follow_supported_payload_shapes() {
    for (method, params, expected) in [
        (
            "textDocument/hover",
            json!({"textDocument":{"uri":"file:///a.php"}}),
            vec!["file:///a.php"],
        ),
        (
            "codeAction/resolve",
            json!({"data":{"uri":"file:///a.php"}}),
            vec!["file:///a.php"],
        ),
        (
            "completionItem/resolve",
            json!({"data":{"workspaceUri":"file:///a.php","uri":"file:///ignored.php"}}),
            vec!["file:///a.php"],
        ),
        (
            "callHierarchy/incomingCalls",
            json!({"item":{"uri":"file:///b.php","data":{"workspaceUri":"file:///a.php"}}}),
            vec!["file:///a.php", "file:///b.php"],
        ),
        (
            "typeHierarchy/supertypes",
            json!({"item":{"uri":"file:///a.php","data":{"workspaceUri":"file:///a.php"}}}),
            vec!["file:///a.php"],
        ),
        ("completionItem/resolve", json!({"data":"Legacy"}), vec![]),
    ] {
        let request = Request::build(method).params(params).id(1).finish();
        assert_eq!(request_document_uris(&request), expected, "{method}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn dispatch_completed_response_retains_admission_until_its_receipt_is_consumed() {
    let (mut dispatcher, _socket) = direct_probe(Arc::default()).await;
    let receipt = dispatcher.call(Request::build("test/ping").id(1).finish());
    let job = dispatcher
        .state
        .jobs
        .lock()
        .unwrap()
        .get(&Id::Number(1))
        .unwrap()
        .clone();
    tokio::time::timeout(Duration::from_secs(1), job.finished.wait())
        .await
        .unwrap();
    assert_eq!(
        dispatcher.state.requests.available_permits(),
        MAX_REQUESTS - 1,
        "completed but unread response escaped admission"
    );
    assert!(dispatcher.state.payloads.available_permits() < MAX_PAYLOAD_WEIGHT as usize);
    assert!(receipt.await.unwrap().unwrap().error().is_none());
    assert_eq!(dispatcher.state.requests.available_permits(), MAX_REQUESTS);
    assert_eq!(
        dispatcher.state.payloads.available_permits(),
        MAX_PAYLOAD_WEIGHT as usize
    );
}

#[tokio::test(flavor = "current_thread")]
async fn dispatch_non_lsp_ids_are_rejected_without_changing_lifecycle_or_admission() {
    let (mut dispatcher, _socket) = direct_probe(Arc::default()).await;
    for id in [
        Id::Null,
        Id::Number(i64::from(i32::MAX) + 1),
        Id::Number(i64::from(i32::MIN) - 1),
    ] {
        let response = dispatcher
            .call(Request::build("shutdown").id(id).finish())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            response.error().map(|error| error.code),
            Some(ErrorCode::InvalidRequest)
        );
        assert!(!dispatcher.state.shutdown.load(Ordering::SeqCst));
        assert!(dispatcher.state.jobs.lock().unwrap().is_empty());
    }
    assert_eq!(dispatcher.state.requests.available_permits(), MAX_REQUESTS);
    assert!(dispatcher
        .call(Request::build("test/ping").id(1).finish())
        .await
        .unwrap()
        .unwrap()
        .error()
        .is_none());
}
