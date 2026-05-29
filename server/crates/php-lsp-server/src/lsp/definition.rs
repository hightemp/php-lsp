//! Definition LSP handlers extracted from `server.rs`.

use super::super::*;
use super::hierarchy::{implementation_symbols_for_method, implementation_symbols_for_type};

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
        let pos = params.text_document_position_params.position;
        tracing::debug!(
            "gotoTypeDefinition: {}:{}:{}",
            uri_str,
            pos.line,
            pos.character
        );

        let (sym_at_pos, variable_type_fqn, file_symbols) = {
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
            return Ok(self
                .location_for_type_fqn(&type_fqn)
                .await
                .map(GotoDefinitionResponse::Scalar));
        }

        let Some(sym_at_pos) = sym_at_pos else {
            return Ok(None);
        };

        if matches!(
            sym_at_pos.ref_kind,
            RefKind::ClassName | RefKind::Constructor
        ) {
            let type_fqn = import_target_fqn(&sym_at_pos);
            return Ok(self
                .location_for_type_fqn(type_fqn)
                .await
                .map(GotoDefinitionResponse::Scalar));
        }

        let symbol_info = self
            .resolve_fqn_lazy_with_fallback(&sym_at_pos.fqn, sym_at_pos.ref_kind)
            .await;

        let Some(symbol_info) = symbol_info else {
            return Ok(None);
        };
        let Some(type_fqn) = self.type_definition_fqn_for_symbol(&symbol_info, &file_symbols)
        else {
            return Ok(None);
        };

        Ok(self
            .location_for_type_fqn(&type_fqn)
            .await
            .map(GotoDefinitionResponse::Scalar))
    }

    pub(crate) async fn lsp_goto_implementation(
        &self,
        params: GotoImplementationParams,
    ) -> Result<Option<GotoImplementationResponse>> {
        let uri_str = params
            .text_document_position_params
            .text_document
            .uri
            .as_str()
            .to_string();
        let pos = params.text_document_position_params.position;
        tracing::debug!(
            "gotoImplementation: {}:{}:{}",
            uri_str,
            pos.line,
            pos.character
        );

        let (candidate, local_candidate) = {
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

            let local_candidate = candidate.as_ref().and_then(|(fqn, kind, _)| {
                file_symbols
                    .symbols
                    .iter()
                    .find(|sym| sym.fqn == *fqn && sym.kind == *kind)
                    .cloned()
            });
            (candidate, local_candidate)
        };

        let Some((target_fqn, _, ref_kind)) = candidate else {
            return Ok(None);
        };
        let target = self
            .resolve_fqn_lazy_with_fallback(&target_fqn, ref_kind)
            .await
            .or_else(|| local_candidate.map(Arc::new));
        let Some(target) = target else {
            return Ok(None);
        };

        let implementation_symbols = match target.kind {
            php_lsp_types::PhpSymbolKind::Class
            | php_lsp_types::PhpSymbolKind::Interface
            | php_lsp_types::PhpSymbolKind::Trait
            | php_lsp_types::PhpSymbolKind::Enum => {
                implementation_symbols_for_type(&self.index, &target)
            }
            php_lsp_types::PhpSymbolKind::Method => {
                implementation_symbols_for_method(&self.index, &target)
            }
            _ => Vec::new(),
        };

        let mut locations = Vec::new();
        for symbol in implementation_symbols {
            if let Some(location) = self
                .location_for_symbol_selection(&symbol, "gotoImplementation source read")
                .await
            {
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
        let original_pos = params.text_document_position_params.position;
        let template_document = self.template_document(&uri_str);
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
                    .twig_template_location(&uri_str, key)
                    .await
                    .map(GotoDefinitionResponse::Scalar));
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
            framework_string_key_context,
            file_symbols,
            source,
        ) = {
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
            let utf16_index = Utf16LineIndex::new(&source);
            let type_cache =
                RequestTypeCache::new(&uri_str, self.current_document_version(&uri_str));

            let file_symbols = self
                .index
                .file_symbols
                .get(&uri_str)
                .map(|entry| entry.value().clone())
                .unwrap_or_default();

            // Build a cross-file type resolver that uses the workspace index
            let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
                self.resolve_member_type(class_fqn, member_name)
            };
            let callable_param_resolver = |ctx: CallableParameterContext<'_>| {
                resolve_callable_parameter_type_from_index(&self.index, &file_symbols, ctx)
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
                index: &self.index,
                type_cache: &type_cache,
                utf16_index: &utf16_index,
                requested_range: (0, 0, u32::MAX, u32::MAX),
            };
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
                    if matches!(s.ref_kind, RefKind::MethodCall)
                        && self.index.resolve_fqn(&s.fqn).is_none() =>
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
                framework_string_key_context,
                file_symbols,
                source,
            )
        };

        if let Some(def) = shape_def {
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
                uri,
                range,
            })));
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
                let Some(mapped) = template.map_virtual_range_to_original(range) else {
                    return Ok(None);
                };
                range = mapped;
            }
            return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                uri,
                range,
            })));
        }

        if let Some(ref framework_string_key_context) = framework_string_key_context {
            if let Some(location) = self
                .framework_string_key_location(
                    &uri_str,
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

        // Look up symbol in index (with lazy vendor fallback)
        let symbol_info = self
            .resolve_fqn_lazy_with_fallback(&sym_at_pos.fqn, sym_at_pos.ref_kind)
            .await;

        // For constructor refs (`new ClassName()`), fall back to the class
        // declaration when `__construct` is not explicitly defined.
        let symbol_info = if symbol_info.is_none() && sym_at_pos.ref_kind == RefKind::Constructor {
            if let Some(class_fqn) = sym_at_pos.fqn.strip_suffix("::__construct") {
                self.resolve_fqn_lazy_with_fallback(class_fqn, RefKind::ClassName)
                    .await
            } else {
                None
            }
        } else {
            symbol_info
        };

        let result = if let Some(sym) = symbol_info {
            self.location_for_symbol_selection(&sym, "gotoDefinition source read")
                .await
                .map(GotoDefinitionResponse::Scalar)
        } else if let Some(virtual_member) =
            phpdoc_virtual_member_for_symbol(&self.index, &sym_at_pos)
        {
            self.phpdoc_virtual_member_location(&virtual_member)
                .await
                .map(GotoDefinitionResponse::Scalar)
        } else if let Some(virtual_member) = framework_virtual_member_for_symbol(
            &self.index,
            &sym_at_pos,
            Some(&uri_str),
            Some(&file_symbols),
            Some(&source),
        ) {
            self.framework_virtual_member_location(&virtual_member)
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
        let sym = symbol_at_position(tree, &source, pos.line, byte_col, &file_symbols)?;
        let use_stmt = imported_use_statement_for_symbol(&file_symbols, &sym)?;
        let range = range_byte_to_utf16(&source, use_stmt.range);

        Some(GotoDefinitionResponse::Scalar(Location {
            uri: uri.clone(),
            range: Range {
                start: Position::new(range.0, range.1),
                end: Position::new(range.2, range.3),
            },
        }))
    }

    pub(in crate::server) fn file_symbols_for_uri(
        &self,
        uri_str: &str,
    ) -> Option<php_lsp_types::FileSymbols> {
        if let Some(file_symbols) = self.index.file_symbols.get(uri_str) {
            return Some(file_symbols.value().clone());
        }

        let parser = self.open_files.get(uri_str)?;
        let tree = parser.tree()?;
        let source = parser.source();
        Some(extract_file_symbols(tree, &source, uri_str))
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

    async fn stub_source_for_uri(&self, uri_str: &str, label: &'static str) -> Option<String> {
        let rest = uri_str.strip_prefix("phpstub://")?;
        let (extension, file_name) = rest.split_once('/')?;
        if extension.is_empty() || file_name.is_empty() || file_name.contains('/') {
            return None;
        }

        let client_stubs_path = self.stubs_path.lock().await.clone();
        let root = self
            .workspace_root
            .lock()
            .await
            .clone()
            .or_else(|| std::env::current_dir().ok())?;

        for stubs_path in candidate_stubs_paths(&root, client_stubs_path.clone()) {
            let path = stubs_path.join(extension).join(file_name);
            if path.is_file() {
                if let Ok(source) = read_file_to_string_blocking(path, label).await {
                    return Some(source);
                }
            }
        }

        None
    }

    pub(in crate::server) async fn location_for_symbol_selection(
        &self,
        symbol: &php_lsp_types::SymbolInfo,
        label: &'static str,
    ) -> Option<Location> {
        let source = self.source_for_uri(&symbol.uri, label).await?;
        Some(Location {
            uri: symbol.uri.parse::<Uri>().ok()?,
            range: range_from_byte_range(&source, symbol.selection_range),
        })
    }

    pub(in crate::server) async fn phpdoc_virtual_member_location(
        &self,
        member: &PhpDocVirtualMember,
    ) -> Option<Location> {
        let source = self
            .source_for_uri(&member.owner.uri, "phpdoc virtual member source read")
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
        member: &crate::framework::VirtualMember,
    ) -> Option<Location> {
        let (uri, range) = member.sources.iter().find_map(|source| match source {
            crate::framework::VirtualMemberSource::SourceRange { uri, range } => {
                Some((uri.clone(), *range))
            }
            crate::framework::VirtualMemberSource::Synthetic { .. } => None,
        })?;
        let source = self
            .source_for_uri(&uri, "framework virtual member source read")
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

    pub(in crate::server) fn framework_string_key_items(
        &self,
        workspace_root: Option<&Path>,
        namespace_map: Option<&NamespaceMap>,
        uri_str: &str,
        file_symbols: &php_lsp_types::FileSymbols,
        source: &str,
        context: &FrameworkStringKeyAtPosition,
    ) -> Vec<lsp_types::CompletionItem> {
        let Some(workspace_root) = workspace_root else {
            return Vec::new();
        };
        let framework_ctx = crate::framework::FrameworkProviderContext::new(&self.index)
            .with_workspace(Some(workspace_root), namespace_map)
            .with_source_uri(Some(uri_str))
            .with_file(Some(file_symbols), Some(source))
            .with_relevant_files(&[]);
        let registry = crate::framework::default_framework_provider_registry();
        let query = crate::framework::FrameworkStringKeyQuery {
            domain: context.domain.to_string(),
            prefix: context.prefix.clone(),
        };

        registry
            .string_keys(&framework_ctx, &query)
            .into_iter()
            .map(|key| framework_string_key_completion_item(&key, &context.prefix))
            .collect()
    }

    pub(in crate::server) async fn framework_string_key_location(
        &self,
        uri_str: &str,
        file_symbols: &php_lsp_types::FileSymbols,
        source: &str,
        context: &FrameworkStringKeyAtPosition,
    ) -> Option<Location> {
        let workspace_root = self.workspace_root_for_uri(uri_str).await?;
        let namespace_map = self.namespace_map.lock().await.clone();
        let source_range = {
            let framework_ctx = crate::framework::FrameworkProviderContext::new(&self.index)
                .with_workspace(Some(workspace_root.as_path()), namespace_map.as_ref())
                .with_source_uri(Some(uri_str))
                .with_file(Some(file_symbols), Some(source))
                .with_relevant_files(&[]);
            let registry = crate::framework::default_framework_provider_registry();
            let query = crate::framework::FrameworkStringKeyQuery {
                domain: context.domain.to_string(),
                prefix: context.key.clone(),
            };

            registry
                .string_keys(&framework_ctx, &query)
                .into_iter()
                .find(|key| key.key == context.key)
                .and_then(|key| framework_string_key_source_byte_range(&key))
        };
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
            .file_symbols_for_uri(&symbol.uri)
            .unwrap_or_else(|| fallback_file_symbols.clone());

        first_type_definition_fqn(
            return_type,
            &declaring_file_symbols,
            symbol.parent_fqn.as_deref(),
        )
    }

    pub(in crate::server) async fn location_for_type_fqn(&self, fqn: &str) -> Option<Location> {
        if is_builtin_type_name(fqn) {
            return None;
        }

        let symbol = self
            .resolve_fqn_lazy_with_fallback(fqn, RefKind::ClassName)
            .await?;
        self.location_for_symbol_selection(&symbol, "type definition source read")
            .await
    }

    pub(in crate::server) fn reference_locations_for_symbol(
        &self,
        target_fqn: &str,
        target_kind: php_lsp_types::PhpSymbolKind,
        include_declaration: bool,
    ) -> Vec<Location> {
        let mut locations = Vec::new();
        let indexed_references: Vec<_> = self
            .index
            .file_references
            .iter()
            .map(|entry| entry.key().clone())
            .collect();

        for file_uri in indexed_references {
            for reference in
                self.references_for_file(&file_uri, target_fqn, target_kind, include_declaration)
            {
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
