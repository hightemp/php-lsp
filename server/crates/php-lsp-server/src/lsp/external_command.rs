//! External command helpers shared by formatting and analyzer integrations.

use super::super::*;

pub(in crate::server) fn shell_escape(value: &str) -> String {
    if cfg!(windows) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

pub(in crate::server) async fn run_shell_command_with_timeout(
    label: &str,
    command: &str,
    current_dir: Option<&Path>,
    timeout_ms: u64,
    cancellation: Option<OperationCancellationToken>,
) -> std::result::Result<std::process::Output, String> {
    let mut process = if cfg!(windows) {
        let mut command_process = tokio::process::Command::new("cmd");
        command_process.arg("/C").arg(command);
        command_process
    } else {
        let mut command_process = tokio::process::Command::new("sh");
        command_process.arg("-c").arg(command);
        command_process
    };

    if let Some(current_dir) = current_dir {
        process.current_dir(current_dir);
    }

    process
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    process.kill_on_drop(true);
    let child = process
        .spawn()
        .map_err(|err| format!("failed to start {} command: {}", label, err))?;

    let wait = child.wait_with_output();
    tokio::pin!(wait);
    let timeout = tokio::time::sleep(Duration::from_millis(timeout_ms));
    tokio::pin!(timeout);

    let output = if let Some(cancellation) = cancellation {
        tokio::select! {
            result = &mut wait => result,
            _ = &mut timeout => {
                return Err(format!("{} command timed out after {}ms", label, timeout_ms));
            }
            _ = cancellation.cancelled() => {
                return Err(format!("{} command cancelled", label));
            }
        }
    } else {
        tokio::select! {
            result = &mut wait => result,
            _ = &mut timeout => {
                return Err(format!("{} command timed out after {}ms", label, timeout_ms));
            }
        }
    };

    output.map_err(|err| format!("failed to wait for {} command: {}", label, err))
}
