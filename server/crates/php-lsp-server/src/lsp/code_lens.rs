//! One cancellable reference aggregation for every lens in a document.
use super::*;
use php_lsp_index::workspace::{SourceFingerprint, WorkspaceIndexRevision};
use php_lsp_types::{FileSymbols, PhpSymbolKind, SymbolReference};

pub(in crate::server) struct CodeLensControl {
    stop: OperationCancellationToken,
    #[cfg(test)]
    pub(super) hooks: Hooks,
}
impl Default for CodeLensControl {
    fn default() -> Self {
        Self {
            stop: OperationCancellationToken::new(),
            #[cfg(test)]
            hooks: Hooks::default(),
        }
    }
}
impl CodeLensControl {
    pub(in crate::server) fn cancel(&self) {
        self.stop.cancel();
    }
}

struct Input {
    index: Arc<WorkspaceIndex>,
    open: Arc<DashMap<String, FileParser>>,
    templates: Arc<DashMap<String, TemplateDocument>>,
    versions: Arc<DashMap<String, OpenDocumentState>>,
    runtime: Arc<Mutex<Arc<WorkspaceRuntimeState>>>,
    request: WorkspaceRequestContext,
    aggregate: bool,
    control: Arc<CodeLensControl>,
}
struct Stamp {
    uri: String,
    state: Option<OpenDocumentState>,
    fingerprint: Option<SourceFingerprint>,
    template: bool,
}
struct OpenSource {
    symbols: FileSymbols,
    references: Vec<SymbolReference>,
}
struct LensResult {
    lenses: Vec<CodeLens>,
    revision: WorkspaceIndexRevision,
    stamps: Vec<Stamp>,
    open_uris: HashSet<String>,
    input: Input,
}

fn lens_key(kind: PhpSymbolKind, fqn: &str) -> Option<(bool, String)> {
    if is_class_like_kind(kind) {
        Some((false, fqn.trim_start_matches('\\').to_ascii_lowercase()))
    } else if kind == PhpSymbolKind::Method {
        Some((true, fqn.trim_start_matches('\\').to_ascii_lowercase()))
    } else {
        None
    }
}
impl Input {
    fn stopped(
        &self,
        cancel: &OperationCancellationToken,
        revision: WorkspaceIndexRevision,
    ) -> bool {
        cancel.is_cancelled()
            || self.control.stop.is_cancelled()
            || self.index.revision_snapshot() != revision
            || self
                .runtime
                .try_lock()
                .is_ok_and(|state| state.generation != self.request.state.generation)
    }
    fn open_uris(&self) -> HashSet<String> {
        // Only the open inventory is needed for validation, not a second
        // traversal of indexed reference files.
        self.open
            .iter()
            .filter(|entry| {
                self.aggregate || self.index.read().file_symbols().contains_key(entry.key()) || {
                    if let Some(config) = &self.request.workspace {
                        uri_to_path(entry.key()).is_some_and(|path| {
                            workspace_config_for_path_from_configs(
                                std::slice::from_ref(config),
                                &path,
                            )
                            .is_some()
                        })
                    } else {
                        workspace_config_for_uri_from_configs(
                            &self.request.state.configs,
                            entry.key(),
                        )
                        .is_none()
                    }
                }
            })
            .map(|entry| entry.key().clone())
            .collect()
    }
    fn build(self, uri: &str, cancel: &OperationCancellationToken) -> Option<LensResult> {
        let revision = self.index.revision_snapshot();
        if self.stopped(cancel, revision) {
            return None;
        }
        let (uris, open_uris) = reference_scan_uris_from_state(
            &self.index,
            self.aggregate,
            &self.open,
            Some(&self.request),
        );
        let open_uris = open_uris.into_iter().collect::<HashSet<_>>();
        let mut sources = HashMap::new();
        let mut stamps = Vec::new();
        let mut target_source = None;
        let mut ordered = open_uris.iter().collect::<Vec<_>>();
        ordered.sort();
        // Never acquire document locks while holding the index publication barrier.
        for file_uri in &ordered {
            let file_uri = *file_uri;
            if self.stopped(cancel, revision) {
                return None;
            }
            let snapshot = open_document_snapshot_from_state(
                &self.open,
                &self.templates,
                &self.versions,
                file_uri,
            )?;
            let template = snapshot.template_document.is_some();
            stamps.push(Stamp {
                uri: file_uri.clone(),
                state: snapshot.document_state,
                fingerprint: snapshot
                    .document_state
                    .is_none()
                    .then(|| SourceFingerprint::from_bytes(snapshot.source.as_bytes())),
                template,
            });
            if template {
                if file_uri == uri {
                    return None;
                }
                continue;
            }
            #[cfg(test)]
            note_reference_scan(file_uri);
            let references =
                php_lsp_parser::references::collect_symbol_references_in_file_cancellable(
                    &snapshot.tree,
                    &snapshot.source,
                    &snapshot.file_symbols,
                    None,
                    None,
                    &|| self.stopped(cancel, revision),
                )?;
            if file_uri == uri {
                target_source = Some(snapshot.source);
            }
            sources.insert(
                file_uri.clone(),
                OpenSource {
                    symbols: snapshot.file_symbols,
                    references,
                },
            );
        }
        let (target_symbols, source, disk_target) = if let Some(source) = target_source {
            (sources.get(uri)?.symbols.clone(), source, None)
        } else {
            if open_uris.contains(uri) {
                return None;
            }
            let (symbols, fingerprint) = {
                let read = self.index.read();
                let symbols = read.file_symbols().get(uri)?.value().clone();
                let fingerprint = *read.source_fingerprints().get(uri)?.value();
                (symbols, fingerprint)
            };
            if self.stopped(cancel, revision) {
                return None;
            }
            let path = uri_to_path(uri)?;
            let bytes = std::fs::read(&path).ok()?;
            if SourceFingerprint::from_bytes(&bytes) != fingerprint {
                return None;
            }
            (
                symbols.as_ref().clone(),
                String::from_utf8(bytes).ok()?,
                Some((path, fingerprint)),
            )
        };
        let symbols = target_symbols
            .symbols
            .iter()
            .filter(|symbol| is_code_lens_symbol_kind(symbol.kind))
            .collect::<Vec<_>>();
        if symbols.is_empty() {
            return None;
        }
        #[cfg(test)]
        if !self
            .control
            .hooks
            .pause(false, &|| self.stopped(cancel, revision))
        {
            return None;
        }
        let mut targets = HashMap::<(bool, String), Vec<usize>>::new();
        for (i, symbol) in symbols.iter().enumerate() {
            targets
                .entry(lens_key(symbol.kind, &symbol.fqn)?)
                .or_default()
                .push(i);
        }
        let mut locations = vec![Vec::<Location>::new(); symbols.len()];
        {
            let read = self.index.read();
            if self.stopped(cancel, revision) {
                return None;
            }
            let mut parents = HashMap::<String, Vec<String>>::new();
            for entry in read.types().iter() {
                if self.stopped(cancel, revision) {
                    return None;
                }
                let symbol = entry.value();
                if !open_uris.contains(&symbol.uri) {
                    parents.insert(
                        symbol.fqn.trim_start_matches('\\').to_ascii_lowercase(),
                        symbol
                            .extends
                            .iter()
                            .chain(&symbol.implements)
                            .chain(&symbol.traits)
                            .map(|name| name.trim_start_matches('\\').to_ascii_lowercase())
                            .collect(),
                    );
                }
            }
            for file_uri in ordered {
                if let Some(open) = sources.get(file_uri) {
                    for symbol in &open.symbols.symbols {
                        if is_class_like_kind(symbol.kind) {
                            parents.insert(
                                symbol.fqn.trim_start_matches('\\').to_ascii_lowercase(),
                                symbol
                                    .extends
                                    .iter()
                                    .chain(&symbol.implements)
                                    .chain(&symbol.traits)
                                    .map(|name| name.trim_start_matches('\\').to_ascii_lowercase())
                                    .collect(),
                            );
                        }
                    }
                }
            }
            let mut ancestors = HashMap::<String, Vec<String>>::new();
            let mut matches = HashMap::<(bool, String, Option<String>), Vec<usize>>::new();
            for file_uri in uris {
                if self.stopped(cancel, revision) {
                    return None;
                }
                let closed;
                let references = if let Some(open) = sources.get(&file_uri) {
                    &open.references
                } else if open_uris.contains(&file_uri) {
                    continue;
                } else {
                    closed = read.file_references().get(&file_uri)?;
                    #[cfg(test)]
                    note_reference_scan(&file_uri);
                    closed.value()
                };
                let Ok(location_uri) = file_uri.parse::<Uri>() else {
                    continue;
                };
                for reference in references {
                    if self.stopped(cancel, revision) {
                        return None;
                    }
                    #[cfg(test)]
                    self.control.hooks.references.fetch_add(1, Ordering::SeqCst);
                    if reference.is_declaration {
                        continue;
                    }
                    let Some((method, key)) =
                        lens_key(reference.target_kind, &reference.target_fqn)
                    else {
                        continue;
                    };
                    let receiver = method
                        .then(|| {
                            reference
                                .receiver
                                .receiver_fqn()
                                .map(|name| name.trim_start_matches('\\').to_ascii_lowercase())
                        })
                        .flatten();
                    let matching = if let Some(found) =
                        matches.get(&(method, key.clone(), receiver.clone()))
                    {
                        found.clone()
                    } else {
                        #[cfg(test)]
                        self.control.hooks.expansions.fetch_add(1, Ordering::SeqCst);
                        let mut found = targets
                            .get(&(method, key.clone()))
                            .cloned()
                            .unwrap_or_default();
                        if let (Some(receiver), Some((_, member))) =
                            (receiver.as_ref(), key.rsplit_once("::"))
                        {
                            if !ancestors.contains_key(receiver) {
                                let mut seen = HashSet::new();
                                let mut pending = vec![receiver.clone()];
                                while let Some(owner) = pending.pop() {
                                    if self.stopped(cancel, revision) {
                                        return None;
                                    }
                                    if seen.insert(owner.clone()) {
                                        pending.extend(
                                            parents.get(&owner).into_iter().flatten().cloned(),
                                        );
                                    }
                                }
                                ancestors.insert(receiver.clone(), seen.into_iter().collect());
                            }
                            for owner in &ancestors[receiver] {
                                if let Some(indices) =
                                    targets.get(&(true, format!("{owner}::{member}")))
                                {
                                    found.extend(indices);
                                }
                            }
                        }
                        found.sort_unstable();
                        found.dedup();
                        matches.insert((method, key, receiver), found.clone());
                        found
                    };
                    for index in matching {
                        locations[index].push(Location {
                            uri: location_uri.clone(),
                            range: range_from_lsp_tuple(reference.range),
                        });
                    }
                }
            }
        }
        let position_index = php_lsp_parser::utf16::Utf16LineIndex::new(&source);
        let document_uri = uri.parse::<Uri>().ok()?;
        let mut lenses = Vec::new();
        for (symbol, mut locations) in symbols.into_iter().zip(locations) {
            if self.stopped(cancel, revision) {
                return None;
            }
            locations.sort_by(|a, b| {
                a.uri
                    .as_str()
                    .cmp(b.uri.as_str())
                    .then(a.range.start.cmp(&b.range.start))
                    .then(a.range.end.cmp(&b.range.end))
            });
            let r = symbol.selection_range;
            let start = Position::new(r.0, position_index.byte_col_to_utf16(r.0, r.1));
            let end = if r.0 == r.2 {
                Position::new(r.2, position_index.byte_col_to_utf16(r.2, r.3))
            } else {
                start
            };
            let count = locations.len();
            lenses.push(CodeLens {
                range: Range { start, end },
                command: Some(Command {
                    title: reference_count_title(count),
                    command: "editor.action.showReferences".into(),
                    arguments: Some(vec![
                        serde_json::to_value(&document_uri).ok()?,
                        serde_json::to_value(start).ok()?,
                        serde_json::to_value(locations).ok()?,
                    ]),
                }),
                data: Some(serde_json::json!({
                    "fqn": symbol.fqn,
                    "kind": call_hierarchy_kind_key(symbol.kind),
                    "references": count,
                })),
            });
        }
        #[cfg(test)]
        if !self
            .control
            .hooks
            .pause(true, &|| self.stopped(cancel, revision))
        {
            return None;
        }
        if self.stopped(cancel, revision) {
            return None;
        }
        if let Some((path, fingerprint)) = disk_target {
            if SourceFingerprint::from_bytes(&std::fs::read(path).ok()?) != fingerprint {
                return None;
            }
        }
        let result = LensResult {
            lenses,
            revision,
            stamps,
            open_uris,
            input: self,
        };
        result.is_current().then_some(result)
    }
}
impl LensResult {
    fn is_current(&self) -> bool {
        let input = &self.input;
        if input.control.stop.is_cancelled()
            || input.index.revision_snapshot() != self.revision
            || input.open_uris() != self.open_uris
        {
            return false;
        }
        for stamp in &self.stamps {
            let Some(parser) = input.open.get(&stamp.uri) else {
                return false;
            };
            if input.versions.get(&stamp.uri).map(|state| *state) != stamp.state
                || input.templates.contains_key(&stamp.uri) != stamp.template
            {
                return false;
            }
            if stamp
                .fingerprint
                .is_some_and(|fp| fp != SourceFingerprint::from_bytes(parser.source().as_bytes()))
            {
                return false;
            }
        }
        input.index.revision_snapshot() == self.revision && input.open_uris() == self.open_uris
    }
}
impl PhpLspBackend {
    pub(crate) async fn lsp_code_lens(
        &self,
        params: CodeLensParams,
    ) -> Result<Option<Vec<CodeLens>>> {
        let uri = params.text_document.uri.as_str().to_string();
        let request = self.request_context_for_uri(&uri).await;
        let index = request.index(&self.index);
        let generation = request.state.generation;
        let input = Input {
            aggregate: Arc::ptr_eq(&index, &self.index),
            index,
            open: self.open_files.clone(),
            templates: self.template_documents.clone(),
            versions: self.document_versions.clone(),
            runtime: self.runtime_state.clone(),
            request,
            control: self.code_lens_control.clone(),
        };
        let control = input.control.clone();
        let worker =
            run_file_io_blocking_cancellable("codeLens references", uri.clone(), move |cancel| {
                #[cfg(test)]
                let _active = input.control.hooks.enter();
                input.build(&uri, &cancel)
            });
        let result =
            tokio::select! {result=worker=>result.ok().flatten(),_=control.stop.cancelled()=>None};
        let Some(result) = result else {
            return Ok(None);
        };
        if self.runtime_state_snapshot().await.generation != generation || !result.is_current() {
            return Ok(None);
        }
        Ok(Some(result.lenses))
    }
}

#[cfg(test)]
type Pause = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);
#[cfg(test)]
#[derive(Default)]
pub(super) struct Hooks {
    pub before: StdMutex<Option<Pause>>,
    pub after: StdMutex<Option<Pause>>,
    pub references: std::sync::atomic::AtomicUsize,
    pub expansions: std::sync::atomic::AtomicUsize,
    pub active: Arc<std::sync::atomic::AtomicUsize>,
}
#[cfg(test)]
impl Hooks {
    fn enter(&self) -> Active {
        self.active.fetch_add(1, Ordering::SeqCst);
        Active(self.active.clone())
    }
    fn pause(&self, after: bool, stopped: &dyn Fn() -> bool) -> bool {
        let slot = if after { &self.after } else { &self.before };
        if let Some((reached, release)) = slot.lock().unwrap().take() {
            reached.send(()).unwrap();
            loop {
                if stopped() {
                    return false;
                }
                match release.recv_timeout(Duration::from_millis(5)) {
                    Ok(()) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(_) => return false,
                }
            }
        }
        !stopped()
    }
}
#[cfg(test)]
struct Active(Arc<std::sync::atomic::AtomicUsize>);
#[cfg(test)]
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
