//! Typed call edges from cached occurrences and authoritative open documents.
use super::*;
use php_lsp_index::workspace::{SourceFingerprint, WorkspaceIndexRevision};
use php_lsp_types::{
    FileSymbols, PhpSymbolKind, SymbolInfo, SymbolReference, SymbolReferenceCallKind,
};

struct OpenCalls {
    symbols: FileSymbols,
    references: Vec<SymbolReference>,
    state: Option<OpenDocumentState>,
    fingerprint: SourceFingerprint,
}

struct ClosedCalls {
    symbols: Arc<FileSymbols>,
    references: Vec<SymbolReference>,
}

pub(super) struct CallEdge {
    pub caller: Arc<SymbolInfo>,
    pub target: Arc<SymbolInfo>,
    pub range: Range,
}

pub(super) struct CallGraph {
    pub edges: Vec<CallEdge>,
    revision: WorkspaceIndexRevision,
    open: HashMap<String, OpenCalls>,
    skipped: HashSet<String>,
    closed: HashSet<String>,
}

struct Resolver<'a> {
    index: &'a WorkspaceIndex,
    open: &'a HashMap<String, OpenCalls>,
    skipped: &'a HashSet<String>,
    cancellation: &'a OperationCancellationToken,
    bindings: HashMap<(PhpSymbolKind, String), Arc<SymbolInfo>>,
    direct: std::cell::RefCell<HashMap<String, Vec<Arc<SymbolInfo>>>>,
}

impl Resolver<'_> {
    fn top_level(&self, fqn: &str, kind: PhpSymbolKind) -> Option<Arc<SymbolInfo>> {
        if let Some(symbol) = self
            .bindings
            .get(&(kind, fqn.trim_start_matches('\\').to_ascii_lowercase()))
        {
            return Some(symbol.clone());
        }
        self.index
            .resolve_fqn_matching_kinds(fqn, &[kind])
            .filter(|symbol| {
                !self.open.contains_key(&symbol.uri) && !self.skipped.contains(&symbol.uri)
            })
    }

    fn class(&self, fqn: &str) -> Option<Arc<SymbolInfo>> {
        [
            PhpSymbolKind::Class,
            PhpSymbolKind::Interface,
            PhpSymbolKind::Trait,
            PhpSymbolKind::Enum,
        ]
        .into_iter()
        .find_map(|kind| self.top_level(fqn, kind))
    }

    fn method(
        &self,
        receiver: &str,
        name: &str,
        visited: &mut HashSet<String>,
    ) -> Option<Arc<SymbolInfo>> {
        if self.cancellation.is_cancelled()
            || !visited.insert(receiver.trim_start_matches('\\').to_ascii_lowercase())
        {
            return None;
        }
        let class = self.class(receiver)?;
        if let Some(method) = self.direct_method(&class, name) {
            return Some(method);
        }
        class
            .traits
            .iter()
            .chain(
                class
                    .template_bindings
                    .iter()
                    .filter(|binding| binding.kind == php_lsp_types::TemplateBindingKind::Mixin)
                    .map(|binding| &binding.target),
            )
            .chain(class.extends.iter())
            .chain(class.implements.iter())
            .find_map(|parent| self.method(parent, name, visited))
    }

    fn direct_method(&self, class: &SymbolInfo, name: &str) -> Option<Arc<SymbolInfo>> {
        let mut direct_cache = self.direct.borrow_mut();
        let direct = direct_cache
            .entry(class.fqn.to_ascii_lowercase())
            .or_insert_with(|| {
                if let Some(open) = self.open.get(&class.uri) {
                    open.symbols
                        .symbols
                        .iter()
                        .filter(|symbol| {
                            symbol.kind == PhpSymbolKind::Method
                                && symbol
                                    .parent_fqn
                                    .as_ref()
                                    .is_some_and(|parent| parent.eq_ignore_ascii_case(&class.fqn))
                        })
                        .cloned()
                        .map(Arc::new)
                        .collect::<Vec<_>>()
                } else {
                    self.index.get_direct_members(&class.fqn)
                }
            });
        let method = direct
            .iter()
            .find(|symbol| {
                symbol.kind == PhpSymbolKind::Method && symbol.name.eq_ignore_ascii_case(name)
            })
            .cloned();
        drop(direct_cache);
        method
    }

    // Scope-local declarations and imported trait methods occupy private binding;
    // inherited private declarations do not become private members of a child.
    fn local_method(
        &self,
        scope: &str,
        name: &str,
        visited: &mut HashSet<String>,
    ) -> Option<Arc<SymbolInfo>> {
        if self.cancellation.is_cancelled() || !visited.insert(scope.to_ascii_lowercase()) {
            return None;
        }
        let class = self.class(scope)?;
        self.direct_method(&class, name).or_else(|| {
            class
                .traits
                .iter()
                .find_map(|parent| self.local_method(parent, name, visited))
        })
    }

    fn related(&self, receiver: &str, scope: &str, visited: &mut HashSet<String>) -> bool {
        if self.cancellation.is_cancelled() || !visited.insert(receiver.to_ascii_lowercase()) {
            return false;
        }
        if receiver
            .trim_start_matches('\\')
            .eq_ignore_ascii_case(scope.trim_start_matches('\\'))
        {
            return true;
        }
        self.class(receiver).is_some_and(|class| {
            class
                .traits
                .iter()
                .chain(class.extends.iter())
                .chain(class.implements.iter())
                .any(|parent| self.related(parent, scope, visited))
        })
    }

    fn trait_caller_scopes(&self, caller: &SymbolInfo) -> Vec<String> {
        let mut classes: HashMap<String, Arc<SymbolInfo>> = self
            .bindings
            .values()
            .filter(|symbol| matches!(symbol.kind, PhpSymbolKind::Class | PhpSymbolKind::Enum))
            .map(|symbol| (symbol.fqn.to_ascii_lowercase(), symbol.clone()))
            .collect();
        for entry in self.index.read().types().iter() {
            let symbol = entry.value();
            if matches!(symbol.kind, PhpSymbolKind::Class | PhpSymbolKind::Enum)
                && !self.open.contains_key(&symbol.uri)
                && !self.skipped.contains(&symbol.uri)
            {
                classes
                    .entry(symbol.fqn.to_ascii_lowercase())
                    .or_insert_with(|| symbol.clone());
            }
        }
        classes
            .into_values()
            .filter(|class| {
                self.local_method(&class.fqn, &caller.name, &mut HashSet::new())
                    .is_some_and(|method| {
                        php_lsp_types::symbol_fqn_eq(
                            &method.fqn,
                            &caller.fqn,
                            PhpSymbolKind::Method,
                        )
                    })
            })
            .map(|class| class.fqn.clone())
            .collect()
    }

    fn call(&self, reference: &SymbolReference, caller: &SymbolInfo) -> Option<Arc<SymbolInfo>> {
        if reference.is_declaration || reference.is_import_target {
            return None;
        }
        let scopes = caller
            .parent_fqn
            .as_deref()
            .filter(|scope| {
                self.class(scope)
                    .is_some_and(|class| class.kind == PhpSymbolKind::Trait)
            })
            .map(|_| self.trait_caller_scopes(caller))
            .unwrap_or_default();
        let current = if scopes.len() == 1 {
            Some(scopes[0].as_str())
        } else {
            caller.parent_fqn.as_deref()
        };
        match reference.call_site?.kind {
            SymbolReferenceCallKind::Function => self
                .top_level(&reference.target_fqn, PhpSymbolKind::Function)
                .or_else(|| {
                    reference
                        .allows_global_fallback
                        .then(|| {
                            self.top_level(
                                reference.target_fqn.rsplit('\\').next()?,
                                PhpSymbolKind::Function,
                            )
                        })
                        .flatten()
                }),
            SymbolReferenceCallKind::Method => {
                let receiver = reference.receiver.receiver_fqn()?;
                let (_, name) = reference.target_fqn.rsplit_once("::")?;
                if matches!(
                    reference.receiver,
                    php_lsp_types::SymbolReferenceReceiver::ResolvedType { .. }
                ) {
                    if let Some(scope) = caller.parent_fqn.as_deref() {
                        if scopes.len() > 1
                            && scopes.iter().any(|scope| {
                                self.related(receiver, scope, &mut HashSet::new())
                                    && self
                                        .local_method(scope, name, &mut HashSet::new())
                                        .is_some_and(|method| {
                                            method.visibility == php_lsp_types::Visibility::Private
                                        })
                            })
                        {
                            // The source trait does not identify which lexical copy executes.
                            return None;
                        }
                        let scope = current.unwrap_or(scope);
                        if self.related(receiver, scope, &mut HashSet::new()) {
                            if let Some(private) = self
                                .local_method(scope, name, &mut HashSet::new())
                                .filter(|method| {
                                    method.visibility == php_lsp_types::Visibility::Private
                                })
                            {
                                return Some(private);
                            }
                        }
                    }
                }
                self.method(receiver, name, &mut HashSet::new())
                    .filter(|method| {
                        self.accessible(
                            method,
                            receiver,
                            current,
                            matches!(
                                reference.receiver,
                                php_lsp_types::SymbolReferenceReceiver::ResolvedType { .. }
                            ),
                        )
                    })
            }
            SymbolReferenceCallKind::Constructor => self
                .method(&reference.target_fqn, "__construct", &mut HashSet::new())
                .filter(|method| self.accessible(method, &reference.target_fqn, current, false)),
        }
    }
    fn accessible(
        &self,
        method: &SymbolInfo,
        receiver: &str,
        current: Option<&str>,
        object: bool,
    ) -> bool {
        if method.visibility == php_lsp_types::Visibility::Public {
            return true;
        }
        let mut file = FileSymbols::default();
        for open in self.open.values() {
            file.symbols.extend(open.symbols.symbols.iter().cloned());
        }
        let lookup = if object {
            php_lsp_completion::visibility::MemberLookup::Object
        } else {
            php_lsp_completion::visibility::MemberLookup::Class
        };
        php_lsp_completion::visibility::MemberVisibility::new(
            self.index, &file, receiver, current, lookup,
        )
        .is_visible(method)
    }
}

impl CallGraph {
    pub(super) fn is_current(&self, backend: &PhpLspBackend, index: &WorkspaceIndex) -> bool {
        if index.revision_snapshot() != self.revision {
            return false;
        }
        if self.closed.iter().any(|uri| {
            backend.open_files.contains_key(uri) || backend.template_documents.contains_key(uri)
        }) {
            return false;
        }
        for (uri, captured) in &self.open {
            let Some(parser) = backend.open_files.get(uri) else {
                return false;
            };
            if backend.template_documents.contains_key(uri) {
                return false;
            }
            let state = backend.document_versions.get(uri).map(|state| *state);
            if state != captured.state {
                return false;
            }
            if state.is_none()
                && SourceFingerprint::from_bytes(parser.source().as_bytes()) != captured.fingerprint
            {
                return false;
            }
        }
        self.skipped
            .iter()
            .all(|uri| backend.template_documents.contains_key(uri))
    }
}

impl PhpLspBackend {
    pub(super) async fn call_graph(
        &self,
        request: &WorkspaceRequestContext,
        index: Arc<WorkspaceIndex>,
        outgoing: Option<Arc<SymbolInfo>>,
        incoming: Option<Arc<SymbolInfo>>,
    ) -> Option<CallGraph> {
        let revision = index.revision_snapshot();
        let (uris, open_uris) = self.reference_scan_uris(&index, Some(request));
        let mut open = HashMap::new();
        let mut skipped = HashSet::new();
        // Never hold the index publication barrier while acquiring a document lock.
        for uri in open_uris {
            let Some(snapshot) = self.open_document_snapshot(&uri) else {
                if self.open_files.contains_key(&uri) {
                    skipped.insert(uri);
                }
                continue;
            };
            if snapshot.template_document.is_some() {
                skipped.insert(uri);
                continue;
            }
            let source_index = index.clone();
            let captured = run_file_io_blocking_cancellable("callHierarchy open references", uri.clone(), move |cancel| {
                if cancel.is_cancelled() { return None; }
                let member = |owner: &str, name: &str| {
                    if cancel.is_cancelled() { None } else { resolve_member_type_from_index(&source_index, owner, name) }
                };
                let callable = |ctx: CallableParameterContext<'_>| {
                    if cancel.is_cancelled() { None } else { resolve_callable_parameter_type_from_index(&source_index, &snapshot.file_symbols, ctx) }
                };
                let references = php_lsp_parser::references::collect_symbol_references_in_file_with_resolvers(
                    &snapshot.tree,&snapshot.source,&snapshot.file_symbols,Some(&member),Some(&callable));
                if cancel.is_cancelled() { return None; }
                Some(OpenCalls { symbols: snapshot.file_symbols, references, state: snapshot.document_state,
                    fingerprint: SourceFingerprint::from_bytes(snapshot.source.as_bytes()) })
            }).await.ok().flatten()?;
            open.insert(uri, captured);
        }
        // Cached unresolved receivers can depend on another indexed file. Enrich
        // only those candidate files; fully resolved closed inputs need no parsing.
        let candidates =
            {
                let published = index.read();
                let refs = published.file_references();
                let symbols = published.file_symbols();
                let fingerprints = published.source_fingerprints();
                uris.iter()
                    .filter(|uri| !open.contains_key(*uri) && !skipped.contains(*uri))
                    .filter(|uri| outgoing.as_ref().is_none_or(|caller| &caller.uri == *uri))
                    .filter(|uri| {
                        refs.get(*uri).is_some_and(|references| {
                            references.iter().any(|reference| {
                                reference.call_site.is_some()
                                    && matches!(
                                        reference.receiver,
                                        php_lsp_types::SymbolReferenceReceiver::Unresolved
                                    )
                                    && incoming.as_ref().is_none_or(|target| {
                                        reference.target_fqn.rsplit("::").next().is_some_and(
                                            |name| name.eq_ignore_ascii_case(&target.name),
                                        )
                                    })
                            })
                        })
                    })
                    .filter_map(|uri| {
                        Some((
                            uri.clone(),
                            symbols.get(uri)?.value().clone(),
                            fingerprints
                                .get(uri)
                                .map(|fingerprint| *fingerprint.value()),
                        ))
                    })
                    .collect::<Vec<_>>()
            };
        let mut enriched = HashMap::new();
        for (uri, symbols, fingerprint) in candidates {
            let Some(path) = uri_to_path(&uri) else {
                continue;
            };
            let source_index = index.clone();
            let value=run_file_io_blocking_cancellable("callHierarchy unresolved receivers",uri.clone(),move |cancel| {
                if cancel.is_cancelled() { return None; }
                let bytes=std::fs::read(path).ok()?;
                if fingerprint.is_some_and(|expected| expected!=SourceFingerprint::from_bytes(&bytes)) || cancel.is_cancelled() { return None; }
                let source=String::from_utf8(bytes).ok()?;
                let mut parser=FileParser::new();parser.parse_full(&source);
                let member=|owner:&str,name:&str| if cancel.is_cancelled(){None}else{resolve_member_type_from_index(&source_index,owner,name)};
                let callable=|ctx:CallableParameterContext<'_>| if cancel.is_cancelled(){None}else{resolve_callable_parameter_type_from_index(&source_index,&symbols,ctx)};
                let references=php_lsp_parser::references::collect_symbol_references_in_file_with_resolvers(parser.tree()?,&source,&symbols,Some(&member),Some(&callable));
                (!cancel.is_cancelled()).then_some(ClosedCalls{symbols,references})
            }).await.ok()?;
            if let Some(value) = value {
                enriched.insert(uri, value);
            }
        }
        let worker_index = index.clone();
        let graph =
            run_file_io_blocking_cancellable("callHierarchy graph", String::new(), move |cancel| {
                let published = worker_index.read();
                if cancel.is_cancelled() || worker_index.revision_snapshot() != revision {
                    return None;
                }
                let mut edges = Vec::new();
                let mut closed = HashSet::new();
                let symbols_map = published.file_symbols();
                let references_map = published.file_references();
                {
                    let resolver = Resolver {
                        index: &worker_index,
                        open: &open,
                        skipped: &skipped,
                        cancellation: &cancel,
                        bindings: {
                            let mut bindings = HashMap::new();
                            let mut uris: Vec<_> = open.keys().collect();
                            uris.sort();
                            for uri in uris {
                                for symbol in &open[uri].symbols.symbols {
                                    if symbol.kind == PhpSymbolKind::Function
                                        || is_type_hierarchy_symbol_kind(symbol.kind)
                                    {
                                        bindings
                                            .entry((
                                                symbol.kind,
                                                symbol
                                                    .fqn
                                                    .trim_start_matches('\\')
                                                    .to_ascii_lowercase(),
                                            ))
                                            .or_insert_with(|| Arc::new(symbol.clone()));
                                    }
                                }
                            }
                            bindings
                        },
                        direct: Default::default(),
                    };
                    for uri in uris {
                        if cancel.is_cancelled() {
                            return None;
                        }
                        if skipped.contains(&uri) {
                            continue;
                        }
                        if outgoing.as_ref().is_some_and(|caller| caller.uri != uri) {
                            continue;
                        }
                        let closed_symbols;
                        let closed_refs;
                        let (symbols, references) = if let Some(open) = open.get(&uri) {
                            (&open.symbols, &open.references)
                        } else if let Some(enriched) = enriched.get(&uri) {
                            (enriched.symbols.as_ref(), &enriched.references)
                        } else {
                            let Some(symbols) = symbols_map.get(&uri) else {
                                continue;
                            };
                            closed_symbols = symbols.value().clone();
                            let Some(references) = references_map.get(&uri) else {
                                continue;
                            };
                            closed_refs = references;
                            closed.insert(uri.clone());
                            (closed_symbols.as_ref(), closed_refs.value())
                        };
                        for reference in references {
                            if cancel.is_cancelled() {
                                return None;
                            }
                            let Some(range) =
                                reference.call_site.and_then(|call| call.caller_range)
                            else {
                                continue;
                            };
                            let Some(caller) = symbols.symbols.iter().find(|symbol| {
                                is_call_hierarchy_symbol_kind(symbol.kind) && symbol.range == range
                            }) else {
                                continue;
                            };
                            if outgoing.as_ref().is_some_and(|requested| {
                                requested.uri != caller.uri
                                    || requested.range != caller.range
                                    || requested.kind != caller.kind
                            }) {
                                continue;
                            }
                            let Some(target) = resolver.call(reference, caller) else {
                                continue;
                            };
                            edges.push(CallEdge {
                                caller: Arc::new(caller.clone()),
                                target,
                                range: range_from_lsp_tuple(reference.range),
                            });
                        }
                    }
                }
                closed.extend(
                    published
                        .file_symbols()
                        .iter()
                        .map(|entry| entry.key().clone())
                        .filter(|uri| !open.contains_key(uri) && !skipped.contains(uri)),
                );
                drop(published);
                Some(CallGraph {
                    edges,
                    revision,
                    open,
                    skipped,
                    closed,
                })
            })
            .await
            .ok()
            .flatten()?;
        graph.is_current(self, &index).then_some(graph)
    }
}

#[cfg(test)]
#[path = "call_hierarchy_graph_tests.rs"]
mod tests;
