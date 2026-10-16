//! Bounded handler polling outside the transport task, with document commit fences.
//!
//! The standard tower-lsp transport is retained. Its response concurrency exceeds
//! the number of admitted long-lived RPC receipts; notifications enqueue bounded
//! work and return immediately. Thus ordinary RPC saturation cannot hide cancel.
use crate::server::OperationCancellationToken;
use futures::future::poll_fn;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::task::{Context, Poll, Wake, Waker};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{oneshot, Mutex, Notify, OwnedSemaphorePermit, Semaphore};
use tower::Service;
use tower_lsp::jsonrpc::{Error, ErrorCode, Id, Request, Response};
use tower_lsp::{ClientSocket, Server};

const MAX_REQUESTS: usize = 16;
const MAX_NOTIFICATIONS: usize = 128;
const MAX_PAYLOAD_WEIGHT: u32 = 32 * 1024 * 1024;
const MAX_ID_BYTES: usize = 64 * 1024;
// Dispatcher registry/receipt plus tower-lsp Pending and response ownership.
const ID_WEIGHT_COPIES: usize = 8;
const QUERY_POLLS: usize = 2;
const MUTATION_POLLS: usize = 1;
// At most MAX_REQUESTS ordinary receipts plus one shutdown receipt may wait.
const TRANSPORT_CONCURRENCY: usize = MAX_REQUESTS + 2;
type RpcResult<E> = Result<Option<Response>, E>;
type RpcFuture<E> = Pin<Box<dyn Future<Output = RpcResult<E>> + Send>>;

struct Completion(OperationCancellationToken);
impl Completion {
    fn new() -> Self {
        Self(OperationCancellationToken::new())
    }
    fn finish(&self) {
        self.0.cancel();
    }
    fn done(&self) -> bool {
        self.0.is_cancelled()
    }
    async fn wait(&self) {
        self.0.cancelled().await;
    }
}
struct DocumentLane {
    epoch: AtomicU64,
    tail: StdMutex<Weak<Completion>>,
}
struct DocumentRead {
    lane: Arc<DocumentLane>,
    epoch: u64,
}
impl DocumentRead {
    fn current(&self) -> bool {
        self.lane.epoch.load(Ordering::SeqCst) == self.epoch
    }
}

tokio::task_local! {static DOCUMENT_COMMIT: Option<Arc<Completion>>;}
/// Release only after the authoritative document mutation/capture has committed.
/// The rest of a notification may wait for diagnostics without blocking edits.
pub(crate) fn document_committed() {
    let _ = DOCUMENT_COMMIT.try_with(|done| {
        if let Some(done) = done {
            done.finish();
        }
    });
}

struct Reservation {
    _admission: Option<OwnedSemaphorePermit>,
    _payload: OwnedSemaphorePermit,
}
struct Job {
    #[cfg(test)]
    entered: Completion,
    reservation: StdMutex<Option<Reservation>>,
    id: Option<Id>,
    cancel: OperationCancellationToken,
    started: AtomicBool,
    replied: AtomicBool,
    finished: Completion,
    document: Vec<DocumentRead>,
    commit: Option<Arc<Completion>>,
    lifecycle: bool,
    cancellable_analysis: bool,
    global_document_epoch: Option<u64>,
}
struct State {
    stop: OperationCancellationToken,
    terminal: OperationCancellationToken,
    payloads: Arc<Semaphore>,
    shutdown: AtomicBool,
    requests: Arc<Semaphore>,
    notifications: Arc<Semaphore>,
    queries: Arc<Semaphore>,
    mutations: Arc<Semaphore>,
    lifecycle: Arc<Semaphore>,
    jobs: StdMutex<HashMap<Id, Arc<Job>>>,
    documents: StdMutex<HashMap<String, Weak<DocumentLane>>>,
    document_epoch: AtomicU64,
}
impl State {
    fn new() -> Self {
        Self {
            stop: OperationCancellationToken::new(),
            terminal: OperationCancellationToken::new(),
            payloads: Arc::new(Semaphore::new(MAX_PAYLOAD_WEIGHT as usize)),
            shutdown: AtomicBool::new(false),
            requests: Arc::new(Semaphore::new(MAX_REQUESTS)),
            notifications: Arc::new(Semaphore::new(MAX_NOTIFICATIONS)),
            queries: Arc::new(Semaphore::new(QUERY_POLLS)),
            mutations: Arc::new(Semaphore::new(MUTATION_POLLS)),
            lifecycle: Arc::new(Semaphore::new(1)),
            jobs: StdMutex::default(),
            documents: StdMutex::default(),
            document_epoch: AtomicU64::new(0),
        }
    }
    fn forget(&self, job: &Arc<Job>) {
        if !job.finished.done() || !job.replied.load(Ordering::SeqCst) {
            return;
        }
        job.reservation.lock().unwrap().take();
        if let Some(id) = &job.id {
            let mut jobs = self.jobs.lock().unwrap();
            if jobs
                .get(id)
                .is_some_and(|current| Arc::ptr_eq(current, job))
            {
                jobs.remove(id);
            }
        }
    }
    fn end_session(&self, reason: &str) {
        if !self.terminal.is_cancelled() {
            tracing::error!("Closing LSP session: {reason}");
        }
        self.shutdown.store(true, Ordering::SeqCst);
        self.terminal.cancel();
    }
    fn document(
        &self,
        request: &Request,
    ) -> (
        Vec<DocumentRead>,
        Vec<Arc<Completion>>,
        Option<Arc<Completion>>,
    ) {
        let uris = request_document_uris(request);
        let mutation = is_document_notification(request.method());
        if mutation && !uris.is_empty() {
            self.document_epoch.fetch_add(1, Ordering::SeqCst);
        }
        let mut documents = self.documents.lock().unwrap();
        documents.retain(|_, lane| lane.strong_count() > 0);
        // Cross-file queries must not overtake already accepted document commits.
        // Mutations on separate documents remain independent.
        let mut previous = if !mutation
            && (!uris.is_empty()
                || matches!(
                    request.method(),
                    "workspace/symbol" | "completionItem/resolve"
                )) {
            documents
                .values()
                .filter_map(Weak::upgrade)
                .filter_map(|lane| lane.tail.lock().unwrap().upgrade())
                .collect()
        } else {
            Vec::new()
        };
        let mut reads = Vec::new();
        let mut commit = None;
        for uri in uris {
            let lane = documents
                .get(uri)
                .and_then(Weak::upgrade)
                .unwrap_or_else(|| {
                    let lane = Arc::new(DocumentLane {
                        epoch: AtomicU64::new(0),
                        tail: StdMutex::new(Weak::new()),
                    });
                    documents.insert(uri.to_string(), Arc::downgrade(&lane));
                    lane
                });
            if mutation {
                let mut tail = lane.tail.lock().unwrap();
                if let Some(earlier) = tail.upgrade() {
                    previous.push(earlier);
                }
                lane.epoch.fetch_add(1, Ordering::SeqCst);
                let done = Arc::new(Completion::new());
                *tail = Arc::downgrade(&done);
                commit = Some(done);
            }
            let epoch = lane.epoch.load(Ordering::SeqCst);
            reads.push(DocumentRead { lane, epoch });
        }
        (reads, previous, commit)
    }
    fn cancelled(&self, job: &Job) -> bool {
        job.cancel.is_cancelled()
            || self.terminal.is_cancelled()
            || (!job.lifecycle && self.stop.is_cancelled())
    }
    async fn cancellation(&self, job: &Job) {
        tokio::select! {
            _=job.cancel.cancelled()=>{},
            _=self.terminal.cancelled()=>{},
            _=self.stop.cancelled(),if !job.lifecycle=>{},
        }
    }
}
struct Finish {
    state: Arc<State>,
    job: Arc<Job>,
}
impl Drop for Finish {
    fn drop(&mut self) {
        if let Some(commit) = &self.job.commit {
            commit.finish();
        }
        self.job.finished.finish();
        self.state.forget(&self.job);
    }
}
struct Receipt {
    state: Arc<State>,
    job: Arc<Job>,
    done: bool,
}
impl Drop for Receipt {
    fn drop(&mut self) {
        if !self.done {
            self.job.cancel.cancel();
        }
        self.job.replied.store(true, Ordering::SeqCst);
        self.state.forget(&self.job);
    }
}
struct PollWake(Notify);
impl Wake for PollWake {
    fn wake(self: Arc<Self>) {
        self.0.notify_one();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.notify_one();
    }
}

struct Dispatcher<S> {
    inner: Arc<Mutex<S>>,
    state: Arc<State>,
}
impl<S> Dispatcher<S> {
    fn new(inner: S) -> Self {
        Self {
            inner: Arc::new(Mutex::new(inner)),
            state: Arc::new(State::new()),
        }
    }
}
impl<S> Drop for Dispatcher<S> {
    fn drop(&mut self) {
        self.state.terminal.cancel();
    }
}

fn valid_lsp_id(id: &Id) -> bool {
    match id {
        Id::String(id) => id.len() <= MAX_ID_BYTES,
        Id::Number(id) => i32::try_from(*id).is_ok(),
        Id::Null => false,
    }
}
fn cancellation_id(request: &Request) -> Option<Id> {
    match request.params()?.get("id")? {
        serde_json::Value::String(id) if id.len() <= MAX_ID_BYTES => Some(Id::String(id.clone())),
        serde_json::Value::Number(id) => id
            .as_i64()
            .filter(|id| i32::try_from(*id).is_ok())
            .map(Id::Number),
        _ => None,
    }
}
fn is_document_notification(method: &str) -> bool {
    matches!(
        method,
        "textDocument/didOpen"
            | "textDocument/didChange"
            | "textDocument/didSave"
            | "textDocument/didClose"
    )
}
fn request_document_uris(request: &Request) -> Vec<&str> {
    let Some(params) = request.params() else {
        return Vec::new();
    };
    let mut uris = Vec::new();
    let direct = match request.method() {
        "codeAction/resolve" => params.get("data").and_then(|data| data.get("uri")),
        "completionItem/resolve" => params.get("data").and_then(|data| data.get("workspaceUri")),
        "callHierarchy/incomingCalls"
        | "callHierarchy/outgoingCalls"
        | "typeHierarchy/supertypes"
        | "typeHierarchy/subtypes" => {
            if let Some(uri) = params
                .get("item")
                .and_then(|item| item.get("data"))
                .and_then(|data| data.get("workspaceUri"))
                .and_then(|uri| uri.as_str())
            {
                uris.push(uri);
            }
            params.get("item").and_then(|item| item.get("uri"))
        }
        _ => params.get("textDocument").and_then(|doc| doc.get("uri")),
    };
    if let Some(uri) = direct.and_then(|uri| uri.as_str()) {
        if !uris.contains(&uri) {
            uris.push(uri);
        }
    }
    uris
}
// Intake accounting avoids serializing/copying document text a second time.
// This caps retained JSON payload weight, not native parser/index RSS.
fn payload_weight(request: &Request) -> u32 {
    fn value_weight(value: &serde_json::Value) -> usize {
        let base = std::mem::size_of::<serde_json::Value>();
        base.saturating_add(match value {
            serde_json::Value::String(text) => text.capacity(),
            serde_json::Value::Array(values) => values
                .iter()
                .fold(values.capacity().saturating_mul(base), |n, v| {
                    n.saturating_add(value_weight(v))
                }),
            serde_json::Value::Object(values) => values.iter().fold(0usize, |n, (key, v)| {
                n.saturating_add(key.capacity())
                    .saturating_add(base.saturating_mul(4))
                    .saturating_add(value_weight(v))
            }),
            _ => 0,
        })
    }
    let id_weight = match request.id() {
        Some(Id::String(id)) => id.capacity().saturating_mul(ID_WEIGHT_COPIES),
        _ => 0,
    };
    request
        .params()
        .map_or(0, value_weight)
        .saturating_add(id_weight)
        .saturating_add(request.method().len())
        .saturating_add(std::mem::size_of::<Request>())
        .min(MAX_PAYLOAD_WEIGHT as usize + 1) as u32
}
fn error_response<E>(id: Option<Id>, error: Error) -> RpcResult<E> {
    Ok(id.map(|id| Response::from_error(id, error)))
}
fn busy() -> Error {
    Error {
        code: ErrorCode::ServerError(-32000),
        message: "Server request capacity is occupied; retry later".into(),
        data: None,
    }
}

impl<S> Service<Request> for Dispatcher<S>
where
    S: Service<Request, Response = Option<Response>> + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
{
    type Response = Option<Response>;
    type Error = S::Error;
    type Future = RpcFuture<S::Error>;
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
    fn call(&mut self, request: Request) -> Self::Future {
        // LSP cancellation can address only integer|string IDs. Reject before
        // cloning, and never retain an oversized ID in an unreserved error.
        if request.id().is_some_and(|id| !valid_lsp_id(id)) {
            return Box::pin(async { error_response(Some(Id::Null), Error::invalid_request()) });
        }
        if request.method() == "shutdown"
            && request.params().is_some_and(|params| !params.is_null())
        {
            let (_, id, _) = request.into_parts();
            return Box::pin(async move {
                error_response(id, Error::invalid_params("shutdown takes no parameters"))
            });
        }
        let id = request.id().cloned();
        let method = request.method();
        if method == "$/cancelRequest" {
            if id.is_some() {
                return Box::pin(async move { error_response(id, Error::invalid_request()) });
            }
            if let Some(id) = cancellation_id(&request) {
                if let Some(job) = self.state.jobs.lock().unwrap().get(&id) {
                    job.cancel.cancel();
                }
            }
            return Box::pin(async { Ok(None) });
        }
        if method == "exit" {
            self.state.terminal.cancel();
            let inner = self.inner.clone();
            tokio::spawn(async move {
                let future = inner.lock().await.call(request);
                let _ = future.await;
            });
            return Box::pin(async { Ok(None) });
        }
        if self.state.shutdown.load(Ordering::SeqCst) || self.state.terminal.is_cancelled() {
            return Box::pin(async move { error_response(id, Error::invalid_request()) });
        }
        if is_document_notification(method) && id.is_some() {
            return Box::pin(async move { error_response(id, Error::invalid_request()) });
        }
        if matches!(method, "initialize" | "shutdown") && id.is_none() {
            return Box::pin(async { Ok(None) });
        }
        let previous_id = if let Some(id) = &id {
            let jobs = self.state.jobs.lock().unwrap();
            if jobs
                .get(id)
                .is_some_and(|old| !old.replied.load(Ordering::SeqCst))
            {
                let duplicate_id = id.clone();
                return Box::pin(async move {
                    error_response(Some(duplicate_id), Error::invalid_request())
                });
            }
            jobs.get(id).cloned()
        } else {
            None
        };
        let lifecycle = method == "shutdown";
        let admission = if lifecycle {
            None
        } else {
            let capacity = if id.is_some() {
                &self.state.requests
            } else {
                &self.state.notifications
            };
            match capacity.clone().try_acquire_owned() {
                Ok(permit) => Some(permit),
                Err(_) => {
                    if id.is_none() {
                        self.state.end_session("mandatory notification backlog exhausted; document resynchronization requires a new session");
                    }
                    return Box::pin(async move { error_response(id, busy()) });
                }
            }
        };
        let payload = match self
            .state
            .payloads
            .clone()
            .try_acquire_many_owned(if lifecycle {
                0
            } else {
                payload_weight(&request)
            }) {
            Ok(permit) => permit,
            Err(_) => {
                if id.is_none() {
                    self.state.end_session("mandatory notification payload budget exhausted; document resynchronization requires a new session");
                }
                return Box::pin(async move { error_response(id, busy()) });
            }
        };
        if lifecycle {
            self.state.shutdown.store(true, Ordering::SeqCst);
            self.state.stop.cancel();
        }
        let (document, previous_document, commit) = self.state.document(&request);
        let global_document_epoch = (method == "workspace/symbol"
            || (method == "completionItem/resolve" && document.is_empty()))
        .then(|| self.state.document_epoch.load(Ordering::SeqCst));
        let job = Arc::new(Job {
            #[cfg(test)]
            entered: Completion::new(),
            reservation: StdMutex::new(Some(Reservation {
                _admission: admission,
                _payload: payload,
            })),
            id: id.clone(),
            cancel: OperationCancellationToken::new(),
            started: AtomicBool::new(false),
            replied: AtomicBool::new(id.is_none()),
            finished: Completion::new(),
            document,
            commit,
            lifecycle,
            global_document_epoch,
            cancellable_analysis: id.is_some() && !matches!(method, "initialize" | "shutdown"),
        });
        if let Some(id) = &id {
            self.state
                .jobs
                .lock()
                .unwrap()
                .insert(id.clone(), job.clone());
        }
        let (tx, rx) = oneshot::channel();
        let state = self.state.clone();
        let inner = self.inner.clone();
        let worker_job = job.clone();
        tokio::spawn(async move {
            let _finish = Finish {
                state: state.clone(),
                job: worker_job.clone(),
            };
            #[cfg(test)]
            worker_job.entered.finish();
            // Even a pre-cancelled reuse must retain the physical predecessor
            // fence: tower-lsp's late Pending cleanup is keyed only by ID.
            if let Some(previous) = previous_id {
                previous.finished.wait().await;
            }
            let run = async {
                for previous in previous_document {
                    tokio::select! {
                        biased;
                        _=state.cancellation(&worker_job)=>return error_response(worker_job.id.clone(),Error::request_cancelled()),
                        _=previous.wait()=>{},
                    }
                }
                let started = worker_job.clone();
                let service = inner.clone();
                let future = async move {
                    let future = {
                        let mut service = service.lock().await;
                        poll_fn(|cx| service.poll_ready(cx)).await?;
                        let future = service.call(request);
                        started.started.store(true, Ordering::SeqCst);
                        future
                    };
                    future.await
                };
                drive(
                    inner.clone(),
                    state.clone(),
                    worker_job.clone(),
                    Box::pin(DOCUMENT_COMMIT.scope(worker_job.commit.clone(), future)),
                )
                .await
            };
            // Queued notifications/requests can be abandoned without calling the inner service.
            let result = if state.cancelled(&worker_job) {
                error_response(worker_job.id.clone(), Error::request_cancelled())
            } else {
                run.await
            };
            let _ = tx.send(result);
        });
        if id.is_none() {
            return Box::pin(async { Ok(None) });
        }
        let state = self.state.clone();
        // This owner exists before the returned future's first poll.
        let receipt = Receipt {
            state: state.clone(),
            job: job.clone(),
            done: false,
        };
        Box::pin(async move {
            let mut receipt = receipt;
            let result = tokio::select! {biased;_=state.cancellation(&job)=>error_response(id.clone(),Error::request_cancelled()),result=rx=>result.unwrap_or_else(|_|error_response(id.clone(),Error::internal_error()))};
            receipt.done = true;
            if !state.cancelled(&job)
                && (job.document.iter().any(|read| !read.current())
                    || job
                        .global_document_epoch
                        .is_some_and(|epoch| epoch != state.document_epoch.load(Ordering::SeqCst)))
            {
                error_response(id, Error::content_modified())
            } else {
                result
            }
        })
    }
}

async fn cancel_inner<S>(inner: &Arc<Mutex<S>>, job: &Job)
where
    S: Service<Request, Response = Option<Response>>,
    S::Future: Send,
{
    if job.started.load(Ordering::SeqCst) {
        if let Some(id) = &job.id {
            let request = Request::build("$/cancelRequest")
                .params(serde_json::json!({"id":id}))
                .finish();
            let future = inner.lock().await.call(request);
            let _ = future.await;
        }
    }
}
async fn drive<S>(
    inner: Arc<Mutex<S>>,
    state: Arc<State>,
    job: Arc<Job>,
    mut future: RpcFuture<S::Error>,
) -> RpcResult<S::Error>
where
    S: Service<Request, Response = Option<Response>> + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
{
    let pool = if job.lifecycle {
        &state.lifecycle
    } else if job.id.is_none() {
        &state.mutations
    } else {
        &state.queries
    };
    let wake = Arc::new(PollWake(Notify::new()));
    loop {
        if state.cancelled(&job) {
            cancel_inner(&inner, &job).await;
            return error_response(job.id.clone(), Error::request_cancelled());
        }
        let permit = tokio::select! {biased;_=state.cancellation(&job)=>{cancel_inner(&inner,&job).await;return error_response(job.id.clone(),Error::request_cancelled());},permit=pool.clone().acquire_owned()=>permit.expect("CPU capacity remains open")};
        let poll_wake = wake.clone();
        let worker_state = state.clone();
        let worker_job = job.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            if worker_state.cancelled(&worker_job) {
                return (
                    future,
                    Poll::Ready(error_response(
                        worker_job.id.clone(),
                        Error::request_cancelled(),
                    )),
                );
            }
            let waker = Waker::from(poll_wake);
            let mut cx = Context::from_waker(&waker);
            let polled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if worker_job.cancellable_analysis {
                    let checked_state = worker_state.clone();
                    let checked_job = worker_job.clone();
                    php_lsp_parser::control::run_cancellable(
                        Arc::new(move || checked_state.cancelled(&checked_job)),
                        || future.as_mut().poll(&mut cx),
                    )
                    .unwrap_or_else(|| {
                        Poll::Ready(error_response(
                            worker_job.id.clone(),
                            Error::request_cancelled(),
                        ))
                    })
                } else {
                    future.as_mut().poll(&mut cx)
                }
            }));
            let result = match polled {
                Ok(result) => result,
                Err(panic) => {
                    if worker_job.id.is_none() {
                        worker_state.end_session(
                            "mandatory notification panicked; its partial state cannot be reused",
                        );
                    }
                    std::panic::resume_unwind(panic)
                }
            };
            (future, result)
        })
        .await;
        match result {
            Ok((next, Poll::Pending)) => future = next,
            Ok((_future, Poll::Ready(result))) => {
                if state.cancelled(&job) {
                    cancel_inner(&inner, &job).await;
                    return error_response(job.id.clone(), Error::request_cancelled());
                }
                return result;
            }
            Err(error) => {
                tracing::error!("LSP handler poll failed: {error}");
                cancel_inner(&inner, &job).await;
                return error_response(job.id.clone(), Error::internal_error());
            }
        }
        tokio::select! {_=state.cancellation(&job)=>{},_=wake.0.notified()=>{}}
    }
}

/// Serve LSP using the same admission/ordering boundary as production stdio.
pub async fn serve<I, O, S>(stdin: I, stdout: O, service: S, socket: ClientSocket)
where
    I: AsyncRead + Unpin,
    O: AsyncWrite,
    S: Service<Request, Response = Option<Response>> + Send + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>> + Send + 'static,
    S::Future: Send + 'static,
{
    serve_dispatcher(stdin, stdout, Dispatcher::new(service), socket).await;
}
async fn serve_dispatcher<I, O, S>(
    stdin: I,
    stdout: O,
    dispatcher: Dispatcher<S>,
    socket: ClientSocket,
) where
    I: AsyncRead + Unpin,
    O: AsyncWrite,
    S: Service<Request, Response = Option<Response>> + Send + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>> + Send + 'static,
    S::Future: Send + 'static,
{
    let terminal = dispatcher.state.terminal.clone();
    tokio::select! {
        _=terminal.cancelled()=>{},
        _=Server::new(stdin,stdout,socket).concurrency_level(TRANSPORT_CONCURRENCY).serve(dispatcher)=>{},
    }
}

#[cfg(test)]
#[path = "dispatch_tests.rs"]
mod tests;

#[cfg(test)]
pub(crate) async fn test_document_post(uri: &str, method: &'static str) {
    tests::pause_document_post(uri, method).await;
}
