//! Per-workspace ownership for background indexing runs.

use super::super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::server) struct IndexingRunIdentity {
    pub(in crate::server) workspace_folder: PathBuf,
    pub(in crate::server) run_id: u64,
}

#[derive(Clone)]
struct ActiveIndexingRun {
    run_id: u64,
    token: OperationCancellationToken,
}

struct IndexingRunSlot {
    latest_run_id: u64,
    active: Option<ActiveIndexingRun>,
}

#[derive(Default)]
pub(in crate::server) struct IndexingRunCoordinator {
    next_run_id: AtomicU64,
    slots: DashMap<PathBuf, IndexingRunSlot>,
    aggregate_commit_gate: StdMutex<()>,
    aggregate_source_revision: AtomicU64,
}

pub(in crate::server) struct IndexingRunGuard {
    lease: IndexingRunLease,
}

#[derive(Clone)]
pub(in crate::server) struct IndexingRunLease {
    coordinator: Arc<IndexingRunCoordinator>,
    workspace_folder: PathBuf,
    run_id: u64,
    token: OperationCancellationToken,
}

impl IndexingRunCoordinator {
    pub(in crate::server) fn start(
        self: &Arc<Self>,
        workspace_folder: PathBuf,
    ) -> IndexingRunGuard {
        let _aggregate_commit = self
            .aggregate_commit_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let run_id = self
            .next_run_id
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        let token = OperationCancellationToken::new();
        self.aggregate_source_revision
            .fetch_add(1, Ordering::SeqCst);
        match self.slots.entry(workspace_folder.clone()) {
            dashmap::mapref::entry::Entry::Occupied(mut entry) => {
                if let Some(active) = entry.get().active.as_ref() {
                    active.token.cancel();
                }
                *entry.get_mut() = IndexingRunSlot {
                    latest_run_id: run_id,
                    active: Some(ActiveIndexingRun {
                        run_id,
                        token: token.clone(),
                    }),
                };
            }
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                entry.insert(IndexingRunSlot {
                    latest_run_id: run_id,
                    active: Some(ActiveIndexingRun {
                        run_id,
                        token: token.clone(),
                    }),
                });
            }
        }

        IndexingRunGuard {
            lease: IndexingRunLease {
                coordinator: self.clone(),
                workspace_folder,
                run_id,
                token,
            },
        }
    }

    pub(in crate::server) fn is_active(&self, workspace_folder: &Path) -> bool {
        self.slots.get(workspace_folder).is_some_and(|slot| {
            slot.active
                .as_ref()
                .is_some_and(|active| !active.token.is_cancelled())
        })
    }

    pub(in crate::server) fn cancel_and_remove(&self, workspace_folder: &Path) {
        let _aggregate_commit = self
            .aggregate_commit_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.aggregate_source_revision
            .fetch_add(1, Ordering::SeqCst);
        if let Some((_, slot)) = self.slots.remove(workspace_folder) {
            if let Some(active) = slot.active {
                active.token.cancel();
            }
        }
    }

    pub(in crate::server) fn cancel_all(&self) {
        let _aggregate_commit = self
            .aggregate_commit_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.aggregate_source_revision
            .fetch_add(1, Ordering::SeqCst);
        let active_tokens = self
            .slots
            .iter()
            .filter_map(|slot| slot.active.as_ref().map(|active| active.token.clone()))
            .collect::<Vec<_>>();
        self.slots.clear();
        for token in active_tokens {
            token.cancel();
        }
    }

    fn finish(&self, workspace_folder: &Path, run_id: u64) {
        let _aggregate_commit = self
            .aggregate_commit_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(mut slot) = self.slots.get_mut(workspace_folder) else {
            return;
        };
        if slot
            .active
            .as_ref()
            .is_some_and(|active| active.run_id == run_id)
        {
            slot.active = None;
        }
    }

    pub(in crate::server) fn is_latest(&self, identity: &IndexingRunIdentity) -> bool {
        self.slots
            .get(&identity.workspace_folder)
            .is_some_and(|slot| slot.latest_run_id == identity.run_id)
    }

    pub(in crate::server) fn commit_aggregate_if_current<T>(
        &self,
        runs: &[IndexingRunLease],
        expected_source_revision: u64,
        commit: impl FnOnce() -> T,
    ) -> Option<T> {
        let _aggregate_commit = self
            .aggregate_commit_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (self.aggregate_source_revision.load(Ordering::SeqCst) == expected_source_revision
            && runs.iter().all(IndexingRunLease::is_current))
        .then(commit)
    }

    pub(in crate::server) fn aggregate_source_revision(&self) -> u64 {
        let _aggregate_commit = self
            .aggregate_commit_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.aggregate_source_revision.load(Ordering::SeqCst)
    }

    pub(in crate::server) fn commit_unleased_index_mutation<T>(
        &self,
        commit: impl FnOnce() -> T,
    ) -> T {
        let _aggregate_commit = self
            .aggregate_commit_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = commit();
        self.aggregate_source_revision
            .fetch_add(1, Ordering::SeqCst);
        result
    }
}

impl IndexingRunGuard {
    pub(in crate::server) fn lease(&self) -> IndexingRunLease {
        self.lease.clone()
    }
}

impl Drop for IndexingRunGuard {
    fn drop(&mut self) {
        self.lease
            .coordinator
            .finish(&self.lease.workspace_folder, self.lease.run_id);
    }
}

impl IndexingRunLease {
    pub(in crate::server) fn coordinator(&self) -> Arc<IndexingRunCoordinator> {
        self.coordinator.clone()
    }

    pub(in crate::server) fn workspace_folder(&self) -> &Path {
        &self.workspace_folder
    }

    pub(in crate::server) fn run_id(&self) -> u64 {
        self.run_id
    }

    pub(in crate::server) fn token(&self) -> &OperationCancellationToken {
        &self.token
    }

    pub(in crate::server) fn identity(&self) -> IndexingRunIdentity {
        IndexingRunIdentity {
            workspace_folder: self.workspace_folder.clone(),
            run_id: self.run_id,
        }
    }

    pub(in crate::server) fn is_current(&self) -> bool {
        if self.token.is_cancelled() || !self.is_latest() {
            return false;
        }
        self.coordinator
            .slots
            .get(&self.workspace_folder)
            .is_some_and(|slot| {
                slot.latest_run_id == self.run_id
                    && slot.active.as_ref().is_some_and(|active| {
                        active.run_id == self.run_id && !active.token.is_cancelled()
                    })
            })
    }

    pub(in crate::server) fn is_latest(&self) -> bool {
        self.coordinator.is_latest(&self.identity())
    }

    pub(in crate::server) fn commit_if_current<T>(&self, commit: impl FnOnce() -> T) -> Option<T> {
        if self.token.is_cancelled() {
            return None;
        }
        let slot = self.coordinator.slots.get(&self.workspace_folder)?;
        let current = slot.latest_run_id == self.run_id
            && slot
                .active
                .as_ref()
                .is_some_and(|active| active.run_id == self.run_id && !active.token.is_cancelled());
        current.then(commit)
    }

    pub(in crate::server) fn commit_index_if_current<T>(
        &self,
        commit: impl FnOnce() -> T,
    ) -> Option<T> {
        let _aggregate_commit = self
            .coordinator
            .aggregate_commit_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = self.commit_if_current(commit);
        if result.is_some() {
            self.coordinator
                .aggregate_source_revision
                .fetch_add(1, Ordering::SeqCst);
        }
        result
    }
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
