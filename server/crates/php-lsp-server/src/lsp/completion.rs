//! Completion LSP handlers extracted from `server.rs`.

use super::super::*;

impl PhpLspBackend {
    pub(crate) async fn lsp_signature_help(
        &self,
        params: SignatureHelpParams,
    ) -> Result<Option<SignatureHelp>> {
        let uri_str = params
            .text_document_position_params
            .text_document
            .uri
            .as_str()
            .to_string();
        let pos = params.text_document_position_params.position;
        tracing::debug!("signatureHelp: {}:{}:{}", uri_str, pos.line, pos.character);

        let (sym_at_pos, active_parameter) = {
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
            let file_symbols = self
                .index
                .file_symbols
                .get(&uri_str)
                .map(|entry| entry.value().clone())
                .unwrap_or_else(|| extract_file_symbols(tree, &source, &uri_str));

            let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
                self.resolve_member_type(class_fqn, member_name)
            };

            let context = match signature_help_context_at_position(
                tree,
                &source,
                pos.line,
                byte_col,
                &file_symbols,
                Some(&resolver),
            ) {
                Some(context) => context,
                None => return Ok(None),
            };

            (context.symbol, context.active_parameter)
        };

        let symbol_info = self
            .resolve_fqn_lazy_with_fallback(&sym_at_pos.fqn, sym_at_pos.ref_kind)
            .await;

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

        Ok(symbol_info.and_then(|sym| build_signature_help(&sym, active_parameter)))
    }

    pub(crate) async fn lsp_completion(
        &self,
        params: CompletionParams,
    ) -> Result<Option<CompletionResponse>> {
        let uri_str = params
            .text_document_position
            .text_document
            .uri
            .as_str()
            .to_string();
        let original_pos = params.text_document_position.position;
        let template_document = self.template_document(&uri_str);
        if let Some(template) = &template_document {
            if let Some(path_context) =
                template.twig_template_path_context_at_position(original_pos)
            {
                let workspace_root = self.workspace_root_for_uri(&uri_str).await;
                let namespace_map = self.namespace_map.lock().await.clone();
                let file_symbols = php_lsp_types::FileSymbols::default();
                let context = FrameworkStringKeyAtPosition {
                    domain: "twig",
                    prefix: path_context.prefix,
                    key: path_context.key,
                };
                let items: Vec<CompletionItem> = self
                    .framework_string_key_items(
                        workspace_root.as_deref(),
                        namespace_map.as_ref(),
                        &uri_str,
                        &file_symbols,
                        template.original_source(),
                        &context,
                    )
                    .into_iter()
                    .map(framework_string_key_completion_item_to_ls)
                    .collect();
                return if items.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(CompletionResponse::Array(items)))
                };
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
        tracing::debug!("completion: {}:{}:{}", uri_str, pos.line, pos.character);

        let (tree, source) = {
            let parser = match self.open_files.get(&uri_str) {
                Some(p) => p,
                None => return Ok(None),
            };
            let tree = match parser.tree() {
                Some(t) => t.clone(),
                None => return Ok(None),
            };
            (tree, parser.source())
        };
        let byte_col = utf16_col_to_byte(&source, pos.line, pos.character);
        let file_symbols = extract_file_symbols(&tree, &source, &uri_str);
        let framework_string_key_context =
            framework_string_key_context_at_position(&source, pos.line, byte_col);
        let (framework_workspace_root, framework_namespace_map) =
            if framework_string_key_context.is_some() {
                (
                    self.workspace_root_for_uri(&uri_str).await,
                    self.namespace_map.lock().await.clone(),
                )
            } else {
                (None, None)
            };
        let type_cache = RequestTypeCache::new(&uri_str, self.current_document_version(&uri_str));

        // Detect completion context
        let context = detect_context(&tree, &source, pos.line, byte_col, &file_symbols);
        let context = match context {
            php_lsp_completion::context::CompletionContext::MemberAccess {
                object_expr,
                class_fqn,
                member_prefix,
            } => php_lsp_completion::context::CompletionContext::MemberAccess {
                class_fqn: class_fqn.or_else(|| {
                    self.infer_completion_object_type(
                        &object_expr,
                        &tree,
                        &uri_str,
                        &source,
                        &file_symbols,
                        pos.line,
                        byte_col,
                        &type_cache,
                    )
                }),
                object_expr,
                member_prefix,
            },
            other => other,
        };

        if context == php_lsp_completion::context::CompletionContext::None
            && framework_string_key_context.is_none()
        {
            return Ok(None);
        }

        let completion_class_fqn =
            match &context {
                php_lsp_completion::context::CompletionContext::MemberAccess {
                    class_fqn: Some(class_fqn),
                    ..
                } => Some(class_fqn.clone()),
                php_lsp_completion::context::CompletionContext::StaticAccess {
                    class_fqn, ..
                } if !class_fqn.is_empty() => Some(class_fqn.clone()),
                _ => None,
            };

        if let Some(class_fqn) = completion_class_fqn {
            self.lazy_index_class_dependencies(&class_fqn).await;
        }

        let inference_ctx = CompletionInferenceContext {
            tree: &tree,
            source_uri: &uri_str,
            source: &source,
            file_symbols: &file_symbols,
            type_cache: &type_cache,
            line: pos.line,
            byte_col,
        };

        // Get completion items from the provider
        let mut lsp_items = if framework_string_key_context.is_some() {
            Vec::new()
        } else {
            match &context {
                php_lsp_completion::context::CompletionContext::ArrayKey {
                    array_expr,
                    key_prefix,
                } => self.shape_key_completion_items(&inference_ctx, array_expr, key_prefix),
                _ => provide_completions(&context, &self.index, &file_symbols),
            }
        };
        if let Some(ref framework_string_key_context) = framework_string_key_context {
            lsp_items.extend(self.framework_string_key_items(
                framework_workspace_root.as_deref(),
                framework_namespace_map.as_ref(),
                &uri_str,
                &file_symbols,
                &source,
                framework_string_key_context,
            ));
        }
        if let php_lsp_completion::context::CompletionContext::Variable { prefix } = &context {
            add_local_variable_completion_items(
                &mut lsp_items,
                &tree,
                &source,
                pos.line,
                byte_col,
                prefix,
            );
        }
        if let php_lsp_completion::context::CompletionContext::MemberAccess {
            object_expr,
            member_prefix,
            class_fqn,
        } = &context
        {
            self.add_object_shape_completion_items(
                &mut lsp_items,
                &inference_ctx,
                object_expr,
                member_prefix,
            );
            if let Some(class_fqn) = class_fqn {
                let mut seen_labels: HashSet<String> =
                    lsp_items.iter().map(|item| item.label.clone()).collect();
                for member in framework_virtual_member_candidates(
                    &self.index,
                    class_fqn,
                    Some(&uri_str),
                    Some(&file_symbols),
                    Some(&source),
                    None,
                ) {
                    let label = member.name.trim_start_matches('$').to_string();
                    if seen_labels.insert(label) {
                        lsp_items.push(framework_virtual_completion_item(&member, member_prefix));
                    }
                }
            }
        }
        if let php_lsp_completion::context::CompletionContext::StaticAccess {
            class_fqn,
            member_prefix,
            ..
        } = &context
        {
            let mut seen_labels: HashSet<String> =
                lsp_items.iter().map(|item| item.label.clone()).collect();
            for member in framework_virtual_member_candidates(
                &self.index,
                class_fqn,
                Some(&uri_str),
                Some(&file_symbols),
                Some(&source),
                Some(crate::framework::VirtualMemberKind::Method),
            ) {
                let label = member.name.trim_start_matches('$').to_string();
                if seen_labels.insert(label) {
                    lsp_items.push(framework_virtual_completion_item(&member, member_prefix));
                }
            }
        }

        let enable_auto_imports = template_document.is_none()
            && matches!(
                context,
                php_lsp_completion::context::CompletionContext::Free { .. }
                    | php_lsp_completion::context::CompletionContext::Namespace { .. }
            );

        // Convert lsp_types::CompletionItem to ls_types::CompletionItem
        // We need to map between the two different type systems
        let items: Vec<CompletionItem> = lsp_items
            .into_iter()
            .map(|mut item| {
                let kind = item.kind.map(lsp_completion_kind_to_ls);

                let tags = item.tags.map(|tags| {
                    tags.into_iter()
                        .filter_map(|t| {
                            if t == lsp_types::CompletionItemTag::DEPRECATED {
                                Some(CompletionItemTag::DEPRECATED)
                            } else {
                                None
                            }
                        })
                        .collect()
                });

                let auto_import_edit = if enable_auto_imports {
                    item.data
                        .as_ref()
                        .and_then(|data| data.as_str())
                        .and_then(|fqn| self.index.resolve_fqn(fqn))
                        .and_then(|sym| {
                            build_completion_auto_import_edit(&source, &file_symbols, &sym)
                        })
                } else {
                    None
                };
                let mut additional_text_edits: Vec<TextEdit> = item
                    .additional_text_edits
                    .take()
                    .unwrap_or_default()
                    .into_iter()
                    .map(lsp_text_edit_to_ls)
                    .collect();
                if let Some(edit) = auto_import_edit {
                    additional_text_edits.insert(0, edit);
                }
                let additional_text_edits =
                    (!additional_text_edits.is_empty()).then_some(additional_text_edits);

                CompletionItem {
                    label: item.label,
                    kind,
                    detail: item.detail,
                    sort_text: item.sort_text,
                    filter_text: item.filter_text,
                    insert_text: item.insert_text,
                    insert_text_format: item.insert_text_format.map(lsp_insert_text_format_to_ls),
                    additional_text_edits,
                    commit_characters: item.commit_characters,
                    tags,
                    data: item.data,
                    ..Default::default()
                }
            })
            .collect();

        if items.is_empty() {
            Ok(None)
        } else {
            Ok(Some(CompletionResponse::Array(items)))
        }
    }

    pub(crate) async fn lsp_completion_resolve(
        &self,
        mut item: CompletionItem,
    ) -> Result<CompletionItem> {
        if framework_virtual_completion_data(&item).is_some() {
            return Ok(item);
        }

        let virtual_data =
            phpdoc_virtual_completion_data(&item).map(|(owner_fqn, member_kind, member_name)| {
                (
                    owner_fqn.to_string(),
                    member_kind.to_string(),
                    member_name.to_string(),
                )
            });
        if let Some((owner_fqn, member_kind, member_name)) = virtual_data {
            let kind = match member_kind.as_str() {
                "property" => PhpDocVirtualMemberKind::Property,
                "method" => PhpDocVirtualMemberKind::Method,
                _ => return Ok(item),
            };
            if let Some(member) = phpdoc_virtual_member(&self.index, &owner_fqn, &member_name, kind)
            {
                item.detail = Some(match member.kind {
                    PhpDocVirtualMemberKind::Property => {
                        let access = member
                            .access
                            .map(phpdoc_property_tag)
                            .unwrap_or("@property");
                        match &member.type_info {
                            Some(type_info) => format!("{} {}", access, type_info),
                            None => access.to_string(),
                        }
                    }
                    PhpDocVirtualMemberKind::Method => {
                        let mut detail = String::from("()");
                        if let Some(ref return_type) = member.return_type {
                            detail.push_str(": ");
                            detail.push_str(&return_type.to_string());
                        }
                        detail
                    }
                });
                item.documentation = Some(Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: phpdoc_virtual_member_markdown(&member),
                }));
            }
            return Ok(item);
        }

        // Try to resolve more details for the completion item
        // The FQN is stored in item.data
        if let Some(ref data) = item.data {
            if let Some(fqn) = data.as_str() {
                if let Some(sym) = self.resolve_fqn_lazy(fqn).await {
                    // Add full documentation
                    let mut doc_parts = Vec::new();

                    // Signature
                    if let Some(ref sig) = sym.signature {
                        let params_str: Vec<String> = sig
                            .params
                            .iter()
                            .map(|p| {
                                let mut s = String::new();
                                if let Some(ref t) = p.type_info {
                                    s.push_str(&t.to_string());
                                    s.push(' ');
                                }
                                if p.is_variadic {
                                    s.push_str("...");
                                }
                                if p.is_by_ref {
                                    s.push('&');
                                }
                                s.push('$');
                                s.push_str(&p.name);
                                if let Some(ref default) = p.default_value {
                                    s.push_str(" = ");
                                    s.push_str(default);
                                }
                                s
                            })
                            .collect();
                        let mut sig_str = format!("({})", params_str.join(", "));
                        if let Some(ref ret) = sig.return_type {
                            sig_str.push_str(&format!(": {}", ret));
                        }
                        item.detail = Some(sig_str);
                    }

                    // PHPDoc
                    if let Some(ref doc) = sym.doc_comment {
                        let phpdoc = parse_phpdoc(doc);
                        if let Some(ref summary) = phpdoc.summary {
                            doc_parts.push(summary.clone());
                        }

                        if phpdoc.deprecated.is_some() {
                            doc_parts.push("**@deprecated**".to_string());
                            if let Some(ref tags) = item.tags {
                                if !tags.contains(&CompletionItemTag::DEPRECATED) {
                                    let mut tags = tags.clone();
                                    tags.push(CompletionItemTag::DEPRECATED);
                                    item.tags = Some(tags);
                                }
                            } else {
                                item.tags = Some(vec![CompletionItemTag::DEPRECATED]);
                            }
                        }

                        // Param docs
                        if !phpdoc.params.is_empty() {
                            doc_parts.push(String::new());
                            for param in &phpdoc.params {
                                let type_str = param
                                    .type_info
                                    .as_ref()
                                    .map(|t| format!(" `{}`", t))
                                    .unwrap_or_default();
                                let desc = param
                                    .description
                                    .as_ref()
                                    .map(|d| format!(" — {}", d))
                                    .unwrap_or_default();
                                doc_parts
                                    .push(format!("@param{} `${}`{}", type_str, param.name, desc));
                            }
                        }

                        // Return type
                        if let Some(ref ret) = phpdoc.return_type {
                            doc_parts.push(format!("\n@return `{}`", ret));
                        }

                        let extra_sections = phpdoc_extra_markdown_sections(&phpdoc);
                        if !extra_sections.is_empty() {
                            doc_parts.push(String::new());
                            doc_parts.extend(extra_sections);
                        }
                    }

                    if !doc_parts.is_empty() {
                        item.documentation = Some(Documentation::MarkupContent(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: doc_parts.join("\n"),
                        }));
                    }
                }
            }
        }

        Ok(item)
    }
}

impl PhpLspBackend {
    pub(in crate::server) fn resolve_member_type(
        &self,
        class_fqn: &str,
        member_name: &str,
    ) -> Option<String> {
        resolve_member_type_from_index(&self.index, class_fqn, member_name)
    }

    pub(in crate::server) fn resolve_completion_member_type(
        &self,
        class_fqn: &str,
        member_name: &str,
        file_symbols: &php_lsp_types::FileSymbols,
        source_uri: Option<&str>,
        source: Option<&str>,
    ) -> Option<String> {
        self.resolve_member_type(class_fqn, member_name)
            .or_else(|| {
                let member_fqn = format!("{}::{}", class_fqn, member_name);
                let bare_name = member_name.strip_prefix('$').unwrap_or(member_name);
                file_symbols.symbols.iter().find_map(|sym| {
                    if sym.fqn == member_fqn
                        || (sym.parent_fqn.as_deref() == Some(class_fqn)
                            && (sym.name == member_name || sym.name == bare_name))
                    {
                        symbol_return_type_fqn(&self.index, class_fqn, sym)
                    } else {
                        None
                    }
                })
            })
            .or_else(|| {
                framework_virtual_member_type_fqn(
                    &self.index,
                    class_fqn,
                    member_name,
                    source_uri,
                    Some(file_symbols),
                    source,
                )
            })
    }

    pub(in crate::server) fn resolve_completion_member_type_cached(
        &self,
        class_fqn: &str,
        member_name: &str,
        file_symbols: &php_lsp_types::FileSymbols,
        source_uri: Option<&str>,
        source: Option<&str>,
        type_cache: &RequestTypeCache,
    ) -> Option<String> {
        type_cache.cached_string(
            (0, 0, 0, 0),
            "completion-member-type",
            format!("{class_fqn}::{member_name}"),
            || {
                self.resolve_completion_member_type(
                    class_fqn,
                    member_name,
                    file_symbols,
                    source_uri,
                    source,
                )
            },
        )
    }

    pub(in crate::server) fn resolve_completion_member_call_type(
        &self,
        class_fqn: &str,
        member_name: &str,
        member_text: &str,
        file_symbols: &php_lsp_types::FileSymbols,
        type_cache: &RequestTypeCache,
    ) -> Option<String> {
        type_cache.cached_string(
            (0, 0, 0, 0),
            "completion-member-call-type",
            format!("{class_fqn}::{member_name}:{member_text}"),
            || {
                let symbol = self
                    .index
                    .resolve_member(&format!("{}::{}", class_fqn, member_name))
                    .or_else(|| {
                        let member_fqn = format!("{}::{}", class_fqn, member_name);
                        file_symbols.symbols.iter().find_map(|sym| {
                            (sym.fqn == member_fqn
                                || (sym.parent_fqn.as_deref() == Some(class_fqn)
                                    && sym.name == member_name))
                                .then(|| Arc::new(sym.clone()))
                        })
                    })?;
                let signature = symbol.signature.as_ref()?;
                let return_type = signature.return_type.as_ref()?;
                let arguments = completion_call_arguments_by_param(
                    member_text,
                    signature,
                    file_symbols,
                    &self.index,
                );
                let template_names: HashSet<String> = symbol
                    .templates
                    .iter()
                    .map(|template| template.name.clone())
                    .collect();
                let substitutions =
                    call_site_template_substitutions(&arguments, signature, &template_names);
                let resolved = resolve_call_site_type_info(
                    return_type,
                    &arguments,
                    &template_names,
                    &substitutions,
                );
                type_info_fqn_from_index(&self.index, class_fqn, &symbol.uri, &resolved)
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::server) fn infer_completion_object_type(
        &self,
        object_expr: &str,
        tree: &tree_sitter::Tree,
        source_uri: &str,
        source: &str,
        file_symbols: &php_lsp_types::FileSymbols,
        line: u32,
        byte_col: u32,
        type_cache: &RequestTypeCache,
    ) -> Option<String> {
        type_cache.cached_string(
            (line, byte_col, line, byte_col),
            "completion-object-type",
            object_expr,
            || {
                let object_expr = object_expr.trim();
                if let Some(class_fqn) = infer_new_expression_type(object_expr, file_symbols) {
                    return Some(class_fqn);
                }
                if let Some(class_fqn) = infer_static_call_expression_type(
                    object_expr,
                    file_symbols,
                    |class_fqn, method_name| {
                        self.resolve_completion_member_type_cached(
                            class_fqn,
                            method_name,
                            file_symbols,
                            Some(source_uri),
                            Some(source),
                            type_cache,
                        )
                    },
                ) {
                    return Some(class_fqn);
                }

                if object_expr.contains("->") || object_expr.contains("?->") {
                    return self.infer_completion_member_chain_type(
                        object_expr,
                        tree,
                        source_uri,
                        source,
                        file_symbols,
                        line,
                        byte_col,
                        type_cache,
                    );
                }

                if object_expr == "$this" {
                    current_class_fqn_at_range(file_symbols, (line, byte_col, line, byte_col))
                        .or_else(|| current_class_fqn(file_symbols))
                } else if object_expr.starts_with('$') {
                    self.infer_completion_variable_type(
                        tree,
                        source_uri,
                        source,
                        file_symbols,
                        line,
                        byte_col,
                        object_expr,
                        type_cache,
                    )
                } else {
                    None
                }
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::server) fn infer_completion_variable_type(
        &self,
        tree: &tree_sitter::Tree,
        source_uri: &str,
        source: &str,
        file_symbols: &php_lsp_types::FileSymbols,
        line: u32,
        byte_col: u32,
        var_name: &str,
        type_cache: &RequestTypeCache,
    ) -> Option<String> {
        type_cache.cached_string(
            (line, byte_col, line, byte_col),
            "completion-variable-type",
            var_name,
            || {
                let resolve_member_type = |class_fqn: &str, member_name: &str| {
                    self.resolve_completion_member_type_cached(
                        class_fqn,
                        member_name,
                        file_symbols,
                        Some(source_uri),
                        Some(source),
                        type_cache,
                    )
                };
                let callable_param_resolver = |ctx: CallableParameterContext<'_>| {
                    resolve_callable_parameter_type_from_index(&self.index, file_symbols, ctx)
                };
                infer_variable_type_at_position_with_resolvers(
                    tree,
                    source,
                    file_symbols,
                    line,
                    byte_col,
                    var_name,
                    Some(&resolve_member_type),
                    Some(&callable_param_resolver),
                )
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::server) fn infer_completion_member_chain_type(
        &self,
        object_expr: &str,
        tree: &tree_sitter::Tree,
        source_uri: &str,
        source: &str,
        file_symbols: &php_lsp_types::FileSymbols,
        line: u32,
        byte_col: u32,
        type_cache: &RequestTypeCache,
    ) -> Option<String> {
        type_cache.cached_string(
            (line, byte_col, line, byte_col),
            "completion-member-chain-type",
            object_expr,
            || {
                let normalized = object_expr.replace("?->", "->");
                let mut parts = normalized.split("->");
                let base_expr = parts.next()?.trim();
                let mut class_fqn = if base_expr == "$this" {
                    current_class_fqn_at_range(file_symbols, (line, byte_col, line, byte_col))
                        .or_else(|| current_class_fqn(file_symbols))?
                } else if base_expr.starts_with('$') {
                    self.infer_completion_variable_type(
                        tree,
                        source_uri,
                        source,
                        file_symbols,
                        line,
                        byte_col,
                        base_expr,
                        type_cache,
                    )?
                } else {
                    infer_new_expression_type(base_expr, file_symbols).or_else(|| {
                        infer_static_call_expression_type(
                            base_expr,
                            file_symbols,
                            |class_fqn, method_name| {
                                self.resolve_completion_member_type_cached(
                                    class_fqn,
                                    method_name,
                                    file_symbols,
                                    Some(source_uri),
                                    Some(source),
                                    type_cache,
                                )
                            },
                        )
                    })?
                };

                for raw_member in parts {
                    let member = raw_member.trim();
                    if member.is_empty() {
                        return None;
                    }

                    let is_method_call = member.contains('(');
                    let member_name = member
                        .split('(')
                        .next()
                        .unwrap_or(member)
                        .trim()
                        .trim_start_matches('$');
                    if member_name.is_empty() {
                        return None;
                    }

                    let lookup_name = if is_method_call {
                        member_name.to_string()
                    } else {
                        format!("${}", member_name)
                    };
                    class_fqn = if is_method_call {
                        self.resolve_completion_member_call_type(
                            &class_fqn,
                            &lookup_name,
                            member,
                            file_symbols,
                            type_cache,
                        )
                        .or_else(|| {
                            self.resolve_completion_member_type_cached(
                                &class_fqn,
                                &lookup_name,
                                file_symbols,
                                Some(source_uri),
                                Some(source),
                                type_cache,
                            )
                        })?
                    } else {
                        self.resolve_completion_member_type_cached(
                            &class_fqn,
                            &lookup_name,
                            file_symbols,
                            Some(source_uri),
                            Some(source),
                            type_cache,
                        )?
                    };
                }

                Some(class_fqn)
            },
        )
    }

    pub(in crate::server) fn infer_completion_type_info(
        &self,
        ctx: &CompletionInferenceContext<'_>,
        expr: &str,
    ) -> Option<php_lsp_types::TypeInfo> {
        ctx.type_cache.cached_type_info(
            (ctx.line, ctx.byte_col, ctx.line, ctx.byte_col),
            "completion-type-info",
            expr,
            || {
                let resolve_member_type = |class_fqn: &str, member_name: &str| {
                    self.resolve_completion_member_type_cached(
                        class_fqn,
                        member_name,
                        ctx.file_symbols,
                        Some(ctx.source_uri),
                        Some(ctx.source),
                        ctx.type_cache,
                    )
                };
                let callable_param_resolver = |callable_ctx: CallableParameterContext<'_>| {
                    resolve_callable_parameter_type_from_index(
                        &self.index,
                        ctx.file_symbols,
                        callable_ctx,
                    )
                };
                infer_variable_type_info_at_position_with_resolvers(
                    ctx.tree,
                    ctx.source,
                    ctx.file_symbols,
                    ctx.line,
                    ctx.byte_col,
                    expr,
                    Some(&resolve_member_type),
                    Some(&callable_param_resolver),
                )
            },
        )
    }

    pub(in crate::server) fn shape_key_completion_items(
        &self,
        ctx: &CompletionInferenceContext<'_>,
        array_expr: &str,
        key_prefix: &str,
    ) -> Vec<lsp_types::CompletionItem> {
        let Some(type_info) = self.infer_completion_type_info(ctx, array_expr) else {
            return Vec::new();
        };

        shape_completion_items_from_type_info(&type_info, ShapeCompletionKind::ArrayKey, key_prefix)
    }

    pub(in crate::server) fn add_object_shape_completion_items(
        &self,
        items: &mut Vec<lsp_types::CompletionItem>,
        ctx: &CompletionInferenceContext<'_>,
        object_expr: &str,
        member_prefix: &str,
    ) {
        let Some(type_info) = self.infer_completion_type_info(ctx, object_expr) else {
            return;
        };

        let mut seen: HashSet<String> = items.iter().map(|item| item.label.clone()).collect();
        for item in shape_completion_items_from_type_info(
            &type_info,
            ShapeCompletionKind::ObjectProperty,
            member_prefix,
        ) {
            if seen.insert(item.label.clone()) {
                items.push(item);
            }
        }
    }
}
