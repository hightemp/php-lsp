//! Hierarchy LSP handlers extracted from `server.rs`.

use super::super::*;

impl PhpLspBackend {
    pub(crate) async fn lsp_prepare_call_hierarchy(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> Result<Option<Vec<CallHierarchyItem>>> {
        let uri_str = params
            .text_document_position_params
            .text_document
            .uri
            .as_str()
            .to_string();
        let pos = params.text_document_position_params.position;

        let (candidate, local_candidate, containing_candidate, allow_containing_fallback) = {
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
            let file_symbols = self
                .index
                .file_symbols
                .get(&uri_str)
                .map(|entry| entry.value().clone())
                .unwrap_or_else(|| extract_file_symbols(tree, &source, &uri_str));
            let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
                self.resolve_member_type(class_fqn, member_name)
            };
            let callable_param_resolver = |ctx: CallableParameterContext<'_>| {
                resolve_callable_parameter_type_from_index(&self.index, &file_symbols, ctx)
            };
            let sym_at_pos = symbol_at_position_with_resolvers(
                tree,
                &source,
                pos.line,
                byte_col,
                &file_symbols,
                Some(&resolver),
                Some(&callable_param_resolver),
            );
            let allow_containing_fallback = sym_at_pos
                .as_ref()
                .is_none_or(|sym| !is_call_hierarchy_ref_kind(sym.ref_kind));
            let candidate = sym_at_pos
                .as_ref()
                .filter(|sym| is_call_hierarchy_ref_kind(sym.ref_kind))
                .map(|sym| (sym.fqn.clone(), sym.ref_kind));
            let local_candidate = candidate.as_ref().and_then(|(fqn, _)| {
                file_symbols
                    .symbols
                    .iter()
                    .find(|sym| sym.fqn == *fqn && is_call_hierarchy_symbol_kind(sym.kind))
                    .cloned()
            });
            let point_range = (pos.line, byte_col, pos.line, byte_col);
            let containing_candidate =
                containing_callable_symbol(&file_symbols, point_range).cloned();
            (
                candidate,
                local_candidate,
                containing_candidate,
                allow_containing_fallback,
            )
        };

        let mut symbol = None;
        if let Some((fqn, ref_kind)) = candidate {
            symbol = self.resolve_fqn_lazy_with_fallback(&fqn, ref_kind).await;
            if symbol.is_none() && ref_kind == RefKind::Constructor {
                if let Some(class_fqn) = fqn.strip_suffix("::__construct") {
                    symbol = self
                        .resolve_fqn_lazy_with_fallback(class_fqn, RefKind::ClassName)
                        .await;
                }
            }
            if symbol.is_none() {
                symbol = local_candidate.map(Arc::new);
            }
        }

        if symbol.is_none() && allow_containing_fallback {
            symbol = containing_candidate.map(Arc::new);
        }

        let Some(symbol) = symbol else {
            return Ok(None);
        };
        if !is_call_hierarchy_symbol_kind(symbol.kind) {
            return Ok(None);
        }

        Ok(call_hierarchy_item_from_symbol(&symbol).map(|item| vec![item]))
    }

    pub(crate) async fn lsp_incoming_calls(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyIncomingCall>>> {
        let Some((target, target_kind)) =
            call_hierarchy_target_from_item(&self.index, &params.item)
        else {
            return Ok(None);
        };

        let mut calls_by_caller: HashMap<String, (php_lsp_types::SymbolInfo, Vec<Range>)> =
            HashMap::new();
        for entry in self.index.file_symbols.iter() {
            let file_uri = entry.key().clone();
            let file_symbols = entry.value().clone();

            if let Some(parser) = self.open_files.get(&file_uri) {
                if let Some(tree) = parser.tree() {
                    let source = parser.source();
                    incoming_call_hierarchy_for_file(
                        tree,
                        &source,
                        &file_symbols,
                        &target.fqn,
                        target_kind,
                        &mut calls_by_caller,
                    );
                }
                continue;
            }

            let Some(path) = uri_to_path(&file_uri) else {
                continue;
            };
            let Ok(source) =
                read_file_to_string_blocking(path, "callHierarchy/incoming read").await
            else {
                continue;
            };
            let mut parser = FileParser::new();
            parser.parse_full(&source);
            if let Some(tree) = parser.tree() {
                incoming_call_hierarchy_for_file(
                    tree,
                    &source,
                    &file_symbols,
                    &target.fqn,
                    target_kind,
                    &mut calls_by_caller,
                );
            }
        }

        let mut calls: Vec<_> = calls_by_caller
            .into_values()
            .filter_map(|(caller, ranges)| {
                Some(CallHierarchyIncomingCall {
                    from: call_hierarchy_item_from_symbol(&caller)?,
                    from_ranges: ranges,
                })
            })
            .collect();
        calls.sort_by(|left, right| left.from.name.cmp(&right.from.name));

        if calls.is_empty() {
            Ok(None)
        } else {
            Ok(Some(calls))
        }
    }

    pub(crate) async fn lsp_outgoing_calls(
        &self,
        params: CallHierarchyOutgoingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyOutgoingCall>>> {
        let Some((caller, _)) = call_hierarchy_target_from_item(&self.index, &params.item) else {
            return Ok(None);
        };
        if !is_call_hierarchy_symbol_kind(caller.kind) {
            return Ok(None);
        }

        let file_uri = caller.uri.clone();
        let file_symbols = self
            .index
            .file_symbols
            .get(&file_uri)
            .map(|entry| entry.value().clone())
            .unwrap_or_default();

        let calls = if let Some(parser) = self.open_files.get(&file_uri) {
            let Some(tree) = parser.tree() else {
                return Ok(None);
            };
            let source = parser.source();
            outgoing_call_hierarchy_for_tree(tree, &source, &file_symbols, &self.index, &caller)
        } else {
            let Some(path) = uri_to_path(&file_uri) else {
                return Ok(None);
            };
            let Ok(source) =
                read_file_to_string_blocking(path, "callHierarchy/outgoing read").await
            else {
                return Ok(None);
            };
            let mut parser = FileParser::new();
            parser.parse_full(&source);
            let Some(tree) = parser.tree() else {
                return Ok(None);
            };
            outgoing_call_hierarchy_for_tree(tree, &source, &file_symbols, &self.index, &caller)
        };

        if calls.is_empty() {
            Ok(None)
        } else {
            Ok(Some(calls))
        }
    }

    pub(crate) async fn lsp_prepare_type_hierarchy(
        &self,
        params: TypeHierarchyPrepareParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let uri_str = params
            .text_document_position_params
            .text_document
            .uri
            .as_str()
            .to_string();
        let pos = params.text_document_position_params.position;

        let (candidate, local_candidate, containing_class_fqn) = {
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
            let file_symbols = self
                .index
                .file_symbols
                .get(&uri_str)
                .map(|entry| entry.value().clone())
                .unwrap_or_else(|| extract_file_symbols(tree, &source, &uri_str));
            let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
                self.resolve_member_type(class_fqn, member_name)
            };
            let callable_param_resolver = |ctx: CallableParameterContext<'_>| {
                resolve_callable_parameter_type_from_index(&self.index, &file_symbols, ctx)
            };
            let sym_at_pos = symbol_at_position_with_resolvers(
                tree,
                &source,
                pos.line,
                byte_col,
                &file_symbols,
                Some(&resolver),
                Some(&callable_param_resolver),
            );
            let candidate = sym_at_pos.as_ref().and_then(|sym| match sym.ref_kind {
                RefKind::ClassName => Some(sym.fqn.clone()),
                RefKind::Constructor => sym
                    .fqn
                    .strip_suffix("::__construct")
                    .map(str::to_string)
                    .or_else(|| Some(sym.fqn.clone())),
                _ => None,
            });
            let local_candidate = candidate.as_ref().and_then(|fqn| {
                file_symbols
                    .symbols
                    .iter()
                    .find(|sym| sym.fqn == *fqn && is_type_hierarchy_symbol_kind(sym.kind))
                    .cloned()
            });
            let point_range = (pos.line, byte_col, pos.line, byte_col);
            let containing_class_fqn = current_class_fqn_at_range(&file_symbols, point_range);
            (candidate, local_candidate, containing_class_fqn)
        };

        let mut symbol = None;
        if let Some(fqn) = candidate {
            symbol = self
                .resolve_fqn_lazy_with_fallback(&fqn, RefKind::ClassName)
                .await;
            if symbol.is_none() {
                symbol = local_candidate.map(Arc::new);
            }
        }

        if symbol.is_none() {
            if let Some(class_fqn) = containing_class_fqn {
                symbol = self
                    .resolve_fqn_lazy_with_fallback(&class_fqn, RefKind::ClassName)
                    .await;
            }
        }

        let Some(symbol) = symbol else {
            return Ok(None);
        };

        Ok(type_hierarchy_item_from_symbol(&symbol).map(|item| vec![item]))
    }

    pub(crate) async fn lsp_supertypes(
        &self,
        params: TypeHierarchySupertypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let Some(symbol) = type_hierarchy_symbol_from_item(&self.index, &params.item) else {
            return Ok(None);
        };

        let parent_fqns = direct_type_parent_fqns(&symbol);
        let mut parents = Vec::new();
        for parent_fqn in parent_fqns {
            self.lazy_index_class(&parent_fqn).await;
            if let Some(parent) = self
                .resolve_fqn_lazy_with_fallback(&parent_fqn, RefKind::ClassName)
                .await
            {
                if let Some(item) = type_hierarchy_item_from_symbol(&parent) {
                    parents.push(item);
                }
            }
        }
        parents.sort_by(|left, right| left.name.cmp(&right.name));

        if parents.is_empty() {
            Ok(None)
        } else {
            Ok(Some(parents))
        }
    }

    pub(crate) async fn lsp_subtypes(
        &self,
        params: TypeHierarchySubtypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let Some(symbol) = type_hierarchy_symbol_from_item(&self.index, &params.item) else {
            return Ok(None);
        };

        let subtypes: Vec<_> = direct_type_subtypes(&self.index, &symbol.fqn)
            .into_iter()
            .filter_map(|symbol| type_hierarchy_item_from_symbol(&symbol))
            .collect();

        if subtypes.is_empty() {
            Ok(None)
        } else {
            Ok(Some(subtypes))
        }
    }
}
