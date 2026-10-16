//! Real stdio process exit must not depend on the client closing stdin.
use serde_json::{json, Value};
use std::{process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

async fn send(stdin: &mut tokio::process::ChildStdin, message: Value) {
    let body = serde_json::to_vec(&message).unwrap();
    stdin
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await
        .unwrap();
    stdin.write_all(&body).await.unwrap();
    stdin.flush().await.unwrap();
}
async fn read(stdout: &mut BufReader<tokio::process::ChildStdout>) -> Value {
    let mut length = None;
    loop {
        let mut line = String::new();
        assert!(stdout.read_line(&mut line).await.unwrap() > 0);
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            length = Some(value.trim().parse::<usize>().unwrap());
        }
    }
    let mut body = vec![0; length.unwrap()];
    stdout.read_exact(&mut body).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}
#[tokio::test(flavor = "current_thread")]
async fn dispatch_stdio_overload_exits_while_client_stdin_remains_open() {
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_php-lsp"))
        .env("RUST_LOG", "off")
        .env("TOKIO_WORKER_THREADS", "2")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    send(&mut stdin,json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{},"initializationOptions":{"stubExtensions":[],"diagnosticsMode":"off","indexVendor":false}}})).await;
    let initialized = tokio::time::timeout(Duration::from_secs(5), read(&mut stdout))
        .await
        .unwrap();
    assert_eq!(initialized["id"], 1);
    // Exceeds the documented 32 MiB intake weight; never parsed/indexed as PHP.
    let text = " ".repeat(32 * 1024 * 1024 + 1024);
    send(&mut stdin,json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///oversized.php","languageId":"php","version":1,"text":text}}})).await;
    let result = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
    if result.is_err() {
        child.kill().await.unwrap();
        let _ = child.wait().await;
    }
    assert!(result
        .expect("runtime waited for stdin EOF after transport overflow")
        .unwrap()
        .success());
    drop(stdin);
}
