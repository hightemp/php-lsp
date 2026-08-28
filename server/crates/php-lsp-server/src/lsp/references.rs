//! References LSP handlers extracted from `server.rs`.

use super::super::*;
use super::hierarchy::call_hierarchy_kind_key;

fn is_code_lens_symbol_kind(kind: php_lsp_types::PhpSymbolKind) -> bool {
    matches!(
        kind,
        php_lsp_types::PhpSymbolKind::Class
            | php_lsp_types::PhpSymbolKind::Interface
            | php_lsp_types::PhpSymbolKind::Trait
            | php_lsp_types::PhpSymbolKind::Enum
            | php_lsp_types::PhpSymbolKind::Method
    )
}

fn reference_count_title(count: usize) -> String {
    if count == 1 {
        "1 reference".to_string()
    } else {
        format!("{} references", count)
    }
}

enum OpenReferenceSnapshot {
    Ordinary(Vec<php_lsp_types::SymbolReference>),
    Template,
}

enum OpenSymbolAuthority {
    Ordinary(php_lsp_types::FileSymbols),
    NonOrdinary,
    Closed,
}

struct OpenReferenceOverlay<'a> {
    symbol_cache: &'a mut HashMap<String, OpenSymbolAuthority>,
    qualified_targets: &'a HashSet<(php_lsp_types::PhpSymbolKind, String)>,
}

fn reference_target_key(fqn: &str, kind: php_lsp_types::PhpSymbolKind) -> String {
    match kind {
        php_lsp_types::PhpSymbolKind::Function => fqn.trim_start_matches('\\').to_ascii_lowercase(),
        php_lsp_types::PhpSymbolKind::GlobalConstant => php_lsp_types::global_constant_fqn_key(fqn),
        _ => fqn.trim_start_matches('\\').to_string(),
    }
}

pub(super) fn local_symbol_for_reference(
    file_symbols: &php_lsp_types::FileSymbols,
    symbol: &SymbolAtPosition,
) -> Option<Arc<php_lsp_types::SymbolInfo>> {
    let matches_fqn = |candidate: &&php_lsp_types::SymbolInfo, fqn: &str| {
        php_lsp_types::symbol_fqn_eq(&candidate.fqn, fqn, candidate.kind)
    };

    if symbol.ref_kind == RefKind::Constructor {
        if let Some(constructor) = file_symbols.symbols.iter().find(|candidate| {
            candidate.kind == php_lsp_types::PhpSymbolKind::Method
                && matches_fqn(candidate, &symbol.fqn)
        }) {
            return Some(Arc::new(constructor.clone()));
        }
        let class_fqn = symbol
            .fqn
            .strip_suffix("::__construct")
            .unwrap_or(&symbol.fqn);
        return file_symbols
            .symbols
            .iter()
            .find(|candidate| {
                matches!(
                    candidate.kind,
                    php_lsp_types::PhpSymbolKind::Class
                        | php_lsp_types::PhpSymbolKind::Interface
                        | php_lsp_types::PhpSymbolKind::Trait
                        | php_lsp_types::PhpSymbolKind::Enum
                ) && matches_fqn(candidate, class_fqn)
            })
            .cloned()
            .map(Arc::new);
    }

    let exact = file_symbols
        .symbols
        .iter()
        .find(|candidate| {
            let kind_matches = match symbol.ref_kind {
                RefKind::ClassName => matches!(
                    candidate.kind,
                    php_lsp_types::PhpSymbolKind::Class
                        | php_lsp_types::PhpSymbolKind::Interface
                        | php_lsp_types::PhpSymbolKind::Trait
                        | php_lsp_types::PhpSymbolKind::Enum
                ),
                RefKind::FunctionCall => candidate.kind == php_lsp_types::PhpSymbolKind::Function,
                RefKind::MethodCall => candidate.kind == php_lsp_types::PhpSymbolKind::Method,
                RefKind::PropertyAccess | RefKind::StaticPropertyAccess => {
                    candidate.kind == php_lsp_types::PhpSymbolKind::Property
                }
                RefKind::ClassConstant => matches!(
                    candidate.kind,
                    php_lsp_types::PhpSymbolKind::ClassConstant
                        | php_lsp_types::PhpSymbolKind::EnumCase
                ),
                RefKind::GlobalConstant => {
                    candidate.kind == php_lsp_types::PhpSymbolKind::GlobalConstant
                }
                RefKind::NamespaceName => candidate.kind == php_lsp_types::PhpSymbolKind::Namespace,
                RefKind::Variable | RefKind::Constructor | RefKind::Unknown => false,
            };
            kind_matches && matches_fqn(candidate, &symbol.fqn)
        })
        .cloned()
        .map(Arc::new);
    if exact.is_some() {
        return exact;
    }

    if symbol.allows_global_fallback {
        let global = file_symbols.symbols.iter().find(|candidate| {
            !candidate.fqn.trim_start_matches('\\').contains('\\')
                && match symbol.ref_kind {
                    RefKind::FunctionCall => {
                        candidate.kind == php_lsp_types::PhpSymbolKind::Function
                            && candidate.name.eq_ignore_ascii_case(&symbol.name)
                    }
                    RefKind::GlobalConstant => {
                        candidate.kind == php_lsp_types::PhpSymbolKind::GlobalConstant
                            && candidate.name == symbol.name
                    }
                    _ => false,
                }
        });
        return global.cloned().map(Arc::new);
    }

    None
}

impl PhpLspBackend {
    fn open_symbol_authority(&self, uri: &str) -> OpenSymbolAuthority {
        if let Some(snapshot) = self.open_document_snapshot(uri) {
            if snapshot.template_document.is_none() {
                OpenSymbolAuthority::Ordinary(snapshot.file_symbols)
            } else {
                OpenSymbolAuthority::NonOrdinary
            }
        } else if self.open_files.contains_key(uri) || self.template_documents.contains_key(uri) {
            OpenSymbolAuthority::NonOrdinary
        } else {
            OpenSymbolAuthority::Closed
        }
    }

    fn open_reference_snapshot(&self, uri: &str) -> Option<OpenReferenceSnapshot> {
        let snapshot = self.open_document_snapshot(uri)?;
        if snapshot.template_document.is_some() {
            return Some(OpenReferenceSnapshot::Template);
        }
        Some(OpenReferenceSnapshot::Ordinary(
            collect_symbol_references_in_file(
                &snapshot.tree,
                &snapshot.source,
                &snapshot.file_symbols,
            ),
        ))
    }

    fn reference_snapshot_for_scan(
        &self,
        index: &WorkspaceIndex,
        uri: &str,
    ) -> Option<Vec<php_lsp_types::SymbolReference>> {
        if let Some(snapshot) = self.open_reference_snapshot(uri) {
            return match snapshot {
                OpenReferenceSnapshot::Ordinary(references) => Some(references),
                OpenReferenceSnapshot::Template => None,
            };
        }

        let indexed_references = index
            .file_references
            .get(uri)
            .map(|entry| entry.value().clone());

        // Recheck after cloning the closed-file data. If the document opened
        // concurrently, the exact open snapshot always takes precedence.
        if let Some(snapshot) = self.open_reference_snapshot(uri) {
            return match snapshot {
                OpenReferenceSnapshot::Ordinary(references) => Some(references),
                OpenReferenceSnapshot::Template => None,
            };
        }
        if self.open_files.contains_key(uri) || self.template_documents.contains_key(uri) {
            return None;
        }

        indexed_references
    }

    fn reference_matches_with_open_overlay(
        &self,
        index: &WorkspaceIndex,
        reference: &php_lsp_types::SymbolReference,
        target_fqn: &str,
        target_kind: php_lsp_types::PhpSymbolKind,
        include_declaration: bool,
        overlay: &mut OpenReferenceOverlay<'_>,
    ) -> bool {
        if reference.is_declaration && !include_declaration {
            return false;
        }
        let is_global_fallback_candidate = reference.allows_global_fallback
            && matches!(
                target_kind,
                php_lsp_types::PhpSymbolKind::Function
                    | php_lsp_types::PhpSymbolKind::GlobalConstant
            )
            && reference
                .target_fqn
                .rsplit_once('\\')
                .is_some_and(|(_, short_name)| {
                    php_lsp_types::symbol_fqn_eq(short_name, target_fqn, target_kind)
                });
        if is_global_fallback_candidate
            && overlay.qualified_targets.contains(&(
                target_kind,
                reference_target_key(&reference.target_fqn, target_kind),
            ))
        {
            return false;
        }
        if symbol_reference_matches(
            index,
            reference,
            target_fqn,
            target_kind,
            include_declaration,
        ) {
            return true;
        }
        if !is_global_fallback_candidate {
            return false;
        }

        let target_key = reference_target_key(&reference.target_fqn, target_kind);
        let qualified_target_exists = overlay
            .qualified_targets
            .contains(&(target_kind, target_key))
            || match index.resolve_fqn_matching_kinds(&reference.target_fqn, &[target_kind]) {
                None => false,
                Some(resolved) => {
                    let authority = overlay
                        .symbol_cache
                        .entry(resolved.uri.clone())
                        .or_insert_with(|| self.open_symbol_authority(&resolved.uri));
                    match authority {
                        OpenSymbolAuthority::Ordinary(file_symbols) => {
                            file_symbols.symbols.iter().any(|symbol| {
                                symbol.kind == target_kind
                                    && php_lsp_types::symbol_fqn_eq(
                                        &symbol.fqn,
                                        &reference.target_fqn,
                                        symbol.kind,
                                    )
                            })
                        }
                        OpenSymbolAuthority::NonOrdinary => false,
                        OpenSymbolAuthority::Closed => true,
                    }
                }
            };
        !qualified_target_exists
    }

    pub(in crate::server) fn reference_scan_matches(
        &self,
        index: &WorkspaceIndex,
        request: Option<&WorkspaceRequestContext>,
        target_fqn: &str,
        target_kind: php_lsp_types::PhpSymbolKind,
        include_declaration: bool,
    ) -> Vec<(String, Vec<php_lsp_types::SymbolReference>)> {
        let mut uris: HashSet<String> = index
            .file_references
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        let open_uris: Vec<String> = self
            .open_files
            .iter()
            .filter(|entry| {
                std::ptr::eq(index, self.index.as_ref())
                    || index.file_symbols.contains_key(entry.key())
                    || request.is_none_or(|request| {
                        if let Some(config) = request.workspace.as_ref() {
                            return uri_to_path(entry.key()).is_some_and(|path| {
                                workspace_config_for_path_from_configs(
                                    std::slice::from_ref(config),
                                    &path,
                                )
                                .is_some()
                            });
                        }
                        workspace_config_for_uri_from_configs(&request.state.configs, entry.key())
                            .is_none()
                    })
            })
            .map(|entry| entry.key().clone())
            .collect();
        uris.extend(open_uris.iter().cloned());

        let mut uris: Vec<_> = uris.into_iter().collect();
        uris.sort();
        let mut open_symbol_cache = HashMap::new();
        let mut open_qualified_targets = HashSet::new();
        for uri in open_uris {
            let authority = self.open_symbol_authority(&uri);
            if let OpenSymbolAuthority::Ordinary(file_symbols) = &authority {
                open_qualified_targets.extend(
                    file_symbols
                        .symbols
                        .iter()
                        .filter(|symbol| {
                            matches!(
                                symbol.kind,
                                php_lsp_types::PhpSymbolKind::Function
                                    | php_lsp_types::PhpSymbolKind::GlobalConstant
                            )
                        })
                        .map(|symbol| {
                            (symbol.kind, reference_target_key(&symbol.fqn, symbol.kind))
                        }),
                );
            }
            open_symbol_cache.insert(uri, authority);
        }
        uris.into_iter()
            .filter_map(|uri| {
                let mut references = self.reference_snapshot_for_scan(index, &uri)?;
                let mut overlay = OpenReferenceOverlay {
                    symbol_cache: &mut open_symbol_cache,
                    qualified_targets: &open_qualified_targets,
                };
                references.retain(|reference| {
                    self.reference_matches_with_open_overlay(
                        index,
                        reference,
                        target_fqn,
                        target_kind,
                        include_declaration,
                        &mut overlay,
                    )
                });
                Some((uri, references))
            })
            .collect()
    }

    pub(crate) async fn lsp_document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let uri_str = params
            .text_document_position_params
            .text_document
            .uri
            .as_str()
            .to_string();
        let request = self.request_context_for_uri(&uri_str).await;
        let request_index = request.index(&self.index);
        let pos = params.text_document_position_params.position;

        let parser = match self.open_files.get(&uri_str) {
            Some(parser) => parser,
            None => return Ok(None),
        };
        let tree = match parser.tree() {
            Some(tree) => tree,
            None => return Ok(None),
        };
        let source = parser.source();
        let byte_col = utf16_col_to_byte(&source, pos.line, pos.character);
        let file_symbols = extract_file_symbols(tree, &source, &uri_str);
        let sym = match symbol_at_position(tree, &source, pos.line, byte_col, &file_symbols) {
            Some(sym) => sym,
            None => return Ok(None),
        };

        if sym.ref_kind == RefKind::Variable {
            let highlights: Vec<DocumentHighlight> =
                find_variable_references_at_position(tree, &source, pos.line, byte_col, true)
                    .into_iter()
                    .map(|reference| document_highlight_from_range(&source, reference.range, true))
                    .collect();
            return if highlights.is_empty() {
                Ok(None)
            } else {
                Ok(Some(highlights))
            };
        }

        let Some(kind) = php_symbol_kind_for_ref_kind(sym.ref_kind) else {
            return Ok(None);
        };
        let local_symbol = local_symbol_for_reference(&file_symbols, &sym);
        let resolved = if local_symbol.is_some() {
            local_symbol
        } else {
            resolve_fqn_with_ref_kind(
                &request_index,
                &sym.fqn,
                sym.ref_kind,
                sym.allows_global_fallback,
            )
            .filter(|symbol| symbol.uri != uri_str)
        };
        let (target_fqn, target_kind) = if let Some(resolved) = resolved {
            (resolved.fqn.clone(), resolved.kind)
        } else {
            (sym.fqn.clone(), kind)
        };

        if matches!(
            target_kind,
            php_lsp_types::PhpSymbolKind::Function | php_lsp_types::PhpSymbolKind::GlobalConstant
        ) {
            let highlights: Vec<DocumentHighlight> = self
                .references_for_file(&request_index, &uri_str, &target_fqn, target_kind, true)
                .into_iter()
                .map(|reference| DocumentHighlight {
                    range: range_from_lsp_tuple(reference.range),
                    kind: Some(DocumentHighlightKind::TEXT),
                })
                .collect();
            return if highlights.is_empty() {
                Ok(None)
            } else {
                Ok(Some(highlights))
            };
        }

        let read_write_capable = target_kind == php_lsp_types::PhpSymbolKind::Property;

        let highlights: Vec<DocumentHighlight> =
            find_references_in_file(tree, &source, &file_symbols, &target_fqn, target_kind, true)
                .into_iter()
                .map(|reference| {
                    document_highlight_from_range(&source, reference.range, read_write_capable)
                })
                .collect();

        if highlights.is_empty() {
            Ok(None)
        } else {
            Ok(Some(highlights))
        }
    }

    pub(crate) async fn lsp_references(
        &self,
        params: ReferenceParams,
    ) -> Result<Option<Vec<Location>>> {
        let uri_str = params
            .text_document_position
            .text_document
            .uri
            .as_str()
            .to_string();
        let request = self.request_context_for_uri(&uri_str).await;
        let request_index = request.index(&self.index);
        let pos = params.text_document_position.position;
        let include_declaration = params.context.include_declaration;

        // Resolve symbol under cursor to get FQN
        let (target_fqn, target_kind) = {
            let parser = match self.open_files.get(&uri_str) {
                Some(p) => p,
                None => return Ok(None),
            };
            let tree = match parser.tree() {
                Some(t) => t,
                None => return Ok(None),
            };
            let source = parser.source();
            let byte_col = utf16_col_to_byte(&source, pos.line, pos.character);
            let file_symbols = extract_file_symbols(tree, &source, &uri_str);

            let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
                resolve_member_type_from_index(&request_index, class_fqn, member_name)
            };
            let callable_param_resolver = |ctx: CallableParameterContext<'_>| {
                resolve_callable_parameter_type_from_index(&request_index, &file_symbols, ctx)
            };

            match symbol_at_position_with_resolvers(
                tree,
                &source,
                pos.line,
                byte_col,
                &file_symbols,
                Some(&resolver),
                Some(&callable_param_resolver),
            ) {
                Some(sym) => {
                    if sym.ref_kind == RefKind::Variable {
                        let refs = find_variable_references_at_position(
                            tree,
                            &source,
                            pos.line,
                            byte_col,
                            include_declaration,
                        );
                        if refs.is_empty() {
                            return Ok(None);
                        }
                        let uri = match uri_str.parse::<Uri>() {
                            Ok(u) => u,
                            Err(_) => return Ok(None),
                        };
                        let locations: Vec<Location> = refs
                            .into_iter()
                            .map(|r| {
                                let rng = range_byte_to_utf16(&source, r.range);
                                Location {
                                    uri: uri.clone(),
                                    range: Range {
                                        start: Position::new(rng.0, rng.1),
                                        end: Position::new(rng.2, rng.3),
                                    },
                                }
                            })
                            .collect();
                        return Ok(Some(locations));
                    }

                    let kind = match sym.ref_kind {
                        RefKind::ClassName | RefKind::Constructor => {
                            php_lsp_types::PhpSymbolKind::Class
                        }
                        RefKind::FunctionCall => php_lsp_types::PhpSymbolKind::Function,
                        RefKind::MethodCall => php_lsp_types::PhpSymbolKind::Method,
                        RefKind::PropertyAccess | RefKind::StaticPropertyAccess => {
                            php_lsp_types::PhpSymbolKind::Property
                        }
                        RefKind::ClassConstant => php_lsp_types::PhpSymbolKind::ClassConstant,
                        RefKind::GlobalConstant => php_lsp_types::PhpSymbolKind::GlobalConstant,
                        RefKind::Variable => return Ok(None),
                        RefKind::NamespaceName | RefKind::Unknown => return Ok(None),
                    };

                    // Prefer this exact parser generation before the staged
                    // global index catches up.
                    let local_symbol = local_symbol_for_reference(&file_symbols, &sym);
                    let resolved = if local_symbol.is_some() {
                        local_symbol
                    } else {
                        resolve_fqn_with_ref_kind(
                            &request_index,
                            &sym.fqn,
                            sym.ref_kind,
                            sym.allows_global_fallback,
                        )
                        .filter(|symbol| symbol.uri != uri_str)
                    };
                    if let Some(resolved) = resolved {
                        (resolved.fqn.clone(), resolved.kind)
                    } else {
                        (sym.fqn.clone(), kind)
                    }
                }
                None => return Ok(None),
            }
        };

        // Search all indexed files for references
        let mut locations = Vec::new();
        let scanned_files = self.reference_scan_matches(
            &request_index,
            Some(&request),
            &target_fqn,
            target_kind,
            include_declaration,
        );

        for (scanned_file_count, (file_uri, references)) in scanned_files.into_iter().enumerate() {
            cooperative_heavy_request_yield(scanned_file_count).await;

            for r in references {
                if let Ok(uri) = file_uri.parse::<Uri>() {
                    locations.push(Location {
                        uri,
                        range: range_from_lsp_tuple(r.range),
                    });
                }
            }
        }

        if locations.is_empty() {
            Ok(None)
        } else {
            Ok(Some(locations))
        }
    }

    pub(crate) async fn lsp_code_lens(
        &self,
        params: CodeLensParams,
    ) -> Result<Option<Vec<CodeLens>>> {
        let uri_str = params.text_document.uri.as_str().to_string();
        let request = self.request_context_for_uri(&uri_str).await;
        let request_index = request.index(&self.index);
        let document_uri = match uri_str.parse::<Uri>() {
            Ok(uri) => uri,
            Err(_) => return Ok(None),
        };

        let (file_symbols, source) = if let Some(parser) = self.open_files.get(&uri_str) {
            let Some(tree) = parser.tree() else {
                return Ok(None);
            };
            let source = parser.source();
            (extract_file_symbols(tree, &source, &uri_str), source)
        } else {
            let Some(file_symbols) = request_index
                .file_symbols
                .get(&uri_str)
                .map(|entry| entry.value().as_ref().clone())
            else {
                return Ok(None);
            };
            let Some(path) = uri_to_path(&uri_str) else {
                return Ok(None);
            };
            let Ok(source) = read_file_to_string_blocking(path, "codeLens source read").await
            else {
                return Ok(None);
            };
            (file_symbols, source)
        };

        let mut lenses = Vec::new();
        for symbol in file_symbols
            .symbols
            .iter()
            .filter(|symbol| is_code_lens_symbol_kind(symbol.kind))
        {
            let locations = self.reference_locations_for_symbol(
                &request_index,
                Some(&request),
                &symbol.fqn,
                symbol.kind,
                false,
            );
            let range_tuple = range_byte_to_utf16(&source, symbol.selection_range);
            let start = Position::new(range_tuple.0, range_tuple.1);
            let end = if range_tuple.0 == range_tuple.2 {
                Position::new(range_tuple.2, range_tuple.3)
            } else {
                start
            };

            let arguments = match (
                serde_json::to_value(document_uri.clone()),
                serde_json::to_value(start),
                serde_json::to_value(&locations),
            ) {
                (Ok(uri), Ok(position), Ok(locations)) => Some(vec![uri, position, locations]),
                _ => None,
            };

            lenses.push(CodeLens {
                range: Range { start, end },
                command: Some(Command {
                    title: reference_count_title(locations.len()),
                    command: "editor.action.showReferences".to_string(),
                    arguments,
                }),
                data: Some(serde_json::json!({
                    "fqn": symbol.fqn,
                    "kind": call_hierarchy_kind_key(symbol.kind),
                    "references": locations.len(),
                })),
            });
        }

        if lenses.is_empty() {
            Ok(None)
        } else {
            Ok(Some(lenses))
        }
    }
}

pub(in crate::server) fn line_byte_col_to_byte(
    source: &str,
    line: u32,
    byte_col: u32,
) -> Option<usize> {
    let mut offset = 0usize;

    for (current_line, l) in source.split_inclusive('\n').enumerate() {
        if current_line as u32 == line {
            let col = byte_col as usize;
            return (col <= l.len()).then_some(offset + col);
        }
        offset += l.len();
    }

    None
}

pub(in crate::server) fn starts_with_assignment_operator(text: &str) -> bool {
    matches!(
        text.as_bytes(),
        [b'=', rest @ ..] if !matches!(rest.first(), Some(b'=' | b'>'))
    ) || text.starts_with("+=")
        || text.starts_with("-=")
        || text.starts_with("*=")
        || text.starts_with("/=")
        || text.starts_with("%=")
        || text.starts_with(".=")
        || text.starts_with("&=")
        || text.starts_with("|=")
        || text.starts_with("^=")
        || text.starts_with("??=")
        || text.starts_with("<<=")
        || text.starts_with(">>=")
}

pub(in crate::server) fn is_declaration_like_write(
    before_trimmed: &str,
    after_trimmed: &str,
) -> bool {
    let segment = before_trimmed
        .rsplit([';', '{', '}'])
        .next()
        .unwrap_or(before_trimmed)
        .trim_start();
    let declaration_tail = after_trimmed.starts_with([',', ')', ';', '=']);

    declaration_tail
        && (segment.contains("function ")
            || segment.starts_with("public ")
            || segment.starts_with("protected ")
            || segment.starts_with("private ")
            || segment.starts_with("readonly ")
            || segment.starts_with("static ")
            || segment.starts_with("var "))
}

pub(in crate::server) fn is_write_reference(source: &str, range: (u32, u32, u32, u32)) -> bool {
    let Some(start) = line_byte_col_to_byte(source, range.0, range.1) else {
        return false;
    };
    let Some(end) = line_byte_col_to_byte(source, range.2, range.3) else {
        return false;
    };
    if start > end || end > source.len() {
        return false;
    }

    let line_start = source[..start].rfind('\n').map(|idx| idx + 1).unwrap_or(0);
    let line_end = source[end..]
        .find('\n')
        .map(|idx| end + idx)
        .unwrap_or(source.len());
    let before_trimmed = source[line_start..start].trim_end();
    let after_trimmed = source[end..line_end].trim_start();

    starts_with_assignment_operator(after_trimmed)
        || after_trimmed.starts_with("++")
        || after_trimmed.starts_with("--")
        || before_trimmed.ends_with("++")
        || before_trimmed.ends_with("--")
        || is_declaration_like_write(before_trimmed, after_trimmed)
}

pub(in crate::server) fn document_highlight_kind(
    source: &str,
    range: (u32, u32, u32, u32),
    read_write_capable: bool,
) -> DocumentHighlightKind {
    if !read_write_capable {
        return DocumentHighlightKind::TEXT;
    }

    if is_write_reference(source, range) {
        DocumentHighlightKind::WRITE
    } else {
        DocumentHighlightKind::READ
    }
}

pub(in crate::server) fn document_highlight_from_range(
    source: &str,
    range: (u32, u32, u32, u32),
    read_write_capable: bool,
) -> DocumentHighlight {
    let rng = range_byte_to_utf16(source, range);
    DocumentHighlight {
        range: Range {
            start: Position::new(rng.0, rng.1),
            end: Position::new(rng.2, rng.3),
        },
        kind: Some(document_highlight_kind(source, range, read_write_capable)),
    }
}

pub(in crate::server) fn php_symbol_kind_for_ref_kind(
    ref_kind: RefKind,
) -> Option<php_lsp_types::PhpSymbolKind> {
    match ref_kind {
        RefKind::ClassName | RefKind::Constructor => Some(php_lsp_types::PhpSymbolKind::Class),
        RefKind::FunctionCall => Some(php_lsp_types::PhpSymbolKind::Function),
        RefKind::MethodCall => Some(php_lsp_types::PhpSymbolKind::Method),
        RefKind::PropertyAccess | RefKind::StaticPropertyAccess => {
            Some(php_lsp_types::PhpSymbolKind::Property)
        }
        RefKind::ClassConstant => Some(php_lsp_types::PhpSymbolKind::ClassConstant),
        RefKind::GlobalConstant => Some(php_lsp_types::PhpSymbolKind::GlobalConstant),
        RefKind::Variable | RefKind::NamespaceName | RefKind::Unknown => None,
    }
}
