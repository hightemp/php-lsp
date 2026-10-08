//! Check the real stdio binary and its stderr, without a global test subscriber.

use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

struct LoggingSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr: tokio::task::JoinHandle<String>,
}

impl LoggingSession {
    fn start(rust_log: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_php-lsp"))
            .env("RUST_LOG", rust_log)
            .env("TOKIO_WORKER_THREADS", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut stderr = child.stderr.take().unwrap();
        let stderr = tokio::spawn(async move {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).await.unwrap();
            String::from_utf8(bytes).unwrap()
        });
        Self {
            child,
            stdin,
            stdout,
            stderr,
        }
    }

    async fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        let body = serde_json::to_vec(
            &json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}),
        )
        .unwrap();
        self.stdin
            .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
            .await
            .unwrap();
        self.stdin.write_all(&body).await.unwrap();
        self.stdin.flush().await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let mut length = None;
                loop {
                    let mut header = String::new();
                    assert!(
                        self.stdout.read_line(&mut header).await.unwrap() > 0,
                        "unexpected stdout EOF"
                    );
                    if header == "\r\n" {
                        break;
                    }
                    if let Some(raw) = header.strip_prefix("Content-Length:") {
                        length = Some(raw.trim().parse::<usize>().unwrap());
                    }
                }
                let mut body = vec![0; length.expect("framed JSON-RPC stdout")];
                self.stdout.read_exact(&mut body).await.unwrap();
                let message: Value = serde_json::from_slice(&body).unwrap();
                if message.get("id") == Some(&json!(id)) {
                    assert!(message.get("error").is_none(), "{message}");
                    return message["result"].clone();
                }
            }
        })
        .await
        .expect("JSON-RPC response deadline")
    }

    async fn initialize(&mut self, level: Option<&str>) {
        let mut options =
            json!({"stubExtensions":[], "diagnosticsMode":"off", "indexVendor":false});
        if let Some(level) = level {
            options["logLevel"] = json!(level);
        }
        self.request(
            1,
            "initialize",
            json!({"capabilities":{}, "rootUri":null, "initializationOptions":options}),
        )
        .await;
    }

    async fn probe(&mut self) {
        self.request(2, "textDocument/declaration", json!({"textDocument":{"uri":"file:///logging-probe.php"}, "position":{"line":0,"character":0}})).await;
    }

    async fn finish(mut self) -> String {
        self.request(3, "shutdown", Value::Null).await;
        drop(self.stdin);
        let status = tokio::time::timeout(Duration::from_secs(10), self.child.wait())
            .await
            .expect("binary shutdown deadline")
            .unwrap();
        assert!(status.success(), "{status}");
        self.stderr.await.unwrap()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn initialize_log_level_enables_debug_over_startup_rust_log() {
    let mut session = LoggingSession::start("error");
    session.initialize(Some("debug")).await;
    session.probe().await;
    let logs = session.finish().await;
    assert!(
        logs.contains("logging-probe.php"),
        "explicit debug did not enable a real declaration log: {logs}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn initialize_log_level_suppresses_startup_debug() {
    let mut session = LoggingSession::start("debug");
    session.initialize(Some("error")).await;
    session.probe().await;
    let logs = session.finish().await;
    assert!(
        !logs.contains("logging-probe.php"),
        "explicit error still emitted a real declaration log: {logs}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn omitted_log_level_keeps_startup_rust_log() {
    let mut session = LoggingSession::start("debug");
    session.initialize(None).await;
    session.probe().await;
    let logs = session.finish().await;
    assert!(
        logs.contains("logging-probe.php"),
        "omitted setting changed RUST_LOG: {logs}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn invalid_startup_rust_log_does_not_prevent_a_valid_lsp_override() {
    let mut session = LoggingSession::start("==not-a-filter");
    session.initialize(Some(" DeBuG ")).await;
    session.probe().await;
    let logs = session.finish().await;
    assert!(logs.contains("logging-probe.php"), "{logs}");
}

#[tokio::test(flavor = "current_thread")]
async fn cli_version_output_stays_separate_from_lsp_logging() {
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        Command::new(env!("CARGO_BIN_EXE_php-lsp"))
            .arg("--version")
            .env("RUST_LOG", "trace")
            .env("TOKIO_WORKER_THREADS", "1")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("php-lsp {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert!(output.stderr.is_empty());
}
