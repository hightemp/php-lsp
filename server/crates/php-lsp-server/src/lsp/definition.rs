//! Definition LSP handlers extracted from `server.rs`.

use super::super::*;
use super::hierarchy::{implementation_symbols_for_method, implementation_symbols_for_type};
use super::references::local_symbol_for_reference;

fn local_type_symbol(
    file_symbols: &php_lsp_types::FileSymbols,
    fqn: &str,
) -> Option<Arc<php_lsp_types::SymbolInfo>> {
    file_symbols
        .symbols
        .iter()
        .find(|symbol| {
            matches!(
                symbol.kind,
                php_lsp_types::PhpSymbolKind::Class
                    | php_lsp_types::PhpSymbolKind::Interface
                    | php_lsp_types::PhpSymbolKind::Trait
                    | php_lsp_types::PhpSymbolKind::Enum
            ) && php_lsp_types::symbol_fqn_eq(&symbol.fqn, fqn, symbol.kind)
        })
        .cloned()
        .map(Arc::new)
}

fn snapshot_symbol_location(symbol: &php_lsp_types::SymbolInfo, source: &str) -> Option<Location> {
    Some(Location {
        uri: symbol.uri.parse::<Uri>().ok()?,
        range: range_from_byte_range(source, symbol.selection_range),
    })
}

fn local_implementation_types(
    file_symbols: &php_lsp_types::FileSymbols,
    target_fqn: &str,
) -> Vec<Arc<php_lsp_types::SymbolInfo>> {
    let mut known_parents = HashSet::from([target_fqn.to_ascii_lowercase()]);
    let mut implementations = Vec::new();
    loop {
        let mut changed = false;
        for symbol in file_symbols.symbols.iter().filter(|symbol| {
            matches!(
                symbol.kind,
                php_lsp_types::PhpSymbolKind::Class
                    | php_lsp_types::PhpSymbolKind::Interface
                    | php_lsp_types::PhpSymbolKind::Trait
                    | php_lsp_types::PhpSymbolKind::Enum
            )
        }) {
            let symbol_key = symbol.fqn.to_ascii_lowercase();
            if known_parents.contains(&symbol_key)
                || !symbol
                    .extends
                    .iter()
                    .chain(symbol.implements.iter())
                    .any(|parent| known_parents.contains(&parent.to_ascii_lowercase()))
            {
                continue;
            }
            known_parents.insert(symbol_key);
            changed = true;
            if matches!(
                symbol.kind,
                php_lsp_types::PhpSymbolKind::Class | php_lsp_types::PhpSymbolKind::Enum
            ) && !symbol.modifiers.is_abstract
            {
                implementations.push(Arc::new(symbol.clone()));
            }
        }
        if !changed {
            break;
        }
    }
    implementations
}

fn local_implementation_methods(
    file_symbols: &php_lsp_types::FileSymbols,
    target: &php_lsp_types::SymbolInfo,
) -> Vec<Arc<php_lsp_types::SymbolInfo>> {
    let Some(parent_fqn) = target.parent_fqn.as_deref() else {
        return Vec::new();
    };
    let implementation_types = local_implementation_types(file_symbols, parent_fqn);
    file_symbols
        .symbols
        .iter()
        .filter(|symbol| {
            symbol.kind == php_lsp_types::PhpSymbolKind::Method
                && symbol.name.eq_ignore_ascii_case(&target.name)
                && implementation_types.iter().any(|implementation| {
                    symbol
                        .parent_fqn
                        .as_deref()
                        .is_some_and(|owner| owner.eq_ignore_ascii_case(&implementation.fqn))
                })
        })
        .cloned()
        .map(Arc::new)
        .collect()
}

impl PhpLspBackend {
    pub(crate) async fn lsp_goto_declaration(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .clone();
        let pos = params.text_document_position_params.position;
        tracing::debug!(
            "gotoDeclaration: {}:{}:{}",
            uri.as_str(),
            pos.line,
            pos.character
        );

        if let Some(import_declaration) = self.import_declaration_at_position(&uri, pos) {
            return Ok(Some(import_declaration));
        }

        self.goto_definition(params).await
    }

    pub(crate) async fn lsp_goto_type_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let uri_str = uri.as_str().to_string();
        let request = self.request_context_for_uri(&uri_str).await;
        let request_index = request.index(&self.index);
        let original_pos = params.text_document_position_params.position;
        let Some(OpenDocumentSnapshot {
            tree,
            source,
            template_document,
            file_symbols,
            ..
        }) = self.open_document_snapshot(&uri_str)
        else {
            return Ok(None);
        };
        let pos = if let Some(template) = &template_document {
            match template.map_original_position_to_virtual(original_pos) {
                Some(pos) => pos,
                None => return Ok(None),
            }
        } else {
            original_pos
        };
        let map_template_response = |response| {
            if let Some(template) = &template_document {
                map_goto_definition_response_for_template(&uri_str, template, response)
            } else {
                response
            }
        };
        tracing::debug!(
            "gotoTypeDefinition: {}:{}:{}",
            uri_str,
            pos.line,
            pos.character
        );

        let (sym_at_pos, variable_type_fqn, file_symbols) = {
            let tree = &tree;
            let byte_col = utf16_col_to_byte(&source, pos.line, pos.character);
            let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
                resolve_member_type_from_index(&request_index, class_fqn, member_name)
            };
            let callable_param_resolver = |ctx: CallableParameterContext<'_>| {
                resolve_callable_parameter_type_from_index(&request_index, &file_symbols, ctx)
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
            let variable_type_fqn = if let Some(sym) = &sym_at_pos {
                if sym.ref_kind == RefKind::Variable {
                    variable_name_node_at_range(
                        tree,
                        &source,
                        (pos.line, byte_col, pos.line, byte_col),
                    )
                    .and_then(|variable_node| {
                        infer_variable_hover_info_at_node_with_resolvers(
                            variable_node,
                            &source,
                            &file_symbols,
                            variable_node.start_byte(),
                            &sym.name,
                            Some(&resolver),
                            Some(&callable_param_resolver),
                        )
                    })
                    .and_then(|info| info.resolved_type_fqn)
                    .or_else(|| {
                        infer_variable_type_at_position_with_resolvers(
                            tree,
                            &source,
                            &file_symbols,
                            pos.line,
                            byte_col,
                            &sym.name,
                            Some(&resolver),
                            Some(&callable_param_resolver),
                        )
                    })
                } else {
                    None
                }
            } else {
                None
            };

            (sym_at_pos, variable_type_fqn, file_symbols)
        };

        if let Some(type_fqn) = variable_type_fqn {
            if let Some(local_type) = local_type_symbol(&file_symbols, &type_fqn) {
                return Ok(snapshot_symbol_location(&local_type, &source)
                    .map(GotoDefinitionResponse::Scalar)
                    .map(&map_template_response));
            }
            return Ok(self
                .location_for_type_fqn_excluding_uri(&request, &type_fqn, &uri_str)
                .await
                .map(GotoDefinitionResponse::Scalar)
                .map(&map_template_response));
        }

        let Some(sym_at_pos) = sym_at_pos else {
            return Ok(None);
        };

        if matches!(
            sym_at_pos.ref_kind,
            RefKind::ClassName | RefKind::Constructor
        ) {
            let type_fqn = import_target_fqn(&sym_at_pos);
            if let Some(local_type) = local_type_symbol(&file_symbols, type_fqn) {
                return Ok(snapshot_symbol_location(&local_type, &source)
                    .map(GotoDefinitionResponse::Scalar)
                    .map(&map_template_response));
            }
            return Ok(self
                .location_for_type_fqn_excluding_uri(&request, type_fqn, &uri_str)
                .await
                .map(GotoDefinitionResponse::Scalar)
                .map(&map_template_response));
        }

        let symbol_info =
            if let Some(local_symbol) = local_symbol_for_reference(&file_symbols, &sym_at_pos) {
                Some(local_symbol)
            } else {
                self.resolve_fqn_lazy_with_fallback_in_request(
                    &request,
                    &sym_at_pos.fqn,
                    sym_at_pos.ref_kind,
                    sym_at_pos.allows_global_fallback,
                )
                .await
                .filter(|symbol| symbol.uri != uri_str)
            };

        let Some(symbol_info) = symbol_info else {
            return Ok(None);
        };
        let type_fqn = if symbol_info.uri == uri_str {
            let Some(return_type) = symbol_info
                .signature
                .as_ref()
                .and_then(|signature| signature.return_type.as_ref())
            else {
                return Ok(None);
            };
            first_type_definition_fqn(
                return_type,
                &file_symbols,
                symbol_info.parent_fqn.as_deref(),
            )
        } else {
            self.type_definition_fqn_for_symbol(&request_index, &symbol_info, &file_symbols)
        };
        let Some(type_fqn) = type_fqn else {
            return Ok(None);
        };

        if let Some(local_type) = local_type_symbol(&file_symbols, &type_fqn) {
            return Ok(snapshot_symbol_location(&local_type, &source)
                .map(GotoDefinitionResponse::Scalar)
                .map(&map_template_response));
        }

        Ok(self
            .location_for_type_fqn_excluding_uri(&request, &type_fqn, &uri_str)
            .await
            .map(GotoDefinitionResponse::Scalar)
            .map(map_template_response))
    }

    pub(crate) async fn lsp_goto_implementation(
        &self,
        params: GotoImplementationParams,
    ) -> Result<Option<GotoImplementationResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let uri_str = uri.as_str().to_string();
        let request = self.request_context_for_uri(&uri_str).await;
        let request_index = request.index(&self.index);
        let original_pos = params.text_document_position_params.position;
        let Some(OpenDocumentSnapshot {
            tree,
            source,
            template_document,
            file_symbols,
            ..
        }) = self.open_document_snapshot(&uri_str)
        else {
            return Ok(None);
        };
        let pos = if let Some(template) = &template_document {
            match template.map_original_position_to_virtual(original_pos) {
                Some(pos) => pos,
                None => return Ok(None),
            }
        } else {
            original_pos
        };
        tracing::debug!(
            "gotoImplementation: {}:{}:{}",
            uri_str,
            pos.line,
            pos.character
        );

        let (candidate, local_candidate) = {
            let tree = &tree;
            let byte_col = utf16_col_to_byte(&source, pos.line, pos.character);
            let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
                resolve_member_type_from_index(&request_index, class_fqn, member_name)
            };
            let callable_param_resolver = |ctx: CallableParameterContext<'_>| {
                resolve_callable_parameter_type_from_index(&request_index, &file_symbols, ctx)
            };
            let Some(sym_at_pos) = symbol_at_position_with_resolvers(
                tree,
                &source,
                pos.line,
                byte_col,
                &file_symbols,
                Some(&resolver),
                Some(&callable_param_resolver),
            ) else {
                return Ok(None);
            };

            let candidate = match sym_at_pos.ref_kind {
                RefKind::ClassName => Some((
                    sym_at_pos.fqn.clone(),
                    php_lsp_types::PhpSymbolKind::Class,
                    RefKind::ClassName,
                )),
                RefKind::Constructor => {
                    let class_fqn = sym_at_pos
                        .fqn
                        .strip_suffix("::__construct")
                        .unwrap_or(&sym_at_pos.fqn)
                        .to_string();
                    Some((
                        class_fqn,
                        php_lsp_types::PhpSymbolKind::Class,
                        RefKind::ClassName,
                    ))
                }
                RefKind::MethodCall => Some((
                    sym_at_pos.fqn.clone(),
                    php_lsp_types::PhpSymbolKind::Method,
                    RefKind::MethodCall,
                )),
                _ => None,
            };

            let local_candidate = candidate.as_ref().and_then(|(fqn, kind, ref_kind)| {
                if *ref_kind == RefKind::ClassName {
                    return local_type_symbol(&file_symbols, fqn)
                        .map(|symbol| symbol.as_ref().clone());
                }
                file_symbols
                    .symbols
                    .iter()
                    .find(|sym| {
                        sym.kind == *kind && php_lsp_types::symbol_fqn_eq(&sym.fqn, fqn, sym.kind)
                    })
                    .cloned()
            });
            (candidate, local_candidate)
        };

        let Some((target_fqn, _, ref_kind)) = candidate else {
            return Ok(None);
        };
        let target = if let Some(local_candidate) = local_candidate {
            Some(Arc::new(local_candidate))
        } else {
            self.resolve_fqn_lazy_with_fallback_in_request(&request, &target_fqn, ref_kind, false)
                .await
                .filter(|symbol| symbol.uri != uri_str)
        };
        let Some(target) = target else {
            return Ok(None);
        };

        let mut implementation_symbols = match target.kind {
            php_lsp_types::PhpSymbolKind::Class
            | php_lsp_types::PhpSymbolKind::Interface
            | php_lsp_types::PhpSymbolKind::Trait
            | php_lsp_types::PhpSymbolKind::Enum => {
                implementation_symbols_for_type(&request_index, &target)
            }
            php_lsp_types::PhpSymbolKind::Method => {
                implementation_symbols_for_method(&request_index, &target)
            }
            _ => Vec::new(),
        };
        implementation_symbols.retain(|symbol| symbol.uri != uri_str);
        implementation_symbols.extend(match target.kind {
            php_lsp_types::PhpSymbolKind::Class
            | php_lsp_types::PhpSymbolKind::Interface
            | php_lsp_types::PhpSymbolKind::Trait
            | php_lsp_types::PhpSymbolKind::Enum => {
                local_implementation_types(&file_symbols, &target.fqn)
            }
            php_lsp_types::PhpSymbolKind::Method => {
                local_implementation_methods(&file_symbols, &target)
            }
            _ => Vec::new(),
        });
        let mut seen_implementations = HashSet::new();
        implementation_symbols.retain(|symbol| {
            seen_implementations.insert((
                symbol.uri.clone(),
                symbol.kind,
                symbol.fqn.to_ascii_lowercase(),
            ))
        });

        let mut locations = Vec::new();
        for symbol in implementation_symbols {
            let mut location = if symbol.uri == uri_str {
                snapshot_symbol_location(&symbol, &source)
            } else {
                self.location_for_symbol_selection_in_request(
                    &request,
                    &symbol,
                    "gotoImplementation source read",
                )
                .await
            };
            if let Some(mut location) = location.take() {
                if symbol.uri == uri_str {
                    if let Some(template) = &template_document {
                        location = map_location_for_template(&uri_str, template, location);
                    }
                }
                locations.push(location);
            }
        }
        locations.sort_by(|left, right| {
            (
                left.uri.as_str(),
                left.range.start.line,
                left.range.start.character,
            )
                .cmp(&(
                    right.uri.as_str(),
                    right.range.start.line,
                    right.range.start.character,
                ))
        });

        if locations.is_empty() {
            Ok(None)
        } else {
            Ok(Some(GotoImplementationResponse::Array(locations)))
        }
    }

    pub(crate) async fn lsp_goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let uri_str = uri.as_str().to_string();
        let request = self.request_context_for_uri(&uri_str).await;
        let request_index = request.index(&self.index);
        let original_pos = params.text_document_position_params.position;
        let Some(OpenDocumentSnapshot {
            tree,
            source,
            template_document,
            document_state,
            file_symbols,
        }) = self.open_document_snapshot(&uri_str)
        else {
            return Ok(None);
        };
        if let Some(template) = &template_document {
            if let Some(path_context) =
                template.twig_template_path_context_at_position(original_pos)
            {
                let key = if path_context.prefix.is_empty() {
                    path_context.key.as_str()
                } else {
                    path_context.prefix.as_str()
                };
                return Ok(self
                    .twig_template_location(&request, key)
                    .await
                    .map(GotoDefinitionResponse::Scalar));
            }
            let original_source = template.original_source();
            let original_byte_col =
                utf16_col_to_byte(original_source, original_pos.line, original_pos.character);
            if let Some(path_context) = twig_static_template_path_context_at_position(
                original_source,
                original_pos.line,
                original_byte_col,
            ) {
                if let Some(location) = self
                    .twig_template_location(&request, &path_context.key)
                    .await
                {
                    return Ok(Some(GotoDefinitionResponse::Scalar(location)));
                }
            }
            if let Some(route_context) = twig_route_key_context_at_position(
                original_source,
                original_pos.line,
                original_byte_col,
            ) {
                let file_symbols = php_lsp_types::FileSymbols::default();
                if let Some(location) = self
                    .framework_string_key_location(
                        &request,
                        &file_symbols,
                        original_source,
                        &route_context,
                    )
                    .await
                {
                    return Ok(Some(GotoDefinitionResponse::Scalar(location)));
                }
            }
        }
        let pos = if let Some(template) = &template_document {
            match template.map_original_position_to_virtual(original_pos) {
                Some(pos) => pos,
                None => return Ok(None),
            }
        } else {
            original_pos
        };
        tracing::debug!("gotoDefinition: {}:{}:{}", uri_str, pos.line, pos.character);

        // Extract symbol-at-position inside a block so DashMap guard is dropped
        let (
            sym_at_pos,
            local_var_def,
            this_class_def,
            shape_def,
            shape_member_info,
            framework_string_key_context,
            file_symbols,
        ) = {
            let tree = &tree;
            let byte_col = utf16_col_to_byte(&source, pos.line, pos.character);
            let utf16_index = Utf16LineIndex::new(&source);
            let type_cache =
                RequestTypeCache::new(&uri_str, document_state.map(|state| state.version));

            // Build a cross-file type resolver that uses the workspace index
            let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
                resolve_member_type_from_index(&request_index, class_fqn, member_name)
            };
            let callable_param_resolver = |ctx: CallableParameterContext<'_>| {
                resolve_callable_parameter_type_from_index(&request_index, &file_symbols, ctx)
            };

            let local_var_def = variable_definition_at_position(tree, &source, pos.line, byte_col)
                .map(|d| range_byte_to_utf16(&source, d));
            let shape_def = shape_definition_at_position(&source, pos.line, byte_col)
                .map(|d| range_byte_to_utf16(&source, d));
            let framework_string_key_context =
                framework_string_key_context_at_position(&source, pos.line, byte_col);

            let ctx = InlayHintContext {
                tree,
                source: &source,
                file_symbols: &file_symbols,
                index: &request_index,
                type_cache: &type_cache,
                utf16_index: &utf16_index,
                requested_range: (0, 0, u32::MAX, u32::MAX),
                allow_twig_property_accessors: template_document
                    .as_ref()
                    .is_some_and(|template| template.kind() == crate::template::TemplateKind::Twig),
                allow_blocking_file_io: false,
            };
            let shape_member_info = shape_member_access_info_at_position(&ctx, pos.line, byte_col);
            let inferred_member_symbol = server_member_symbol_at_position(&ctx, pos.line, byte_col);
            let primary_sym = symbol_at_position_with_resolvers(
                tree,
                &source,
                pos.line,
                byte_col,
                &file_symbols,
                Some(&resolver),
                Some(&callable_param_resolver),
            );
            let sym = match primary_sym {
                Some(s)
                    if matches!(s.ref_kind, RefKind::MethodCall | RefKind::PropertyAccess)
                        && request_index.resolve_fqn(&s.fqn).is_none() =>
                {
                    inferred_member_symbol.or(Some(s))
                }
                Some(s) => Some(s),
                None => inferred_member_symbol,
            };
            let this_class_def = sym.as_ref().and_then(|sym| {
                if sym.ref_kind == RefKind::Variable && sym.name == "$this" {
                    current_class_symbol_at_range(
                        &file_symbols,
                        (pos.line, byte_col, pos.line, byte_col),
                    )
                    .map(|class_sym| {
                        (
                            class_sym.uri.clone(),
                            range_byte_to_utf16(&source, class_sym.selection_range),
                        )
                    })
                } else {
                    None
                }
            });
            (
                sym,
                local_var_def,
                this_class_def,
                shape_def,
                shape_member_info,
                framework_string_key_context,
                file_symbols,
            )
        };

        if let (Some(template), Some(info)) = (&template_document, &shape_member_info) {
            if let Some(variable_name) = &info.definition_variable_name {
                if let Some(location) = template.twig_shape_key_definition(
                    variable_name,
                    info.definition_target,
                    &info.definition_path,
                ) {
                    return Ok(Some(GotoDefinitionResponse::Scalar(location)));
                }
            }
            return Ok(None);
        }

        if let Some(def) = shape_def {
            let range = Range {
                start: Position::new(def.0, def.1),
                end: Position::new(def.2, def.3),
            };
            if let Some(template) = &template_document {
                if let Some(mapped) = template.map_virtual_range_to_original(range) {
                    return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                        uri,
                        range: mapped,
                    })));
                } else {
                    // Fall through to source-backed Twig shape definitions below.
                }
            } else {
                return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                    uri,
                    range,
                })));
            }
        }

        if let Some((target_uri, def)) = this_class_def {
            let mut range = Range {
                start: Position::new(def.0, def.1),
                end: Position::new(def.2, def.3),
            };
            if let Some(template) = &template_document {
                let Some(mapped) = template.map_virtual_range_to_original(range) else {
                    return Ok(None);
                };
                range = mapped;
            }
            return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                uri: target_uri.parse::<Uri>().unwrap_or_else(|_| uri.clone()),
                range,
            })));
        }

        // Local variable definition (same file/scope).
        if let Some(def) = local_var_def {
            let mut range = Range {
                start: Position::new(def.0, def.1),
                end: Position::new(def.2, def.3),
            };
            if let Some(template) = &template_document {
                if let Some(mapped) = template.map_virtual_range_to_original(range) {
                    range = mapped;
                } else if template.kind() == crate::template::TemplateKind::Twig {
                    if let Some(current_variable) = sym_at_pos
                        .as_ref()
                        .filter(|sym| sym.ref_kind == RefKind::Variable)
                        .and_then(|sym| {
                            template.map_virtual_range_to_original(range_from_byte_range(
                                &source, sym.range,
                            ))
                        })
                    {
                        range = current_variable;
                    } else {
                        return Ok(None);
                    }
                } else {
                    return Ok(None);
                }
            }
            return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                uri,
                range,
            })));
        }

        if let Some(template) = &template_document {
            if template.kind() == crate::template::TemplateKind::Twig {
                if let Some(range) = sym_at_pos
                    .as_ref()
                    .filter(|sym| sym.ref_kind == RefKind::Variable)
                    .and_then(|sym| {
                        template.map_virtual_range_to_original(range_from_byte_range(
                            &source, sym.range,
                        ))
                    })
                {
                    return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                        uri,
                        range,
                    })));
                }
            }
        }

        if let Some(ref framework_string_key_context) = framework_string_key_context {
            if let Some(location) = self
                .framework_string_key_location(
                    &request,
                    &file_symbols,
                    &source,
                    framework_string_key_context,
                )
                .await
            {
                return Ok(Some(GotoDefinitionResponse::Scalar(location)));
            }
        }

        let sym_at_pos = match sym_at_pos {
            Some(s) => {
                tracing::debug!(
                    "goto_definition: sym_at_pos fqn='{}', name='{}', ref_kind={:?}",
                    s.fqn,
                    s.name,
                    s.ref_kind
                );
                s
            }
            None => {
                tracing::debug!("goto_definition: no symbol at position");
                return Ok(None);
            }
        };

        // Prefer the exact request snapshot for same-file declarations. The
        // global index may still contain the previous document generation.
        let local_symbol_info = local_symbol_for_reference(&file_symbols, &sym_at_pos);
        let symbol_info = if local_symbol_info.is_some() {
            local_symbol_info
        } else {
            self.resolve_fqn_lazy_with_fallback_in_request(
                &request,
                &sym_at_pos.fqn,
                sym_at_pos.ref_kind,
                sym_at_pos.allows_global_fallback,
            )
            .await
            .filter(|symbol| symbol.uri != uri_str)
        };

        // For constructor refs (`new ClassName()`), fall back to the class
        // declaration when `__construct` is not explicitly defined.
        let symbol_info = if symbol_info.is_none() && sym_at_pos.ref_kind == RefKind::Constructor {
            if let Some(class_fqn) = sym_at_pos.fqn.strip_suffix("::__construct") {
                self.resolve_fqn_lazy_with_fallback_in_request(
                    &request,
                    class_fqn,
                    RefKind::ClassName,
                    false,
                )
                .await
                .filter(|symbol| symbol.uri != uri_str)
            } else {
                None
            }
        } else {
            symbol_info
        };
        let twig_accessor_symbol = template_document
            .as_ref()
            .is_some_and(|template| template.kind() == crate::template::TemplateKind::Twig)
            .then(|| twig_property_accessor_method_for_symbol(&request_index, &sym_at_pos))
            .flatten();
        let symbol_info = symbol_info.or(twig_accessor_symbol);

        let result = if let Some(sym) = symbol_info {
            if sym.uri == uri_str {
                snapshot_symbol_location(&sym, &source).map(GotoDefinitionResponse::Scalar)
            } else {
                self.location_for_symbol_selection_in_request(
                    &request,
                    &sym,
                    "gotoDefinition source read",
                )
                .await
                .map(GotoDefinitionResponse::Scalar)
            }
        } else if let Some(virtual_member) =
            phpdoc_virtual_member_for_symbol(&request_index, &sym_at_pos)
        {
            self.phpdoc_virtual_member_location(&request, &virtual_member)
                .await
                .map(GotoDefinitionResponse::Scalar)
        } else if let Some(virtual_member) = framework_virtual_member_for_symbol(
            &request_index,
            &sym_at_pos,
            Some(&uri_str),
            Some(&file_symbols),
            Some(&source),
        ) {
            self.framework_virtual_member_location(&request, &virtual_member)
                .await
                .map(GotoDefinitionResponse::Scalar)
        } else {
            None
        };

        // Fallback: when a member call on `$this->prop` fails because the declared
        // property type doesn't have that member, try resolving from the actual
        // assignment (e.g., `$this->em = $this->createStub(...)` → Stub type).
        let result = if result.is_none()
            && (sym_at_pos.ref_kind == RefKind::MethodCall
                || sym_at_pos.ref_kind == RefKind::PropertyAccess)
        {
            tracing::debug!(
                "goto_definition: primary resolution failed, trying property assignment fallback for obj_expr={:?}",
                sym_at_pos.object_expr
            );
            if let Some(ref obj_expr) = sym_at_pos.object_expr {
                if let Some(prop_name) = obj_expr.strip_prefix("$this->") {
                    // Only handle simple property access (no chaining)
                    if !prop_name.contains("->") {
                        self.try_property_assignment_type_fallback(
                            &request,
                            &uri_str,
                            prop_name,
                            &sym_at_pos.name,
                        )
                        .await
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            result
        };

        Ok(result.map(|response| {
            if let Some(template) = &template_document {
                map_goto_definition_response_for_template(&uri_str, template, response)
            } else {
                response
            }
        }))
    }
}

impl PhpLspBackend {
    pub(in crate::server) fn import_declaration_at_position(
        &self,
        uri: &Uri,
        pos: Position,
    ) -> Option<GotoDefinitionResponse> {
        let uri_str = uri.as_str().to_string();
        let parser = self.open_files.get(&uri_str)?;
        let tree = parser.tree()?;
        let source = parser.source();
        let byte_col = utf16_col_to_byte(&source, pos.line, pos.character);
        let file_symbols = extract_file_symbols(tree, &source, &uri_str);
        let scoped_symbols = file_symbols.scoped_at_byte_position(pos.line, byte_col);
        let sym = symbol_at_position(tree, &source, pos.line, byte_col, &scoped_symbols)?;
        let use_stmt = imported_use_statement_for_symbol(&scoped_symbols, &sym)?;
        let range = range_byte_to_utf16(&source, use_stmt.range);

        Some(GotoDefinitionResponse::Scalar(Location {
            uri: uri.clone(),
            range: Range {
                start: Position::new(range.0, range.1),
                end: Position::new(range.2, range.3),
            },
        }))
    }

    pub(in crate::server) fn file_symbols_for_uri_in_index(
        &self,
        index: &WorkspaceIndex,
        uri_str: &str,
    ) -> Option<php_lsp_types::FileSymbols> {
        if let Some(snapshot) = self.open_document_snapshot(uri_str) {
            return Some(snapshot.file_symbols);
        }

        if let Some(file_symbols) = index.file_symbols.get(uri_str) {
            return Some(file_symbols.value().as_ref().clone());
        }
        None
    }

    pub(in crate::server) async fn source_for_uri(
        &self,
        uri_str: &str,
        label: &'static str,
    ) -> Option<String> {
        if uri_str.starts_with("phpstub://") {
            return self.stub_source_for_uri(uri_str, label).await;
        }

        if let Some(parser) = self.open_files.get(uri_str) {
            return Some(parser.source());
        }

        let path = uri_to_path(uri_str)?;
        read_file_to_string_blocking(path, label).await.ok()
    }

    pub(in crate::server) async fn source_for_uri_in_request(
        &self,
        request: &WorkspaceRequestContext,
        uri_str: &str,
        label: &'static str,
    ) -> Option<String> {
        if !uri_str.starts_with("phpstub://") {
            return self.source_for_uri(uri_str, label).await;
        }
        self.stub_source_for_uri_in_request(request, uri_str, label)
            .await
    }

    async fn stub_source_for_uri(&self, uri_str: &str, label: &'static str) -> Option<String> {
        let rest = uri_str.strip_prefix("phpstub://")?;
        let (extension, relative_file) = rest.split_once('/')?;
        if !php_lsp_index::stubs::is_valid_stub_extension_name(extension)
            || relative_file.is_empty()
            || relative_file.contains(':')
            || relative_file.contains('\\')
            || relative_file
                .split('/')
                .any(|component| component.is_empty() || component == "." || component == "..")
        {
            return None;
        }

        let relative_path = Path::new(extension).join(relative_file);

        let runtime_state = self.runtime_state_snapshot().await;
        let mut configs = runtime_state.configs.clone();
        if configs.is_empty() {
            let root = self
                .workspace_root
                .lock()
                .await
                .clone()
                .or_else(|| std::env::current_dir().ok())?;
            configs.push(WorkspaceRootConfig {
                workspace_folder: root.clone(),
                root,
                namespace_map: None,
                runtime_config: runtime_state.fallback.clone(),
                index: self.index.clone(),
                vendor_file_lru: self.vendor_file_lru.clone(),
            });
        }
        for config in configs {
            for stubs_path in
                candidate_stubs_paths(&config.root, config.runtime_config.stubs_path.clone())
            {
                if !php_lsp_index::stubs::is_real_stub_file(&stubs_path, &relative_path) {
                    continue;
                }
                let path = stubs_path.join(&relative_path);
                if let Ok(source) = read_file_to_string_blocking(path, label).await {
                    return Some(source);
                }
            }
        }

        None
    }

    async fn stub_source_for_uri_in_request(
        &self,
        request: &WorkspaceRequestContext,
        uri_str: &str,
        label: &'static str,
    ) -> Option<String> {
        let rest = uri_str.strip_prefix("phpstub://")?;
        let (extension, relative_file) = rest.split_once('/')?;
        if !php_lsp_index::stubs::is_valid_stub_extension_name(extension)
            || relative_file.is_empty()
            || relative_file.contains(':')
            || relative_file.contains('\\')
            || relative_file
                .split('/')
                .any(|component| component.is_empty() || component == "." || component == "..")
        {
            return None;
        }
        let (root, stubs_path) = if let Some(config) = request.workspace.as_ref() {
            (
                config.root.clone(),
                config.runtime_config.stubs_path.clone(),
            )
        } else {
            (
                std::env::current_dir().ok()?,
                request.state.fallback.stubs_path.clone(),
            )
        };
        let relative_path = Path::new(extension).join(relative_file);
        for stubs_path in candidate_stubs_paths(&root, stubs_path) {
            if !php_lsp_index::stubs::is_real_stub_file(&stubs_path, &relative_path) {
                continue;
            }
            if let Ok(source) =
                read_file_to_string_blocking(stubs_path.join(&relative_path), label).await
            {
                return Some(source);
            }
        }
        None
    }

    pub(in crate::server) async fn location_for_symbol_selection_in_request(
        &self,
        request: &WorkspaceRequestContext,
        symbol: &php_lsp_types::SymbolInfo,
        label: &'static str,
    ) -> Option<Location> {
        let source = self
            .source_for_uri_in_request(request, &symbol.uri, label)
            .await?;
        Some(Location {
            uri: symbol.uri.parse::<Uri>().ok()?,
            range: range_from_byte_range(&source, symbol.selection_range),
        })
    }

    pub(in crate::server) async fn phpdoc_virtual_member_location(
        &self,
        request: &WorkspaceRequestContext,
        member: &PhpDocVirtualMember,
    ) -> Option<Location> {
        let source = self
            .source_for_uri_in_request(
                request,
                &member.owner.uri,
                "phpdoc virtual member source read",
            )
            .await?;
        let doc_comment = member.owner.doc_comment.as_ref()?;
        let doc_start = source.find(doc_comment)?;
        let range = phpdoc_virtual_member_range(&source, doc_comment, doc_start, member)?;
        let utf16_range = range_byte_to_utf16(&source, range);
        Some(Location {
            uri: member.owner.uri.parse::<Uri>().ok()?,
            range: Range {
                start: Position::new(utf16_range.0, utf16_range.1),
                end: Position::new(utf16_range.2, utf16_range.3),
            },
        })
    }

    pub(in crate::server) async fn framework_virtual_member_location(
        &self,
        request: &WorkspaceRequestContext,
        member: &crate::framework::VirtualMember,
    ) -> Option<Location> {
        let (uri, range) = member.sources.iter().find_map(|source| match source {
            crate::framework::VirtualMemberSource::SourceRange { uri, range } => {
                Some((uri.clone(), *range))
            }
            crate::framework::VirtualMemberSource::Synthetic { .. } => None,
        })?;
        let source = self
            .source_for_uri_in_request(request, &uri, "framework virtual member source read")
            .await?;
        let utf16_range = range_byte_to_utf16(&source, range);
        Some(Location {
            uri: uri.parse::<Uri>().ok()?,
            range: Range {
                start: Position::new(utf16_range.0, utf16_range.1),
                end: Position::new(utf16_range.2, utf16_range.3),
            },
        })
    }

    pub(in crate::server) async fn cached_framework_string_keys(
        &self,
        request: &WorkspaceRequestContext,
        domain: &str,
    ) -> Vec<crate::framework::FrameworkStringKey> {
        let Some(workspace_root) = request.root() else {
            return Vec::new();
        };
        let runtime_config = request.runtime_config();
        let traversal_limits = runtime_config.traversal_limits;
        let exclude_paths = runtime_config.exclude_paths.clone();
        let key = FrameworkStringKeyCacheKey {
            root: workspace_root.to_path_buf(),
            domain: domain.to_string(),
            traversal_limits,
            exclude_paths: exclude_paths.clone(),
        };
        if let Some(keys) = self.framework_string_key_cache.lock().await.get(&key) {
            return keys;
        }

        let root = workspace_root.to_path_buf();
        let domain = domain.to_string();
        let path_label = format!("{} ({})", root.display(), domain);
        let keys = match run_file_io_blocking("framework string-key scan", path_label, move || {
            crate::framework::framework_string_keys_for_workspace_with_limits(
                &root,
                &domain,
                traversal_limits,
                &exclude_paths,
            )
        })
        .await
        {
            Ok(keys) => keys,
            Err(message) => {
                tracing::warn!("{}", message);
                Vec::new()
            }
        };

        self.framework_string_key_cache
            .lock()
            .await
            .insert(key, keys.clone());
        keys
    }

    pub(in crate::server) async fn framework_string_key_items(
        &self,
        request: &WorkspaceRequestContext,
        context: &FrameworkStringKeyAtPosition,
    ) -> Vec<lsp_types::CompletionItem> {
        self.cached_framework_string_keys(request, context.domain)
            .await
            .into_iter()
            .filter(|key| key.key.starts_with(&context.prefix))
            .map(|key| framework_string_key_completion_item(&key, &context.prefix))
            .collect()
    }

    pub(in crate::server) async fn framework_string_key_location(
        &self,
        request: &WorkspaceRequestContext,
        _file_symbols: &php_lsp_types::FileSymbols,
        _source: &str,
        context: &FrameworkStringKeyAtPosition,
    ) -> Option<Location> {
        let source_range = self
            .cached_framework_string_keys(request, context.domain)
            .await
            .into_iter()
            .find(|key| key.key == context.key)
            .and_then(|key| framework_string_key_source_byte_range(&key));
        let (uri, range) = source_range?;
        let source = self
            .source_for_uri(&uri, "framework string key source read")
            .await?;
        Some(Location {
            uri: uri.parse::<Uri>().ok()?,
            range: range_from_byte_range(&source, range),
        })
    }

    pub(in crate::server) fn type_definition_fqn_for_symbol(
        &self,
        index: &WorkspaceIndex,
        symbol: &php_lsp_types::SymbolInfo,
        fallback_file_symbols: &php_lsp_types::FileSymbols,
    ) -> Option<String> {
        if matches!(
            symbol.kind,
            php_lsp_types::PhpSymbolKind::Class
                | php_lsp_types::PhpSymbolKind::Interface
                | php_lsp_types::PhpSymbolKind::Trait
                | php_lsp_types::PhpSymbolKind::Enum
        ) {
            return Some(symbol.fqn.clone());
        }

        let return_type = symbol.signature.as_ref()?.return_type.as_ref()?;
        let declaring_file_symbols = self
            .file_symbols_for_uri_in_index(index, &symbol.uri)
            .unwrap_or_else(|| fallback_file_symbols.clone());

        first_type_definition_fqn(
            return_type,
            &declaring_file_symbols,
            symbol.parent_fqn.as_deref(),
        )
    }

    pub(in crate::server) async fn location_for_type_fqn(
        &self,
        request: &WorkspaceRequestContext,
        fqn: &str,
    ) -> Option<Location> {
        if is_builtin_type_name(fqn) {
            return None;
        }

        let symbol = self
            .resolve_fqn_lazy_with_fallback_in_request(request, fqn, RefKind::ClassName, false)
            .await?;
        self.location_for_symbol_selection_in_request(
            request,
            &symbol,
            "type definition source read",
        )
        .await
    }

    async fn location_for_type_fqn_excluding_uri(
        &self,
        request: &WorkspaceRequestContext,
        fqn: &str,
        excluded_uri: &str,
    ) -> Option<Location> {
        if is_builtin_type_name(fqn) {
            return None;
        }

        let symbol = self
            .resolve_fqn_lazy_with_fallback_in_request(request, fqn, RefKind::ClassName, false)
            .await?;
        if symbol.uri == excluded_uri {
            return None;
        }
        self.location_for_symbol_selection_in_request(
            request,
            &symbol,
            "type definition source read",
        )
        .await
    }

    pub(in crate::server) fn reference_locations_for_symbol(
        &self,
        index: &WorkspaceIndex,
        request: Option<&WorkspaceRequestContext>,
        target_fqn: &str,
        target_kind: php_lsp_types::PhpSymbolKind,
        include_declaration: bool,
    ) -> Vec<Location> {
        let mut locations = Vec::new();
        for (file_uri, references) in self.reference_scan_matches(
            index,
            request,
            target_fqn,
            target_kind,
            include_declaration,
        ) {
            for reference in references {
                if let Ok(uri) = file_uri.parse::<Uri>() {
                    locations.push(Location {
                        uri,
                        range: range_from_lsp_tuple(reference.range),
                    });
                }
            }
        }

        locations
    }
}
