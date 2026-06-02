//! Inlay Hints LSP handlers extracted from `server.rs`.

use super::super::*;

impl PhpLspBackend {
    pub(crate) async fn lsp_inlay_hint(
        &self,
        params: InlayHintParams,
    ) -> Result<Option<Vec<InlayHint>>> {
        let uri_str = params.text_document.uri.as_str().to_string();
        let php_version = *self.php_version.lock().await;
        let template_document = self.template_document(&uri_str);

        let (tree, source, file_symbols, document_version) = {
            let parser = match self.open_files.get(&uri_str) {
                Some(parser) => parser,
                None => return Ok(None),
            };
            let tree = match parser.tree() {
                Some(tree) => tree.clone(),
                None => return Ok(None),
            };
            let source = parser.source();
            let file_symbols = self
                .index
                .file_symbols
                .get(&uri_str)
                .map(|entry| entry.value().clone())
                .unwrap_or_else(|| extract_file_symbols(&tree, &source, &uri_str));

            (
                tree,
                source,
                file_symbols,
                self.current_document_version(&uri_str),
            )
        };

        let index = self.index.clone();
        let original_requested_range = params.range;
        let requested_range = if template_document.is_some() {
            full_document_range(&source)
        } else {
            original_requested_range
        };
        let compute_uri = uri_str.clone();
        let mut hints =
            match run_file_io_blocking("inlayHint compute", uri_str.clone(), move || {
                inlay_hints(
                    &compute_uri,
                    document_version,
                    &tree,
                    &source,
                    &file_symbols,
                    &index,
                    requested_range,
                    php_version,
                )
            })
            .await
            {
                Ok(hints) => hints,
                Err(message) => {
                    tracing::warn!("{}", message);
                    Vec::new()
                }
            };

        if let Some(template) = &template_document {
            hints = map_inlay_hints_to_template_original(template, original_requested_range, hints);
        }

        self.hydrate_inlay_hint_label_locations(&mut hints).await;

        if hints.is_empty() {
            Ok(None)
        } else {
            Ok(Some(hints))
        }
    }

    async fn hydrate_inlay_hint_label_locations(&self, hints: &mut [InlayHint]) {
        for hint in hints {
            let InlayHintLabel::LabelParts(parts) = &mut hint.label else {
                continue;
            };
            for part in parts {
                if part.location.is_some() {
                    continue;
                }
                let Some(InlayHintLabelPartTooltip::String(fqn)) = part.tooltip.as_ref() else {
                    continue;
                };
                if let Some(location) = self.location_for_type_fqn(fqn).await {
                    part.location = Some(location);
                }
            }
        }
    }
}

fn map_inlay_hints_to_template_original(
    template: &TemplateDocument,
    requested_range: Range,
    hints: Vec<InlayHint>,
) -> Vec<InlayHint> {
    hints
        .into_iter()
        .filter_map(|mut hint| {
            let original_position = template.map_virtual_position_to_original(hint.position)?;
            if !position_in_range(original_position, requested_range) {
                return None;
            }
            hint.position = original_position;

            if let Some(text_edits) = hint.text_edits.take() {
                let mut mapped_edits = Vec::with_capacity(text_edits.len());
                for mut edit in text_edits {
                    edit.range = template.map_virtual_range_to_original(edit.range)?;
                    mapped_edits.push(edit);
                }
                hint.text_edits = Some(mapped_edits);
            }

            Some(hint)
        })
        .collect()
}

fn full_document_range(source: &str) -> Range {
    let line = source.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let line_start = source.rfind('\n').map_or(0, |idx| idx + 1);
    Range {
        start: Position::new(0, 0),
        end: Position::new(line, source[line_start..].encode_utf16().count() as u32),
    }
}

fn position_in_range(position: Position, range: Range) -> bool {
    position_after_or_equal(position, range.start) && position_before_or_equal(position, range.end)
}

fn position_after_or_equal(left: Position, right: Position) -> bool {
    (left.line, left.character) >= (right.line, right.character)
}

fn position_before_or_equal(left: Position, right: Position) -> bool {
    (left.line, left.character) <= (right.line, right.character)
}

#[allow(clippy::too_many_arguments)]
pub(in crate::server) fn inlay_hints(
    uri_str: &str,
    document_version: Option<i32>,
    tree: &tree_sitter::Tree,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    requested_range: Range,
    php_version: PhpVersion,
) -> Vec<InlayHint> {
    let utf16_index = Utf16LineIndex::new(source);
    let byte_range = lsp_range_to_byte_range(source, requested_range);
    let mut hints = Vec::new();
    let type_cache = RequestTypeCache::new(uri_str, document_version);
    let ctx = InlayHintContext {
        tree,
        source,
        file_symbols,
        index,
        type_cache: &type_cache,
        utf16_index: &utf16_index,
        requested_range: byte_range,
    };

    collect_call_argument_inlay_hints(&ctx, tree.root_node(), &mut hints);
    collect_local_variable_type_inlay_hints(&ctx, tree.root_node(), &mut hints);
    collect_phpdoc_parameter_type_inlay_hints(
        tree.root_node(),
        source,
        &utf16_index,
        byte_range,
        &mut hints,
    );
    collect_phpdoc_return_type_inlay_hints(
        tree,
        source,
        &utf16_index,
        byte_range,
        php_version,
        &mut hints,
    );

    hints.sort_by(|left, right| {
        (
            left.position.line,
            left.position.character,
            inlay_hint_label_text(&left.label),
        )
            .cmp(&(
                right.position.line,
                right.position.character,
                inlay_hint_label_text(&right.label),
            ))
    });
    hints
}

pub(in crate::server) struct InlayHintContext<'a> {
    pub(in crate::server) tree: &'a tree_sitter::Tree,
    pub(in crate::server) source: &'a str,
    pub(in crate::server) file_symbols: &'a php_lsp_types::FileSymbols,
    pub(in crate::server) index: &'a WorkspaceIndex,
    pub(in crate::server) type_cache: &'a RequestTypeCache,
    pub(in crate::server) utf16_index: &'a Utf16LineIndex,
    pub(in crate::server) requested_range: (u32, u32, u32, u32),
}

pub(in crate::server) fn collect_call_argument_inlay_hints(
    ctx: &InlayHintContext<'_>,
    node: tree_sitter::Node,
    hints: &mut Vec<InlayHint>,
) {
    if matches!(
        node.kind(),
        "function_call_expression"
            | "member_call_expression"
            | "nullsafe_member_call_expression"
            | "scoped_call_expression"
            | "object_creation_expression"
    ) {
        if let Some(callable) = resolve_callable_for_inlay_hint(ctx, node) {
            add_call_argument_inlay_hints(
                node,
                &callable,
                ctx.source,
                ctx.utf16_index,
                ctx.requested_range,
                hints,
            );
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_call_argument_inlay_hints(ctx, child, hints);
    }
}

pub(in crate::server) fn resolve_callable_for_inlay_hint(
    ctx: &InlayHintContext<'_>,
    node: tree_sitter::Node,
) -> Option<Arc<php_lsp_types::SymbolInfo>> {
    let name_node = call_target_name_node(node)?;
    let (_, sym) = resolve_reference_symbol_at_node_cached(
        ctx.tree,
        ctx.source,
        name_node,
        ctx.file_symbols,
        ctx.index,
        ctx.type_cache,
    )?;
    matches!(
        sym.kind,
        php_lsp_types::PhpSymbolKind::Function | php_lsp_types::PhpSymbolKind::Method
    )
    .then_some(sym)
}

pub(in crate::server) fn call_target_name_node(
    node: tree_sitter::Node,
) -> Option<tree_sitter::Node> {
    match node.kind() {
        "function_call_expression" => node
            .child_by_field_name("function")
            .or_else(|| node.named_child(0)),
        "member_call_expression" | "nullsafe_member_call_expression" | "scoped_call_expression" => {
            member_reference_name_node(node)
        }
        "object_creation_expression" => object_creation_class_node(node),
        _ => None,
    }
}

pub(in crate::server) fn add_call_argument_inlay_hints(
    call_node: tree_sitter::Node,
    callable: &php_lsp_types::SymbolInfo,
    source: &str,
    utf16_index: &Utf16LineIndex,
    requested_range: (u32, u32, u32, u32),
    hints: &mut Vec<InlayHint>,
) {
    let Some(signature) = callable.signature.as_ref() else {
        return;
    };

    for (arg_index, argument) in call_arguments(call_node, source).into_iter().enumerate() {
        if argument.name.is_some() {
            continue;
        }
        let Some(param) = signature_param_for_arg(signature, arg_index) else {
            continue;
        };
        if param.name.is_empty() {
            continue;
        }
        let arg_range = node_range_node(argument.value_node);
        if !byte_ranges_overlap(arg_range, requested_range) {
            continue;
        }
        let start = argument.value_node.start_position();
        hints.push(InlayHint {
            position: Position::new(
                start.row as u32,
                utf16_index.byte_col_to_utf16(start.row as u32, start.column as u32),
            ),
            label: InlayHintLabel::String(format!("{}:", param.name)),
            kind: Some(InlayHintKind::PARAMETER),
            text_edits: None,
            tooltip: Some(InlayHintTooltip::String(callable.fqn.clone())),
            padding_left: Some(false),
            padding_right: Some(true),
            data: None,
        });
    }
}

pub(in crate::server) fn collect_local_variable_type_inlay_hints(
    ctx: &InlayHintContext<'_>,
    node: tree_sitter::Node,
    hints: &mut Vec<InlayHint>,
) {
    let mut seen = HashSet::new();
    collect_local_variable_type_inlay_hints_inner(ctx, node, hints, &mut seen);
}

pub(in crate::server) fn collect_local_variable_type_inlay_hints_inner(
    ctx: &InlayHintContext<'_>,
    node: tree_sitter::Node,
    hints: &mut Vec<InlayHint>,
    seen: &mut HashSet<(u32, u32, String)>,
) {
    match node.kind() {
        "expression_statement" => {
            add_assignment_variable_type_inlay_hint(ctx, node, hints, seen);
        }
        "foreach_statement" => {
            add_foreach_variable_type_inlay_hint(ctx, node, hints, seen);
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_local_variable_type_inlay_hints_inner(ctx, child, hints, seen);
    }
}

pub(in crate::server) fn add_assignment_variable_type_inlay_hint(
    ctx: &InlayHintContext<'_>,
    statement: tree_sitter::Node,
    hints: &mut Vec<InlayHint>,
    seen: &mut HashSet<(u32, u32, String)>,
) {
    let Some(expr) = statement.named_child(0) else {
        return;
    };
    if expr.kind() != "assignment_expression" {
        return;
    }
    let Some(left) = expr.child_by_field_name("left") else {
        return;
    };
    let Some(right) = expr.child_by_field_name("right") else {
        return;
    };
    if left.kind() != "variable_name"
        || !is_plain_assignment_expression(left, right, ctx.source)
        || !byte_ranges_overlap(node_range_node(left), ctx.requested_range)
    {
        return;
    }

    add_local_variable_type_inlay_hint(ctx, left, right.end_byte(), Some(right), hints, seen);
}

pub(in crate::server) fn add_foreach_variable_type_inlay_hint(
    ctx: &InlayHintContext<'_>,
    statement: tree_sitter::Node,
    hints: &mut Vec<InlayHint>,
    seen: &mut HashSet<(u32, u32, String)>,
) {
    let Some(value_node) = foreach_value_variable_node_for_inlay(statement, ctx.source) else {
        return;
    };
    if !byte_ranges_overlap(node_range_node(value_node), ctx.requested_range) {
        return;
    }

    add_local_variable_type_inlay_hint(ctx, value_node, value_node.end_byte(), None, hints, seen);
}

#[derive(Debug, Clone)]
pub(in crate::server) struct LocalVariableInlayType {
    pub(in crate::server) display: String,
    pub(in crate::server) target_fqn: Option<String>,
}

#[derive(Debug, Clone)]
pub(in crate::server) struct LocalVariableHoverData {
    pub(in crate::server) variable_name: String,
    pub(in crate::server) type_hint: Option<LocalVariableInlayType>,
    pub(in crate::server) phpdoc_comment: Option<String>,
}

pub(in crate::server) fn add_local_variable_type_inlay_hint(
    ctx: &InlayHintContext<'_>,
    variable_node: tree_sitter::Node,
    usage_start: usize,
    rhs_node: Option<tree_sitter::Node>,
    hints: &mut Vec<InlayHint>,
    seen: &mut HashSet<(u32, u32, String)>,
) {
    let Some(variable_name) = variable_text_for_node(ctx.source, variable_node) else {
        return;
    };
    let Some(type_hint) =
        local_variable_inlay_type(ctx, variable_node, usage_start, &variable_name, rhs_node)
    else {
        return;
    };
    if enclosing_foreach_statement_for_variable(ctx.source, variable_node).is_some()
        && type_hint
            .display
            .trim()
            .trim_start_matches('\\')
            .eq_ignore_ascii_case("mixed")
    {
        return;
    }

    let end = variable_node.end_position();
    let position = Position::new(
        end.row as u32,
        ctx.utf16_index
            .byte_col_to_utf16(end.row as u32, end.column as u32),
    );
    let label_text = format!(": {}", type_hint.display);
    if !seen.insert((position.line, position.character, label_text)) {
        return;
    }

    hints.push(InlayHint {
        position,
        label: local_variable_inlay_label(ctx, &type_hint),
        kind: Some(InlayHintKind::TYPE),
        text_edits: None,
        tooltip: Some(InlayHintTooltip::String(local_variable_inlay_tooltip(
            &type_hint,
        ))),
        padding_left: Some(false),
        padding_right: Some(true),
        data: None,
    });
}

pub(in crate::server) fn local_variable_inlay_type(
    ctx: &InlayHintContext<'_>,
    variable_node: tree_sitter::Node,
    usage_start: usize,
    variable_name: &str,
    rhs_node: Option<tree_sitter::Node>,
) -> Option<LocalVariableInlayType> {
    ctx.type_cache.cached_local_inlay(
        node_range_node(variable_node),
        "local-variable-inlay",
        format!("{variable_name}:{usage_start}"),
        || {
            let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
                ctx.type_cache.cached_string(
                    (0, 0, 0, 0),
                    "member-type",
                    format!("{class_fqn}::{member_name}"),
                    || resolve_member_type_from_index(ctx.index, class_fqn, member_name),
                )
            };
            let callable_param_resolver = |callable_ctx: CallableParameterContext<'_>| {
                resolve_callable_parameter_type_from_index(
                    ctx.index,
                    ctx.file_symbols,
                    callable_ctx,
                )
            };
            let parser_info = infer_variable_hover_info_at_node_with_resolvers(
                variable_node,
                ctx.source,
                ctx.file_symbols,
                usage_start,
                variable_name,
                Some(&resolver),
                Some(&callable_param_resolver),
            );
            let allow_scalar =
                enclosing_foreach_statement_for_variable(ctx.source, variable_node).is_some();

            if let Some(type_hint) = parser_info.as_ref().and_then(|info| {
                info.phpdoc_comment.as_ref().and_then(|_| {
                    local_variable_type_from_hover_info(info, ctx.file_symbols, allow_scalar)
                })
            }) {
                return Some(type_hint);
            }

            if let Some(type_hint) =
                rhs_node.and_then(|rhs| local_variable_inlay_type_from_expression(ctx, rhs))
            {
                return Some(type_hint);
            }

            if let Some(type_hint) = foreach_variable_inlay_type_from_index(ctx, variable_node) {
                return Some(type_hint);
            }

            parser_info.as_ref().and_then(|info| {
                local_variable_type_from_hover_info(info, ctx.file_symbols, allow_scalar)
            })
        },
    )
}

pub(in crate::server) fn local_variable_inlay_type_from_expression(
    ctx: &InlayHintContext<'_>,
    expression: tree_sitter::Node,
) -> Option<LocalVariableInlayType> {
    let expression = normalized_expression_node(expression);
    match expression.kind() {
        "object_creation_expression" => {
            local_variable_inlay_type_from_new_expression(ctx, expression)
        }
        "function_call_expression"
        | "member_call_expression"
        | "nullsafe_member_call_expression"
        | "scoped_call_expression" => {
            local_variable_inlay_type_from_call_expression(ctx, expression)
        }
        "cast_expression" => local_variable_inlay_type_from_cast_expression(ctx, expression),
        "conditional_expression" => {
            local_variable_inlay_type_from_conditional_expression(ctx, expression)
        }
        "variable_name" => local_variable_inlay_type_from_variable_expression(ctx, expression),
        _ => None,
    }
}

pub(in crate::server) fn local_variable_inlay_type_from_conditional_expression(
    ctx: &InlayHintContext<'_>,
    expression: tree_sitter::Node,
) -> Option<LocalVariableInlayType> {
    let type_info = conditional_expression_type_info(ctx.source, expression)?;
    local_variable_inlay_type_from_type_info(ctx, "", "", &type_info, true)
}

pub(in crate::server) fn conditional_expression_type_info(
    source: &str,
    expression: tree_sitter::Node,
) -> Option<php_lsp_types::TypeInfo> {
    let text = node_text(source, expression);
    let question = find_top_level_conditional_question(text)?;
    let colon = find_top_level_needle(text, question + 1, text.len(), ":")?;
    let if_type = scalar_literal_type_info_from_text(&text[question + 1..colon])?;
    let else_type = scalar_literal_type_info_from_text(&text[colon + 1..])?;
    merge_conditional_branch_type_infos(if_type, else_type)
}

pub(in crate::server) fn find_top_level_conditional_question(text: &str) -> Option<usize> {
    split_top_level_text_scan(text, |idx, ch, nested| {
        (ch == '?' && !nested && !text[idx..].starts_with("?->")).then_some(idx)
    })
}

pub(in crate::server) fn scalar_literal_type_info_from_text(
    text: &str,
) -> Option<php_lsp_types::TypeInfo> {
    let text = text.trim();
    let lower = text.to_ascii_lowercase();
    if text.starts_with(['\'', '"']) {
        return Some(php_lsp_types::TypeInfo::Simple("string".to_string()));
    }
    if lower == "true" || lower == "false" {
        return Some(php_lsp_types::TypeInfo::Simple("bool".to_string()));
    }
    if lower == "null" {
        return Some(php_lsp_types::TypeInfo::LiteralNull);
    }

    let numeric = lower.trim_start_matches(['+', '-']);
    if numeric.parse::<i64>().is_ok() {
        return Some(php_lsp_types::TypeInfo::Simple("int".to_string()));
    }
    if numeric.parse::<f64>().is_ok() && numeric.contains('.') {
        return Some(php_lsp_types::TypeInfo::Simple("float".to_string()));
    }

    None
}

pub(in crate::server) fn merge_conditional_branch_type_infos(
    left: php_lsp_types::TypeInfo,
    right: php_lsp_types::TypeInfo,
) -> Option<php_lsp_types::TypeInfo> {
    match (left, right) {
        (php_lsp_types::TypeInfo::LiteralNull, php_lsp_types::TypeInfo::LiteralNull) => None,
        (php_lsp_types::TypeInfo::LiteralNull, other)
        | (other, php_lsp_types::TypeInfo::LiteralNull) => {
            Some(php_lsp_types::TypeInfo::Nullable(Box::new(other)))
        }
        (left, right) if left == right => Some(left),
        (left, right) => Some(php_lsp_types::TypeInfo::Union(vec![left, right])),
    }
}

#[derive(Debug, Clone)]
pub(in crate::server) struct IndexedExpressionTypeInfo {
    type_info: php_lsp_types::TypeInfo,
    owner_fqn: String,
    uri: String,
}

pub(in crate::server) fn foreach_variable_inlay_type_from_index(
    ctx: &InlayHintContext<'_>,
    variable_node: tree_sitter::Node,
) -> Option<LocalVariableInlayType> {
    let foreach_stmt = enclosing_foreach_statement_for_variable(ctx.source, variable_node)?;
    let iterable_node = foreach_iterable_node_for_inlay(foreach_stmt)?;
    let iterable_type = indexed_expression_type_info(ctx, iterable_node)?;
    let value_type = iterable_value_type_info(&iterable_type.type_info, None)?;

    local_variable_inlay_type_from_type_info(
        ctx,
        &iterable_type.owner_fqn,
        &iterable_type.uri,
        &value_type,
        true,
    )
}

pub(in crate::server) fn enclosing_foreach_statement_for_variable<'tree>(
    source: &str,
    variable_node: tree_sitter::Node<'tree>,
) -> Option<tree_sitter::Node<'tree>> {
    let variable_name = variable_text_for_node(source, variable_node)?;
    let mut current = variable_node;

    loop {
        if current.kind() == "foreach_statement" {
            let value_node = foreach_value_variable_node_for_inlay(current, source)?;
            if variable_text_for_node(source, value_node).as_deref() == Some(&variable_name)
                && variable_node.start_byte() >= current.start_byte()
                && variable_node.end_byte() <= current.end_byte()
            {
                return Some(current);
            }
        }
        current = current.parent()?;
    }
}

pub(in crate::server) fn foreach_iterable_node_for_inlay(
    statement: tree_sitter::Node,
) -> Option<tree_sitter::Node> {
    (statement.kind() == "foreach_statement")
        .then(|| statement.named_child(0))
        .flatten()
}

pub(in crate::server) fn indexed_expression_type_info(
    ctx: &InlayHintContext<'_>,
    expression: tree_sitter::Node,
) -> Option<IndexedExpressionTypeInfo> {
    let expression = normalized_expression_node(expression);
    match expression.kind() {
        "function_call_expression"
        | "member_call_expression"
        | "nullsafe_member_call_expression"
        | "scoped_call_expression" => indexed_call_expression_type_info(ctx, expression),
        _ => None,
    }
}

pub(in crate::server) fn indexed_call_expression_type_info(
    ctx: &InlayHintContext<'_>,
    expression: tree_sitter::Node,
) -> Option<IndexedExpressionTypeInfo> {
    if let Some(type_info) = doctrine_repository_call_type_info(ctx, expression) {
        return Some(type_info);
    }

    let name_node = call_target_name_node(expression)?;
    let Some((sym_at_pos, symbol)) = resolve_reference_symbol_at_node_cached(
        ctx.tree,
        ctx.source,
        name_node,
        ctx.file_symbols,
        ctx.index,
        ctx.type_cache,
    ) else {
        return server_member_call_expression_type_info(ctx, expression);
    };
    if !matches!(
        symbol.kind,
        php_lsp_types::PhpSymbolKind::Function | php_lsp_types::PhpSymbolKind::Method
    ) {
        return None;
    }

    let return_type = symbol_effective_return_type(&symbol)?;
    let owner_fqn = sym_at_pos
        .fqn
        .rsplit_once("::")
        .map(|(owner, _)| owner.to_string())
        .or_else(|| symbol.parent_fqn.clone())
        .unwrap_or_default();
    let type_info = resolve_call_site_return_type(ctx, expression, &symbol, &return_type);
    let type_info =
        doctrine_collection_getter_return_type_info(ctx, &symbol, &owner_fqn, &type_info)
            .unwrap_or(type_info);

    Some(IndexedExpressionTypeInfo {
        type_info,
        owner_fqn,
        uri: symbol.uri.clone(),
    })
}

pub(in crate::server) fn server_member_call_expression_type_info(
    ctx: &InlayHintContext<'_>,
    expression: tree_sitter::Node,
) -> Option<IndexedExpressionTypeInfo> {
    let expression = normalized_expression_node(expression);
    let (receiver_fqn, symbol) = server_member_call_symbol(ctx, expression)?;

    let return_type = symbol_effective_return_type(&symbol)?;
    let owner_fqn = symbol.parent_fqn.as_deref().unwrap_or(&receiver_fqn);
    let type_info = resolve_call_site_return_type(ctx, expression, &symbol, &return_type);
    Some(IndexedExpressionTypeInfo {
        type_info,
        owner_fqn: owner_fqn.to_string(),
        uri: symbol.uri.clone(),
    })
}

pub(in crate::server) fn server_member_call_symbol(
    ctx: &InlayHintContext<'_>,
    expression: tree_sitter::Node,
) -> Option<(String, Arc<php_lsp_types::SymbolInfo>)> {
    let expression = normalized_expression_node(expression);
    if !matches!(
        expression.kind(),
        "member_call_expression" | "nullsafe_member_call_expression"
    ) {
        return None;
    }

    let object = expression.child_by_field_name("object")?;
    let name = expression.child_by_field_name("name")?;
    let method_name = node_text(ctx.source, name).trim();
    let receiver_type = server_expression_type_info(ctx, object)?;
    let receiver_fqn = type_info_fqn_from_index(
        ctx.index,
        &receiver_type.owner_fqn,
        &receiver_type.uri,
        &receiver_type.type_info,
    )?;
    let method_fqn = format!("{receiver_fqn}::{method_name}");
    let symbol = ctx.index.resolve_fqn(&method_fqn)?;
    (symbol.kind == php_lsp_types::PhpSymbolKind::Method).then_some((receiver_fqn, symbol))
}

pub(in crate::server) fn server_member_symbol_at_position(
    ctx: &InlayHintContext<'_>,
    line: u32,
    byte_col: u32,
) -> Option<SymbolAtPosition> {
    let point = tree_sitter::Point::new(line as usize, byte_col as usize);
    let mut node = ctx
        .tree
        .root_node()
        .descendant_for_point_range(point, point)?;
    while !node.is_named() {
        node = node.parent()?;
    }

    let point_range = (line, byte_col, line, byte_col);
    let mut current = Some(node);
    while let Some(candidate) = current {
        if matches!(
            candidate.kind(),
            "member_call_expression" | "nullsafe_member_call_expression"
        ) {
            let name_node = member_reference_name_node(candidate)?;
            if byte_range_contains(node_range_node(name_node), point_range) {
                let method_name = node_text(ctx.source, name_node).trim().to_string();
                let (_, symbol) = server_member_call_symbol(ctx, candidate)?;
                return Some(SymbolAtPosition {
                    fqn: symbol.fqn.clone(),
                    name: method_name,
                    ref_kind: RefKind::MethodCall,
                    object_expr: candidate
                        .child_by_field_name("object")
                        .map(|object| node_text(ctx.source, object).trim().to_string()),
                    range: node_range_node(name_node),
                });
            }
        }
        current = candidate.parent();
    }

    None
}

pub(in crate::server) fn server_expression_type_info(
    ctx: &InlayHintContext<'_>,
    expression: tree_sitter::Node,
) -> Option<IndexedExpressionTypeInfo> {
    let expression = normalized_expression_node(expression);
    match expression.kind() {
        "object_creation_expression" => {
            let class_node = object_creation_class_node(expression)?;
            let class_name = node_text(ctx.source, class_node).trim();
            let fqn = resolve_class_name_pub(class_name, ctx.file_symbols)
                .trim_start_matches('\\')
                .to_string();
            if fqn.is_empty() {
                return None;
            }
            let uri = ctx
                .index
                .resolve_fqn(&fqn)
                .map(|symbol| symbol.uri.clone())
                .unwrap_or_default();
            Some(IndexedExpressionTypeInfo {
                type_info: php_lsp_types::TypeInfo::Simple(fqn.clone()),
                owner_fqn: fqn,
                uri,
            })
        }
        "variable_name" => server_variable_type_info(ctx, expression),
        "function_call_expression"
        | "member_call_expression"
        | "nullsafe_member_call_expression"
        | "scoped_call_expression" => indexed_call_expression_type_info(ctx, expression),
        _ => None,
    }
}

pub(in crate::server) fn server_variable_type_info(
    ctx: &InlayHintContext<'_>,
    variable_node: tree_sitter::Node,
) -> Option<IndexedExpressionTypeInfo> {
    if let Some(foreach_stmt) = enclosing_foreach_statement_for_variable(ctx.source, variable_node)
    {
        let iterable_node = foreach_iterable_node_for_inlay(foreach_stmt)?;
        let iterable_type = indexed_expression_type_info(ctx, iterable_node)?;
        let value_type = iterable_value_type_info(&iterable_type.type_info, None)?;
        return Some(IndexedExpressionTypeInfo {
            type_info: value_type,
            owner_fqn: iterable_type.owner_fqn,
            uri: iterable_type.uri,
        });
    }

    call_site_variable_phpdoc_type(ctx, variable_node).map(|type_info| IndexedExpressionTypeInfo {
        type_info,
        owner_fqn: current_class_fqn(ctx.file_symbols).unwrap_or_default(),
        uri: String::new(),
    })
}

pub(in crate::server) fn doctrine_repository_call_type_info(
    ctx: &InlayHintContext<'_>,
    expression: tree_sitter::Node,
) -> Option<IndexedExpressionTypeInfo> {
    let expression = normalized_expression_node(expression);
    if !matches!(
        expression.kind(),
        "member_call_expression" | "nullsafe_member_call_expression"
    ) {
        return None;
    }

    let object = expression.child_by_field_name("object")?;
    let name = expression.child_by_field_name("name")?;
    let method_name = node_text(ctx.source, name).trim();
    let entity_fqn = doctrine_get_repository_entity_fqn(ctx, object)?;

    if let Some(repository_fqn) = doctrine_repository_class_for_entity(ctx, &entity_fqn) {
        let method_fqn = format!("{repository_fqn}::{method_name}");
        if let Some(symbol) = ctx.index.resolve_fqn(&method_fqn) {
            if symbol.kind == php_lsp_types::PhpSymbolKind::Method {
                let return_type = symbol_effective_return_type(&symbol)?;
                let owner_fqn = symbol.parent_fqn.as_deref().unwrap_or(&repository_fqn);
                let type_info =
                    resolve_call_site_return_type(ctx, expression, &symbol, &return_type);
                return Some(IndexedExpressionTypeInfo {
                    type_info,
                    owner_fqn: owner_fqn.to_string(),
                    uri: symbol.uri.clone(),
                });
            }
        }
    }

    let type_info = doctrine_standard_repository_method_return_type(method_name, &entity_fqn)?;
    let uri = ctx
        .index
        .resolve_fqn(&entity_fqn)
        .map(|symbol| symbol.uri.clone())
        .unwrap_or_default();

    Some(IndexedExpressionTypeInfo {
        type_info,
        owner_fqn: entity_fqn,
        uri,
    })
}

pub(in crate::server) fn doctrine_get_repository_entity_fqn(
    ctx: &InlayHintContext<'_>,
    object: tree_sitter::Node,
) -> Option<String> {
    let object = normalized_expression_node(object);
    if !matches!(
        object.kind(),
        "member_call_expression" | "nullsafe_member_call_expression"
    ) {
        return None;
    }

    let name = object.child_by_field_name("name")?;
    if node_text(ctx.source, name).trim() != "getRepository" {
        return None;
    }

    let first_arg = call_arguments(object, ctx.source).into_iter().next()?;
    let raw = node_text(ctx.source, first_arg.value_node);
    class_string_fqn_from_expression_text(raw, ctx.file_symbols, ctx.index)
}

pub(in crate::server) fn doctrine_repository_class_for_entity(
    ctx: &InlayHintContext<'_>,
    entity_fqn: &str,
) -> Option<String> {
    let normalized_entity = entity_fqn.trim_start_matches('\\');
    ctx.type_cache.cached_string(
        (0, 0, 0, 0),
        "doctrine-repository-class",
        normalized_entity,
        || {
            doctrine_repository_class_from_template_binding(ctx.index, normalized_entity).or_else(
                || {
                    doctrine_repository_class_from_entity_attribute(
                        ctx.index,
                        ctx.file_symbols,
                        normalized_entity,
                    )
                },
            )
        },
    )
}

pub(in crate::server) fn doctrine_repository_class_from_template_binding(
    index: &WorkspaceIndex,
    entity_fqn: &str,
) -> Option<String> {
    index.types.iter().find_map(|entry| {
        let symbol = entry.value();
        if !matches!(symbol.kind, php_lsp_types::PhpSymbolKind::Class) {
            return None;
        }

        symbol.template_bindings.iter().find_map(|binding| {
            if binding.kind != php_lsp_types::TemplateBindingKind::Extends
                || !is_doctrine_repository_base(&binding.target)
            {
                return None;
            }

            let bound_entity = binding.args.first().and_then(type_info_simple_fqn)?;
            fqn_eq(&bound_entity, entity_fqn).then(|| symbol.fqn.clone())
        })
    })
}

pub(in crate::server) fn doctrine_repository_class_from_entity_attribute(
    index: &WorkspaceIndex,
    current_file_symbols: &php_lsp_types::FileSymbols,
    entity_fqn: &str,
) -> Option<String> {
    let entity = index.resolve_fqn(entity_fqn)?;
    let path = uri_to_path(&entity.uri)?;
    let source = std::fs::read_to_string(path).ok()?;
    let declaration_line = entity.range.0 as usize;
    let start_line = declaration_line.saturating_sub(32);
    let attribute_text = source
        .lines()
        .skip(start_line)
        .take(declaration_line.saturating_sub(start_line) + 1)
        .collect::<Vec<_>>()
        .join("\n");
    let repository_name = doctrine_repository_class_name_from_attribute_text(&attribute_text)?;

    let entity_file_symbols = index.file_symbols.get(&entity.uri);
    let file_symbols = entity_file_symbols
        .as_ref()
        .map(|symbols| symbols.value())
        .unwrap_or(current_file_symbols);
    let resolved = resolve_class_name_pub(&repository_name, file_symbols)
        .trim_start_matches('\\')
        .to_string();

    (!resolved.is_empty() && index.resolve_fqn(&resolved).is_some()).then_some(resolved)
}

pub(in crate::server) fn doctrine_repository_class_name_from_attribute_text(
    text: &str,
) -> Option<String> {
    doctrine_class_name_argument_from_attribute_text(text, "repositoryClass")
}

pub(in crate::server) fn doctrine_standard_repository_method_return_type(
    method_name: &str,
    entity_fqn: &str,
) -> Option<php_lsp_types::TypeInfo> {
    let entity = php_lsp_types::TypeInfo::Simple(entity_fqn.to_string());
    if matches!(method_name, "find" | "findOneBy") || method_name.starts_with("findOneBy") {
        return Some(php_lsp_types::TypeInfo::Nullable(Box::new(entity)));
    }

    if matches!(method_name, "findAll" | "findBy") || method_name.starts_with("findBy") {
        return Some(php_lsp_types::TypeInfo::Generic {
            base: "list".to_string(),
            args: vec![entity],
        });
    }

    if method_name == "count" || method_name.starts_with("countBy") {
        return Some(php_lsp_types::TypeInfo::Simple("int".to_string()));
    }

    None
}

pub(in crate::server) fn is_doctrine_repository_base(fqn: &str) -> bool {
    matches!(
        fqn.trim_start_matches('\\'),
        "Doctrine\\ORM\\EntityRepository"
            | "Doctrine\\Bundle\\DoctrineBundle\\Repository\\ServiceEntityRepository"
            | "Doctrine\\Persistence\\ObjectRepository"
    )
}

pub(in crate::server) fn type_info_simple_fqn(
    type_info: &php_lsp_types::TypeInfo,
) -> Option<String> {
    match type_info {
        php_lsp_types::TypeInfo::Simple(name) => Some(name.trim_start_matches('\\').to_string()),
        php_lsp_types::TypeInfo::Nullable(inner) => type_info_simple_fqn(inner),
        _ => None,
    }
}

pub(in crate::server) fn fqn_eq(left: &str, right: &str) -> bool {
    left.trim_start_matches('\\') == right.trim_start_matches('\\')
}

pub(in crate::server) fn doctrine_collection_getter_return_type_info(
    ctx: &InlayHintContext<'_>,
    method: &php_lsp_types::SymbolInfo,
    owner_fqn: &str,
    return_type: &php_lsp_types::TypeInfo,
) -> Option<php_lsp_types::TypeInfo> {
    let collection_base = collection_base_type_name(return_type)?;
    let path = uri_to_path(&method.uri)?;
    let source = std::fs::read_to_string(path).ok()?;
    let property_name = returned_this_property_name_from_method_source(&source, method)
        .or_else(|| property_name_from_getter(&method.name))?;
    let target_fqn = doctrine_collection_target_entity_for_property(
        ctx.index,
        ctx.file_symbols,
        &method.uri,
        owner_fqn,
        &property_name,
        method.range.0 as usize,
        &source,
    )?;

    Some(php_lsp_types::TypeInfo::Generic {
        base: collection_base,
        args: vec![
            php_lsp_types::TypeInfo::Simple("int".to_string()),
            php_lsp_types::TypeInfo::Simple(target_fqn),
        ],
    })
}

pub(in crate::server) fn collection_base_type_name(
    type_info: &php_lsp_types::TypeInfo,
) -> Option<String> {
    match type_info {
        php_lsp_types::TypeInfo::Simple(name) if is_collection_type_name(name) => {
            Some(name.clone())
        }
        php_lsp_types::TypeInfo::Nullable(inner) => collection_base_type_name(inner),
        _ => None,
    }
}

pub(in crate::server) fn is_collection_type_name(name: &str) -> bool {
    let lower = name.trim_start_matches('\\').to_ascii_lowercase();
    lower == "collection"
        || lower.ends_with("\\collection")
        || lower == "doctrine\\common\\collections\\collection"
}

pub(in crate::server) fn returned_this_property_name_from_method_source(
    source: &str,
    method: &php_lsp_types::SymbolInfo,
) -> Option<String> {
    let start = method.range.0 as usize;
    let end = method.range.2 as usize;
    let method_source = source
        .lines()
        .skip(start)
        .take(end.saturating_sub(start) + 1)
        .collect::<Vec<_>>()
        .join("\n");
    let marker = "return $this->";
    let after_marker = method_source
        .find(marker)
        .map(|idx| &method_source[idx + marker.len()..])?;
    let end = after_marker
        .char_indices()
        .find_map(|(idx, ch)| (!(ch.is_ascii_alphanumeric() || ch == '_')).then_some(idx))
        .unwrap_or(after_marker.len());
    let property = after_marker[..end].trim();
    (!property.is_empty()).then(|| property.to_string())
}

pub(in crate::server) fn property_name_from_getter(method_name: &str) -> Option<String> {
    let rest = method_name.strip_prefix("get")?;
    let mut chars = rest.chars();
    let first = chars.next()?;
    let mut property = first.to_ascii_lowercase().to_string();
    property.push_str(chars.as_str());
    Some(property)
}

#[allow(clippy::too_many_arguments)]
pub(in crate::server) fn doctrine_collection_target_entity_for_property(
    index: &WorkspaceIndex,
    current_file_symbols: &php_lsp_types::FileSymbols,
    uri: &str,
    owner_fqn: &str,
    property_name: &str,
    before_line: usize,
    source: &str,
) -> Option<String> {
    let owner = index.resolve_fqn(owner_fqn)?;
    if owner.uri != uri {
        return None;
    }

    let property_pattern = format!("${property_name}");
    let lines: Vec<&str> = source.lines().collect();
    let search_end = before_line.min(lines.len().saturating_sub(1));
    for line_index in 0..=search_end {
        let line = lines[line_index];
        if !line.contains(&property_pattern) || !line.contains("Collection") {
            continue;
        }

        let start_line = line_index.saturating_sub(32);
        let metadata = lines[start_line..=line_index].join("\n");
        let Some(target_name) = doctrine_target_entity_class_name_from_attribute_text(&metadata)
        else {
            continue;
        };

        let owner_file_symbols = index.file_symbols.get(uri);
        let file_symbols = owner_file_symbols
            .as_ref()
            .map(|symbols| symbols.value())
            .unwrap_or(current_file_symbols);
        let resolved = resolve_class_name_pub(&target_name, file_symbols)
            .trim_start_matches('\\')
            .to_string();
        if !resolved.is_empty()
            && (index.resolve_fqn(&resolved).is_some()
                || file_symbols
                    .symbols
                    .iter()
                    .any(|symbol| symbol.fqn == resolved))
        {
            return Some(resolved);
        }
    }

    None
}

pub(in crate::server) fn doctrine_target_entity_class_name_from_attribute_text(
    text: &str,
) -> Option<String> {
    doctrine_class_name_argument_from_attribute_text(text, "targetEntity")
}

pub(in crate::server) fn doctrine_class_name_argument_from_attribute_text(
    text: &str,
    argument: &str,
) -> Option<String> {
    let marker_start = text.find(argument)?;
    let after_marker = &text[marker_start + argument.len()..];
    let separator = after_marker
        .char_indices()
        .find_map(|(idx, ch)| matches!(ch, ':' | '=').then_some(idx))?;
    let after_separator = after_marker[separator + 1..].trim_start();
    let mut end = 0usize;
    for (idx, ch) in after_separator.char_indices() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '\\') {
            end = idx + ch.len_utf8();
        } else {
            break;
        }
    }

    let class_name = after_separator[..end].trim().trim_start_matches('\\');
    if class_name.is_empty() || !after_separator[end..].trim_start().starts_with("::class") {
        return None;
    }

    Some(class_name.to_string())
}

pub(in crate::server) fn local_variable_inlay_type_from_new_expression(
    ctx: &InlayHintContext<'_>,
    expression: tree_sitter::Node,
) -> Option<LocalVariableInlayType> {
    let class_node = object_creation_class_node(expression)?;
    let class_name = node_text(ctx.source, class_node).trim();
    let fqn = resolve_class_name_pub(class_name, ctx.file_symbols)
        .trim_start_matches('\\')
        .to_string();
    if fqn.is_empty() {
        return None;
    }

    Some(LocalVariableInlayType {
        display: shorten_inlay_type_display(&fqn, ctx.file_symbols),
        target_fqn: Some(fqn),
    })
}

pub(in crate::server) fn local_variable_inlay_type_from_call_expression(
    ctx: &InlayHintContext<'_>,
    expression: tree_sitter::Node,
) -> Option<LocalVariableInlayType> {
    let info = indexed_call_expression_type_info(ctx, expression)?;
    local_variable_inlay_type_from_type_info(ctx, &info.owner_fqn, &info.uri, &info.type_info, true)
}

pub(in crate::server) fn completion_call_arguments_by_param(
    member_text: &str,
    signature: &php_lsp_types::Signature,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
) -> HashMap<String, php_lsp_types::TypeInfo> {
    let mut arguments = HashMap::new();
    let Some(args_text) = call_arguments_text(member_text) else {
        return arguments;
    };

    for (arg_index, raw_arg) in split_top_level_argument_texts(args_text)
        .into_iter()
        .enumerate()
    {
        let (name, value) = split_named_argument_text(raw_arg);
        let Some(param) = signature_param_for_call_arg(signature, arg_index, name) else {
            continue;
        };
        let Some(type_info) = call_site_argument_type_from_text(value, file_symbols, index) else {
            continue;
        };
        arguments.insert(param.name.trim_start_matches('$').to_string(), type_info);
    }

    arguments
}

pub(in crate::server) fn call_arguments_text(member_text: &str) -> Option<&str> {
    let open = member_text.find('(')?;
    let close = matching_paren_in_text(member_text, open)?;
    Some(member_text[open + 1..close].trim())
}

pub(in crate::server) fn matching_paren_in_text(text: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for (idx, ch) in text[open..].char_indices() {
        let idx = open + idx;
        if let Some(quote_ch) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote_ch {
                quote = None;
            }
            continue;
        }

        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }

        match ch {
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }

    None
}

pub(in crate::server) fn split_top_level_argument_texts(args_text: &str) -> Vec<&str> {
    split_top_level_text(args_text, ',')
}

pub(in crate::server) fn split_named_argument_text(arg_text: &str) -> (Option<&str>, &str) {
    let arg_text = arg_text.trim();
    let Some(colon) = find_named_argument_colon(arg_text) else {
        return (None, arg_text);
    };
    let name = arg_text[..colon].trim();
    let value = arg_text[colon + 1..].trim();
    if name.is_empty() || value.is_empty() {
        (None, arg_text)
    } else {
        (Some(name), value)
    }
}

pub(in crate::server) fn find_named_argument_colon(arg_text: &str) -> Option<usize> {
    split_top_level_text_scan(arg_text, |idx, ch, nested| {
        if ch != ':' || nested {
            return None;
        }
        let prev = arg_text[..idx].chars().next_back();
        let next = arg_text[idx + ch.len_utf8()..].chars().next();
        (prev != Some(':') && next != Some(':')).then_some(idx)
    })
}

pub(in crate::server) fn call_site_argument_type_from_text(
    raw: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
) -> Option<php_lsp_types::TypeInfo> {
    let raw = raw.trim();
    let lower = raw.to_ascii_lowercase();

    if let Some(class_fqn) = class_string_fqn_from_expression_text(raw, file_symbols, index) {
        return Some(php_lsp_types::TypeInfo::ClassString(Some(Box::new(
            php_lsp_types::TypeInfo::Simple(class_fqn),
        ))));
    }

    if let Some(value) = unquote_php_string_literal(raw) {
        let resolved = resolve_class_name_pub(&value, file_symbols)
            .trim_start_matches('\\')
            .to_string();
        if index.resolve_fqn(&resolved).is_some()
            || file_symbols
                .symbols
                .iter()
                .any(|symbol| symbol.fqn == resolved)
        {
            return Some(php_lsp_types::TypeInfo::ClassString(Some(Box::new(
                php_lsp_types::TypeInfo::Simple(resolved),
            ))));
        }
        return Some(php_lsp_types::TypeInfo::LiteralString(raw.to_string()));
    }

    if lower == "true" {
        return Some(php_lsp_types::TypeInfo::LiteralBool(true));
    }
    if lower == "false" {
        return Some(php_lsp_types::TypeInfo::LiteralBool(false));
    }
    if lower == "null" {
        return Some(php_lsp_types::TypeInfo::LiteralNull);
    }

    let numeric = lower.trim_start_matches(['+', '-']);
    if numeric.parse::<i64>().is_ok() {
        return Some(php_lsp_types::TypeInfo::LiteralInt(raw.to_string()));
    }
    if numeric.parse::<f64>().is_ok() && numeric.contains('.') {
        return Some(php_lsp_types::TypeInfo::LiteralFloat(raw.to_string()));
    }

    None
}

pub(in crate::server) fn unquote_php_string_literal(raw: &str) -> Option<String> {
    if raw.len() < 2 {
        return None;
    }
    let quote = raw.as_bytes()[0] as char;
    if !matches!(quote, '\'' | '"') || !raw.ends_with(quote) {
        return None;
    }
    Some(raw[1..raw.len() - 1].replace("\\\\", "\\"))
}

pub(in crate::server) fn split_top_level_text(text: &str, delimiter: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut paren_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for (idx, ch) in text.char_indices() {
        if let Some(quote_ch) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote_ch {
                quote = None;
            }
            continue;
        }

        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }

        let nested = paren_depth > 0 || angle_depth > 0 || bracket_depth > 0 || brace_depth > 0;
        if ch == delimiter && !nested {
            let part = text[start..idx].trim();
            if !part.is_empty() {
                parts.push(part);
            }
            start = idx + ch.len_utf8();
            continue;
        }

        match ch {
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '<' => angle_depth += 1,
            '>' => angle_depth = angle_depth.saturating_sub(1),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }
    }

    let part = text[start..].trim();
    if !part.is_empty() {
        parts.push(part);
    }
    parts
}

pub(in crate::server) fn split_top_level_text_scan<T>(
    text: &str,
    mut f: impl FnMut(usize, char, bool) -> Option<T>,
) -> Option<T> {
    let mut paren_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for (idx, ch) in text.char_indices() {
        if let Some(quote_ch) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote_ch {
                quote = None;
            }
            continue;
        }

        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }

        let nested = paren_depth > 0 || angle_depth > 0 || bracket_depth > 0 || brace_depth > 0;
        if let Some(value) = f(idx, ch, nested) {
            return Some(value);
        }

        match ch {
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '<' => angle_depth += 1,
            '>' => angle_depth = angle_depth.saturating_sub(1),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }
    }

    None
}

pub(in crate::server) fn resolve_call_site_return_type(
    ctx: &InlayHintContext<'_>,
    call_node: tree_sitter::Node,
    symbol: &php_lsp_types::SymbolInfo,
    return_type: &php_lsp_types::TypeInfo,
) -> php_lsp_types::TypeInfo {
    let Some(signature) = symbol.signature.as_ref() else {
        return return_type.clone();
    };

    let arguments = call_site_arguments_by_param(ctx, call_node, signature);
    let template_names: HashSet<String> = symbol
        .templates
        .iter()
        .map(|template| template.name.clone())
        .collect();
    let substitutions = call_site_template_substitutions(&arguments, signature, &template_names);
    resolve_call_site_type_info(return_type, &arguments, &template_names, &substitutions)
}

pub(in crate::server) fn symbol_effective_return_type(
    symbol: &php_lsp_types::SymbolInfo,
) -> Option<php_lsp_types::TypeInfo> {
    let native = symbol
        .signature
        .as_ref()
        .and_then(|signature| signature.return_type.as_ref());
    let phpdoc = symbol.doc_comment.as_deref().and_then(|doc| {
        let parsed = parse_phpdoc(doc);
        if symbol.kind == php_lsp_types::PhpSymbolKind::Property {
            parsed.var_type
        } else {
            parsed.return_type
        }
    });

    match (native, phpdoc) {
        (Some(native), Some(phpdoc))
            if type_info_specificity_score(&phpdoc) > type_info_specificity_score(native) =>
        {
            Some(phpdoc)
        }
        (Some(native), _) => Some(native.clone()),
        (None, Some(phpdoc)) => Some(phpdoc),
        (None, None) => None,
    }
}

pub(in crate::server) fn type_info_specificity_score(type_info: &php_lsp_types::TypeInfo) -> usize {
    match type_info {
        php_lsp_types::TypeInfo::Mixed
        | php_lsp_types::TypeInfo::Void
        | php_lsp_types::TypeInfo::Never
        | php_lsp_types::TypeInfo::LiteralNull => 0,
        php_lsp_types::TypeInfo::Simple(name) => {
            if is_builtin_type_name(name) {
                1
            } else {
                3
            }
        }
        php_lsp_types::TypeInfo::Self_
        | php_lsp_types::TypeInfo::Static_
        | php_lsp_types::TypeInfo::Parent_ => 3,
        php_lsp_types::TypeInfo::Nullable(inner) => type_info_specificity_score(inner),
        php_lsp_types::TypeInfo::Union(types) | php_lsp_types::TypeInfo::Intersection(types) => {
            types.iter().map(type_info_specificity_score).sum()
        }
        php_lsp_types::TypeInfo::Generic { args, .. } => {
            4 + args.iter().map(type_info_specificity_score).sum::<usize>()
        }
        php_lsp_types::TypeInfo::ArrayShape(items) => {
            5 + items
                .iter()
                .map(|item| type_info_specificity_score(&item.value))
                .sum::<usize>()
        }
        php_lsp_types::TypeInfo::ObjectShape(items) => {
            5 + items
                .iter()
                .map(|item| type_info_specificity_score(&item.value))
                .sum::<usize>()
        }
        php_lsp_types::TypeInfo::Callable {
            params,
            return_type,
        } => {
            3 + params
                .iter()
                .map(type_info_specificity_score)
                .sum::<usize>()
                + return_type
                    .as_ref()
                    .map(|return_type| type_info_specificity_score(return_type))
                    .unwrap_or_default()
        }
        php_lsp_types::TypeInfo::ClassString(inner) => {
            3 + inner
                .as_ref()
                .map(|inner| type_info_specificity_score(inner))
                .unwrap_or_default()
        }
        php_lsp_types::TypeInfo::LiteralString(_)
        | php_lsp_types::TypeInfo::LiteralInt(_)
        | php_lsp_types::TypeInfo::LiteralFloat(_)
        | php_lsp_types::TypeInfo::LiteralBool(_) => 2,
        php_lsp_types::TypeInfo::Conditional {
            if_type, else_type, ..
        } => 3 + type_info_specificity_score(if_type) + type_info_specificity_score(else_type),
    }
}

pub(in crate::server) fn call_site_arguments_by_param(
    ctx: &InlayHintContext<'_>,
    call_node: tree_sitter::Node,
    signature: &php_lsp_types::Signature,
) -> HashMap<String, php_lsp_types::TypeInfo> {
    let mut arguments = HashMap::new();
    for (arg_index, arg) in call_arguments(call_node, ctx.source)
        .into_iter()
        .enumerate()
    {
        let Some(param) = signature_param_for_call_arg(signature, arg_index, arg.name.as_deref())
        else {
            continue;
        };
        let Some(type_info) = call_site_argument_type(ctx, arg.value_node) else {
            continue;
        };
        arguments.insert(param.name.trim_start_matches('$').to_string(), type_info);
    }
    arguments
}

pub(in crate::server) fn call_site_argument_type(
    ctx: &InlayHintContext<'_>,
    node: tree_sitter::Node,
) -> Option<php_lsp_types::TypeInfo> {
    let node = normalized_expression_node(node);
    ctx.type_cache.cached_type_info(
        node_range_node(node),
        "call-site-argument-type",
        node.kind(),
        || call_site_argument_type_uncached(ctx, node),
    )
}

pub(in crate::server) fn call_site_argument_type_uncached(
    ctx: &InlayHintContext<'_>,
    node: tree_sitter::Node,
) -> Option<php_lsp_types::TypeInfo> {
    let raw = node_text(ctx.source, node).trim();
    let lower = raw.to_ascii_lowercase();

    if let Some(class_fqn) = class_string_fqn_from_expression_text(raw, ctx.file_symbols, ctx.index)
    {
        return Some(php_lsp_types::TypeInfo::ClassString(Some(Box::new(
            php_lsp_types::TypeInfo::Simple(class_fqn),
        ))));
    }

    if let Some(value) = unquote_php_string_literal(raw) {
        let resolved = resolve_class_name_pub(&value, ctx.file_symbols)
            .trim_start_matches('\\')
            .to_string();
        if ctx.index.resolve_fqn(&resolved).is_some()
            || ctx
                .file_symbols
                .symbols
                .iter()
                .any(|symbol| symbol.fqn == resolved)
        {
            return Some(php_lsp_types::TypeInfo::ClassString(Some(Box::new(
                php_lsp_types::TypeInfo::Simple(resolved),
            ))));
        }
        return Some(php_lsp_types::TypeInfo::LiteralString(raw.to_string()));
    }
    if node.kind().contains("string") {
        return Some(php_lsp_types::TypeInfo::LiteralString(raw.to_string()));
    }
    if lower == "true" {
        return Some(php_lsp_types::TypeInfo::LiteralBool(true));
    }
    if lower == "false" {
        return Some(php_lsp_types::TypeInfo::LiteralBool(false));
    }
    if lower == "null" {
        return Some(php_lsp_types::TypeInfo::LiteralNull);
    }

    let numeric = lower.trim_start_matches(['+', '-']);
    if numeric.parse::<i64>().is_ok() {
        return Some(php_lsp_types::TypeInfo::LiteralInt(raw.to_string()));
    }
    if numeric.parse::<f64>().is_ok() && numeric.contains('.') {
        return Some(php_lsp_types::TypeInfo::LiteralFloat(raw.to_string()));
    }

    if node.kind() == "object_creation_expression" {
        let class_node = object_creation_class_node(node)?;
        let class_name = node_text(ctx.source, class_node).trim();
        let fqn = resolve_class_name_pub(class_name, ctx.file_symbols)
            .trim_start_matches('\\')
            .to_string();
        if !fqn.is_empty() {
            return Some(php_lsp_types::TypeInfo::Simple(fqn));
        }
    }

    if node.kind() == "variable_name" {
        return call_site_variable_phpdoc_type(ctx, node);
    }

    None
}

pub(in crate::server) fn class_string_fqn_from_expression_text(
    raw: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
) -> Option<String> {
    let class_name = raw.trim().strip_suffix("::class")?.trim();
    if class_name.is_empty() {
        return None;
    }

    let fqn = resolve_class_name_pub(class_name, file_symbols)
        .trim_start_matches('\\')
        .to_string();
    (index.resolve_fqn(&fqn).is_some()
        || file_symbols.symbols.iter().any(|symbol| symbol.fqn == fqn))
    .then_some(fqn)
}

pub(in crate::server) fn call_site_variable_phpdoc_type(
    ctx: &InlayHintContext<'_>,
    node: tree_sitter::Node,
) -> Option<php_lsp_types::TypeInfo> {
    let variable_name = variable_text_for_node(ctx.source, node)?;
    let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
        ctx.type_cache.cached_string(
            (0, 0, 0, 0),
            "member-type",
            format!("{class_fqn}::{member_name}"),
            || resolve_member_type_from_index(ctx.index, class_fqn, member_name),
        )
    };
    let callable_param_resolver = |callable_ctx: CallableParameterContext<'_>| {
        resolve_callable_parameter_type_from_index(ctx.index, ctx.file_symbols, callable_ctx)
    };
    let info = infer_variable_hover_info_at_node_with_resolvers(
        node,
        ctx.source,
        ctx.file_symbols,
        node.start_byte(),
        &variable_name,
        Some(&resolver),
        Some(&callable_param_resolver),
    )?;
    let phpdoc = parse_phpdoc(info.phpdoc_comment.as_deref()?);
    phpdoc
        .var_type
        .map(|type_info| resolve_call_site_type_names(&type_info, ctx.file_symbols))
}

pub(in crate::server) fn resolve_callable_parameter_type_from_index(
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    ctx: CallableParameterContext<'_>,
) -> Option<php_lsp_types::TypeInfo> {
    let symbol = index.resolve_fqn(ctx.target_fqn)?;
    let signature = symbol.signature.as_ref()?;
    let callable_param =
        signature_param_for_call_arg(signature, ctx.argument_index, ctx.argument_name)?;
    let expected = callable_param.type_info.as_ref()?;
    let template_names = callable_template_names_from_index(index, &symbol, ctx.target_fqn);
    let mut substitutions = receiver_template_substitutions_from_index(index, file_symbols, &ctx);

    for arg in ctx.argument_types {
        if arg.argument_index == ctx.argument_index {
            continue;
        }
        let Some(param) = signature_param_for_call_arg(
            signature,
            arg.argument_index,
            arg.argument_name.as_deref(),
        ) else {
            continue;
        };
        let Some(param_type) = param.type_info.as_ref() else {
            continue;
        };
        bind_template_type_info(
            param_type,
            &arg.type_info,
            &template_names,
            &mut substitutions,
        );
    }

    let expected = substitute_call_site_type_info(expected, &substitutions);
    callable_param_type_from_type_info(&expected, ctx.parameter_index)
}

pub(in crate::server) fn callable_template_names_from_index(
    index: &WorkspaceIndex,
    symbol: &php_lsp_types::SymbolInfo,
    target_fqn: &str,
) -> HashSet<String> {
    let mut names = symbol
        .templates
        .iter()
        .map(|template| template.name.clone())
        .collect::<HashSet<_>>();
    if let Some((class_fqn, _)) = target_fqn.rsplit_once("::") {
        if let Some(class_symbol) = index.resolve_fqn(class_fqn) {
            names.extend(
                class_symbol
                    .templates
                    .iter()
                    .map(|template| template.name.clone()),
            );
        }
    }
    names
}

pub(in crate::server) fn receiver_template_substitutions_from_index(
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    ctx: &CallableParameterContext<'_>,
) -> HashMap<String, php_lsp_types::TypeInfo> {
    let mut substitutions = HashMap::new();
    let Some((class_fqn, _)) = ctx.target_fqn.rsplit_once("::") else {
        return substitutions;
    };
    let Some(php_lsp_types::TypeInfo::Generic { base, args }) = ctx.receiver_type else {
        return substitutions;
    };
    let resolved_base = resolve_class_name_pub(base, file_symbols)
        .trim_start_matches('\\')
        .to_string();
    if resolved_base != class_fqn.trim_start_matches('\\') {
        return substitutions;
    }
    let Some(class_symbol) = index.resolve_fqn(class_fqn) else {
        return substitutions;
    };
    for (template, arg) in class_symbol.templates.iter().zip(args.iter()) {
        substitutions.insert(template.name.clone(), arg.clone());
    }
    substitutions
}

pub(in crate::server) fn callable_param_type_from_type_info(
    type_info: &php_lsp_types::TypeInfo,
    parameter_index: usize,
) -> Option<php_lsp_types::TypeInfo> {
    match type_info {
        php_lsp_types::TypeInfo::Callable { params, .. } => params.get(parameter_index).cloned(),
        php_lsp_types::TypeInfo::Nullable(inner) => {
            callable_param_type_from_type_info(inner, parameter_index)
        }
        php_lsp_types::TypeInfo::Union(types) | php_lsp_types::TypeInfo::Intersection(types) => {
            types.iter().find_map(|type_info| {
                callable_param_type_from_type_info(type_info, parameter_index)
            })
        }
        _ => None,
    }
}

pub(in crate::server) fn call_site_template_substitutions(
    arguments: &HashMap<String, php_lsp_types::TypeInfo>,
    signature: &php_lsp_types::Signature,
    template_names: &HashSet<String>,
) -> HashMap<String, php_lsp_types::TypeInfo> {
    let mut substitutions = HashMap::new();
    for param in &signature.params {
        let Some(param_type) = param.type_info.as_ref() else {
            continue;
        };
        let Some(arg_type) = arguments.get(param.name.trim_start_matches('$')) else {
            continue;
        };
        bind_template_type_info(param_type, arg_type, template_names, &mut substitutions);
    }
    substitutions
}

pub(in crate::server) fn resolve_call_site_type_info(
    type_info: &php_lsp_types::TypeInfo,
    arguments: &HashMap<String, php_lsp_types::TypeInfo>,
    template_names: &HashSet<String>,
    substitutions: &HashMap<String, php_lsp_types::TypeInfo>,
) -> php_lsp_types::TypeInfo {
    let substituted = substitute_call_site_type_info(type_info, substitutions);
    match substituted {
        php_lsp_types::TypeInfo::Conditional {
            subject,
            target,
            if_type,
            else_type,
        } => {
            let subject_key = subject.trim().trim_start_matches('$');
            let Some(actual) = arguments.get(subject_key) else {
                return conditional_union_fallback(*if_type, *else_type);
            };
            let mut branch_substitutions = substitutions.clone();
            if type_pattern_matches_actual(
                &target,
                actual,
                template_names,
                &mut branch_substitutions,
            ) {
                substitute_call_site_type_info(&if_type, &branch_substitutions)
            } else {
                substitute_call_site_type_info(&else_type, &branch_substitutions)
            }
        }
        other => other,
    }
}

pub(in crate::server) fn conditional_union_fallback(
    if_type: php_lsp_types::TypeInfo,
    else_type: php_lsp_types::TypeInfo,
) -> php_lsp_types::TypeInfo {
    if if_type == else_type {
        if_type
    } else {
        php_lsp_types::TypeInfo::Union(vec![if_type, else_type])
    }
}

pub(in crate::server) fn substitute_call_site_type_info(
    type_info: &php_lsp_types::TypeInfo,
    substitutions: &HashMap<String, php_lsp_types::TypeInfo>,
) -> php_lsp_types::TypeInfo {
    match type_info {
        php_lsp_types::TypeInfo::Simple(name) => substitutions
            .get(name)
            .cloned()
            .unwrap_or_else(|| php_lsp_types::TypeInfo::Simple(name.clone())),
        php_lsp_types::TypeInfo::Generic { base, args } => php_lsp_types::TypeInfo::Generic {
            base: base.clone(),
            args: args
                .iter()
                .map(|arg| substitute_call_site_type_info(arg, substitutions))
                .collect(),
        },
        php_lsp_types::TypeInfo::ArrayShape(items) => php_lsp_types::TypeInfo::ArrayShape(
            items
                .iter()
                .map(|item| php_lsp_types::ArrayShapeItem {
                    key: item.key.clone(),
                    optional: item.optional,
                    value: substitute_call_site_type_info(&item.value, substitutions),
                })
                .collect(),
        ),
        php_lsp_types::TypeInfo::ObjectShape(items) => php_lsp_types::TypeInfo::ObjectShape(
            items
                .iter()
                .map(|item| php_lsp_types::ArrayShapeItem {
                    key: item.key.clone(),
                    optional: item.optional,
                    value: substitute_call_site_type_info(&item.value, substitutions),
                })
                .collect(),
        ),
        php_lsp_types::TypeInfo::Callable {
            params,
            return_type,
        } => php_lsp_types::TypeInfo::Callable {
            params: params
                .iter()
                .map(|param| substitute_call_site_type_info(param, substitutions))
                .collect(),
            return_type: return_type.as_ref().map(|return_type| {
                Box::new(substitute_call_site_type_info(return_type, substitutions))
            }),
        },
        php_lsp_types::TypeInfo::ClassString(Some(inner)) => {
            php_lsp_types::TypeInfo::ClassString(Some(Box::new(substitute_call_site_type_info(
                inner,
                substitutions,
            ))))
        }
        php_lsp_types::TypeInfo::Conditional {
            subject,
            target,
            if_type,
            else_type,
        } => php_lsp_types::TypeInfo::Conditional {
            subject: subject.clone(),
            target: Box::new(substitute_call_site_type_info(target, substitutions)),
            if_type: Box::new(substitute_call_site_type_info(if_type, substitutions)),
            else_type: Box::new(substitute_call_site_type_info(else_type, substitutions)),
        },
        php_lsp_types::TypeInfo::Union(types) => php_lsp_types::TypeInfo::Union(
            types
                .iter()
                .map(|type_info| substitute_call_site_type_info(type_info, substitutions))
                .collect(),
        ),
        php_lsp_types::TypeInfo::Intersection(types) => php_lsp_types::TypeInfo::Intersection(
            types
                .iter()
                .map(|type_info| substitute_call_site_type_info(type_info, substitutions))
                .collect(),
        ),
        php_lsp_types::TypeInfo::Nullable(inner) => php_lsp_types::TypeInfo::Nullable(Box::new(
            substitute_call_site_type_info(inner, substitutions),
        )),
        php_lsp_types::TypeInfo::ClassString(None)
        | php_lsp_types::TypeInfo::LiteralString(_)
        | php_lsp_types::TypeInfo::LiteralInt(_)
        | php_lsp_types::TypeInfo::LiteralFloat(_)
        | php_lsp_types::TypeInfo::LiteralBool(_)
        | php_lsp_types::TypeInfo::LiteralNull
        | php_lsp_types::TypeInfo::Void
        | php_lsp_types::TypeInfo::Never
        | php_lsp_types::TypeInfo::Mixed
        | php_lsp_types::TypeInfo::Self_
        | php_lsp_types::TypeInfo::Static_
        | php_lsp_types::TypeInfo::Parent_ => type_info.clone(),
    }
}

pub(in crate::server) fn bind_template_type_info(
    pattern: &php_lsp_types::TypeInfo,
    actual: &php_lsp_types::TypeInfo,
    template_names: &HashSet<String>,
    substitutions: &mut HashMap<String, php_lsp_types::TypeInfo>,
) {
    match (pattern, actual) {
        (php_lsp_types::TypeInfo::Simple(name), actual) if template_names.contains(name) => {
            substitutions
                .entry(name.clone())
                .or_insert_with(|| actual.clone());
        }
        (
            php_lsp_types::TypeInfo::ClassString(Some(pattern_inner)),
            php_lsp_types::TypeInfo::ClassString(Some(actual_inner)),
        ) => bind_template_type_info(pattern_inner, actual_inner, template_names, substitutions),
        (
            php_lsp_types::TypeInfo::Generic {
                base: pattern_base,
                args: pattern_args,
            },
            php_lsp_types::TypeInfo::Generic {
                base: actual_base,
                args: actual_args,
            },
        ) if pattern_base.eq_ignore_ascii_case(actual_base) => {
            for (pattern_arg, actual_arg) in pattern_args.iter().zip(actual_args.iter()) {
                bind_template_type_info(pattern_arg, actual_arg, template_names, substitutions);
            }
        }
        (php_lsp_types::TypeInfo::Nullable(pattern_inner), actual) => {
            bind_template_type_info(pattern_inner, actual, template_names, substitutions);
        }
        (php_lsp_types::TypeInfo::Union(patterns), actual)
        | (php_lsp_types::TypeInfo::Intersection(patterns), actual) => {
            for pattern in patterns {
                bind_template_type_info(pattern, actual, template_names, substitutions);
            }
        }
        _ => {}
    }
}

pub(in crate::server) fn type_pattern_matches_actual(
    pattern: &php_lsp_types::TypeInfo,
    actual: &php_lsp_types::TypeInfo,
    template_names: &HashSet<String>,
    substitutions: &mut HashMap<String, php_lsp_types::TypeInfo>,
) -> bool {
    match (pattern, actual) {
        (php_lsp_types::TypeInfo::Mixed, _) => true,
        (php_lsp_types::TypeInfo::Simple(name), actual) if template_names.contains(name) => {
            substitutions
                .entry(name.clone())
                .or_insert_with(|| actual.clone());
            true
        }
        (php_lsp_types::TypeInfo::Simple(expected), php_lsp_types::TypeInfo::Simple(actual)) => {
            same_type_name(expected, actual)
        }
        (
            php_lsp_types::TypeInfo::ClassString(Some(pattern_inner)),
            php_lsp_types::TypeInfo::ClassString(Some(actual_inner)),
        ) => {
            type_pattern_matches_actual(pattern_inner, actual_inner, template_names, substitutions)
        }
        (php_lsp_types::TypeInfo::ClassString(None), php_lsp_types::TypeInfo::ClassString(_)) => {
            true
        }
        (
            php_lsp_types::TypeInfo::Generic {
                base: expected_base,
                args: expected_args,
            },
            php_lsp_types::TypeInfo::Generic {
                base: actual_base,
                args: actual_args,
            },
        ) if same_type_name(expected_base, actual_base)
            && expected_args.len() == actual_args.len() =>
        {
            expected_args
                .iter()
                .zip(actual_args.iter())
                .all(|(expected_arg, actual_arg)| {
                    type_pattern_matches_actual(
                        expected_arg,
                        actual_arg,
                        template_names,
                        substitutions,
                    )
                })
        }
        (php_lsp_types::TypeInfo::Union(types), actual) => types.iter().any(|type_info| {
            let mut branch_substitutions = substitutions.clone();
            let matches = type_pattern_matches_actual(
                type_info,
                actual,
                template_names,
                &mut branch_substitutions,
            );
            if matches {
                *substitutions = branch_substitutions;
            }
            matches
        }),
        (php_lsp_types::TypeInfo::Intersection(types), actual) => types.iter().all(|type_info| {
            type_pattern_matches_actual(type_info, actual, template_names, substitutions)
        }),
        (php_lsp_types::TypeInfo::Nullable(_), php_lsp_types::TypeInfo::LiteralNull) => true,
        (php_lsp_types::TypeInfo::Nullable(inner), actual) => {
            type_pattern_matches_actual(inner, actual, template_names, substitutions)
        }
        (
            php_lsp_types::TypeInfo::LiteralString(expected),
            php_lsp_types::TypeInfo::LiteralString(actual),
        )
        | (
            php_lsp_types::TypeInfo::LiteralInt(expected),
            php_lsp_types::TypeInfo::LiteralInt(actual),
        )
        | (
            php_lsp_types::TypeInfo::LiteralFloat(expected),
            php_lsp_types::TypeInfo::LiteralFloat(actual),
        ) => expected == actual,
        (
            php_lsp_types::TypeInfo::LiteralBool(expected),
            php_lsp_types::TypeInfo::LiteralBool(actual),
        ) => expected == actual,
        (php_lsp_types::TypeInfo::LiteralNull, php_lsp_types::TypeInfo::LiteralNull) => true,
        _ => false,
    }
}

pub(in crate::server) fn resolve_call_site_type_names(
    type_info: &php_lsp_types::TypeInfo,
    file_symbols: &php_lsp_types::FileSymbols,
) -> php_lsp_types::TypeInfo {
    match type_info {
        php_lsp_types::TypeInfo::Simple(name) if is_builtin_type_name(name) => {
            php_lsp_types::TypeInfo::Simple(name.clone())
        }
        php_lsp_types::TypeInfo::Simple(name) => php_lsp_types::TypeInfo::Simple(
            resolve_class_name_pub(name, file_symbols)
                .trim_start_matches('\\')
                .to_string(),
        ),
        php_lsp_types::TypeInfo::Generic { base, args } => php_lsp_types::TypeInfo::Generic {
            base: if is_builtin_type_name(base) {
                base.clone()
            } else {
                resolve_class_name_pub(base, file_symbols)
                    .trim_start_matches('\\')
                    .to_string()
            },
            args: args
                .iter()
                .map(|arg| resolve_call_site_type_names(arg, file_symbols))
                .collect(),
        },
        php_lsp_types::TypeInfo::ClassString(Some(inner)) => php_lsp_types::TypeInfo::ClassString(
            Some(Box::new(resolve_call_site_type_names(inner, file_symbols))),
        ),
        php_lsp_types::TypeInfo::Conditional {
            subject,
            target,
            if_type,
            else_type,
        } => php_lsp_types::TypeInfo::Conditional {
            subject: subject.clone(),
            target: Box::new(resolve_call_site_type_names(target, file_symbols)),
            if_type: Box::new(resolve_call_site_type_names(if_type, file_symbols)),
            else_type: Box::new(resolve_call_site_type_names(else_type, file_symbols)),
        },
        php_lsp_types::TypeInfo::Union(types) => php_lsp_types::TypeInfo::Union(
            types
                .iter()
                .map(|type_info| resolve_call_site_type_names(type_info, file_symbols))
                .collect(),
        ),
        php_lsp_types::TypeInfo::Intersection(types) => php_lsp_types::TypeInfo::Intersection(
            types
                .iter()
                .map(|type_info| resolve_call_site_type_names(type_info, file_symbols))
                .collect(),
        ),
        php_lsp_types::TypeInfo::Nullable(inner) => php_lsp_types::TypeInfo::Nullable(Box::new(
            resolve_call_site_type_names(inner, file_symbols),
        )),
        php_lsp_types::TypeInfo::ArrayShape(_)
        | php_lsp_types::TypeInfo::ObjectShape(_)
        | php_lsp_types::TypeInfo::Callable { .. }
        | php_lsp_types::TypeInfo::ClassString(None)
        | php_lsp_types::TypeInfo::LiteralString(_)
        | php_lsp_types::TypeInfo::LiteralInt(_)
        | php_lsp_types::TypeInfo::LiteralFloat(_)
        | php_lsp_types::TypeInfo::LiteralBool(_)
        | php_lsp_types::TypeInfo::LiteralNull
        | php_lsp_types::TypeInfo::Void
        | php_lsp_types::TypeInfo::Never
        | php_lsp_types::TypeInfo::Mixed
        | php_lsp_types::TypeInfo::Self_
        | php_lsp_types::TypeInfo::Static_
        | php_lsp_types::TypeInfo::Parent_ => type_info.clone(),
    }
}

pub(in crate::server) fn same_type_name(left: &str, right: &str) -> bool {
    left.trim_start_matches('\\')
        .eq_ignore_ascii_case(right.trim_start_matches('\\'))
}

pub(in crate::server) fn local_variable_inlay_type_from_cast_expression(
    ctx: &InlayHintContext<'_>,
    expression: tree_sitter::Node,
) -> Option<LocalVariableInlayType> {
    let cast_type = expression.child_by_field_name("type")?;
    let display = local_variable_cast_type_display(node_text(ctx.source, cast_type))?;
    Some(LocalVariableInlayType {
        display,
        target_fqn: None,
    })
}

pub(in crate::server) fn local_variable_cast_type_display(raw_type: &str) -> Option<String> {
    let normalized = raw_type
        .trim()
        .trim_matches(|ch| ch == '(' || ch == ')')
        .to_ascii_lowercase();
    let display = match normalized.as_str() {
        "array" => "array",
        "binary" | "string" => "string",
        "bool" | "boolean" => "bool",
        "double" | "float" | "real" => "float",
        "int" | "integer" => "int",
        "object" => "object",
        _ => return None,
    };
    Some(display.to_string())
}

pub(in crate::server) fn local_variable_inlay_type_from_variable_expression(
    ctx: &InlayHintContext<'_>,
    expression: tree_sitter::Node,
) -> Option<LocalVariableInlayType> {
    let variable_name = variable_text_for_node(ctx.source, expression)?;
    let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
        ctx.type_cache.cached_string(
            (0, 0, 0, 0),
            "member-type",
            format!("{class_fqn}::{member_name}"),
            || resolve_member_type_from_index(ctx.index, class_fqn, member_name),
        )
    };
    let callable_param_resolver = |callable_ctx: CallableParameterContext<'_>| {
        resolve_callable_parameter_type_from_index(ctx.index, ctx.file_symbols, callable_ctx)
    };
    let info = infer_variable_hover_info_at_node_with_resolvers(
        expression,
        ctx.source,
        ctx.file_symbols,
        expression.start_byte(),
        &variable_name,
        Some(&resolver),
        Some(&callable_param_resolver),
    )?;

    local_variable_type_from_hover_info(&info, ctx.file_symbols, false)
}

pub(in crate::server) fn is_plain_assignment_expression(
    left: tree_sitter::Node,
    right: tree_sitter::Node,
    source: &str,
) -> bool {
    left.end_byte() <= right.start_byte()
        && source
            .get(left.end_byte()..right.start_byte())
            .is_some_and(|between| between.trim() == "=")
}

pub(in crate::server) fn foreach_value_variable_node_for_inlay<'tree>(
    statement: tree_sitter::Node<'tree>,
    source: &str,
) -> Option<tree_sitter::Node<'tree>> {
    let value_expr = match statement.named_child(1)? {
        pair if pair.kind() == "pair" => {
            let count = pair.named_child_count();
            pair.named_child(count.saturating_sub(1))?
        }
        value => value,
    };
    variable_node_in_foreach_part_for_inlay(value_expr, source)
}

pub(in crate::server) fn variable_node_in_foreach_part_for_inlay<'tree>(
    node: tree_sitter::Node<'tree>,
    source: &str,
) -> Option<tree_sitter::Node<'tree>> {
    if node.kind() == "variable_name" && node_text(source, node).starts_with('$') {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = variable_node_in_foreach_part_for_inlay(child, source) {
            return Some(found);
        }
    }
    None
}

pub(in crate::server) fn local_variable_hover_data(
    ctx: &InlayHintContext<'_>,
    variable_node: tree_sitter::Node,
) -> Option<LocalVariableHoverData> {
    let variable_name = variable_text_for_node(ctx.source, variable_node)?;
    let usage_start = variable_node.start_byte();
    let current_rhs = current_assignment_rhs_for_variable(variable_node, ctx.source);
    let parser_usage_start = current_rhs
        .as_ref()
        .map(|rhs| rhs.end_byte())
        .unwrap_or(usage_start);
    let rhs_node = current_rhs.or_else(|| {
        let scope = local_variable_scope_node(variable_node);
        latest_assignment_rhs_before_usage(scope, &variable_name, usage_start, ctx.source)
            .map(|(_, rhs)| rhs)
    });

    let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
        ctx.type_cache.cached_string(
            (0, 0, 0, 0),
            "member-type",
            format!("{class_fqn}::{member_name}"),
            || resolve_member_type_from_index(ctx.index, class_fqn, member_name),
        )
    };
    let callable_param_resolver = |callable_ctx: CallableParameterContext<'_>| {
        resolve_callable_parameter_type_from_index(ctx.index, ctx.file_symbols, callable_ctx)
    };
    let parser_info = infer_variable_hover_info_at_node_with_resolvers(
        variable_node,
        ctx.source,
        ctx.file_symbols,
        parser_usage_start,
        &variable_name,
        Some(&resolver),
        Some(&callable_param_resolver),
    );
    let type_hint = parser_info
        .as_ref()
        .and_then(|info| {
            info.phpdoc_comment
                .as_ref()
                .and_then(|_| local_variable_type_from_hover_info(info, ctx.file_symbols, true))
        })
        .or_else(|| rhs_node.and_then(|rhs| local_variable_inlay_type_from_expression(ctx, rhs)))
        .or_else(|| foreach_variable_inlay_type_from_index(ctx, variable_node))
        .or_else(|| {
            parser_info
                .as_ref()
                .and_then(|info| local_variable_type_from_hover_info(info, ctx.file_symbols, true))
        });
    let phpdoc_comment = parser_info.and_then(|info| info.phpdoc_comment);

    if type_hint.is_none() && phpdoc_comment.is_none() {
        return None;
    }

    Some(LocalVariableHoverData {
        variable_name,
        type_hint,
        phpdoc_comment,
    })
}

pub(in crate::server) fn current_assignment_rhs_for_variable<'tree>(
    variable_node: tree_sitter::Node<'tree>,
    source: &str,
) -> Option<tree_sitter::Node<'tree>> {
    let assignment = variable_node.parent()?;
    if assignment.kind() != "assignment_expression" {
        return None;
    }
    let left = assignment.child_by_field_name("left")?;
    let right = assignment.child_by_field_name("right")?;
    (left.id() == variable_node.id() && is_plain_assignment_expression(left, right, source))
        .then_some(right)
}

pub(in crate::server) fn latest_assignment_rhs_before_usage<'tree>(
    node: tree_sitter::Node<'tree>,
    variable_name: &str,
    usage_start: usize,
    source: &str,
) -> Option<(usize, tree_sitter::Node<'tree>)> {
    let mut best = None;
    collect_latest_assignment_rhs_before_usage(
        node,
        variable_name,
        usage_start,
        source,
        &mut best,
        true,
    );
    best
}

pub(in crate::server) fn collect_latest_assignment_rhs_before_usage<'tree>(
    node: tree_sitter::Node<'tree>,
    variable_name: &str,
    usage_start: usize,
    source: &str,
    best: &mut Option<(usize, tree_sitter::Node<'tree>)>,
    is_scope_root: bool,
) {
    if node.start_byte() > usage_start {
        return;
    }
    if !is_scope_root && is_variable_inference_scope_boundary_for_hover(node) {
        return;
    }

    if let Some(rhs) = assignment_rhs_for_variable_node(node, variable_name, source)
        .filter(|rhs| rhs.end_byte() <= usage_start)
    {
        let candidate = (node.start_byte(), rhs);
        if best
            .as_ref()
            .is_none_or(|(best_start, _)| candidate.0 >= *best_start)
        {
            *best = Some(candidate);
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.start_byte() > usage_start {
            break;
        }
        collect_latest_assignment_rhs_before_usage(
            child,
            variable_name,
            usage_start,
            source,
            best,
            false,
        );
    }
}

pub(in crate::server) fn assignment_rhs_for_variable_node<'tree>(
    node: tree_sitter::Node<'tree>,
    variable_name: &str,
    source: &str,
) -> Option<tree_sitter::Node<'tree>> {
    if node.kind() != "assignment_expression" {
        return None;
    }
    let left = node.child_by_field_name("left")?;
    let right = node.child_by_field_name("right")?;
    if left.kind() != "variable_name"
        || variable_text_for_node(source, left).as_deref() != Some(variable_name)
        || !is_plain_assignment_expression(left, right, source)
    {
        return None;
    }
    Some(right)
}

pub(in crate::server) fn local_variable_scope_node(
    mut node: tree_sitter::Node,
) -> tree_sitter::Node {
    loop {
        if matches!(
            node.kind(),
            "method_declaration" | "function_definition" | "anonymous_function"
        ) {
            return node;
        }
        let Some(parent) = node.parent() else {
            return node;
        };
        node = parent;
    }
}

pub(in crate::server) fn is_variable_inference_scope_boundary_for_hover(
    node: tree_sitter::Node,
) -> bool {
    matches!(
        node.kind(),
        "method_declaration"
            | "function_definition"
            | "arrow_function"
            | "anonymous_function"
            | "anonymous_function_creation_expression"
            | "class_declaration"
            | "interface_declaration"
            | "trait_declaration"
            | "enum_declaration"
    )
}

pub(in crate::server) fn local_variable_type_from_hover_info(
    info: &php_lsp_parser::resolve::VariableHoverInfo,
    file_symbols: &php_lsp_types::FileSymbols,
    allow_scalar: bool,
) -> Option<LocalVariableInlayType> {
    let display = info
        .type_display
        .as_deref()
        .or(info.resolved_type_fqn.as_deref())?
        .trim();
    if display.is_empty() || (!allow_scalar && !is_useful_local_variable_type_hint(display)) {
        return None;
    }

    let target_fqn = info.resolved_type_fqn.as_ref().and_then(|fqn| {
        type_display_has_single_object_target(display).then(|| {
            fqn.trim_start_matches('\\')
                .trim_start_matches('?')
                .to_string()
        })
    });

    Some(LocalVariableInlayType {
        display: shorten_inlay_type_display(display, file_symbols),
        target_fqn,
    })
}

pub(in crate::server) fn local_variable_inlay_type_from_type_info(
    ctx: &InlayHintContext<'_>,
    owner_fqn: &str,
    uri: &str,
    type_info: &php_lsp_types::TypeInfo,
    allow_scalar: bool,
) -> Option<LocalVariableInlayType> {
    if !is_explicit_local_variable_type_hint(type_info) {
        return None;
    }

    let display =
        local_variable_type_info_display(ctx.index, owner_fqn, uri, type_info, ctx.file_symbols);
    if display.trim().is_empty()
        || (!allow_scalar && !is_useful_local_variable_type_hint(display.as_str()))
    {
        return None;
    }

    Some(LocalVariableInlayType {
        display,
        target_fqn: single_inlay_target_fqn_from_type_info(ctx.index, owner_fqn, uri, type_info),
    })
}

pub(in crate::server) fn local_variable_type_info_display(
    index: &WorkspaceIndex,
    owner_fqn: &str,
    uri: &str,
    type_info: &php_lsp_types::TypeInfo,
    file_symbols: &php_lsp_types::FileSymbols,
) -> String {
    match type_info {
        php_lsp_types::TypeInfo::Simple(name) => {
            local_variable_simple_type_display(index, owner_fqn, uri, name, file_symbols)
        }
        php_lsp_types::TypeInfo::Generic { base, args } => {
            let base =
                local_variable_simple_type_display(index, owner_fqn, uri, base, file_symbols);
            let args = args
                .iter()
                .map(|arg| {
                    local_variable_type_info_display(index, owner_fqn, uri, arg, file_symbols)
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("{base}<{args}>")
        }
        php_lsp_types::TypeInfo::Union(types) => types
            .iter()
            .map(|type_info| {
                local_variable_type_info_display(index, owner_fqn, uri, type_info, file_symbols)
            })
            .collect::<Vec<_>>()
            .join("|"),
        php_lsp_types::TypeInfo::Intersection(types) => types
            .iter()
            .map(|type_info| {
                local_variable_type_info_display(index, owner_fqn, uri, type_info, file_symbols)
            })
            .collect::<Vec<_>>()
            .join("&"),
        php_lsp_types::TypeInfo::Nullable(inner) => {
            format!(
                "?{}",
                local_variable_type_info_display(index, owner_fqn, uri, inner, file_symbols)
            )
        }
        php_lsp_types::TypeInfo::Conditional {
            if_type, else_type, ..
        } => [if_type.as_ref(), else_type.as_ref()]
            .into_iter()
            .map(|type_info| {
                local_variable_type_info_display(index, owner_fqn, uri, type_info, file_symbols)
            })
            .collect::<Vec<_>>()
            .join("|"),
        php_lsp_types::TypeInfo::Self_ | php_lsp_types::TypeInfo::Static_ => {
            shorten_inlay_type_display(owner_fqn, file_symbols)
        }
        php_lsp_types::TypeInfo::Parent_ => "parent".to_string(),
        php_lsp_types::TypeInfo::ArrayShape(_)
        | php_lsp_types::TypeInfo::ObjectShape(_)
        | php_lsp_types::TypeInfo::Callable { .. }
        | php_lsp_types::TypeInfo::ClassString(_)
        | php_lsp_types::TypeInfo::LiteralString(_)
        | php_lsp_types::TypeInfo::LiteralInt(_)
        | php_lsp_types::TypeInfo::LiteralFloat(_)
        | php_lsp_types::TypeInfo::LiteralBool(_)
        | php_lsp_types::TypeInfo::LiteralNull
        | php_lsp_types::TypeInfo::Void
        | php_lsp_types::TypeInfo::Never
        | php_lsp_types::TypeInfo::Mixed => type_info.to_string(),
    }
}

pub(in crate::server) fn local_variable_simple_type_display(
    index: &WorkspaceIndex,
    owner_fqn: &str,
    uri: &str,
    name: &str,
    file_symbols: &php_lsp_types::FileSymbols,
) -> String {
    let name = name.trim();
    let lower = name.trim_start_matches('\\').to_ascii_lowercase();
    if matches!(lower.as_str(), "self" | "static") && !owner_fqn.is_empty() {
        return shorten_inlay_type_display(owner_fqn, file_symbols);
    }
    if lower == "parent" {
        return "parent".to_string();
    }
    if is_builtin_type_name(name) {
        return name.trim_start_matches('\\').to_string();
    }

    simple_type_fqn_from_owner_or_index(index, owner_fqn, uri, name)
        .map(|fqn| shorten_inlay_type_display(&fqn, file_symbols))
        .unwrap_or_else(|| shorten_inlay_type_display(name, file_symbols))
}

pub(in crate::server) fn single_inlay_target_fqn_from_type_info(
    index: &WorkspaceIndex,
    owner_fqn: &str,
    uri: &str,
    type_info: &php_lsp_types::TypeInfo,
) -> Option<String> {
    match type_info {
        php_lsp_types::TypeInfo::Simple(name) => {
            let lower = name.trim_start_matches('\\').to_ascii_lowercase();
            if matches!(lower.as_str(), "self" | "static") && !owner_fqn.is_empty() {
                return Some(owner_fqn.trim_start_matches('\\').to_string());
            }
            if lower == "parent" || is_builtin_type_name(name) {
                return None;
            }
            simple_type_fqn_from_owner_or_index(index, owner_fqn, uri, name)
        }
        php_lsp_types::TypeInfo::Nullable(inner) => {
            single_inlay_target_fqn_from_type_info(index, owner_fqn, uri, inner)
        }
        php_lsp_types::TypeInfo::Self_ | php_lsp_types::TypeInfo::Static_
            if !owner_fqn.is_empty() =>
        {
            Some(owner_fqn.trim_start_matches('\\').to_string())
        }
        _ => None,
    }
}

pub(in crate::server) fn simple_type_fqn_from_owner_or_index(
    index: &WorkspaceIndex,
    owner_fqn: &str,
    uri: &str,
    type_name: &str,
) -> Option<String> {
    let type_name = type_name.trim();
    if type_name.is_empty()
        || type_name.starts_with('\\')
        || type_name.contains('\\')
        || is_builtin_type_name(type_name)
    {
        return simple_type_fqn_from_index(index, uri, type_name);
    }

    if let Some((owner_namespace, _)) = owner_fqn.rsplit_once('\\') {
        let candidate = format!("{owner_namespace}\\{type_name}");
        if index.resolve_fqn(&candidate).is_some() {
            return Some(candidate);
        }
    }

    simple_type_fqn_from_index(index, uri, type_name)
}

pub(in crate::server) fn is_explicit_local_variable_type_hint(
    type_info: &php_lsp_types::TypeInfo,
) -> bool {
    match type_info {
        php_lsp_types::TypeInfo::Void
        | php_lsp_types::TypeInfo::Never
        | php_lsp_types::TypeInfo::Mixed
        | php_lsp_types::TypeInfo::LiteralNull => false,
        php_lsp_types::TypeInfo::Simple(name) => {
            let lower = name.trim_start_matches('\\').to_ascii_lowercase();
            !matches!(lower.as_str(), "mixed" | "void" | "never" | "null")
        }
        php_lsp_types::TypeInfo::Nullable(inner) => is_explicit_local_variable_type_hint(inner),
        php_lsp_types::TypeInfo::Union(types) | php_lsp_types::TypeInfo::Intersection(types) => {
            types.iter().any(is_explicit_local_variable_type_hint)
        }
        _ => true,
    }
}

pub(in crate::server) fn type_display_has_single_object_target(display: &str) -> bool {
    let display = display.trim().trim_start_matches('?');
    !display.is_empty()
        && !display.contains(['<', '>', '{', '}', '|', '&', '(', ')', ',', ' '])
        && !is_scalar_local_variable_type_hint(display)
}

pub(in crate::server) fn local_variable_inlay_label(
    ctx: &InlayHintContext<'_>,
    type_hint: &LocalVariableInlayType,
) -> InlayHintLabel {
    if let Some(target_fqn) = type_hint.target_fqn.as_deref().filter(|fqn| {
        ctx.index
            .resolve_fqn(fqn.trim_start_matches('\\'))
            .is_some_and(|symbol| {
                matches!(
                    symbol.kind,
                    php_lsp_types::PhpSymbolKind::Class
                        | php_lsp_types::PhpSymbolKind::Interface
                        | php_lsp_types::PhpSymbolKind::Trait
                        | php_lsp_types::PhpSymbolKind::Enum
                )
            })
    }) {
        let mut parts = vec![InlayHintLabelPart {
            value: ": ".to_string(),
            ..Default::default()
        }];
        let clickable_value = if let Some(rest) = type_hint.display.strip_prefix('?') {
            parts.push(InlayHintLabelPart {
                value: "?".to_string(),
                ..Default::default()
            });
            rest.to_string()
        } else {
            type_hint.display.clone()
        };

        parts.push(InlayHintLabelPart {
            value: clickable_value,
            tooltip: Some(InlayHintLabelPartTooltip::String(target_fqn.to_string())),
            location: None,
            command: None,
        });

        return InlayHintLabel::LabelParts(parts);
    }

    InlayHintLabel::String(format!(": {}", type_hint.display))
}

pub(in crate::server) fn local_variable_inlay_tooltip(
    type_hint: &LocalVariableInlayType,
) -> String {
    let type_text = type_hint
        .target_fqn
        .as_deref()
        .unwrap_or(type_hint.display.as_str());
    format!("Inferred local variable type: {type_text}")
}

pub(in crate::server) fn markdown_type_info_class_links(
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    owner_fqn: &str,
    uri: &str,
    type_info: &php_lsp_types::TypeInfo,
) -> Option<String> {
    let (markdown, has_links) =
        markdown_type_info_inner(index, file_symbols, owner_fqn, uri, type_info);
    has_links.then_some(markdown)
}

fn markdown_type_info_inner(
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    owner_fqn: &str,
    uri: &str,
    type_info: &php_lsp_types::TypeInfo,
) -> (String, bool) {
    match type_info {
        php_lsp_types::TypeInfo::Simple(name) => {
            markdown_simple_type_name(index, file_symbols, owner_fqn, uri, name)
        }
        php_lsp_types::TypeInfo::Generic { base, args } => {
            let (base, base_has_link) =
                markdown_simple_type_name(index, file_symbols, owner_fqn, uri, base);
            let mut has_links = base_has_link;
            let args = args
                .iter()
                .map(|arg| {
                    let (arg, arg_has_link) =
                        markdown_type_info_inner(index, file_symbols, owner_fqn, uri, arg);
                    has_links |= arg_has_link;
                    arg
                })
                .collect::<Vec<_>>()
                .join(", ");
            (format!("{base}&lt;{args}&gt;"), has_links)
        }
        php_lsp_types::TypeInfo::ArrayShape(items) => {
            markdown_shape_type_info(index, file_symbols, owner_fqn, uri, "array", items)
        }
        php_lsp_types::TypeInfo::ObjectShape(items) => {
            markdown_shape_type_info(index, file_symbols, owner_fqn, uri, "object", items)
        }
        php_lsp_types::TypeInfo::Callable {
            params,
            return_type,
        } => {
            let mut has_links = false;
            let params = params
                .iter()
                .map(|param| {
                    let (param, param_has_link) =
                        markdown_type_info_inner(index, file_symbols, owner_fqn, uri, param);
                    has_links |= param_has_link;
                    param
                })
                .collect::<Vec<_>>()
                .join(", ");
            let mut markdown = format!("{}({params})", markdown_code_span("callable"));
            if let Some(return_type) = return_type {
                let (return_type, return_has_link) =
                    markdown_type_info_inner(index, file_symbols, owner_fqn, uri, return_type);
                has_links |= return_has_link;
                markdown.push_str(": ");
                markdown.push_str(&return_type);
            }
            (markdown, has_links)
        }
        php_lsp_types::TypeInfo::ClassString(Some(inner)) => {
            let (inner, has_links) =
                markdown_type_info_inner(index, file_symbols, owner_fqn, uri, inner);
            (
                format!("{}&lt;{inner}&gt;", markdown_code_span("class-string")),
                has_links,
            )
        }
        php_lsp_types::TypeInfo::ClassString(None) => (markdown_code_span("class-string"), false),
        php_lsp_types::TypeInfo::Conditional {
            subject,
            target,
            if_type,
            else_type,
        } => {
            let (target, target_has_link) =
                markdown_type_info_inner(index, file_symbols, owner_fqn, uri, target);
            let (if_type, if_has_link) =
                markdown_type_info_inner(index, file_symbols, owner_fqn, uri, if_type);
            let (else_type, else_has_link) =
                markdown_type_info_inner(index, file_symbols, owner_fqn, uri, else_type);
            (
                format!(
                    "({} is {target} ? {if_type} : {else_type})",
                    markdown_code_span(subject)
                ),
                target_has_link || if_has_link || else_has_link,
            )
        }
        php_lsp_types::TypeInfo::Union(types) => {
            markdown_joined_type_info(index, file_symbols, owner_fqn, uri, types, "|")
        }
        php_lsp_types::TypeInfo::Intersection(types) => {
            markdown_joined_type_info(index, file_symbols, owner_fqn, uri, types, "&")
        }
        php_lsp_types::TypeInfo::Nullable(inner) => {
            let (inner, has_links) =
                markdown_type_info_inner(index, file_symbols, owner_fqn, uri, inner);
            (format!("?{inner}"), has_links)
        }
        php_lsp_types::TypeInfo::Self_ => {
            markdown_special_type_name(index, owner_fqn, "self", owner_fqn)
        }
        php_lsp_types::TypeInfo::Static_ => {
            markdown_special_type_name(index, owner_fqn, "static", owner_fqn)
        }
        php_lsp_types::TypeInfo::Parent_ => {
            let parent_fqn = parent_type_fqn(index, file_symbols, owner_fqn);
            match parent_fqn {
                Some(parent_fqn) => {
                    markdown_special_type_name(index, &parent_fqn, "parent", owner_fqn)
                }
                None => (markdown_code_span("parent"), false),
            }
        }
        php_lsp_types::TypeInfo::LiteralString(_)
        | php_lsp_types::TypeInfo::LiteralInt(_)
        | php_lsp_types::TypeInfo::LiteralFloat(_)
        | php_lsp_types::TypeInfo::LiteralBool(_)
        | php_lsp_types::TypeInfo::LiteralNull
        | php_lsp_types::TypeInfo::Void
        | php_lsp_types::TypeInfo::Never
        | php_lsp_types::TypeInfo::Mixed => (markdown_code_span(&type_info.to_string()), false),
    }
}

fn markdown_joined_type_info(
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    owner_fqn: &str,
    uri: &str,
    types: &[php_lsp_types::TypeInfo],
    separator: &str,
) -> (String, bool) {
    let mut has_links = false;
    let markdown = types
        .iter()
        .map(|type_info| {
            let (part, part_has_link) =
                markdown_type_info_inner(index, file_symbols, owner_fqn, uri, type_info);
            has_links |= part_has_link;
            part
        })
        .collect::<Vec<_>>()
        .join(separator);
    (markdown, has_links)
}

fn markdown_shape_type_info(
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    owner_fqn: &str,
    uri: &str,
    shape_kind: &str,
    items: &[php_lsp_types::ArrayShapeItem],
) -> (String, bool) {
    let mut has_links = false;
    let items = items
        .iter()
        .map(|item| {
            let (value, value_has_link) =
                markdown_type_info_inner(index, file_symbols, owner_fqn, uri, &item.value);
            has_links |= value_has_link;
            match item.key.as_deref() {
                Some(key) if item.optional => format!("{}?: {value}", markdown_code_span(key)),
                Some(key) => format!("{}: {value}", markdown_code_span(key)),
                None => value,
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    (
        format!("{}{{{items}}}", markdown_code_span(shape_kind)),
        has_links,
    )
}

fn markdown_simple_type_name(
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    owner_fqn: &str,
    uri: &str,
    name: &str,
) -> (String, bool) {
    let display = name.trim();
    let lower = display.trim_start_matches('\\').to_ascii_lowercase();
    match lower.as_str() {
        "self" | "static" => {
            return markdown_special_type_name(index, owner_fqn, display, owner_fqn)
        }
        "parent" => {
            let Some(parent_fqn) = parent_type_fqn(index, file_symbols, owner_fqn) else {
                return (markdown_code_span(display), false);
            };
            return markdown_special_type_name(index, &parent_fqn, display, owner_fqn);
        }
        _ => {}
    }
    if is_builtin_type_name(display) {
        return (markdown_code_span(display.trim_start_matches('\\')), false);
    }

    let target_fqn = simple_type_fqn_from_owner_or_index(index, owner_fqn, uri, display);
    markdown_type_name_link(index, display, target_fqn.as_deref())
}

fn markdown_special_type_name(
    index: &WorkspaceIndex,
    target_fqn: &str,
    display: &str,
    owner_fqn: &str,
) -> (String, bool) {
    if owner_fqn.trim().is_empty() || target_fqn.trim().is_empty() {
        return (markdown_code_span(display), false);
    }
    markdown_type_name_link(index, display, Some(target_fqn))
}

fn markdown_type_name_link(
    index: &WorkspaceIndex,
    display: &str,
    target_fqn: Option<&str>,
) -> (String, bool) {
    let Some(target_fqn) = target_fqn else {
        return (markdown_code_span(display), false);
    };
    let Some(symbol) = index.resolve_fqn(target_fqn.trim_start_matches('\\')) else {
        return (markdown_code_span(display), false);
    };
    if !is_class_like_symbol(&symbol) {
        return (markdown_code_span(display), false);
    }
    let destination = markdown_file_location_destination(&symbol);
    (
        format!("[{}](<{}>)", markdown_code_span(display), destination),
        true,
    )
}

fn parent_type_fqn(
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    owner_fqn: &str,
) -> Option<String> {
    let owner_fqn = owner_fqn.trim_start_matches('\\');
    if owner_fqn.is_empty() {
        return None;
    }

    file_symbols
        .symbols
        .iter()
        .find(|symbol| fqn_matches(&symbol.fqn, owner_fqn))
        .and_then(|symbol| symbol.extends.first().cloned())
        .or_else(|| {
            index
                .resolve_fqn(owner_fqn)
                .and_then(|symbol| symbol.extends.first().cloned())
        })
}

fn is_class_like_symbol(symbol: &php_lsp_types::SymbolInfo) -> bool {
    matches!(
        symbol.kind,
        php_lsp_types::PhpSymbolKind::Class
            | php_lsp_types::PhpSymbolKind::Interface
            | php_lsp_types::PhpSymbolKind::Trait
            | php_lsp_types::PhpSymbolKind::Enum
    )
}

pub(in crate::server) fn local_variable_type_markdown(
    index: &WorkspaceIndex,
    type_hint: &LocalVariableInlayType,
) -> String {
    let Some(target_fqn) = type_hint.target_fqn.as_deref() else {
        return markdown_code_span(&type_hint.display);
    };
    let Some(symbol) = index.resolve_fqn(target_fqn.trim_start_matches('\\')) else {
        return markdown_code_span(&type_hint.display);
    };
    let destination = markdown_file_location_destination(&symbol);
    if let Some(rest) = type_hint.display.strip_prefix('?') {
        return format!("?[{}](<{}>)", markdown_code_span(rest), destination);
    }
    format!(
        "[{}](<{}>)",
        markdown_code_span(&type_hint.display),
        destination
    )
}

pub(in crate::server) fn magic_property_hover_markdown(
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    sym_at_pos: &SymbolAtPosition,
) -> Option<String> {
    if sym_at_pos.ref_kind != RefKind::PropertyAccess {
        return None;
    }
    let (class_fqn, member_name) = sym_at_pos.fqn.rsplit_once("::")?;
    let getter_fqn = format!("{class_fqn}::__get");
    let getter = index.resolve_fqn(&getter_fqn)?;
    if getter.kind != php_lsp_types::PhpSymbolKind::Method {
        return None;
    }
    let return_type = symbol_effective_return_type(&getter)?;
    if !is_explicit_local_variable_type_hint(&return_type) {
        return None;
    }

    let owner_fqn = getter.parent_fqn.as_deref().unwrap_or(class_fqn);
    let display =
        local_variable_type_info_display(index, owner_fqn, &getter.uri, &return_type, file_symbols);
    if display.trim().is_empty() {
        return None;
    }

    let type_hint = LocalVariableInlayType {
        display: display.clone(),
        target_fqn: single_inlay_target_fqn_from_type_info(
            index,
            owner_fqn,
            &getter.uri,
            &return_type,
        ),
    };

    let mut content = String::new();
    content.push_str("```php\n");
    content.push_str("property ");
    content.push_str(class_fqn);
    content.push_str("::");
    content.push_str(member_name);
    content.push_str(": ");
    content.push_str(&display);
    content.push_str("\n```\n");
    content.push_str("\n**Type:** ");
    content.push_str(&local_variable_type_markdown(index, &type_hint));
    content.push('\n');
    Some(content)
}

pub(in crate::server) fn markdown_file_location_destination(
    symbol: &php_lsp_types::SymbolInfo,
) -> String {
    let line = symbol.selection_range.0.saturating_add(1);
    format!("{}#L{}", symbol.uri, line)
}

pub(in crate::server) fn markdown_code_span(text: &str) -> String {
    if text.contains('`') {
        format!("`` {} ``", text)
    } else {
        format!("`{}`", text)
    }
}

pub(in crate::server) fn shorten_inlay_type_display(
    display: &str,
    file_symbols: &php_lsp_types::FileSymbols,
) -> String {
    if !display.contains('\\')
        || display.contains(['<', '>', '{', '}', '|', '&', '?', '(', ')', ',', ' '])
    {
        return display.to_string();
    }

    if let Some(use_stmt) = file_symbols
        .use_statements
        .iter()
        .find(|use_stmt| use_stmt.kind == php_lsp_types::UseKind::Class && use_stmt.fqn == display)
    {
        return use_stmt
            .alias
            .clone()
            .unwrap_or_else(|| display.rsplit('\\').next().unwrap_or(display).to_string());
    }

    if let Some(namespace) = file_symbols.namespace.as_deref() {
        if let Some(rest) = display
            .strip_prefix(namespace)
            .and_then(|rest| rest.strip_prefix('\\'))
        {
            return rest.to_string();
        }
    }

    display.rsplit('\\').next().unwrap_or(display).to_string()
}

pub(in crate::server) fn is_useful_local_variable_type_hint(display: &str) -> bool {
    let display = display.trim();
    if display.is_empty() {
        return false;
    }

    if display.contains('<') || display.contains('{') || display.contains('\\') {
        return true;
    }
    if display.contains('|') {
        return display.split('|').any(is_useful_local_variable_type_hint);
    }
    if display.contains('&') {
        return display.split('&').any(is_useful_local_variable_type_hint);
    }

    !is_scalar_local_variable_type_hint(display.trim_start_matches('?'))
}

pub(in crate::server) fn is_scalar_local_variable_type_hint(display: &str) -> bool {
    matches!(
        display
            .trim_start_matches('\\')
            .to_ascii_lowercase()
            .as_str(),
        "array"
            | "bool"
            | "boolean"
            | "callable"
            | "false"
            | "float"
            | "int"
            | "integer"
            | "iterable"
            | "mixed"
            | "never"
            | "null"
            | "object"
            | "resource"
            | "scalar"
            | "string"
            | "true"
            | "void"
    )
}

pub(in crate::server) fn collect_phpdoc_parameter_type_inlay_hints(
    node: tree_sitter::Node,
    source: &str,
    utf16_index: &Utf16LineIndex,
    requested_range: (u32, u32, u32, u32),
    hints: &mut Vec<InlayHint>,
) {
    if matches!(node.kind(), "function_definition" | "method_declaration") {
        add_phpdoc_parameter_type_inlay_hints(node, source, utf16_index, requested_range, hints);
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_phpdoc_parameter_type_inlay_hints(
            child,
            source,
            utf16_index,
            requested_range,
            hints,
        );
    }
}

pub(in crate::server) fn add_phpdoc_parameter_type_inlay_hints(
    node: tree_sitter::Node,
    source: &str,
    utf16_index: &Utf16LineIndex,
    requested_range: (u32, u32, u32, u32),
    hints: &mut Vec<InlayHint>,
) {
    let Some(doc_comment) = doc_comment_before_node(node, source) else {
        return;
    };
    let phpdoc = parse_phpdoc(&doc_comment);
    if phpdoc.params.is_empty() {
        return;
    }

    let Some(parameters) = node.child_by_field_name("parameters") else {
        return;
    };
    let mut cursor = parameters.walk();
    for parameter in parameters.named_children(&mut cursor) {
        if !matches!(
            parameter.kind(),
            "simple_parameter" | "variadic_parameter" | "property_promotion_parameter"
        ) || parameter.child_by_field_name("type").is_some()
        {
            continue;
        }
        let Some(name_node) = parameter.child_by_field_name("name") else {
            continue;
        };
        if !byte_ranges_overlap(node_range_node(name_node), requested_range) {
            continue;
        }
        let raw_name = node_text(source, name_node);
        let name = raw_name.trim_start_matches('$');
        let Some(param_doc) = phpdoc.params.iter().find(|param| param.name == name) else {
            continue;
        };
        let Some(type_info) = param_doc.type_info.as_ref() else {
            continue;
        };
        let end = name_node.end_position();
        hints.push(InlayHint {
            position: Position::new(
                end.row as u32,
                utf16_index.byte_col_to_utf16(end.row as u32, end.column as u32),
            ),
            label: InlayHintLabel::String(format!(": {}", type_info)),
            kind: Some(InlayHintKind::TYPE),
            text_edits: None,
            tooltip: Some(InlayHintTooltip::String("PHPDoc @param".to_string())),
            padding_left: Some(false),
            padding_right: Some(false),
            data: None,
        });
    }
}

pub(in crate::server) fn collect_phpdoc_return_type_inlay_hints(
    tree: &tree_sitter::Tree,
    source: &str,
    utf16_index: &Utf16LineIndex,
    requested_range: (u32, u32, u32, u32),
    php_version: PhpVersion,
    hints: &mut Vec<InlayHint>,
) {
    for candidate in find_missing_return_type_candidates(tree, source, requested_range) {
        let label = return_type_hint(&candidate.return_type, php_version)
            .unwrap_or_else(|| candidate.return_type.to_string());
        hints.push(InlayHint {
            position: Position::new(
                candidate.insert_position.0,
                utf16_index
                    .byte_col_to_utf16(candidate.insert_position.0, candidate.insert_position.1),
            ),
            label: InlayHintLabel::String(format!(": {}", label)),
            kind: Some(InlayHintKind::TYPE),
            text_edits: None,
            tooltip: Some(InlayHintTooltip::String("PHPDoc @return".to_string())),
            padding_left: Some(false),
            padding_right: Some(false),
            data: None,
        });
    }
}

pub(in crate::server) fn doc_comment_before_node(
    node: tree_sitter::Node,
    source: &str,
) -> Option<String> {
    let mut prev = node.prev_sibling();
    while let Some(sibling) = prev {
        if sibling.kind() == "comment" {
            let text = node_text(source, sibling);
            if text.starts_with("/**") {
                return Some(text.to_string());
            }
            return None;
        }
        prev = sibling.prev_sibling();
    }
    None
}

pub(in crate::server) fn inlay_hint_label_text(label: &InlayHintLabel) -> String {
    match label {
        InlayHintLabel::String(value) => value.clone(),
        InlayHintLabel::LabelParts(parts) => parts.iter().map(|part| part.value.as_str()).collect(),
    }
}
