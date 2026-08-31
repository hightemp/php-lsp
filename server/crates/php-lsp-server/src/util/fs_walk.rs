//! Deterministic, symlink-aware filesystem traversal shared by server scanners.

use file_id::FileId;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub(crate) struct TraversalLimits {
    pub(crate) max_files: Option<usize>,
    pub(crate) max_entries: Option<usize>,
}

impl TraversalLimits {
    pub(crate) fn capped_files(self, cap: usize) -> Self {
        Self {
            max_files: Some(self.max_files.map_or(cap, |limit| limit.min(cap))),
            max_entries: self.max_entries,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TraversalStopReason {
    MaxFiles { limit: usize },
    MaxEntries { limit: usize },
    Cancelled,
    DeadlineExceeded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SymlinkTargetKind {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum PhysicalIdentity {
    FileId(FileId),
    CanonicalPath(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SymlinkAlias {
    pub(crate) logical_path: PathBuf,
    pub(crate) physical_target: PathBuf,
    pub(crate) target_identity: PhysicalIdentity,
    pub(crate) target_kind: SymlinkTargetKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PhysicalFilePath {
    pub(crate) logical_path: PathBuf,
    pub(crate) physical_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PhysicalFileGroup {
    pub(crate) identity: PhysicalIdentity,
    pub(crate) paths: Vec<PhysicalFilePath>,
}

impl PhysicalFileGroup {
    pub(crate) fn representative(&self) -> &Path {
        self.paths
            .first()
            .map(|path| path.logical_path.as_path())
            .unwrap_or_else(|| Path::new(""))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TraversalStats {
    pub(crate) visited_entries: usize,
    pub(crate) visited_directories: usize,
    pub(crate) identity_lookups: usize,
    pub(crate) duplicate_directories: usize,
    pub(crate) duplicate_files: usize,
    pub(crate) skipped_errors: usize,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct FileWalkOutcome {
    pub(crate) files: Vec<PathBuf>,
    pub(crate) physical_files: Vec<PhysicalFileGroup>,
    pub(crate) symlink_aliases: Vec<SymlinkAlias>,
    pub(crate) stats: TraversalStats,
    pub(crate) stop_reason: Option<TraversalStopReason>,
}

impl FileWalkOutcome {
    pub(crate) fn truncated(&self) -> bool {
        matches!(
            self.stop_reason,
            Some(TraversalStopReason::MaxFiles { .. } | TraversalStopReason::MaxEntries { .. })
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PendingPath {
    logical_path: PathBuf,
    is_root: bool,
}

/// Walk files below `roots` in stable logical-path order.
///
/// `skip_path` is evaluated before metadata is read and is intended for logical
/// project exclusions. `descend_directory` may reject known heavy directory
/// names while still allowing an explicitly configured root with the same
/// basename. Symlink targets are followed, but physical identities prevent
/// cycles and duplicate file publication.
pub(crate) fn walk_files<Skip, Descend, Include, Cancel>(
    roots: &[PathBuf],
    limits: TraversalLimits,
    mut skip_path: Skip,
    mut descend_directory: Descend,
    mut include_file: Include,
    mut cancelled: Cancel,
) -> FileWalkOutcome
where
    Skip: FnMut(&Path) -> bool,
    Descend: FnMut(&Path, bool) -> bool,
    Include: FnMut(&Path) -> bool,
    Cancel: FnMut() -> Option<TraversalStopReason>,
{
    let mut pending = BinaryHeap::new();
    for root in roots {
        pending.push(Reverse(PendingPath {
            logical_path: root.clone(),
            is_root: true,
        }));
    }

    let mut seen_directories = HashSet::<PhysicalIdentity>::new();
    let mut file_paths = HashMap::<PhysicalIdentity, HashMap<PathBuf, PathBuf>>::new();
    let mut symlink_aliases = Vec::new();
    let mut stats = TraversalStats::default();
    let mut stop_reason = None;

    while let Some(Reverse(pending_path)) = pending.pop() {
        if let Some(reason) = cancelled() {
            stop_reason = Some(reason);
            break;
        }
        if limits
            .max_entries
            .is_some_and(|limit| stats.visited_entries >= limit)
        {
            stop_reason = limits
                .max_entries
                .map(|limit| TraversalStopReason::MaxEntries { limit });
            break;
        }

        stats.visited_entries += 1;
        let path = pending_path.logical_path;
        if skip_path(&path) {
            continue;
        }

        let link_metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                stats.skipped_errors += 1;
                tracing::debug!(
                    "Skipping inaccessible filesystem entry {}: {}",
                    path.display(),
                    error
                );
                continue;
            }
        };
        let is_symlink = link_metadata.file_type().is_symlink();
        let target_metadata = if is_symlink {
            match std::fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    stats.skipped_errors += 1;
                    tracing::debug!("Skipping broken symlink {}: {}", path.display(), error);
                    continue;
                }
            }
        } else {
            link_metadata
        };

        let target_kind = if target_metadata.is_dir() {
            SymlinkTargetKind::Directory
        } else if target_metadata.is_file() {
            SymlinkTargetKind::File
        } else {
            continue;
        };

        if target_kind == SymlinkTargetKind::Directory
            && !descend_directory(&path, pending_path.is_root)
        {
            continue;
        }
        let collect_file = target_kind == SymlinkTargetKind::File && include_file(&path);
        if target_kind == SymlinkTargetKind::File && !is_symlink && !collect_file {
            continue;
        }

        stats.identity_lookups += 1;
        let identity = match physical_identity(&path) {
            Ok(identity) => identity,
            Err(error) => {
                stats.skipped_errors += 1;
                tracing::debug!(
                    "Skipping entry without stable identity {}: {}",
                    path.display(),
                    error
                );
                continue;
            }
        };

        if is_symlink {
            match std::fs::canonicalize(&path) {
                Ok(physical_target) => symlink_aliases.push(SymlinkAlias {
                    logical_path: path.clone(),
                    physical_target,
                    target_identity: identity.clone(),
                    target_kind,
                }),
                Err(error) => {
                    stats.skipped_errors += 1;
                    tracing::debug!(
                        "Could not canonicalize symlink target {}: {}",
                        path.display(),
                        error
                    );
                }
            }
        }

        match target_kind {
            SymlinkTargetKind::Directory => {
                if !seen_directories.insert(identity) {
                    stats.duplicate_directories += 1;
                    continue;
                }
                stats.visited_directories += 1;

                let entries = match std::fs::read_dir(&path) {
                    Ok(entries) => entries,
                    Err(error) => {
                        stats.skipped_errors += 1;
                        tracing::debug!(
                            "Skipping unreadable directory {}: {}",
                            path.display(),
                            error
                        );
                        continue;
                    }
                };
                for entry in entries {
                    match entry {
                        Ok(entry) => pending.push(Reverse(PendingPath {
                            logical_path: entry.path(),
                            is_root: false,
                        })),
                        Err(error) => {
                            stats.skipped_errors += 1;
                            tracing::debug!(
                                "Skipping unreadable directory entry below {}: {}",
                                path.display(),
                                error
                            );
                        }
                    }
                }
            }
            SymlinkTargetKind::File => {
                if !collect_file {
                    continue;
                }
                if let Some(paths) = file_paths.get_mut(&identity) {
                    stats.duplicate_files += 1;
                    let physical_path =
                        std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                    paths.insert(path, physical_path);
                    continue;
                }
                if limits
                    .max_files
                    .is_some_and(|limit| file_paths.len() >= limit)
                {
                    stop_reason = limits
                        .max_files
                        .map(|limit| TraversalStopReason::MaxFiles { limit });
                    break;
                }
                let physical_path = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                file_paths.insert(identity, HashMap::from([(path, physical_path)]));
            }
        }
    }

    symlink_aliases.sort_by(|left, right| {
        left.logical_path
            .cmp(&right.logical_path)
            .then(left.physical_target.cmp(&right.physical_target))
    });
    symlink_aliases.dedup();

    let mut physical_files = file_paths
        .into_iter()
        .map(|(identity, paths)| {
            let mut paths = paths
                .into_iter()
                .map(|(logical_path, physical_path)| PhysicalFilePath {
                    logical_path,
                    physical_path,
                })
                .collect::<Vec<_>>();
            paths.sort_by(|left, right| left.logical_path.cmp(&right.logical_path));
            PhysicalFileGroup { identity, paths }
        })
        .collect::<Vec<_>>();
    physical_files.sort_by(|left, right| left.representative().cmp(right.representative()));
    let files = physical_files
        .iter()
        .map(|group| group.representative().to_path_buf())
        .collect();

    FileWalkOutcome {
        files,
        physical_files,
        symlink_aliases,
        stats,
        stop_reason,
    }
}

pub(crate) fn symlink_aliases_on_path(path: &Path) -> Vec<SymlinkAlias> {
    let mut aliases = Vec::new();
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        let Ok(metadata) = std::fs::symlink_metadata(&current) else {
            break;
        };
        if !metadata.file_type().is_symlink() {
            continue;
        }
        let Ok(target_metadata) = std::fs::metadata(&current) else {
            break;
        };
        let target_kind = if target_metadata.is_dir() {
            SymlinkTargetKind::Directory
        } else if target_metadata.is_file() {
            SymlinkTargetKind::File
        } else {
            continue;
        };
        let (Ok(target_identity), Ok(physical_target)) =
            (physical_identity(&current), std::fs::canonicalize(&current))
        else {
            continue;
        };
        aliases.push(SymlinkAlias {
            logical_path: current.clone(),
            physical_target,
            target_identity,
            target_kind,
        });
    }
    aliases
}

fn physical_identity(path: &Path) -> std::io::Result<PhysicalIdentity> {
    match file_id::get_file_id(path) {
        Ok(identity) => Ok(PhysicalIdentity::FileId(identity)),
        Err(identity_error) => std::fs::canonicalize(path)
            .map(PhysicalIdentity::CanonicalPath)
            .map_err(|canonical_error| {
                std::io::Error::new(
                    canonical_error.kind(),
                    format!(
                        "file ID failed ({identity_error}); canonical path failed ({canonical_error})"
                    ),
                )
            }),
    }
}

#[cfg(test)]
#[path = "fs_walk_tests.rs"]
mod tests;
