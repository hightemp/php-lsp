//! Hover LSP handlers extracted from `server.rs`.

use super::super::*;

impl PhpLspBackend {
    pub(crate) async fn lsp_hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let uri_str = uri.as_str().to_string();
        let original_pos = params.text_document_position_params.position;
        let template_document = self.template_document(&uri_str);
        let pos = if let Some(template) = &template_document {
            match template.map_original_position_to_virtual(original_pos) {
                Some(pos) => pos,
                None => return Ok(None),
            }
        } else {
            original_pos
        };
        tracing::debug!("hover: {}:{}:{}", uri_str, pos.line, pos.character);

        // Extract symbol-at-position and local variable hover info inside a block so DashMap guard is dropped.
        let (sym_at_pos, local_var_hover, shape_member_hover, file_symbols, source) = {
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

            // Get file symbols for name resolution
            let file_symbols = self
                .index
                .file_symbols
                .get(&uri_str)
                .map(|entry| entry.value().clone())
                .unwrap_or_default();
            let type_cache =
                RequestTypeCache::new(&uri_str, self.current_document_version(&uri_str));

            // Build a cross-file type resolver for method chain resolution
            let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
                type_cache.cached_string(
                    (0, 0, 0, 0),
                    "member-type",
                    format!("{class_fqn}::{member_name}"),
                    || self.resolve_member_type(class_fqn, member_name),
                )
            };
            let callable_param_resolver = |ctx: CallableParameterContext<'_>| {
                resolve_callable_parameter_type_from_index(&self.index, &file_symbols, ctx)
            };

            let ctx = InlayHintContext {
                tree,
                source: &source,
                file_symbols: &file_symbols,
                index: &self.index,
                type_cache: &type_cache,
                utf16_index: &utf16_index,
                requested_range: (0, 0, u32::MAX, u32::MAX),
                allow_twig_property_accessors: template_document
                    .as_ref()
                    .is_some_and(|template| template.kind() == crate::template::TemplateKind::Twig),
                allow_blocking_file_io: false,
            };
            let variable_node_at_position = variable_name_node_at_range(
                tree,
                &source,
                (pos.line, byte_col, pos.line, byte_col),
            );
            let local_var_hover = variable_node_at_position
                .and_then(|variable_node| local_variable_hover_data(&ctx, variable_node));

            let inferred_member_symbol = server_member_symbol_at_position(&ctx, pos.line, byte_col);
            let shape_member_hover = shape_member_access_info_at_position(&ctx, pos.line, byte_col);

            // Find symbol at cursor position (with resolver for chains)
            let primary_sym_at_pos = symbol_at_position_with_request_cache(
                &type_cache,
                tree,
                &source,
                pos.line,
                byte_col,
                &file_symbols,
                "hover",
                Some(&resolver),
                Some(&callable_param_resolver),
            );
            let sym_at_pos = match primary_sym_at_pos {
                Some(s)
                    if matches!(s.ref_kind, RefKind::MethodCall | RefKind::PropertyAccess)
                        && self.index.resolve_fqn(&s.fqn).is_none() =>
                {
                    inferred_member_symbol.unwrap_or(s)
                }
                Some(s) => s,
                None => {
                    if let Some(sym) = inferred_member_symbol {
                        sym
                    } else {
                        let Some(variable_node) = variable_node_at_position else {
                            return Ok(None);
                        };
                        let Some(variable_name) = variable_text_for_node(&source, variable_node)
                        else {
                            return Ok(None);
                        };
                        SymbolAtPosition {
                            fqn: variable_name.clone(),
                            name: variable_name,
                            ref_kind: RefKind::Variable,
                            object_expr: None,
                            range: node_range_node(variable_node),
                        }
                    }
                }
            };

            (
                sym_at_pos,
                local_var_hover,
                shape_member_hover,
                file_symbols,
                source,
            )
        };

        // Look up symbol in index (with lazy vendor fallback)
        let symbol_info = match sym_at_pos.ref_kind {
            RefKind::Variable => None, // Variables are local, handled by gotoDefinition.
            _ => {
                let info = self
                    .resolve_fqn_lazy_with_fallback(&sym_at_pos.fqn, sym_at_pos.ref_kind)
                    .await;
                // For constructor refs, fall back to the class if __construct is
                // not explicitly defined.
                if info.is_none() && sym_at_pos.ref_kind == RefKind::Constructor {
                    if let Some(class_fqn) = sym_at_pos.fqn.strip_suffix("::__construct") {
                        self.resolve_fqn_lazy_with_fallback(class_fqn, RefKind::ClassName)
                            .await
                    } else {
                        None
                    }
                } else {
                    info
                }
            }
        };
        let symbol_info = symbol_info.or_else(|| {
            template_document
                .as_ref()
                .is_some_and(|template| template.kind() == crate::template::TemplateKind::Twig)
                .then(|| twig_property_accessor_method_for_symbol(&self.index, &sym_at_pos))
                .flatten()
        });

        let virtual_member = if symbol_info.is_none() {
            phpdoc_virtual_member_for_symbol(&self.index, &sym_at_pos)
        } else {
            None
        };
        let framework_virtual_member = if symbol_info.is_none() && virtual_member.is_none() {
            framework_virtual_member_for_symbol(
                &self.index,
                &sym_at_pos,
                Some(&uri_str),
                Some(&file_symbols),
                Some(&source),
            )
        } else {
            None
        };
        let magic_property_hover = if symbol_info.is_none()
            && virtual_member.is_none()
            && framework_virtual_member.is_none()
        {
            magic_property_hover_markdown(&self.index, &file_symbols, &sym_at_pos)
        } else {
            None
        };

        let hover_range = range_from_byte_range(&source, sym_at_pos.range);
        let result = if let Some(sym) = symbol_info {
            // Build hover content
            let mut content = String::new();
            let hover_file_symbols =
                hover_file_symbols_for_uri(&self.index, &file_symbols, &sym.uri);
            let type_owner_fqn = hover_symbol_type_owner_fqn(&sym);

            // Symbol kind label
            let kind_label = match sym.kind {
                php_lsp_types::PhpSymbolKind::Class => "class",
                php_lsp_types::PhpSymbolKind::Interface => "interface",
                php_lsp_types::PhpSymbolKind::Trait => "trait",
                php_lsp_types::PhpSymbolKind::Enum => "enum",
                php_lsp_types::PhpSymbolKind::Function => "function",
                php_lsp_types::PhpSymbolKind::Method => "method",
                php_lsp_types::PhpSymbolKind::Property => "property",
                php_lsp_types::PhpSymbolKind::ClassConstant => "const",
                php_lsp_types::PhpSymbolKind::GlobalConstant => "const",
                php_lsp_types::PhpSymbolKind::EnumCase => "case",
                php_lsp_types::PhpSymbolKind::Namespace => "namespace",
            };

            // PHP code block with signature
            content.push_str("```php\n");
            append_hover_symbol_declaration(&mut content, &sym, kind_label);
            content.push_str("\n```\n");

            let parsed_phpdoc = sym.doc_comment.as_deref().map(parse_phpdoc);

            if let Some(parent_fqn) = sym.parent_fqn.as_deref() {
                append_class_fqn_link_line(
                    &mut content,
                    "Declared in",
                    &self.index,
                    parent_fqn,
                    parent_fqn,
                );
            }

            if let Some(ref sig) = sym.signature {
                let phpdoc_params = parsed_phpdoc
                    .as_ref()
                    .map(|phpdoc| phpdoc.params.as_slice())
                    .unwrap_or(&[]);
                append_signature_parameter_lines(
                    &mut content,
                    &self.index,
                    &hover_file_symbols,
                    type_owner_fqn,
                    &sym.uri,
                    &sig.params,
                    phpdoc_params,
                );
                if let Some(ref ret) = sig.return_type {
                    append_type_link_line(
                        &mut content,
                        "Returns",
                        &self.index,
                        &hover_file_symbols,
                        type_owner_fqn,
                        &sym.uri,
                        ret,
                    );
                }
            }

            // PHPDoc summary
            if let Some(phpdoc) = parsed_phpdoc.as_ref() {
                if let Some(ref summary) = phpdoc.summary {
                    content.push_str("\n---\n\n");
                    content.push_str(summary);
                    content.push('\n');
                }

                // @return
                if let Some(ref ret) = phpdoc.return_type {
                    content.push_str("\n**Returns:** ");
                    content.push_str(&type_info_raw_with_links(
                        &self.index,
                        &hover_file_symbols,
                        type_owner_fqn,
                        &sym.uri,
                        ret,
                    ));
                    content.push('\n');
                }

                for section in phpdoc_extra_markdown_sections_with_links(
                    &self.index,
                    &hover_file_symbols,
                    type_owner_fqn,
                    &sym.uri,
                    phpdoc,
                ) {
                    content.push('\n');
                    content.push_str(&section);
                    content.push('\n');
                }

                // @deprecated
                if let Some(ref dep) = phpdoc.deprecated {
                    content.push_str("\n⚠️ **Deprecated**");
                    if !dep.is_empty() {
                        content.push_str(": ");
                        content.push_str(dep);
                    }
                    content.push('\n');
                }
            }

            Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: content,
                }),
                range: Some(hover_range),
            })
        } else if let Some(virtual_member) = virtual_member {
            Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: phpdoc_virtual_member_markdown_with_links(
                        &self.index,
                        &hover_file_symbols_for_uri(
                            &self.index,
                            &file_symbols,
                            &virtual_member.owner.uri,
                        ),
                        &virtual_member,
                    ),
                }),
                range: Some(hover_range),
            })
        } else if let Some(virtual_member) = framework_virtual_member {
            Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: framework_virtual_member_markdown_with_links(
                        &self.index,
                        &hover_file_symbols_for_owner_fqn(
                            &self.index,
                            &file_symbols,
                            &virtual_member.owner_fqn,
                        ),
                        &virtual_member,
                    ),
                }),
                range: Some(hover_range),
            })
        } else if let Some(content) = magic_property_hover {
            Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: content,
                }),
                range: Some(hover_range),
            })
        } else if let Some(shape_member_hover) = shape_member_hover {
            let display = local_variable_type_info_display(
                &self.index,
                &shape_member_hover.owner_fqn,
                &shape_member_hover.uri,
                &shape_member_hover.type_info,
                &file_symbols,
            );
            let type_hint = LocalVariableInlayType {
                display: display.clone(),
                target_fqn: single_inlay_target_fqn_from_type_info(
                    &self.index,
                    &shape_member_hover.owner_fqn,
                    &shape_member_hover.uri,
                    &shape_member_hover.type_info,
                ),
            };
            let mut content = String::new();
            content.push_str("```php\n");
            content.push_str(&display);
            content.push(' ');
            content.push_str(&shape_member_hover.member_name);
            content.push_str("\n```\n");
            content.push_str("\n**Type:** ");
            content.push_str(&local_variable_type_markdown(&self.index, &type_hint));
            content.push('\n');
            Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: content,
                }),
                range: Some(range_from_byte_range(&source, shape_member_hover.range)),
            })
        } else if let Some(var_info) = local_var_hover {
            let mut content = String::new();
            content.push_str("```php\n");
            if let Some(ref type_hint) = var_info.type_hint {
                content.push_str(&type_hint.display);
                content.push(' ');
                content.push_str(&var_info.variable_name);
            } else {
                content.push_str("variable ");
                content.push_str(&var_info.variable_name);
            }
            content.push_str("\n```\n");

            if let Some(ref type_hint) = var_info.type_hint {
                content.push_str("\n**Type:** ");
                content.push_str(&local_variable_type_markdown(&self.index, type_hint));
                content.push('\n');
            }

            if let Some(ref doc) = var_info.phpdoc_comment {
                let phpdoc = parse_phpdoc(doc);
                let local_type_owner_fqn =
                    current_class_fqn_at_range(&file_symbols, sym_at_pos.range).unwrap_or_default();
                if let Some(ref summary) = phpdoc.summary {
                    content.push_str("\n---\n\n");
                    content.push_str(summary);
                    content.push('\n');
                }
                if let Some(ref var_type) = phpdoc.var_type {
                    content.push_str("\n**@var** ");
                    content.push_str(&type_info_raw_with_links(
                        &self.index,
                        &file_symbols,
                        &local_type_owner_fqn,
                        &uri_str,
                        var_type,
                    ));
                    content.push('\n');
                }
            }

            Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: content,
                }),
                range: Some(hover_range),
            })
        } else {
            None
        };

        Ok(result.map(|mut hover| {
            if let (Some(template), Some(range)) = (&template_document, hover.range) {
                hover.range = template.map_virtual_range_to_original(range);
            }
            hover
        }))
    }
}

fn hover_file_symbols_for_uri(
    index: &WorkspaceIndex,
    fallback: &php_lsp_types::FileSymbols,
    uri: &str,
) -> php_lsp_types::FileSymbols {
    index
        .file_symbols
        .get(uri)
        .map(|entry| entry.value().clone())
        .unwrap_or_else(|| fallback.clone())
}

fn hover_file_symbols_for_owner_fqn(
    index: &WorkspaceIndex,
    fallback: &php_lsp_types::FileSymbols,
    owner_fqn: &str,
) -> php_lsp_types::FileSymbols {
    index
        .resolve_fqn(owner_fqn.trim_start_matches('\\'))
        .map(|symbol| hover_file_symbols_for_uri(index, fallback, &symbol.uri))
        .unwrap_or_else(|| fallback.clone())
}

fn hover_symbol_type_owner_fqn(symbol: &php_lsp_types::SymbolInfo) -> &str {
    if let Some(parent_fqn) = symbol.parent_fqn.as_deref() {
        return parent_fqn;
    }
    if matches!(
        symbol.kind,
        php_lsp_types::PhpSymbolKind::Class
            | php_lsp_types::PhpSymbolKind::Interface
            | php_lsp_types::PhpSymbolKind::Trait
            | php_lsp_types::PhpSymbolKind::Enum
    ) {
        return symbol.fqn.as_str();
    }
    ""
}

fn append_hover_symbol_declaration(
    content: &mut String,
    symbol: &php_lsp_types::SymbolInfo,
    kind_label: &str,
) {
    if let Some(signature) = symbol.signature.as_ref() {
        content.push_str(&hover_signature_prefix(symbol, kind_label));
        content.push(' ');
        content.push_str(&symbol.fqn);
        if signature.params.is_empty() {
            content.push_str("()");
        } else {
            content.push_str("(\n");
            for (index, param) in signature.params.iter().enumerate() {
                content.push_str("    ");
                content.push_str(&format_signature_param(param));
                if index + 1 != signature.params.len() {
                    content.push(',');
                }
                content.push('\n');
            }
            content.push(')');
        }
        if let Some(return_type) = signature.return_type.as_ref() {
            content.push_str(": ");
            content.push_str(&return_type.to_string());
        }
        return;
    }

    content.push_str(&hover_symbol_prefix(symbol, kind_label));
    content.push(' ');
    content.push_str(&symbol.fqn);
}

fn hover_signature_prefix(symbol: &php_lsp_types::SymbolInfo, kind_label: &str) -> String {
    match symbol.kind {
        php_lsp_types::PhpSymbolKind::Method => {
            let mut parts = vec![hover_visibility_label(symbol.visibility)];
            push_hover_member_modifiers(&mut parts, symbol);
            parts.push("function");
            parts.join(" ")
        }
        php_lsp_types::PhpSymbolKind::Function => "function".to_string(),
        _ => hover_symbol_prefix(symbol, kind_label),
    }
}

fn hover_symbol_prefix(symbol: &php_lsp_types::SymbolInfo, kind_label: &str) -> String {
    let mut parts = Vec::new();
    match symbol.kind {
        php_lsp_types::PhpSymbolKind::Method
        | php_lsp_types::PhpSymbolKind::Property
        | php_lsp_types::PhpSymbolKind::ClassConstant => {
            parts.push(hover_visibility_label(symbol.visibility));
            push_hover_member_modifiers(&mut parts, symbol);
        }
        php_lsp_types::PhpSymbolKind::Class
        | php_lsp_types::PhpSymbolKind::Interface
        | php_lsp_types::PhpSymbolKind::Trait
        | php_lsp_types::PhpSymbolKind::Enum => {
            if symbol.modifiers.is_abstract {
                parts.push("abstract");
            }
            if symbol.modifiers.is_final {
                parts.push("final");
            }
            if symbol.modifiers.is_readonly {
                parts.push("readonly");
            }
        }
        _ => {}
    }
    parts.push(kind_label);
    parts.join(" ")
}

fn push_hover_member_modifiers(parts: &mut Vec<&str>, symbol: &php_lsp_types::SymbolInfo) {
    if symbol.modifiers.is_abstract {
        parts.push("abstract");
    }
    if symbol.modifiers.is_final {
        parts.push("final");
    }
    if symbol.modifiers.is_static {
        parts.push("static");
    }
    if symbol.modifiers.is_readonly {
        parts.push("readonly");
    }
}

fn hover_visibility_label(visibility: php_lsp_types::Visibility) -> &'static str {
    match visibility {
        php_lsp_types::Visibility::Public => "public",
        php_lsp_types::Visibility::Protected => "protected",
        php_lsp_types::Visibility::Private => "private",
    }
}
