//! External command helpers shared by formatting and analyzer integrations.

use super::super::*;
use tokio::io::AsyncReadExt;

#[path = "process_tree.rs"]
mod process_tree;
use process_tree::ProcessTreeGuard;

const MAX_COMMAND_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

pub(in crate::server) fn shell_escape(value: &str) -> String {
    if cfg!(windows) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

async fn stop_process_tree(guard: &mut ProcessTreeGuard, child: &mut tokio::process::Child) {
    guard.terminate();
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
}

enum CommandEvent {
    TimedOut,
    Cancelled,
    Exited(std::io::Result<std::process::ExitStatus>),
    Stdout(std::io::Result<usize>),
    Stderr(std::io::Result<usize>),
}

pub(in crate::server) async fn run_shell_command_with_timeout(
    label: &str,
    command: &str,
    current_dir: Option<&Path>,
    timeout_ms: u64,
    cancellation: Option<OperationCancellationToken>,
) -> std::result::Result<std::process::Output, String> {
    run_shell_command_with_output_limit(
        label,
        command,
        current_dir,
        timeout_ms,
        cancellation,
        MAX_COMMAND_OUTPUT_BYTES,
    )
    .await
}

pub(in crate::server) async fn run_shell_command_with_output_limit(
    label: &str,
    command: &str,
    current_dir: Option<&Path>,
    timeout_ms: u64,
    cancellation: Option<OperationCancellationToken>,
    max_output_bytes: usize,
) -> std::result::Result<std::process::Output, String> {
    if cancellation
        .as_ref()
        .is_some_and(OperationCancellationToken::is_cancelled)
    {
        return Err(format!("{} command cancelled", label));
    }
    let mut process = if cfg!(windows) {
        let mut command_process = tokio::process::Command::new("cmd");
        command_process.arg("/C").arg(command);
        command_process
    } else {
        let mut command_process = tokio::process::Command::new("sh");
        command_process.arg("-c").arg(command);
        command_process
    };

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        process.as_std_mut().process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        process
            .as_std_mut()
            .creation_flags(windows_sys::Win32::System::Threading::CREATE_SUSPENDED);
    }

    if let Some(current_dir) = current_dir {
        process.current_dir(current_dir);
    }

    process
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    process.kill_on_drop(true);
    let mut child = process
        .spawn()
        .map_err(|err| format!("failed to start {} command: {}", label, err))?;
    let mut guard = match ProcessTreeGuard::attach(&child) {
        Ok(guard) => guard,
        Err(message) => {
            let _ = child.kill().await;
            return Err(message);
        }
    };
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let mut stdout_chunk = [0u8; 8192];
    let mut stderr_chunk = [0u8; 8192];
    let mut stdout_open = true;
    let mut stderr_open = true;
    let mut status = None;
    let timeout = tokio::time::sleep(Duration::from_millis(timeout_ms));
    tokio::pin!(timeout);
    let cancelled = async {
        match cancellation.as_ref() {
            Some(token) => token.cancelled().await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(cancelled);

    loop {
        if !stdout_open && !stderr_open {
            if let Some(status) = status.take() {
                guard.terminate();
                return Ok(std::process::Output {
                    status,
                    stdout: stdout_bytes,
                    stderr: stderr_bytes,
                });
            }
        }
        let event = tokio::select! {
            _ = &mut timeout => CommandEvent::TimedOut,
            _ = &mut cancelled => CommandEvent::Cancelled,
            result = child.wait(), if status.is_none() => CommandEvent::Exited(result),
            result = stdout.read(&mut stdout_chunk), if stdout_open => CommandEvent::Stdout(result),
            result = stderr.read(&mut stderr_chunk), if stderr_open => CommandEvent::Stderr(result),
        };
        let failure = match event {
            CommandEvent::TimedOut => Some(format!(
                "{} command timed out after {}ms",
                label, timeout_ms
            )),
            CommandEvent::Cancelled => Some(format!("{} command cancelled", label)),
            CommandEvent::Exited(Ok(exit)) => {
                status = Some(exit);
                None
            }
            CommandEvent::Exited(Err(err)) => {
                Some(format!("failed to wait for {} command: {}", label, err))
            }
            CommandEvent::Stdout(Ok(0)) => {
                stdout_open = false;
                None
            }
            CommandEvent::Stderr(Ok(0)) => {
                stderr_open = false;
                None
            }
            CommandEvent::Stdout(Ok(count)) => {
                if stdout_bytes
                    .len()
                    .saturating_add(stderr_bytes.len())
                    .saturating_add(count)
                    > max_output_bytes
                {
                    Some(format!(
                        "{} output exceeded {} bytes",
                        label, max_output_bytes
                    ))
                } else {
                    stdout_bytes.extend_from_slice(&stdout_chunk[..count]);
                    None
                }
            }
            CommandEvent::Stderr(Ok(count)) => {
                if stdout_bytes
                    .len()
                    .saturating_add(stderr_bytes.len())
                    .saturating_add(count)
                    > max_output_bytes
                {
                    Some(format!(
                        "{} output exceeded {} bytes",
                        label, max_output_bytes
                    ))
                } else {
                    stderr_bytes.extend_from_slice(&stderr_chunk[..count]);
                    None
                }
            }
            CommandEvent::Stdout(Err(err)) | CommandEvent::Stderr(Err(err)) => {
                Some(format!("failed to read {} command output: {}", label, err))
            }
        };
        if let Some(message) = failure {
            stop_process_tree(&mut guard, &mut child).await;
            return Err(message);
        }
    }
}
