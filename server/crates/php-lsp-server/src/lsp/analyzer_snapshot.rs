//! Disk analyzers may only contribute diagnostics for identical editor bytes.

use crate::server::{run_file_io_blocking_cancellable, OperationCancellationToken};
use std::{path::PathBuf, sync::Arc};

#[derive(Clone)]
pub(in crate::server) struct AnalyzerSourceSnapshot {
    path: PathBuf,
    source: Arc<str>,
    #[cfg(test)]
    validation_gate: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
}

impl AnalyzerSourceSnapshot {
    pub(in crate::server) fn new(path: PathBuf, source: Arc<str>) -> Self {
        Self {
            path,
            source,
            #[cfg(test)]
            validation_gate: None,
        }
    }

    #[cfg(test)]
    pub(in crate::server) fn with_validation_gate(
        mut self,
        reached: Arc<tokio::sync::Notify>,
        resume: Arc<tokio::sync::Notify>,
    ) -> Self {
        self.validation_gate = Some((reached, resume));
        self
    }

    pub(in crate::server) async fn matches_disk(
        &self,
        cancellation: Option<&OperationCancellationToken>,
    ) -> bool {
        let snapshot = self.clone();
        let check = run_file_io_blocking_cancellable(
            "analyzer source validation",
            self.path.display().to_string(),
            move |token| snapshot.matches_disk_blocking(&token),
        );
        let matched = match cancellation {
            Some(token) => tokio::select! {
                biased;
                _=token.cancelled()=>false,
                result=check=>result.unwrap_or(false),
            },
            None => check.await.unwrap_or(false),
        };
        #[cfg(test)]
        if let Some((reached, resume)) = &self.validation_gate {
            reached.notify_one();
            resume.notified().await;
        }
        matched
    }

    fn matches_disk_blocking(&self, token: &OperationCancellationToken) -> bool {
        use std::io::Read;
        if token.is_cancelled()
            || !std::fs::metadata(&self.path).is_ok_and(|metadata| metadata.is_file())
        {
            return false;
        }
        let Ok(mut file) = std::fs::File::open(&self.path) else {
            return false;
        };
        if !file
            .metadata()
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() == self.source.len() as u64)
        {
            return false;
        }
        // Compare a bounded chunk at a time; a growing/replaced file cannot
        // make this validation allocate or read beyond the captured source.
        let mut chunk = [0; 64 * 1024];
        let mut offset = 0;
        while offset < self.source.len() {
            if token.is_cancelled() {
                return false;
            }
            let limit = chunk.len().min(self.source.len() - offset);
            let count = match file.read(&mut chunk[..limit]) {
                Ok(0) => return false,
                Ok(count) => count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return false,
            };
            if chunk[..count] != self.source.as_bytes()[offset..offset + count] {
                return false;
            }
            offset += count;
        }
        !token.is_cancelled() && file.read(&mut chunk[..1]).is_ok_and(|count| count == 0)
    }
}
