//! External symlink alias tracking and dynamic LSP file-watcher registration.

use crate::util::fs_walk::{PhysicalFileGroup, PhysicalFilePath, SymlinkAlias, SymlinkTargetKind};
use crate::util::uri::{path_to_uri, uri_to_path};
use serde_json::to_value;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tower_lsp::ls_types::notification::{DidChangeWatchedFiles, Notification};
use tower_lsp::ls_types::{
    DidChangeWatchedFilesRegistrationOptions, FileChangeType, FileEvent, FileSystemWatcher,
    GlobPattern, OneOf, Registration, RelativePattern, Unregistration, Uri,
};
use tower_lsp::Client;

const CAPABILITY_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const EXTERNAL_WATCH_PATTERNS: &[&str] = &[
    "**/*.php",
    "**/*.twig",
    "**/composer.json",
    "**/composer.lock",
    "**/vendor/composer/installed.json",
    "**/vendor/composer/installed.php",
    "**/vendor/composer/autoload_*.php",
    "**/.php-lsp.toml",
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ExternalWatcherCapabilities {
    pub(crate) dynamic_registration: bool,
    pub(crate) relative_pattern_support: bool,
}

impl ExternalWatcherCapabilities {
    fn supported(self) -> bool {
        self.dynamic_registration && self.relative_pattern_support
    }
}

#[derive(Debug, Clone)]
struct WorkspaceSymlinkSnapshot {
    generation: u64,
    logical_root: PathBuf,
    aliases: Vec<SymlinkAlias>,
    physical_files: Vec<PhysicalFileGroup>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct WatchSpec {
    base: PathBuf,
    pattern: String,
}

#[derive(Debug, Clone)]
struct RegisteredWatchers {
    id: String,
    specs: Vec<WatchSpec>,
}

#[derive(Debug, Default)]
struct ExternalSymlinkState {
    capabilities: ExternalWatcherCapabilities,
    active_workspaces: HashMap<PathBuf, u64>,
    workspaces: HashMap<PathBuf, WorkspaceSymlinkSnapshot>,
    registered: Option<RegisteredWatchers>,
    stale_registrations: Vec<RegisteredWatchers>,
    next_registration: u64,
    warned_unsupported: bool,
}

pub(crate) struct ExternalSymlinkManager {
    client: Client,
    state: Mutex<ExternalSymlinkState>,
    registration_reload: Mutex<()>,
}

impl ExternalSymlinkManager {
    pub(crate) fn new(client: Client) -> Arc<Self> {
        Arc::new(Self {
            client,
            state: Mutex::new(ExternalSymlinkState::default()),
            registration_reload: Mutex::new(()),
        })
    }

    pub(crate) async fn set_capabilities(&self, capabilities: ExternalWatcherCapabilities) {
        self.state.lock().await.capabilities = capabilities;
    }

    pub(crate) async fn publish_workspace(
        self: &Arc<Self>,
        workspace_folder: PathBuf,
        logical_root: PathBuf,
        generation: u64,
        mut aliases: Vec<SymlinkAlias>,
        mut physical_files: Vec<PhysicalFileGroup>,
    ) {
        normalize_alias_snapshot(&mut aliases, &mut physical_files);
        {
            let mut state = self.state.lock().await;
            if !state.publish_workspace_snapshot(
                workspace_folder,
                WorkspaceSymlinkSnapshot {
                    generation,
                    logical_root,
                    aliases,
                    physical_files,
                },
            ) {
                return;
            }
        }
        self.refresh_registration().await;
    }

    pub(crate) async fn set_active_workspaces(
        self: &Arc<Self>,
        active: &[(PathBuf, u64)],
        reset_snapshots: &[PathBuf],
    ) {
        {
            let mut state = self.state.lock().await;
            state.set_active_generations(active, reset_snapshots);
        }
        self.refresh_registration().await;
    }

    pub(crate) async fn publish_additional_aliases(
        self: &Arc<Self>,
        workspace_folder: PathBuf,
        logical_root: PathBuf,
        generation: u64,
        mut aliases: Vec<SymlinkAlias>,
        mut physical_files: Vec<PhysicalFileGroup>,
    ) {
        normalize_alias_snapshot(&mut aliases, &mut physical_files);
        {
            let mut state = self.state.lock().await;
            if !state.publish_additional_snapshot(
                workspace_folder,
                WorkspaceSymlinkSnapshot {
                    generation,
                    logical_root,
                    aliases,
                    physical_files,
                },
            ) {
                return;
            }
        }
        self.refresh_registration().await;
    }

    pub(crate) async fn translate_events(
        self: &Arc<Self>,
        changes: Vec<FileEvent>,
    ) -> Vec<FileEvent> {
        let translated = self.state.lock().await.translate_events(changes);
        self.refresh_registration().await;
        translated
    }

    pub(crate) async fn shutdown(self: &Arc<Self>) {
        {
            let mut state = self.state.lock().await;
            state.active_workspaces.clear();
            state.workspaces.clear();
        }
        self.refresh_registration().await;
    }

    async fn refresh_registration(self: &Arc<Self>) {
        let _reload = self.registration_reload.lock().await;
        let (capabilities, desired_specs, current, next_id, should_warn, needs_change) = {
            let mut state = self.state.lock().await;
            let desired_specs = state.desired_watch_specs();
            let needs_change = match state.registered.as_ref() {
                Some(registered) => registered.specs != desired_specs,
                None => !desired_specs.is_empty(),
            };
            let has_stale = !state.stale_registrations.is_empty();
            if !needs_change && !has_stale {
                return;
            }
            let should_warn = !desired_specs.is_empty()
                && !state.capabilities.supported()
                && !state.warned_unsupported;
            if should_warn {
                state.warned_unsupported = true;
            }
            if needs_change {
                state.next_registration = state.next_registration.saturating_add(1);
            }
            (
                state.capabilities,
                desired_specs,
                state.registered.clone(),
                state.next_registration,
                should_warn,
                needs_change,
            )
        };

        if !capabilities.supported() {
            if should_warn {
                let message = "The LSP client does not support dynamic relative file watchers; external symlink targets are indexed safely but live changes require a reindex";
                tracing::warn!("{}", message);
                self.client
                    .log_message(tower_lsp::ls_types::MessageType::WARNING, message)
                    .await;
            }
            return;
        }

        let new_registration = if !needs_change {
            current.clone()
        } else if desired_specs.is_empty() {
            None
        } else {
            let id = format!("php-lsp-external-symlinks-{next_id}");
            let watchers = desired_specs
                .iter()
                .filter_map(watch_spec_to_watcher)
                .collect::<Vec<_>>();
            if watchers.is_empty() {
                None
            } else {
                let options = DidChangeWatchedFilesRegistrationOptions { watchers };
                let registration = Registration {
                    id: id.clone(),
                    method: DidChangeWatchedFiles::METHOD.to_string(),
                    register_options: to_value(options).ok(),
                };
                match tokio::time::timeout(
                    CAPABILITY_REQUEST_TIMEOUT,
                    self.client.register_capability(vec![registration]),
                )
                .await
                {
                    Ok(Ok(())) => Some(RegisteredWatchers {
                        id,
                        specs: desired_specs.clone(),
                    }),
                    Ok(Err(error)) => {
                        tracing::warn!("Failed to register external symlink watchers: {}", error);
                        return;
                    }
                    Err(_) => {
                        tracing::warn!(
                            "Timed out registering external symlink watchers after {} ms",
                            CAPABILITY_REQUEST_TIMEOUT.as_millis()
                        );
                        return;
                    }
                }
            }
        };

        if needs_change {
            let mut state = self.state.lock().await;
            state.commit_registration(new_registration.clone(), current.clone());
        }

        let stale_registrations = self.state.lock().await.stale_registrations.clone();
        for old in stale_registrations {
            let unregister = Unregistration {
                id: old.id.clone(),
                method: DidChangeWatchedFiles::METHOD.to_string(),
            };
            match tokio::time::timeout(
                CAPABILITY_REQUEST_TIMEOUT,
                self.client.unregister_capability(vec![unregister]),
            )
            .await
            {
                Ok(Ok(())) => {
                    self.state
                        .lock()
                        .await
                        .record_unregistration_result(&old.id, true);
                }
                Ok(Err(error)) => {
                    tracing::warn!("Failed to unregister stale external watchers: {}", error);
                }
                Err(_) => {
                    tracing::warn!(
                        "Timed out unregistering stale external watchers after {} ms",
                        CAPABILITY_REQUEST_TIMEOUT.as_millis()
                    );
                }
            }
        }
    }
}

fn merge_aliases(current: &mut Vec<SymlinkAlias>, aliases: Vec<SymlinkAlias>) {
    for alias in aliases {
        if !current.iter().any(|existing| existing == &alias) {
            current.push(alias);
        }
    }
    current.sort_by(|left, right| left.logical_path.cmp(&right.logical_path));
}

fn normalize_alias_snapshot(
    aliases: &mut Vec<SymlinkAlias>,
    physical_files: &mut [PhysicalFileGroup],
) {
    aliases.sort_by(|left, right| left.logical_path.cmp(&right.logical_path));
    aliases.dedup();
    expand_physical_file_aliases(aliases, physical_files);
    physical_files.sort_by(|left, right| left.representative().cmp(right.representative()));
}

fn expand_physical_file_aliases(
    aliases: &[SymlinkAlias],
    physical_files: &mut [PhysicalFileGroup],
) {
    for group in physical_files {
        let physical_paths = group
            .paths
            .iter()
            .map(|path| path.physical_path.clone())
            .collect::<BTreeSet<_>>();
        for physical_path in physical_paths {
            for alias in aliases {
                let logical_path = match alias.target_kind {
                    SymlinkTargetKind::Directory => physical_path
                        .strip_prefix(&alias.physical_target)
                        .ok()
                        .map(|relative| alias.logical_path.join(relative)),
                    SymlinkTargetKind::File if alias.physical_target == physical_path => {
                        Some(alias.logical_path.clone())
                    }
                    SymlinkTargetKind::File => None,
                };
                let Some(logical_path) = logical_path else {
                    continue;
                };
                if group
                    .paths
                    .iter()
                    .any(|candidate| candidate.logical_path == logical_path)
                {
                    continue;
                }
                group.paths.push(PhysicalFilePath {
                    logical_path,
                    physical_path: physical_path.clone(),
                });
            }
        }
        group
            .paths
            .sort_by(|left, right| left.logical_path.cmp(&right.logical_path));
        group
            .paths
            .dedup_by(|left, right| left.logical_path == right.logical_path);
    }
}

fn merge_physical_files(
    current: &mut Vec<PhysicalFileGroup>,
    physical_files: Vec<PhysicalFileGroup>,
) {
    for mut group in physical_files {
        if let Some(existing) = current
            .iter_mut()
            .find(|existing| existing.identity == group.identity)
        {
            for path in group.paths.drain(..) {
                if !existing
                    .paths
                    .iter()
                    .any(|candidate| candidate.logical_path == path.logical_path)
                {
                    existing.paths.push(path);
                }
            }
            existing
                .paths
                .sort_by(|left, right| left.logical_path.cmp(&right.logical_path));
        } else {
            current.push(group);
        }
    }
    current.sort_by(|left, right| left.representative().cmp(right.representative()));
}

impl ExternalSymlinkState {
    fn set_active_generations(&mut self, active: &[(PathBuf, u64)], reset_snapshots: &[PathBuf]) {
        self.active_workspaces = active.iter().cloned().collect();
        let active_workspaces = self.active_workspaces.clone();
        self.workspaces.retain(|workspace_folder, snapshot| {
            let Some(generation) = active_workspaces.get(workspace_folder) else {
                return false;
            };
            if reset_snapshots.contains(workspace_folder) {
                return false;
            }
            snapshot.generation = *generation;
            true
        });
    }

    fn commit_registration(
        &mut self,
        new_registration: Option<RegisteredWatchers>,
        previous: Option<RegisteredWatchers>,
    ) {
        self.registered = new_registration.clone();
        let Some(previous) = previous else {
            return;
        };
        if new_registration
            .as_ref()
            .is_some_and(|registered| registered.id == previous.id)
        {
            return;
        }
        if !self
            .stale_registrations
            .iter()
            .any(|registered| registered.id == previous.id)
        {
            self.stale_registrations.push(previous);
        }
    }

    fn record_unregistration_result(&mut self, registration_id: &str, succeeded: bool) {
        if succeeded {
            self.stale_registrations
                .retain(|registered| registered.id != registration_id);
        }
    }

    fn publish_workspace_snapshot(
        &mut self,
        workspace_folder: PathBuf,
        snapshot: WorkspaceSymlinkSnapshot,
    ) -> bool {
        if self.active_workspaces.get(&workspace_folder) != Some(&snapshot.generation) {
            return false;
        }
        if self
            .workspaces
            .get(&workspace_folder)
            .is_some_and(|current| current.generation > snapshot.generation)
        {
            return false;
        }
        self.workspaces.insert(workspace_folder, snapshot);
        true
    }

    fn publish_additional_snapshot(
        &mut self,
        workspace_folder: PathBuf,
        snapshot: WorkspaceSymlinkSnapshot,
    ) -> bool {
        if self.active_workspaces.get(&workspace_folder) != Some(&snapshot.generation) {
            return false;
        }
        match self.workspaces.entry(workspace_folder) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                if entry.get().generation != snapshot.generation {
                    return false;
                }
                merge_aliases(&mut entry.get_mut().aliases, snapshot.aliases);
                merge_physical_files(&mut entry.get_mut().physical_files, snapshot.physical_files);
                let snapshot = entry.get_mut();
                expand_physical_file_aliases(&snapshot.aliases, &mut snapshot.physical_files);
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(snapshot);
            }
        }
        true
    }

    fn desired_watch_specs(&self) -> Vec<WatchSpec> {
        let mut directory_roots = BTreeSet::<PathBuf>::new();
        let mut file_specs = BTreeSet::<WatchSpec>::new();
        let covered_workspace_roots = self
            .workspaces
            .values()
            .filter(|snapshot| {
                std::fs::symlink_metadata(&snapshot.logical_root)
                    .is_ok_and(|metadata| !metadata.file_type().is_symlink())
            })
            .filter_map(|snapshot| std::fs::canonicalize(&snapshot.logical_root).ok())
            .collect::<Vec<_>>();
        for snapshot in self.workspaces.values() {
            for alias in &snapshot.aliases {
                if covered_workspace_roots
                    .iter()
                    .any(|root| alias.physical_target.starts_with(root))
                {
                    continue;
                }
                match alias.target_kind {
                    SymlinkTargetKind::Directory => {
                        directory_roots.insert(alias.physical_target.clone());
                    }
                    SymlinkTargetKind::File => {
                        if !is_relevant_external_file(&alias.physical_target) {
                            continue;
                        }
                        let Some(parent) = alias.physical_target.parent() else {
                            continue;
                        };
                        let Some(file_name) = alias.physical_target.file_name() else {
                            continue;
                        };
                        file_specs.insert(WatchSpec {
                            base: parent.to_path_buf(),
                            pattern: file_name.to_string_lossy().to_string(),
                        });
                    }
                }
            }
        }

        let mut minimal_roots = Vec::<PathBuf>::new();
        for root in directory_roots {
            if minimal_roots.iter().any(|parent| root.starts_with(parent)) {
                continue;
            }
            minimal_roots.retain(|child| !child.starts_with(&root));
            minimal_roots.push(root);
            minimal_roots.sort();
        }

        let mut specs = BTreeSet::<WatchSpec>::new();
        for base in &minimal_roots {
            for pattern in EXTERNAL_WATCH_PATTERNS {
                specs.insert(WatchSpec {
                    base: base.clone(),
                    pattern: (*pattern).to_string(),
                });
            }
        }
        for spec in file_specs {
            if !minimal_roots.iter().any(|root| spec.base.starts_with(root)) {
                specs.insert(spec);
            }
        }
        specs.into_iter().collect()
    }

    fn translate_events(&mut self, changes: Vec<FileEvent>) -> Vec<FileEvent> {
        let mut translated = Vec::new();
        for change in changes {
            let Some(path) = uri_to_path(change.uri.as_str()) else {
                translated.push(change);
                continue;
            };

            let mut handled_by_snapshot = false;
            let mut matched_direct_workspace = false;
            for snapshot in self.workspaces.values_mut() {
                let is_direct_workspace_path = path.starts_with(&snapshot.logical_root);
                let mut snapshot_handled = false;
                if is_direct_workspace_path {
                    matched_direct_workspace = true;
                    if change.typ == FileChangeType::DELETED {
                        let events = handle_logical_delete(snapshot, &path);
                        snapshot_handled |= !events.is_empty();
                        translated.extend(events);
                    }
                }
                let had_physical_route = snapshot_has_physical_route(snapshot, &path);
                let events = translate_event_for_snapshot(snapshot, &path, change.typ);
                snapshot_handled |= had_physical_route || !events.is_empty();
                translated.extend(events);
                if is_direct_workspace_path && !snapshot_handled {
                    translated.push(change.clone());
                    snapshot_handled = true;
                }
                handled_by_snapshot |= snapshot_handled;
            }
            if !matched_direct_workspace && !handled_by_snapshot {
                translated.push(change);
            }
        }

        let mut seen = HashSet::new();
        translated.retain(|event| seen.insert((event.uri.as_str().to_string(), event.typ)));
        translated.sort_by(|left, right| {
            left.uri
                .as_str()
                .cmp(right.uri.as_str())
                .then(file_change_rank(left.typ).cmp(&file_change_rank(right.typ)))
        });
        translated
    }
}

fn snapshot_has_physical_route(snapshot: &WorkspaceSymlinkSnapshot, physical_path: &Path) -> bool {
    snapshot.physical_files.iter().any(|group| {
        group
            .paths
            .iter()
            .any(|candidate| candidate.physical_path == physical_path)
    }) || !logical_paths_for_physical(snapshot, physical_path).is_empty()
}

fn handle_logical_delete(
    snapshot: &mut WorkspaceSymlinkSnapshot,
    deleted_path: &Path,
) -> Vec<FileEvent> {
    let removed_alias = snapshot
        .aliases
        .iter()
        .filter(|alias| deleted_path.starts_with(&alias.logical_path))
        .filter(|alias| logical_alias_is_missing(&alias.logical_path))
        .min_by_key(|alias| alias.logical_path.components().count())
        .map(|alias| alias.logical_path.clone());
    if let Some(removed_alias) = removed_alias {
        return remove_logical_alias(snapshot, &removed_alias);
    }

    let physical_path = snapshot
        .physical_files
        .iter()
        .flat_map(|group| group.paths.iter())
        .find(|candidate| candidate.logical_path == deleted_path)
        .map(|candidate| candidate.physical_path.clone());
    physical_path
        .map(|physical_path| remove_physical_file(snapshot, &physical_path))
        .unwrap_or_default()
}

fn logical_alias_is_missing(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(_) => false,
        Err(error) if alias_missing_error_kind(error.kind()) => true,
        Err(error) => {
            tracing::debug!(
                "Could not verify logical symlink alias {} after descendant delete: {}",
                path.display(),
                error
            );
            false
        }
    }
}

fn alias_missing_error_kind(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

fn remove_logical_alias(
    snapshot: &mut WorkspaceSymlinkSnapshot,
    deleted_path: &Path,
) -> Vec<FileEvent> {
    if !snapshot
        .aliases
        .iter()
        .any(|alias| alias.logical_path == deleted_path)
    {
        return Vec::new();
    }
    snapshot
        .aliases
        .retain(|alias| !alias.logical_path.starts_with(deleted_path));
    let mut events = Vec::new();
    for group in &mut snapshot.physical_files {
        let old_representative = group.representative().to_path_buf();
        group
            .paths
            .retain(|candidate| !candidate.logical_path.starts_with(deleted_path));
        if group.paths.is_empty() {
            events.extend(logical_file_event(
                &old_representative,
                FileChangeType::DELETED,
            ));
            continue;
        }
        group
            .paths
            .sort_by(|left, right| left.logical_path.cmp(&right.logical_path));
        let new_representative = group.representative().to_path_buf();
        if new_representative != old_representative {
            events.extend(logical_file_event(
                &old_representative,
                FileChangeType::DELETED,
            ));
            events.extend(logical_file_event(
                &new_representative,
                FileChangeType::CREATED,
            ));
        }
    }
    snapshot
        .physical_files
        .retain(|group| !group.paths.is_empty());
    events
}

fn is_relevant_external_file(path: &Path) -> bool {
    if path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("php") || extension.eq_ignore_ascii_case("twig")
        })
    {
        return true;
    }
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some("composer.json" | "composer.lock" | "installed.json" | ".php-lsp.toml")
    )
}

fn translate_event_for_snapshot(
    snapshot: &mut WorkspaceSymlinkSnapshot,
    physical_path: &Path,
    change_type: FileChangeType,
) -> Vec<FileEvent> {
    if change_type == FileChangeType::DELETED {
        let events = remove_physical_file(snapshot, physical_path);
        if !events.is_empty() {
            return events;
        }
    }

    let identity = (change_type != FileChangeType::DELETED)
        .then(|| file_id::get_file_id(physical_path).ok())
        .flatten();
    let routed_logical_paths = logical_paths_for_physical(snapshot, physical_path);
    if let Some(identity) = identity {
        if let Some(group) = snapshot.physical_files.iter_mut().find(|group| {
            matches!(
                &group.identity,
                crate::util::fs_walk::PhysicalIdentity::FileId(existing) if *existing == identity
            )
        }) {
            let old_representative = group.representative().to_path_buf();
            if change_type == FileChangeType::CREATED {
                for logical_path in &routed_logical_paths {
                    if !group
                        .paths
                        .iter()
                        .any(|candidate| candidate.logical_path == *logical_path)
                    {
                        group.paths.push(PhysicalFilePath {
                            logical_path: logical_path.clone(),
                            physical_path: physical_path.to_path_buf(),
                        });
                    }
                }
                group
                    .paths
                    .sort_by(|left, right| left.logical_path.cmp(&right.logical_path));
            }
            let new_representative = group.representative().to_path_buf();
            if new_representative != old_representative {
                return [
                    logical_file_event(&old_representative, FileChangeType::DELETED),
                    logical_file_event(&new_representative, FileChangeType::CREATED),
                ]
                .into_iter()
                .flatten()
                .collect();
            }
            return logical_file_event(&new_representative, change_type)
                .into_iter()
                .collect();
        }
    }

    let Some(logical_path) = routed_logical_paths.first().cloned() else {
        return Vec::new();
    };
    if change_type == FileChangeType::CREATED {
        if let Some(identity) = identity {
            snapshot.physical_files.push(PhysicalFileGroup {
                identity: crate::util::fs_walk::PhysicalIdentity::FileId(identity),
                paths: routed_logical_paths
                    .into_iter()
                    .map(|logical_path| PhysicalFilePath {
                        logical_path,
                        physical_path: physical_path.to_path_buf(),
                    })
                    .collect(),
            });
            snapshot
                .physical_files
                .sort_by(|left, right| left.representative().cmp(right.representative()));
        }
    }
    logical_file_event(&logical_path, change_type)
        .into_iter()
        .collect()
}

fn remove_physical_file(
    snapshot: &mut WorkspaceSymlinkSnapshot,
    physical_path: &Path,
) -> Vec<FileEvent> {
    let Some(group_index) = snapshot.physical_files.iter().position(|group| {
        group
            .paths
            .iter()
            .any(|candidate| candidate.physical_path == physical_path)
    }) else {
        return Vec::new();
    };
    let old_representative = snapshot.physical_files[group_index]
        .representative()
        .to_path_buf();
    snapshot.physical_files[group_index]
        .paths
        .retain(|candidate| candidate.physical_path != physical_path);
    if snapshot.physical_files[group_index].paths.is_empty() {
        snapshot.physical_files.remove(group_index);
        return logical_file_event(&old_representative, FileChangeType::DELETED)
            .into_iter()
            .collect();
    }
    let group = &mut snapshot.physical_files[group_index];
    group
        .paths
        .sort_by(|left, right| left.logical_path.cmp(&right.logical_path));
    let new_representative = group.representative().to_path_buf();
    if new_representative != old_representative {
        return [
            logical_file_event(&old_representative, FileChangeType::DELETED),
            logical_file_event(&new_representative, FileChangeType::CREATED),
        ]
        .into_iter()
        .flatten()
        .collect();
    }
    Vec::new()
}

fn logical_paths_for_physical(
    snapshot: &WorkspaceSymlinkSnapshot,
    physical_path: &Path,
) -> Vec<PathBuf> {
    let mut candidates = Vec::<PathBuf>::new();
    for alias in &snapshot.aliases {
        match alias.target_kind {
            SymlinkTargetKind::Directory => {
                let Ok(relative) = physical_path.strip_prefix(&alias.physical_target) else {
                    continue;
                };
                candidates.push(alias.logical_path.join(relative));
            }
            SymlinkTargetKind::File if alias.physical_target == physical_path => {
                candidates.push(alias.logical_path.clone());
            }
            SymlinkTargetKind::File => {}
        }
    }
    candidates.sort();
    candidates.dedup();
    candidates
}

fn logical_file_event(path: &Path, typ: FileChangeType) -> Option<FileEvent> {
    let uri = path_to_uri(path).ok()?.parse::<Uri>().ok()?;
    Some(FileEvent { uri, typ })
}

fn watch_spec_to_watcher(spec: &WatchSpec) -> Option<FileSystemWatcher> {
    let base_uri = path_to_uri(&spec.base).ok()?.parse::<Uri>().ok()?;
    Some(FileSystemWatcher {
        glob_pattern: GlobPattern::Relative(RelativePattern {
            base_uri: OneOf::Right(base_uri),
            pattern: spec.pattern.clone(),
        }),
        kind: None,
    })
}

fn file_change_rank(typ: FileChangeType) -> u8 {
    if typ == FileChangeType::DELETED {
        0
    } else if typ == FileChangeType::CREATED {
        1
    } else {
        2
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::fs_walk::{PhysicalIdentity, SymlinkAlias};

    fn uri(path: &Path) -> Uri {
        path_to_uri(path)
            .expect("path URI")
            .parse::<Uri>()
            .expect("LSP URI")
    }

    #[test]
    fn external_watcher_fallback_requires_both_lsp_capabilities() {
        assert!(!ExternalWatcherCapabilities::default().supported());
        assert!(!ExternalWatcherCapabilities {
            dynamic_registration: true,
            relative_pattern_support: false,
        }
        .supported());
        assert!(!ExternalWatcherCapabilities {
            dynamic_registration: false,
            relative_pattern_support: true,
        }
        .supported());
        assert!(ExternalWatcherCapabilities {
            dynamic_registration: true,
            relative_pattern_support: true,
        }
        .supported());
    }

    #[test]
    fn desired_watch_specs_merge_duplicate_and_nested_targets() {
        let mut state = ExternalSymlinkState::default();
        state.workspaces.insert(
            PathBuf::from("/workspace"),
            WorkspaceSymlinkSnapshot {
                generation: 1,
                logical_root: PathBuf::from("/workspace"),
                aliases: vec![
                    SymlinkAlias {
                        logical_path: PathBuf::from("/workspace/linked"),
                        physical_target: PathBuf::from("/external"),
                        target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from(
                            "/external",
                        )),
                        target_kind: SymlinkTargetKind::Directory,
                    },
                    SymlinkAlias {
                        logical_path: PathBuf::from("/workspace/linked/nested"),
                        physical_target: PathBuf::from("/external/nested"),
                        target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from(
                            "/external/nested",
                        )),
                        target_kind: SymlinkTargetKind::Directory,
                    },
                ],
                physical_files: Vec::new(),
            },
        );

        let specs = state.desired_watch_specs();
        assert_eq!(specs.len(), EXTERNAL_WATCH_PATTERNS.len());
        assert!(specs.iter().all(|spec| spec.base == Path::new("/external")));
        let watcher = watch_spec_to_watcher(&specs[0]).expect("relative watcher");
        let GlobPattern::Relative(relative) = watcher.glob_pattern else {
            panic!("external watcher must use RelativePattern");
        };
        assert_eq!(relative.base_uri, OneOf::Right(uri(Path::new("/external"))));
    }

    #[test]
    fn direct_external_installed_json_symlink_gets_a_file_watcher() {
        let mut state = ExternalSymlinkState::default();
        state.workspaces.insert(
            PathBuf::from("/workspace"),
            WorkspaceSymlinkSnapshot {
                generation: 1,
                logical_root: PathBuf::from("/workspace"),
                aliases: vec![SymlinkAlias {
                    logical_path: PathBuf::from("/workspace/vendor/composer/installed.json"),
                    physical_target: PathBuf::from("/external/installed.json"),
                    target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from(
                        "/external/installed.json",
                    )),
                    target_kind: SymlinkTargetKind::File,
                }],
                physical_files: Vec::new(),
            },
        );

        assert_eq!(
            state.desired_watch_specs(),
            vec![WatchSpec {
                base: PathBuf::from("/external"),
                pattern: "installed.json".to_string(),
            }]
        );
    }

    #[test]
    fn physical_events_map_to_the_lexicographically_first_logical_alias() {
        let mut snapshot = WorkspaceSymlinkSnapshot {
            generation: 1,
            logical_root: PathBuf::from("/workspace"),
            aliases: vec![
                SymlinkAlias {
                    logical_path: PathBuf::from("/workspace/z-parent"),
                    physical_target: PathBuf::from("/external"),
                    target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from("/external")),
                    target_kind: SymlinkTargetKind::Directory,
                },
                SymlinkAlias {
                    logical_path: PathBuf::from("/workspace/a-nested"),
                    physical_target: PathBuf::from("/external/nested"),
                    target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from(
                        "/external/nested",
                    )),
                    target_kind: SymlinkTargetKind::Directory,
                },
            ],
            physical_files: Vec::new(),
        };

        let events = translate_event_for_snapshot(
            &mut snapshot,
            Path::new("/external/nested/New.php"),
            FileChangeType::CREATED,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uri, uri(Path::new("/workspace/a-nested/New.php")));
    }

    #[test]
    fn deleting_representative_promotes_the_next_physical_alias() {
        let mut snapshot = WorkspaceSymlinkSnapshot {
            generation: 1,
            logical_root: PathBuf::from("/workspace"),
            aliases: Vec::new(),
            physical_files: vec![PhysicalFileGroup {
                identity: PhysicalIdentity::CanonicalPath(PathBuf::from("identity")),
                paths: vec![
                    PhysicalFilePath {
                        logical_path: PathBuf::from("/workspace/a.php"),
                        physical_path: PathBuf::from("/external/a.php"),
                    },
                    PhysicalFilePath {
                        logical_path: PathBuf::from("/workspace/b.php"),
                        physical_path: PathBuf::from("/external/b.php"),
                    },
                ],
            }],
        };

        let events = translate_event_for_snapshot(
            &mut snapshot,
            Path::new("/external/a.php"),
            FileChangeType::DELETED,
        );
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].uri, uri(Path::new("/workspace/a.php")));
        assert_eq!(events[0].typ, FileChangeType::DELETED);
        assert_eq!(events[1].uri, uri(Path::new("/workspace/b.php")));
        assert_eq!(events[1].typ, FileChangeType::CREATED);
    }

    #[test]
    fn one_physical_event_maps_independently_into_multiple_workspaces() {
        let alias = |logical_root: &str| SymlinkAlias {
            logical_path: PathBuf::from(logical_root).join("linked"),
            physical_target: PathBuf::from("/external"),
            target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from("/external")),
            target_kind: SymlinkTargetKind::Directory,
        };
        let mut state = ExternalSymlinkState::default();
        for workspace in ["/workspace-a", "/workspace-b"] {
            state.workspaces.insert(
                PathBuf::from(workspace),
                WorkspaceSymlinkSnapshot {
                    generation: 1,
                    logical_root: PathBuf::from(workspace),
                    aliases: vec![alias(workspace)],
                    physical_files: Vec::new(),
                },
            );
        }

        let events = state.translate_events(vec![FileEvent {
            uri: uri(Path::new("/external/Changed.php")),
            typ: FileChangeType::CHANGED,
        }]);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].uri,
            uri(Path::new("/workspace-a/linked/Changed.php"))
        );
        assert_eq!(
            events[1].uri,
            uri(Path::new("/workspace-b/linked/Changed.php"))
        );
    }

    #[test]
    fn stale_workspace_generation_cannot_replace_newer_alias_snapshot() {
        let workspace = PathBuf::from("/workspace");
        let snapshot = |generation, target: &str| WorkspaceSymlinkSnapshot {
            generation,
            logical_root: workspace.clone(),
            aliases: vec![SymlinkAlias {
                logical_path: workspace.join("linked"),
                physical_target: PathBuf::from(target),
                target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from(target)),
                target_kind: SymlinkTargetKind::Directory,
            }],
            physical_files: Vec::new(),
        };
        let mut state = ExternalSymlinkState::default();
        state.active_workspaces.insert(workspace.clone(), 8);
        assert!(state.publish_workspace_snapshot(workspace.clone(), snapshot(8, "/new")));
        assert!(!state.publish_workspace_snapshot(workspace.clone(), snapshot(7, "/stale")));
        assert_eq!(
            state.workspaces[&workspace].aliases[0].physical_target,
            PathBuf::from("/new")
        );
    }

    #[test]
    fn removed_workspace_tombstone_rejects_delayed_index_and_vendor_publications() {
        let workspace = PathBuf::from("/workspace");
        let snapshot = || WorkspaceSymlinkSnapshot {
            generation: 5,
            logical_root: workspace.clone(),
            aliases: Vec::new(),
            physical_files: Vec::new(),
        };
        let mut state = ExternalSymlinkState::default();
        state.active_workspaces.insert(workspace.clone(), 5);
        assert!(state.publish_workspace_snapshot(workspace.clone(), snapshot()));

        state.active_workspaces.clear();
        state.workspaces.clear();
        assert!(!state.publish_workspace_snapshot(workspace.clone(), snapshot()));
        assert!(!state.publish_additional_snapshot(workspace.clone(), snapshot()));
        assert!(!state.workspaces.contains_key(&workspace));
    }

    #[test]
    fn non_indexing_runtime_generation_preserves_aliases_but_rejects_old_publishers() {
        let workspace = PathBuf::from("/workspace");
        let snapshot = |generation| WorkspaceSymlinkSnapshot {
            generation,
            logical_root: workspace.clone(),
            aliases: vec![SymlinkAlias {
                logical_path: workspace.join("linked"),
                physical_target: PathBuf::from("/external"),
                target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from("/external")),
                target_kind: SymlinkTargetKind::Directory,
            }],
            physical_files: Vec::new(),
        };
        let mut state = ExternalSymlinkState::default();
        state.active_workspaces.insert(workspace.clone(), 1);
        assert!(state.publish_workspace_snapshot(workspace.clone(), snapshot(1)));

        state.set_active_generations(&[(workspace.clone(), 2)], &[]);
        assert_eq!(state.workspaces[&workspace].generation, 2);
        assert_eq!(state.workspaces[&workspace].aliases.len(), 1);
        assert!(!state.publish_workspace_snapshot(workspace.clone(), snapshot(1)));

        state.set_active_generations(&[(workspace.clone(), 3)], std::slice::from_ref(&workspace));
        assert!(!state.workspaces.contains_key(&workspace));
    }

    #[test]
    fn failed_unregister_remains_pending_until_confirmation() {
        let old = RegisteredWatchers {
            id: "old".to_string(),
            specs: Vec::new(),
        };
        let new = RegisteredWatchers {
            id: "new".to_string(),
            specs: Vec::new(),
        };
        let mut state = ExternalSymlinkState {
            registered: Some(old.clone()),
            ..Default::default()
        };
        state.commit_registration(Some(new.clone()), Some(old));

        assert_eq!(
            state.registered.as_ref().map(|item| item.id.as_str()),
            Some("new")
        );
        assert_eq!(state.stale_registrations.len(), 1);
        state.record_unregistration_result("old", false);
        // Error/timeout keeps the old ID retryable.
        assert_eq!(state.stale_registrations[0].id, "old");
        state.record_unregistration_result("old", true);
        assert!(state.stale_registrations.is_empty());
    }

    #[test]
    fn transient_alias_metadata_errors_do_not_remove_registry_state() {
        assert!(alias_missing_error_kind(std::io::ErrorKind::NotFound));
        assert!(alias_missing_error_kind(std::io::ErrorKind::NotADirectory));
        assert!(!alias_missing_error_kind(
            std::io::ErrorKind::PermissionDenied
        ));
        assert!(!alias_missing_error_kind(std::io::ErrorKind::Other));
    }

    #[test]
    fn deleting_primary_directory_alias_promotes_the_same_physical_file() {
        let mut aliases = vec![
            SymlinkAlias {
                logical_path: PathBuf::from("/workspace/a-linked"),
                physical_target: PathBuf::from("/external"),
                target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from("/external")),
                target_kind: SymlinkTargetKind::Directory,
            },
            SymlinkAlias {
                logical_path: PathBuf::from("/workspace/b-linked"),
                physical_target: PathBuf::from("/external"),
                target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from("/external")),
                target_kind: SymlinkTargetKind::Directory,
            },
            SymlinkAlias {
                logical_path: PathBuf::from("/workspace/a-linked/nested"),
                physical_target: PathBuf::from("/external/nested"),
                target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from("/external/nested")),
                target_kind: SymlinkTargetKind::Directory,
            },
        ];
        let mut physical_files = vec![PhysicalFileGroup {
            identity: PhysicalIdentity::CanonicalPath(PathBuf::from("identity")),
            paths: vec![PhysicalFilePath {
                logical_path: PathBuf::from("/workspace/a-linked/Subject.php"),
                physical_path: PathBuf::from("/external/Subject.php"),
            }],
        }];
        normalize_alias_snapshot(&mut aliases, &mut physical_files);
        assert_eq!(physical_files[0].paths.len(), 2);

        let workspace = PathBuf::from("/workspace");
        let mut state = ExternalSymlinkState::default();
        state.workspaces.insert(
            workspace.clone(),
            WorkspaceSymlinkSnapshot {
                generation: 1,
                logical_root: workspace,
                aliases,
                physical_files,
            },
        );
        let events = state.translate_events(vec![FileEvent {
            uri: uri(Path::new("/workspace/a-linked/Subject.php")),
            typ: FileChangeType::DELETED,
        }]);
        let php_events = events
            .into_iter()
            .filter(|event| event.uri.as_str().ends_with("Subject.php"))
            .collect::<Vec<_>>();
        assert_eq!(php_events.len(), 2);
        assert_eq!(
            php_events[0].uri,
            uri(Path::new("/workspace/a-linked/Subject.php"))
        );
        assert_eq!(php_events[0].typ, FileChangeType::DELETED);
        assert_eq!(
            php_events[1].uri,
            uri(Path::new("/workspace/b-linked/Subject.php"))
        );
        assert_eq!(php_events[1].typ, FileChangeType::CREATED);
        assert_eq!(
            state.workspaces[&PathBuf::from("/workspace")]
                .aliases
                .iter()
                .map(|alias| alias.logical_path.clone())
                .collect::<Vec<_>>(),
            vec![PathBuf::from("/workspace/b-linked")]
        );
    }

    #[test]
    fn event_inside_one_workspace_is_also_routed_to_another_workspace_alias() {
        let mut state = ExternalSymlinkState::default();
        state.workspaces.insert(
            PathBuf::from("/workspace-a"),
            WorkspaceSymlinkSnapshot {
                generation: 1,
                logical_root: PathBuf::from("/workspace-a"),
                aliases: vec![SymlinkAlias {
                    logical_path: PathBuf::from("/workspace-a/linked"),
                    physical_target: PathBuf::from("/workspace-b/shared"),
                    target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from(
                        "/workspace-b/shared",
                    )),
                    target_kind: SymlinkTargetKind::Directory,
                }],
                physical_files: Vec::new(),
            },
        );
        state.workspaces.insert(
            PathBuf::from("/workspace-b"),
            WorkspaceSymlinkSnapshot {
                generation: 1,
                logical_root: PathBuf::from("/workspace-b"),
                aliases: Vec::new(),
                physical_files: Vec::new(),
            },
        );

        let events = state.translate_events(vec![FileEvent {
            uri: uri(Path::new("/workspace-b/shared/Changed.php")),
            typ: FileChangeType::CHANGED,
        }]);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].uri,
            uri(Path::new("/workspace-a/linked/Changed.php"))
        );
        assert_eq!(
            events[1].uri,
            uri(Path::new("/workspace-b/shared/Changed.php"))
        );
    }

    #[test]
    fn physical_event_inside_workspace_also_updates_its_logical_alias() {
        let physical_file = PathBuf::from("/workspace/z-real/Changed.php");
        let logical_file = PathBuf::from("/workspace/a-linked/Changed.php");
        let mut state = ExternalSymlinkState::default();
        state.workspaces.insert(
            PathBuf::from("/workspace"),
            WorkspaceSymlinkSnapshot {
                generation: 1,
                logical_root: PathBuf::from("/workspace"),
                aliases: vec![SymlinkAlias {
                    logical_path: PathBuf::from("/workspace/a-linked"),
                    physical_target: PathBuf::from("/workspace/z-real"),
                    target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from(
                        "/workspace/z-real",
                    )),
                    target_kind: SymlinkTargetKind::Directory,
                }],
                physical_files: vec![PhysicalFileGroup {
                    identity: PhysicalIdentity::CanonicalPath(PathBuf::from("identity")),
                    paths: vec![PhysicalFilePath {
                        logical_path: logical_file.clone(),
                        physical_path: physical_file.clone(),
                    }],
                }],
            },
        );

        let events = state.translate_events(vec![FileEvent {
            uri: uri(&physical_file),
            typ: FileChangeType::CHANGED,
        }]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uri, uri(&logical_file));
    }

    #[test]
    fn logical_candidates_keep_every_alias_in_lexical_order() {
        let snapshot = WorkspaceSymlinkSnapshot {
            generation: 1,
            logical_root: PathBuf::from("/workspace"),
            aliases: vec![
                SymlinkAlias {
                    logical_path: PathBuf::from("/workspace/z-nested"),
                    physical_target: PathBuf::from("/external/nested"),
                    target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from(
                        "/external/nested",
                    )),
                    target_kind: SymlinkTargetKind::Directory,
                },
                SymlinkAlias {
                    logical_path: PathBuf::from("/workspace/a-parent"),
                    physical_target: PathBuf::from("/external"),
                    target_identity: PhysicalIdentity::CanonicalPath(PathBuf::from("/external")),
                    target_kind: SymlinkTargetKind::Directory,
                },
            ],
            physical_files: Vec::new(),
        };

        assert_eq!(
            logical_paths_for_physical(&snapshot, Path::new("/external/nested/New.php")),
            vec![
                PathBuf::from("/workspace/a-parent/nested/New.php"),
                PathBuf::from("/workspace/z-nested/New.php"),
            ]
        );
    }
}
