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

            append_hover_symbol_identity_line(&mut content, &sym);
            if let Some(parent_fqn) = sym.parent_fqn.as_deref() {
                append_class_fqn_link_line(
                    &mut content,
                    "Declared in",
                    &self.index,
                    parent_fqn,
                    parent_fqn,
                );
            }
            append_hover_symbol_source_line(&mut content, &sym);
            append_hover_relation_and_template_lines(
                &mut content,
                &self.index,
                &hover_file_symbols,
                &sym,
            );

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
    match symbol.kind {
        php_lsp_types::PhpSymbolKind::Function | php_lsp_types::PhpSymbolKind::Method => {
            append_hover_callable_declaration(content, symbol, kind_label);
        }
        php_lsp_types::PhpSymbolKind::Property => {
            append_hover_property_declaration(content, symbol);
        }
        php_lsp_types::PhpSymbolKind::ClassConstant => {
            append_hover_class_constant_declaration(content, symbol);
        }
        php_lsp_types::PhpSymbolKind::GlobalConstant => {
            append_hover_global_constant_declaration(content, symbol);
        }
        php_lsp_types::PhpSymbolKind::EnumCase => {
            content.push_str("case ");
            content.push_str(&symbol.name);
        }
        _ => {
            content.push_str(&hover_symbol_prefix(symbol, kind_label));
            content.push(' ');
            content.push_str(&hover_source_like_symbol_name(symbol));
        }
    }
}

fn append_hover_callable_declaration(
    content: &mut String,
    symbol: &php_lsp_types::SymbolInfo,
    kind_label: &str,
) {
    if let Some(signature) = symbol.signature.as_ref() {
        content.push_str(&hover_signature_prefix(symbol, kind_label));
        content.push(' ');
        content.push_str(&hover_source_like_symbol_name(symbol));
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
    content.push_str(&hover_source_like_symbol_name(symbol));
}

fn append_hover_property_declaration(content: &mut String, symbol: &php_lsp_types::SymbolInfo) {
    content.push_str(&hover_member_prefix(symbol));
    if let Some(type_info) = symbol
        .signature
        .as_ref()
        .and_then(|signature| signature.return_type.as_ref())
    {
        content.push(' ');
        content.push_str(&type_info.to_string());
    }
    content.push(' ');
    content.push('$');
    content.push_str(symbol.name.trim_start_matches('$'));
}

fn append_hover_class_constant_declaration(
    content: &mut String,
    symbol: &php_lsp_types::SymbolInfo,
) {
    content.push_str(&hover_member_prefix(symbol));
    content.push_str(" const ");
    if let Some(type_info) = symbol
        .signature
        .as_ref()
        .and_then(|signature| signature.return_type.as_ref())
    {
        content.push_str(&type_info.to_string());
        content.push(' ');
    }
    content.push_str(&symbol.name);
}

fn append_hover_global_constant_declaration(
    content: &mut String,
    symbol: &php_lsp_types::SymbolInfo,
) {
    content.push_str("const ");
    if let Some(type_info) = symbol
        .signature
        .as_ref()
        .and_then(|signature| signature.return_type.as_ref())
    {
        content.push_str(&type_info.to_string());
        content.push(' ');
    }
    content.push_str(&hover_source_like_symbol_name(symbol));
}

fn hover_source_like_symbol_name(symbol: &php_lsp_types::SymbolInfo) -> String {
    match symbol.kind {
        php_lsp_types::PhpSymbolKind::Method
        | php_lsp_types::PhpSymbolKind::Property
        | php_lsp_types::PhpSymbolKind::ClassConstant
        | php_lsp_types::PhpSymbolKind::EnumCase
        | php_lsp_types::PhpSymbolKind::Class
        | php_lsp_types::PhpSymbolKind::Interface
        | php_lsp_types::PhpSymbolKind::Trait
        | php_lsp_types::PhpSymbolKind::Enum
        | php_lsp_types::PhpSymbolKind::Function
        | php_lsp_types::PhpSymbolKind::GlobalConstant => symbol.name.clone(),
        php_lsp_types::PhpSymbolKind::Namespace => symbol.fqn.clone(),
    }
}

fn hover_member_prefix(symbol: &php_lsp_types::SymbolInfo) -> String {
    let mut parts = vec![hover_visibility_label(symbol.visibility)];
    push_hover_member_modifiers(&mut parts, symbol);
    parts.join(" ")
}

fn append_hover_symbol_identity_line(content: &mut String, symbol: &php_lsp_types::SymbolInfo) {
    let destination = markdown_file_location_destination(symbol);
    content.push('\n');
    content.push_str("**Symbol:** ");
    content.push_str(&format!(
        "[{}](<{}>)",
        markdown_code_span(&symbol.fqn),
        destination
    ));
    content.push('\n');
}

fn append_hover_symbol_source_line(content: &mut String, symbol: &php_lsp_types::SymbolInfo) {
    let destination = markdown_file_location_destination(symbol);
    let source_label = hover_symbol_source_label(symbol);
    content.push('\n');
    content.push_str("**Source:** ");
    content.push_str(&format!(
        "[{}](<{}>)",
        markdown_code_span(&source_label),
        destination
    ));
    content.push('\n');
}

fn hover_symbol_source_label(symbol: &php_lsp_types::SymbolInfo) -> String {
    let line = symbol.selection_range.0.saturating_add(1);
    if let Some(path) = uri_to_path(&symbol.uri) {
        return format!("{}:{line}", path.display());
    }
    format!("{}:{line}", symbol.uri)
}

fn append_hover_relation_and_template_lines(
    content: &mut String,
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    symbol: &php_lsp_types::SymbolInfo,
) {
    let owner_fqn = hover_symbol_type_owner_fqn(symbol);
    let ctx = HoverRelationContext {
        index,
        file_symbols,
        owner_fqn,
        symbol,
    };
    append_hover_relation_line(
        content,
        "Extends",
        &ctx,
        &symbol.extends,
        php_lsp_types::TemplateBindingKind::Extends,
    );
    append_hover_relation_line(
        content,
        "Implements",
        &ctx,
        &symbol.implements,
        php_lsp_types::TemplateBindingKind::Implements,
    );
    append_hover_relation_line(
        content,
        "Uses",
        &ctx,
        &symbol.traits,
        php_lsp_types::TemplateBindingKind::Use,
    );
    append_hover_relation_line(
        content,
        "Mixins",
        &ctx,
        &[],
        php_lsp_types::TemplateBindingKind::Mixin,
    );
    append_hover_templates_section(content, index, file_symbols, owner_fqn, symbol);
}

struct HoverRelationContext<'a> {
    index: &'a WorkspaceIndex,
    file_symbols: &'a php_lsp_types::FileSymbols,
    owner_fqn: &'a str,
    symbol: &'a php_lsp_types::SymbolInfo,
}

fn append_hover_relation_line(
    content: &mut String,
    label: &str,
    ctx: &HoverRelationContext<'_>,
    native_targets: &[String],
    binding_kind: php_lsp_types::TemplateBindingKind,
) {
    let binding_targets = ctx
        .symbol
        .template_bindings
        .iter()
        .filter(|binding| binding.kind == binding_kind)
        .map(|binding| normalized_hover_relation_target(&binding.target))
        .collect::<std::collections::HashSet<_>>();

    let mut seen = std::collections::HashSet::new();
    let mut entries = Vec::new();
    for binding in ctx
        .symbol
        .template_bindings
        .iter()
        .filter(|binding| binding.kind == binding_kind)
    {
        let key = hover_relation_entry_key(&binding.target, &binding.args);
        if seen.insert(key) {
            entries.push(hover_relation_entry_markdown(
                ctx.index,
                ctx.file_symbols,
                ctx.owner_fqn,
                ctx.symbol,
                &binding.target,
                &binding.args,
            ));
        }
    }

    for target in native_targets {
        if binding_targets.contains(&normalized_hover_relation_target(target)) {
            continue;
        }
        let key = hover_relation_entry_key(target, &[]);
        if seen.insert(key) {
            entries.push(hover_relation_entry_markdown(
                ctx.index,
                ctx.file_symbols,
                ctx.owner_fqn,
                ctx.symbol,
                target,
                &[],
            ));
        }
    }

    if entries.is_empty() {
        return;
    }

    content.push('\n');
    content.push_str("**");
    content.push_str(label);
    content.push_str(":** ");
    content.push_str(&entries.join(", "));
    content.push('\n');
}

fn hover_relation_entry_markdown(
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    owner_fqn: &str,
    symbol: &php_lsp_types::SymbolInfo,
    target: &str,
    args: &[php_lsp_types::TypeInfo],
) -> String {
    let mut entry = hover_type_info_markdown(
        index,
        file_symbols,
        owner_fqn,
        &symbol.uri,
        &php_lsp_types::TypeInfo::Simple(target.to_string()),
    );
    if !args.is_empty() {
        let args = args
            .iter()
            .map(|arg| hover_type_info_markdown(index, file_symbols, owner_fqn, &symbol.uri, arg))
            .collect::<Vec<_>>()
            .join(", ");
        entry.push_str("&lt;");
        entry.push_str(&args);
        entry.push_str("&gt;");
    }
    entry
}

fn append_hover_templates_section(
    content: &mut String,
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    owner_fqn: &str,
    symbol: &php_lsp_types::SymbolInfo,
) {
    if symbol.templates.is_empty() {
        return;
    }

    let lines = symbol
        .templates
        .iter()
        .map(|template| {
            let mut label = String::new();
            match template.variance {
                php_lsp_types::TemplateVariance::Invariant => {}
                php_lsp_types::TemplateVariance::Covariant => label.push_str("covariant "),
                php_lsp_types::TemplateVariance::Contravariant => {
                    label.push_str("contravariant ");
                }
            }
            label.push_str(&template.name);
            let mut line = format!("- {}", markdown_code_span(&label));
            if let Some(bound) = template.bound.as_ref() {
                line.push_str(" of ");
                line.push_str(&hover_type_info_markdown(
                    index,
                    file_symbols,
                    owner_fqn,
                    &symbol.uri,
                    bound,
                ));
            }
            line
        })
        .collect::<Vec<_>>();

    content.push_str("\n**Templates:**\n\n");
    content.push_str(&lines.join("\n"));
    content.push('\n');
}

fn hover_type_info_markdown(
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    owner_fqn: &str,
    uri: &str,
    type_info: &php_lsp_types::TypeInfo,
) -> String {
    markdown_type_info_class_links(index, file_symbols, owner_fqn, uri, type_info)
        .unwrap_or_else(|| markdown_code_span(&type_info.to_string()))
}

fn hover_relation_entry_key(target: &str, args: &[php_lsp_types::TypeInfo]) -> String {
    let mut key = normalized_hover_relation_target(target);
    if !args.is_empty() {
        key.push('<');
        key.push_str(
            &args
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(","),
        );
        key.push('>');
    }
    key
}

fn normalized_hover_relation_target(target: &str) -> String {
    target.trim_start_matches('\\').to_ascii_lowercase()
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
