//! Template-aware LSP helpers extracted from `server.rs`.

use crate::template::{TemplateShapeDefinitionTarget, TemplateShapeKeyDefinition};
use crate::util::uri::path_to_uri;

use super::super::*;

const TWIG_CONTEXT_PHP_FILE_SCAN_LIMIT: usize = 2048;
const OPEN_TWIG_CONTEXT_REFRESH_LIMIT: usize = 64;

pub(in crate::server) fn template_kind_for_document(
    uri_str: &str,
    language_id: &str,
) -> Option<TemplateKind> {
    if is_blade_template_uri(uri_str) || is_blade_template_language_id(language_id) {
        return Some(TemplateKind::Blade);
    }
    if is_twig_template_uri(uri_str) || is_twig_template_language_id(language_id) {
        return Some(TemplateKind::Twig);
    }
    None
}

pub(in crate::server) fn twig_template_name_for_uri(uri_str: &str, root: &Path) -> Option<String> {
    let path = uri_to_path(uri_str)?;
    for base in [root.join("templates"), root.join("resources/views")] {
        if let Ok(relative) = path.strip_prefix(&base) {
            return normalize_twig_template_name(relative);
        }
    }

    path.file_name()
        .and_then(|file| file.to_str())
        .filter(|file| file.ends_with(".twig"))
        .map(str::to_string)
}

fn workspace_root_for_template_context_uri(
    uri_str: &str,
    workspace_roots: &[PathBuf],
) -> Option<PathBuf> {
    if let Some(path) = uri_to_path(uri_str) {
        if let Some(root) = workspace_roots
            .iter()
            .filter(|root| path.starts_with(root))
            .max_by_key(|root| root.components().count())
        {
            return Some(root.clone());
        }
    }

    workspace_roots.first().cloned()
}

pub(in crate::server) fn twig_template_path_for_key(root: &Path, key: &str) -> Option<PathBuf> {
    let normalized = normalize_twig_key(key);
    if normalized.is_empty()
        || normalized.starts_with('/')
        || normalized
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return None;
    }

    for base in [root.join("templates"), root.join("resources/views")] {
        let path = base.join(&normalized);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

pub(in crate::server) fn normalize_twig_template_name(path: &Path) -> Option<String> {
    let parts: Vec<String> = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect();
    (!parts.is_empty()).then(|| parts.join("/"))
}

pub(in crate::server) fn collect_twig_context_php_files(root: &Path, limit: usize) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for base in [root.join("src"), root.join("app"), root.join("tests")] {
        collect_twig_context_php_files_recursive(&base, limit, &mut files);
        if files.len() >= limit {
            break;
        }
    }
    files.sort();
    files
}

pub(in crate::server) fn collect_twig_context_php_files_recursive(
    root: &Path,
    limit: usize,
    files: &mut Vec<PathBuf>,
) {
    if files.len() >= limit || !root.is_dir() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        if files.len() >= limit {
            return;
        }
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.')
                || matches!(name.as_ref(), "vendor" | "node_modules" | "target" | "var")
            {
                continue;
            }
            collect_twig_context_php_files_recursive(&path, limit, files);
        } else if path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("php"))
        {
            files.push(path);
        }
    }
}

fn collect_twig_render_context_types(
    template_name: &str,
    source: &str,
    source_uri: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: Option<&WorkspaceIndex>,
    variables: &mut HashMap<String, String>,
    shape_definitions: &mut HashMap<String, Vec<TemplateShapeKeyDefinition>>,
) {
    let mut offset = 0usize;
    while let Some((_, name_end, open_paren)) = next_twig_render_call(source, offset) {
        let Some(close_paren) = find_matching_delimiter(source, open_paren, '(', ')') else {
            offset = name_end;
            continue;
        };
        let args = split_top_level_spans(
            source.get(open_paren + 1..close_paren).unwrap_or(""),
            open_paren + 1,
        );
        if args.len() >= 2 {
            let template_arg = trim_source_range(source, args[0].0, args[0].1);
            let context_arg = trim_source_range(source, args[1].0, args[1].1);
            if php_string_literal_value_at_range(source, template_arg.0, template_arg.1)
                .is_some_and(|name| normalize_twig_key(&name) == normalize_twig_key(template_name))
                && !collect_twig_context_compact_types(
                    source,
                    source_uri,
                    context_arg,
                    file_symbols,
                    index,
                    variables,
                    shape_definitions,
                )
            {
                collect_twig_context_array_types(
                    source,
                    source_uri,
                    context_arg,
                    file_symbols,
                    index,
                    variables,
                    shape_definitions,
                );
            }
        }
        offset = close_paren + 1;
    }
}

fn collect_twig_context_compact_types(
    source: &str,
    source_uri: &str,
    range: (usize, usize),
    file_symbols: &php_lsp_types::FileSymbols,
    index: Option<&WorkspaceIndex>,
    variables: &mut HashMap<String, String>,
    shape_definitions: &mut HashMap<String, Vec<TemplateShapeKeyDefinition>>,
) -> bool {
    let (start, end) = trim_source_range(source, range.0, range.1);
    let Some(value) = source.get(start..end).map(str::trim) else {
        return false;
    };
    if !value.starts_with("compact") {
        return false;
    }
    let open = skip_ascii_ws_server(source, start + "compact".len());
    if source.as_bytes().get(open) != Some(&b'(') {
        return false;
    }
    let Some(close) = find_matching_delimiter(source, open, '(', ')') else {
        return false;
    };
    if close > end {
        return false;
    }

    let shape_definition_ctx = TwigShapeDefinitionContext {
        source,
        source_uri,
        file_symbols,
        index,
    };
    let mut found = false;
    for arg in split_top_level_spans(source.get(open + 1..close).unwrap_or(""), open + 1) {
        let arg = trim_source_range(source, arg.0, arg.1);
        let Some(name) = php_string_literal_value_at_range(source, arg.0, arg.1) else {
            continue;
        };
        if !is_template_variable_name(&name) {
            continue;
        }
        found = true;
        let mut visited_variables = HashSet::new();
        let type_text = infer_twig_context_assignment_value_type(
            source,
            start,
            &name,
            file_symbols,
            index,
            &mut visited_variables,
        )
        .or_else(|| infer_twig_context_variable_type(source, start, &name, file_symbols))
        .unwrap_or_else(|| "mixed".to_string());
        merge_twig_context_variable_type(variables, name.clone(), type_text);
        let mut visited_variables = HashSet::new();
        let definitions = collect_twig_context_assignment_shape_definitions(
            &shape_definition_ctx,
            start,
            &name,
            TemplateShapeDefinitionTarget::Direct,
            &mut visited_variables,
        );
        merge_twig_context_variable_shape_definitions(shape_definitions, name, definitions);
    }

    found
}

pub(in crate::server) fn next_twig_render_call(
    source: &str,
    from: usize,
) -> Option<(usize, usize, usize)> {
    let mut offset = from;
    while offset < source.len() {
        let byte = *source.as_bytes().get(offset)?;
        if !is_ident_byte(byte) {
            offset += 1;
            continue;
        }

        let start = offset;
        offset += 1;
        while offset < source.len() && is_ident_byte(source.as_bytes()[offset]) {
            offset += 1;
        }
        let name = source.get(start..offset)?;
        if matches!(name, "render" | "renderView") {
            let open = skip_ascii_ws_server(source, offset);
            if source.as_bytes().get(open) == Some(&b'(') {
                return Some((start, offset, open));
            }
        }
    }
    None
}

fn collect_twig_context_array_types(
    source: &str,
    source_uri: &str,
    range: (usize, usize),
    file_symbols: &php_lsp_types::FileSymbols,
    index: Option<&WorkspaceIndex>,
    variables: &mut HashMap<String, String>,
    shape_definitions: &mut HashMap<String, Vec<TemplateShapeKeyDefinition>>,
) {
    let (start, end) = range;
    let Some((inner_start, inner_end)) = php_array_inner_range(source, start, end) else {
        return;
    };
    let spans = split_top_level_spans(
        source.get(inner_start..inner_end).unwrap_or(""),
        inner_start,
    );
    let shape_definition_ctx = TwigShapeDefinitionContext {
        source,
        source_uri,
        file_symbols,
        index,
    };
    for span in spans {
        let Some(arrow) = find_top_level_double_arrow(source, span.0, span.1) else {
            continue;
        };
        let key_range = trim_source_range(source, span.0, arrow);
        let value_range = trim_source_range(source, arrow + 2, span.1);
        let Some(name) = php_string_literal_value_at_range(source, key_range.0, key_range.1) else {
            continue;
        };
        if !is_template_variable_name(&name) {
            continue;
        }
        let type_text = infer_twig_context_value_type(source, value_range, file_symbols, index)
            .unwrap_or_else(|| "mixed".to_string());
        merge_twig_context_variable_type(variables, name.clone(), type_text);
        let mut visited_variables = HashSet::new();
        let definitions = collect_twig_context_value_shape_definitions(
            &shape_definition_ctx,
            value_range,
            TemplateShapeDefinitionTarget::Direct,
            &mut visited_variables,
        );
        merge_twig_context_variable_shape_definitions(shape_definitions, name, definitions);
    }
}

fn merge_twig_context_variable_type(
    variables: &mut HashMap<String, String>,
    name: String,
    type_text: String,
) {
    if type_text == "mixed" {
        variables.entry(name).or_insert(type_text);
        return;
    }

    match variables.get(&name).map(String::as_str) {
        None | Some("mixed") => {
            variables.insert(name, type_text);
        }
        Some("null") => {
            variables.insert(name, twig_context_type_text_with_null(&type_text));
        }
        Some(existing) if type_text == "null" => {
            variables.insert(name, twig_context_type_text_with_null(existing));
        }
        Some(existing) if twig_context_type_text_has_null(&type_text) => {
            if let Some(merged) =
                merge_twig_context_type_texts(vec![existing.to_string(), type_text])
            {
                variables.insert(name, merged);
            }
        }
        Some(_) => {}
    }
}

fn merge_twig_context_variable_shape_definitions(
    shape_definitions: &mut HashMap<String, Vec<TemplateShapeKeyDefinition>>,
    name: String,
    definitions: Vec<TemplateShapeKeyDefinition>,
) {
    if definitions.is_empty() {
        return;
    }

    let entry = shape_definitions.entry(name).or_default();
    for definition in definitions {
        if !entry.iter().any(|existing| existing == &definition) {
            entry.push(definition);
        }
    }
}

struct TwigShapeDefinitionContext<'a> {
    source: &'a str,
    source_uri: &'a str,
    file_symbols: &'a php_lsp_types::FileSymbols,
    index: Option<&'a WorkspaceIndex>,
}

fn collect_twig_context_value_shape_definitions(
    ctx: &TwigShapeDefinitionContext<'_>,
    range: (usize, usize),
    default_target: TemplateShapeDefinitionTarget,
    visited_variables: &mut HashSet<String>,
) -> Vec<TemplateShapeKeyDefinition> {
    let (start, end) = trim_source_range(ctx.source, range.0, range.1);
    let value = ctx.source.get(start..end).unwrap_or("").trim();
    let mut definitions = Vec::new();

    collect_literal_array_shape_definitions(
        ctx,
        (start, end),
        default_target,
        &[],
        &mut definitions,
    );

    if let Some(variable_name) = simple_php_variable_name(value) {
        definitions.extend(collect_twig_context_assignment_shape_definitions(
            ctx,
            start,
            variable_name,
            default_target,
            visited_variables,
        ));
        return definitions;
    }

    if let Some(found) = collect_twig_context_member_call_shape_definitions(
        ctx,
        (start, end),
        default_target,
        visited_variables,
    ) {
        definitions.extend(found);
        return definitions;
    }

    if let Some(found) = collect_twig_context_member_access_shape_definitions(
        ctx,
        (start, end),
        default_target,
        visited_variables,
    ) {
        definitions.extend(found);
    }

    definitions
}

fn collect_twig_context_assignment_shape_definitions(
    ctx: &TwigShapeDefinitionContext<'_>,
    value_start: usize,
    variable_name: &str,
    default_target: TemplateShapeDefinitionTarget,
    visited_variables: &mut HashSet<String>,
) -> Vec<TemplateShapeKeyDefinition> {
    if !visited_variables.insert(variable_name.to_string()) {
        return Vec::new();
    }

    let mut definitions = Vec::new();
    if let Some(latest_assignment) = latest_simple_variable_assignment_before(
        ctx.source,
        value_start,
        variable_name,
        ctx.file_symbols,
    ) {
        if php_source_range_is_empty_array_literal(ctx.source, latest_assignment) {
            if let Some(append_values) = simple_variable_array_append_values_before(
                ctx.source,
                value_start,
                variable_name,
                ctx.file_symbols,
            ) {
                for append_value in append_values {
                    definitions.extend(collect_twig_context_value_shape_definitions(
                        ctx,
                        append_value,
                        TemplateShapeDefinitionTarget::IterableValue,
                        visited_variables,
                    ));
                }
            }
        } else {
            definitions.extend(collect_twig_context_value_shape_definitions(
                ctx,
                latest_assignment,
                default_target,
                visited_variables,
            ));
        }
    }

    visited_variables.remove(variable_name);
    definitions
}

fn collect_literal_array_shape_definitions(
    ctx: &TwigShapeDefinitionContext<'_>,
    range: (usize, usize),
    target: TemplateShapeDefinitionTarget,
    prefix: &[String],
    definitions: &mut Vec<TemplateShapeKeyDefinition>,
) {
    let Some((inner_start, inner_end)) = php_array_inner_range(ctx.source, range.0, range.1) else {
        return;
    };
    let spans = split_top_level_spans(
        ctx.source.get(inner_start..inner_end).unwrap_or(""),
        inner_start,
    );
    for span in spans {
        let span = trim_source_range(ctx.source, span.0, span.1);
        if span.0 >= span.1 {
            continue;
        }

        if let Some(arrow) = find_top_level_double_arrow(ctx.source, span.0, span.1) {
            let value_range = trim_source_range(ctx.source, arrow + 2, span.1);
            let Some((key, key_start, key_end)) =
                shape_key_from_raw_range(ctx.source, span.0, arrow)
            else {
                continue;
            };
            if key.is_empty() {
                continue;
            }
            let mut path = prefix.to_vec();
            path.push(key);
            if let Some(definition) = template_shape_key_definition_from_byte_range(
                ctx.source,
                ctx.source_uri,
                target,
                path.clone(),
                (key_start, key_end),
            ) {
                definitions.push(definition);
            }
            collect_literal_array_shape_definitions(ctx, value_range, target, &path, definitions);
            continue;
        }

        collect_literal_array_shape_definitions(
            ctx,
            span,
            TemplateShapeDefinitionTarget::IterableValue,
            prefix,
            definitions,
        );
    }
}

fn collect_twig_context_member_call_shape_definitions(
    ctx: &TwigShapeDefinitionContext<'_>,
    range: (usize, usize),
    default_target: TemplateShapeDefinitionTarget,
    visited_variables: &mut HashSet<String>,
) -> Option<Vec<TemplateShapeKeyDefinition>> {
    let index = ctx.index?;
    let (start, end) = trim_source_range(ctx.source, range.0, range.1);
    let call = php_member_call_parts(ctx.source, start, end)?;
    let receiver_type = twig_context_expression_type_text(
        ctx.source,
        call.object_range,
        ctx.file_symbols,
        Some(index),
        visited_variables,
    )?;
    let mut definitions = Vec::new();
    for receiver_fqn in twig_context_candidate_type_fqns(&receiver_type) {
        let method_fqn = format!("{receiver_fqn}::{}", call.method_name);
        let Some(symbol) = index.resolve_member(&method_fqn) else {
            continue;
        };
        if !matches!(symbol.kind, php_lsp_types::PhpSymbolKind::Method) {
            continue;
        }
        if let Some(return_type) = symbol_effective_return_type(&symbol) {
            definitions.extend(collect_symbol_type_shape_definitions(
                index,
                ctx.file_symbols,
                &symbol,
                &receiver_fqn,
                &return_type,
                default_target,
            ));
        }
    }
    (!definitions.is_empty()).then_some(definitions)
}

fn collect_twig_context_member_access_shape_definitions(
    ctx: &TwigShapeDefinitionContext<'_>,
    range: (usize, usize),
    default_target: TemplateShapeDefinitionTarget,
    visited_variables: &mut HashSet<String>,
) -> Option<Vec<TemplateShapeKeyDefinition>> {
    let index = ctx.index?;
    let (start, end) = trim_source_range(ctx.source, range.0, range.1);
    let access = php_member_access_parts(ctx.source, start, end)?;
    let receiver_type = twig_context_expression_type_text(
        ctx.source,
        access.object_range,
        ctx.file_symbols,
        Some(index),
        visited_variables,
    )?;
    let mut definitions = Vec::new();
    for receiver_fqn in twig_context_candidate_type_fqns(&receiver_type) {
        let property_fqn = format!("{receiver_fqn}::${}", access.member_name);
        let Some(symbol) = index.resolve_member(&property_fqn) else {
            continue;
        };
        if !matches!(symbol.kind, php_lsp_types::PhpSymbolKind::Property) {
            continue;
        }
        if let Some(property_type) = symbol_effective_return_type(&symbol) {
            definitions.extend(collect_symbol_type_shape_definitions(
                index,
                ctx.file_symbols,
                &symbol,
                &receiver_fqn,
                &property_type,
                default_target,
            ));
        }
    }
    (!definitions.is_empty()).then_some(definitions)
}

fn collect_symbol_type_shape_definitions(
    index: &WorkspaceIndex,
    fallback_file_symbols: &php_lsp_types::FileSymbols,
    symbol: &php_lsp_types::SymbolInfo,
    fallback_owner_fqn: &str,
    type_info: &php_lsp_types::TypeInfo,
    direct_target: TemplateShapeDefinitionTarget,
) -> Vec<TemplateShapeKeyDefinition> {
    let owner_fqn = symbol.parent_fqn.as_deref().unwrap_or(fallback_owner_fqn);
    let normalized_type_info =
        if let Some(symbol_file_symbols) = index.file_symbols.get(&symbol.uri) {
            resolve_type_info_with_context(type_info, &symbol_file_symbols, owner_fqn)
        } else {
            resolve_type_info_with_context(type_info, fallback_file_symbols, owner_fqn)
        };
    let Some(comment) = symbol.doc_comment.as_deref() else {
        return Vec::new();
    };
    let Some(doc_start) = symbol_doc_comment_start(symbol, comment) else {
        return Vec::new();
    };
    let mut requests = Vec::new();
    collect_type_info_shape_definition_requests(
        &normalized_type_info,
        direct_target,
        &[],
        &[],
        &mut requests,
    );
    let mut definitions = Vec::new();
    for (target, path, segments) in requests {
        if let Some((start, end)) = find_shape_path_range_in_text(comment, 0, &segments) {
            if let Some(range) = doc_comment_relative_range_to_lsp(comment, doc_start, (start, end))
            {
                let definition = TemplateShapeKeyDefinition {
                    target,
                    path,
                    uri: symbol.uri.clone(),
                    range,
                };
                if !definitions.iter().any(|existing| existing == &definition) {
                    definitions.push(definition);
                }
            }
        }
    }
    definitions
}

fn collect_type_info_shape_definition_requests(
    type_info: &php_lsp_types::TypeInfo,
    direct_target: TemplateShapeDefinitionTarget,
    path_prefix: &[String],
    segment_prefix: &[ShapePathSegment],
    requests: &mut Vec<(
        TemplateShapeDefinitionTarget,
        Vec<String>,
        Vec<ShapePathSegment>,
    )>,
) {
    match type_info {
        php_lsp_types::TypeInfo::Nullable(inner) => collect_type_info_shape_definition_requests(
            inner,
            direct_target,
            path_prefix,
            segment_prefix,
            requests,
        ),
        php_lsp_types::TypeInfo::Union(types) | php_lsp_types::TypeInfo::Intersection(types) => {
            for ty in types {
                collect_type_info_shape_definition_requests(
                    ty,
                    direct_target,
                    path_prefix,
                    segment_prefix,
                    requests,
                );
            }
        }
        php_lsp_types::TypeInfo::Conditional {
            if_type, else_type, ..
        } => {
            for ty in [if_type.as_ref(), else_type.as_ref()] {
                collect_type_info_shape_definition_requests(
                    ty,
                    direct_target,
                    path_prefix,
                    segment_prefix,
                    requests,
                );
            }
        }
        php_lsp_types::TypeInfo::Generic { .. } | php_lsp_types::TypeInfo::Simple(_) => {
            if let Some(value_type) = iterable_value_type_info(type_info, None) {
                collect_type_info_shape_definition_requests(
                    &value_type,
                    TemplateShapeDefinitionTarget::IterableValue,
                    path_prefix,
                    segment_prefix,
                    requests,
                );
            }
        }
        php_lsp_types::TypeInfo::ArrayShape(items) => collect_shape_items_definition_requests(
            items,
            ShapeDefinitionKind::Array,
            direct_target,
            path_prefix,
            segment_prefix,
            requests,
        ),
        php_lsp_types::TypeInfo::ObjectShape(items) => collect_shape_items_definition_requests(
            items,
            ShapeDefinitionKind::Object,
            direct_target,
            path_prefix,
            segment_prefix,
            requests,
        ),
        _ => {}
    }
}

fn collect_shape_items_definition_requests(
    items: &[php_lsp_types::ArrayShapeItem],
    kind: ShapeDefinitionKind,
    target: TemplateShapeDefinitionTarget,
    path_prefix: &[String],
    segment_prefix: &[ShapePathSegment],
    requests: &mut Vec<(
        TemplateShapeDefinitionTarget,
        Vec<String>,
        Vec<ShapePathSegment>,
    )>,
) {
    for item in items {
        let Some(key) = item.key.as_deref() else {
            continue;
        };
        let key = php_lsp_types::normalize_shape_key_text(key);
        if key.is_empty() {
            continue;
        }
        let mut path = path_prefix.to_vec();
        path.push(key.clone());
        let mut segments = segment_prefix.to_vec();
        segments.push(ShapePathSegment { key, kind });
        requests.push((target, path.clone(), segments.clone()));
        collect_type_info_shape_definition_requests(
            &item.value,
            target,
            &path,
            &segments,
            requests,
        );
    }
}

fn resolve_type_info_with_context(
    type_info: &php_lsp_types::TypeInfo,
    file_symbols: &php_lsp_types::FileSymbols,
    owner_fqn: &str,
) -> php_lsp_types::TypeInfo {
    match type_info {
        php_lsp_types::TypeInfo::Simple(name) => php_lsp_types::TypeInfo::Simple(
            twig_context_simple_type_text(file_symbols, owner_fqn, name)
                .unwrap_or_else(|| name.trim_start_matches('\\').to_string()),
        ),
        php_lsp_types::TypeInfo::Nullable(inner) => php_lsp_types::TypeInfo::Nullable(Box::new(
            resolve_type_info_with_context(inner, file_symbols, owner_fqn),
        )),
        php_lsp_types::TypeInfo::Union(types) => php_lsp_types::TypeInfo::Union(
            types
                .iter()
                .map(|ty| resolve_type_info_with_context(ty, file_symbols, owner_fqn))
                .collect(),
        ),
        php_lsp_types::TypeInfo::Intersection(types) => php_lsp_types::TypeInfo::Intersection(
            types
                .iter()
                .map(|ty| resolve_type_info_with_context(ty, file_symbols, owner_fqn))
                .collect(),
        ),
        php_lsp_types::TypeInfo::Generic { base, args } => php_lsp_types::TypeInfo::Generic {
            base: twig_context_simple_type_text(file_symbols, owner_fqn, base)
                .unwrap_or_else(|| base.trim_start_matches('\\').to_string()),
            args: args
                .iter()
                .map(|ty| resolve_type_info_with_context(ty, file_symbols, owner_fqn))
                .collect(),
        },
        php_lsp_types::TypeInfo::ArrayShape(items) => php_lsp_types::TypeInfo::ArrayShape(
            items
                .iter()
                .map(|item| php_lsp_types::ArrayShapeItem {
                    key: item.key.clone(),
                    optional: item.optional,
                    value: resolve_type_info_with_context(&item.value, file_symbols, owner_fqn),
                })
                .collect(),
        ),
        php_lsp_types::TypeInfo::ObjectShape(items) => php_lsp_types::TypeInfo::ObjectShape(
            items
                .iter()
                .map(|item| php_lsp_types::ArrayShapeItem {
                    key: item.key.clone(),
                    optional: item.optional,
                    value: resolve_type_info_with_context(&item.value, file_symbols, owner_fqn),
                })
                .collect(),
        ),
        php_lsp_types::TypeInfo::Conditional {
            subject,
            target,
            if_type,
            else_type,
        } => php_lsp_types::TypeInfo::Conditional {
            subject: subject.clone(),
            target: Box::new(resolve_type_info_with_context(
                target,
                file_symbols,
                owner_fqn,
            )),
            if_type: Box::new(resolve_type_info_with_context(
                if_type,
                file_symbols,
                owner_fqn,
            )),
            else_type: Box::new(resolve_type_info_with_context(
                else_type,
                file_symbols,
                owner_fqn,
            )),
        },
        php_lsp_types::TypeInfo::Self_ | php_lsp_types::TypeInfo::Static_ => {
            php_lsp_types::TypeInfo::Simple(owner_fqn.to_string())
        }
        php_lsp_types::TypeInfo::Parent_ => php_lsp_types::TypeInfo::Parent_,
        php_lsp_types::TypeInfo::Callable { .. }
        | php_lsp_types::TypeInfo::ClassString(_)
        | php_lsp_types::TypeInfo::LiteralString(_)
        | php_lsp_types::TypeInfo::LiteralInt(_)
        | php_lsp_types::TypeInfo::LiteralFloat(_)
        | php_lsp_types::TypeInfo::LiteralBool(_)
        | php_lsp_types::TypeInfo::LiteralNull
        | php_lsp_types::TypeInfo::Void
        | php_lsp_types::TypeInfo::Never
        | php_lsp_types::TypeInfo::Mixed => type_info.clone(),
    }
}

fn symbol_doc_comment_start(
    symbol: &php_lsp_types::SymbolInfo,
    comment: &str,
) -> Option<(u32, u32)> {
    let line_count = comment.bytes().filter(|byte| *byte == b'\n').count() as u32 + 1;
    let line = symbol.range.0.checked_sub(line_count)?;
    Some((line, symbol.range.1))
}

fn doc_comment_relative_range_to_lsp(
    comment: &str,
    doc_start: (u32, u32),
    byte_range: (usize, usize),
) -> Option<(u32, u32, u32, u32)> {
    let range = php_lsp_parser::utf16::range_byte_to_utf16(
        comment,
        (
            byte_line_col_at_offset(comment, byte_range.0).0,
            byte_line_col_at_offset(comment, byte_range.0).1,
            byte_line_col_at_offset(comment, byte_range.1).0,
            byte_line_col_at_offset(comment, byte_range.1).1,
        ),
    );
    let start_col = if range.0 == 0 {
        doc_start.1 + range.1
    } else {
        range.1
    };
    let end_col = if range.2 == 0 {
        doc_start.1 + range.3
    } else {
        range.3
    };
    Some((
        doc_start.0 + range.0,
        start_col,
        doc_start.0 + range.2,
        end_col,
    ))
}

fn template_shape_key_definition_from_byte_range(
    source: &str,
    source_uri: &str,
    target: TemplateShapeDefinitionTarget,
    path: Vec<String>,
    byte_range: (usize, usize),
) -> Option<TemplateShapeKeyDefinition> {
    let start = byte_line_col_at_offset(source, byte_range.0);
    let end = byte_line_col_at_offset(source, byte_range.1);
    let range =
        php_lsp_parser::utf16::range_byte_to_utf16(source, (start.0, start.1, end.0, end.1));
    Some(TemplateShapeKeyDefinition {
        target,
        path,
        uri: source_uri.to_string(),
        range,
    })
}

pub(in crate::server) fn php_array_inner_range(
    source: &str,
    start: usize,
    end: usize,
) -> Option<(usize, usize)> {
    let (start, end) = trim_source_range(source, start, end);
    if source.as_bytes().get(start) == Some(&b'[') {
        let close = find_matching_delimiter(source, start, '[', ']')?;
        if close <= end {
            return Some((start + 1, close));
        }
    }
    if source.get(start..end)?.starts_with("array") {
        let open = skip_ascii_ws_server(source, start + "array".len());
        if source.as_bytes().get(open) == Some(&b'(') {
            let close = find_matching_delimiter(source, open, '(', ')')?;
            if close <= end {
                return Some((open + 1, close));
            }
        }
    }
    None
}

fn infer_twig_context_value_type(
    source: &str,
    range: (usize, usize),
    file_symbols: &php_lsp_types::FileSymbols,
    index: Option<&WorkspaceIndex>,
) -> Option<String> {
    infer_twig_context_value_type_inner(source, range, file_symbols, index, &mut HashSet::new())
}

fn infer_twig_context_value_type_inner(
    source: &str,
    range: (usize, usize),
    file_symbols: &php_lsp_types::FileSymbols,
    index: Option<&WorkspaceIndex>,
    visited_variables: &mut HashSet<String>,
) -> Option<String> {
    let (start, end) = trim_source_range(source, range.0, range.1);
    let value = source.get(start..end)?.trim();
    if value.eq_ignore_ascii_case("null") {
        return Some("null".to_string());
    }
    if let Some(type_info) = infer_twig_context_literal_array_type_info(
        source,
        (start, end),
        file_symbols,
        index,
        visited_variables,
    ) {
        return twig_context_type_info_text(file_symbols, "", &type_info);
    }
    if value.starts_with('[') || value.starts_with("array") {
        if let Some(class_name) = first_new_class_name(value) {
            return Some(format!(
                "array<int, {}>",
                resolve_twig_context_class_name(file_symbols, class_name)
            ));
        }
    }

    if let Some(variable_name) = simple_php_variable_name(value) {
        if let Some(type_text) = infer_twig_context_assignment_value_type(
            source,
            start,
            variable_name,
            file_symbols,
            index,
            visited_variables,
        ) {
            return Some(type_text);
        }
        if let Some(type_text) =
            infer_twig_context_variable_type(source, start, variable_name, file_symbols)
        {
            return Some(type_text);
        }
    }

    if let Some(type_text) = twig_context_member_call_type_text(
        source,
        (start, end),
        file_symbols,
        index,
        visited_variables,
    ) {
        return Some(type_text);
    }

    if let Some(type_text) = twig_context_member_access_type_text(
        source,
        (start, end),
        file_symbols,
        index,
        visited_variables,
    ) {
        return Some(type_text);
    }

    if let Some(item_type) = infer_twig_paginated_source_item_type(
        source,
        (start, end),
        file_symbols,
        index,
        visited_variables,
    ) {
        return Some(format!("array<int, {item_type}>"));
    }

    first_new_class_name(value)
        .map(|class_name| resolve_twig_context_class_name(file_symbols, class_name))
}

fn simple_php_variable_name(value: &str) -> Option<&str> {
    let value = value.trim();
    let name = value.strip_prefix('$')?;
    if name.is_empty() {
        return None;
    }
    let mut chars = name.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    chars
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        .then_some(name)
}

fn infer_twig_context_literal_array_type_info(
    source: &str,
    range: (usize, usize),
    file_symbols: &php_lsp_types::FileSymbols,
    index: Option<&WorkspaceIndex>,
    visited_variables: &mut HashSet<String>,
) -> Option<php_lsp_types::TypeInfo> {
    let (inner_start, inner_end) = php_array_inner_range(source, range.0, range.1)?;
    let spans = split_top_level_spans(
        source.get(inner_start..inner_end).unwrap_or(""),
        inner_start,
    );
    if spans.is_empty() {
        return Some(php_lsp_types::TypeInfo::Simple("array".to_string()));
    }

    let mut shape_items = Vec::new();
    let mut list_values = Vec::new();
    let mut saw_shape_key = false;

    for span in spans {
        let span = trim_source_range(source, span.0, span.1);
        if span.0 >= span.1 {
            continue;
        }
        if let Some(arrow) = find_top_level_double_arrow(source, span.0, span.1) {
            let key_range = trim_source_range(source, span.0, arrow);
            let value_range = trim_source_range(source, arrow + 2, span.1);
            if let Some(key) = php_string_literal_value_at_range(source, key_range.0, key_range.1) {
                saw_shape_key = true;
                let value = infer_twig_context_value_type_info_for_shape(
                    source,
                    value_range,
                    file_symbols,
                    index,
                    visited_variables,
                )
                .unwrap_or(php_lsp_types::TypeInfo::Mixed);
                shape_items.push(php_lsp_types::ArrayShapeItem {
                    key: Some(php_lsp_types::normalize_shape_key_text(&key)),
                    optional: false,
                    value,
                });
                continue;
            }
        }

        let value = infer_twig_context_value_type_info_for_shape(
            source,
            span,
            file_symbols,
            index,
            visited_variables,
        )
        .unwrap_or(php_lsp_types::TypeInfo::Mixed);
        list_values.push(value);
    }

    if saw_shape_key && !shape_items.is_empty() && list_values.is_empty() {
        return Some(php_lsp_types::TypeInfo::ArrayShape(shape_items));
    }

    let value_type = merge_twig_context_type_infos(list_values)?;
    Some(php_lsp_types::TypeInfo::Generic {
        base: "array".to_string(),
        args: vec![
            php_lsp_types::TypeInfo::Simple("int".to_string()),
            value_type,
        ],
    })
}

fn infer_twig_context_value_type_info_for_shape(
    source: &str,
    range: (usize, usize),
    file_symbols: &php_lsp_types::FileSymbols,
    index: Option<&WorkspaceIndex>,
    visited_variables: &mut HashSet<String>,
) -> Option<php_lsp_types::TypeInfo> {
    let (start, end) = trim_source_range(source, range.0, range.1);
    let value = source.get(start..end)?.trim();
    if value.eq_ignore_ascii_case("null") {
        return Some(php_lsp_types::TypeInfo::LiteralNull);
    }
    if matches!(value, "true" | "false") {
        return Some(php_lsp_types::TypeInfo::Simple("bool".to_string()));
    }
    if php_string_literal_value_at_range(source, start, end).is_some() {
        return Some(php_lsp_types::TypeInfo::Simple("string".to_string()));
    }
    if value.parse::<i64>().is_ok() {
        return Some(php_lsp_types::TypeInfo::Simple("int".to_string()));
    }
    if value.parse::<f64>().is_ok() && value.contains('.') {
        return Some(php_lsp_types::TypeInfo::Simple("float".to_string()));
    }
    if let Some(type_info) = infer_twig_context_literal_array_type_info(
        source,
        (start, end),
        file_symbols,
        index,
        visited_variables,
    ) {
        return Some(type_info);
    }
    if let Some(class_name) = first_new_class_name(value) {
        return Some(php_lsp_types::TypeInfo::Simple(
            resolve_twig_context_class_name(file_symbols, class_name),
        ));
    }
    if let Some(variable_name) = simple_php_variable_name(value) {
        let type_text = infer_twig_context_assignment_value_type(
            source,
            start,
            variable_name,
            file_symbols,
            index,
            visited_variables,
        )
        .or_else(|| infer_twig_context_variable_type(source, start, variable_name, file_symbols))?;
        return twig_context_type_text_to_type_info(&type_text);
    }
    if let Some(type_text) = twig_context_member_call_type_text(
        source,
        (start, end),
        file_symbols,
        index,
        visited_variables,
    ) {
        return twig_context_type_text_to_type_info(&type_text);
    }
    if let Some(type_text) = twig_context_member_access_type_text(
        source,
        (start, end),
        file_symbols,
        index,
        visited_variables,
    ) {
        return twig_context_type_text_to_type_info(&type_text);
    }
    None
}

fn twig_context_type_text_to_type_info(type_text: &str) -> Option<php_lsp_types::TypeInfo> {
    parse_phpdoc(&format!("/** @var {type_text} $value */")).var_type
}

fn merge_twig_context_type_infos(
    type_infos: Vec<php_lsp_types::TypeInfo>,
) -> Option<php_lsp_types::TypeInfo> {
    let mut unique = Vec::new();
    for type_info in type_infos {
        if !unique.iter().any(|existing| existing == &type_info) {
            unique.push(type_info);
        }
    }
    match unique.len() {
        0 => None,
        1 => unique.into_iter().next(),
        _ => Some(php_lsp_types::TypeInfo::Union(unique)),
    }
}

fn infer_twig_context_array_append_value_type(
    source: &str,
    value_start: usize,
    variable_name: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: Option<&WorkspaceIndex>,
    visited_variables: &mut HashSet<String>,
) -> Option<String> {
    let latest_assignment =
        latest_simple_variable_assignment_before(source, value_start, variable_name, file_symbols)?;
    if !php_source_range_is_empty_array_literal(source, latest_assignment) {
        return None;
    }

    let append_values = simple_variable_array_append_values_before(
        source,
        value_start,
        variable_name,
        file_symbols,
    )?;
    let value_types = append_values
        .into_iter()
        .filter_map(|range| {
            infer_twig_context_value_type_info_for_shape(
                source,
                range,
                file_symbols,
                index,
                visited_variables,
            )
        })
        .collect::<Vec<_>>();
    let value_type = merge_twig_context_type_infos(value_types)?;
    twig_context_type_info_text(
        file_symbols,
        "",
        &php_lsp_types::TypeInfo::Generic {
            base: "array".to_string(),
            args: vec![
                php_lsp_types::TypeInfo::Simple("int".to_string()),
                value_type,
            ],
        },
    )
}

fn infer_twig_context_assignment_value_type(
    source: &str,
    value_start: usize,
    variable_name: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: Option<&WorkspaceIndex>,
    visited_variables: &mut HashSet<String>,
) -> Option<String> {
    if !visited_variables.insert(variable_name.to_string()) {
        return None;
    }

    let result = if let Some(type_text) = infer_twig_context_array_append_value_type(
        source,
        value_start,
        variable_name,
        file_symbols,
        index,
        visited_variables,
    ) {
        Some(type_text)
    } else if let Some(assignments) =
        simple_variable_assignments_before(source, value_start, variable_name, file_symbols)
    {
        assignments.last().copied().and_then(|latest| {
            let latest_type = infer_twig_context_value_type_inner(
                source,
                latest,
                file_symbols,
                index,
                visited_variables,
            )?;
            if latest_type == "null" || twig_context_type_text_has_null(&latest_type) {
                Some(latest_type)
            } else if assignments[..assignments.len().saturating_sub(1)]
                .iter()
                .any(|assignment| php_source_range_is_null_literal(source, *assignment))
            {
                Some(twig_context_type_text_with_null(&latest_type))
            } else {
                Some(latest_type)
            }
        })
    } else {
        None
    };

    visited_variables.remove(variable_name);
    result
}

fn latest_simple_variable_assignment_before(
    source: &str,
    value_start: usize,
    variable_name: &str,
    file_symbols: &php_lsp_types::FileSymbols,
) -> Option<(usize, usize)> {
    simple_variable_assignments_before(source, value_start, variable_name, file_symbols)?
        .into_iter()
        .last()
}

fn simple_variable_assignments_before(
    source: &str,
    value_start: usize,
    variable_name: &str,
    file_symbols: &php_lsp_types::FileSymbols,
) -> Option<Vec<(usize, usize)>> {
    let scope_start = containing_callable_byte_range(source, value_start, file_symbols)
        .map(|range| range.0)
        .unwrap_or(0);
    let search_end = value_start.min(source.len());
    let needle = format!("${variable_name}");
    let mut assignments = Vec::new();
    let mut offset = scope_start;

    while offset < search_end {
        let Some(relative) = source.get(offset..search_end)?.find(&needle) else {
            break;
        };
        let variable_start = offset + relative;
        let after_variable = variable_start + needle.len();
        if source
            .as_bytes()
            .get(after_variable)
            .is_some_and(|byte| is_ident_byte(*byte))
        {
            offset = after_variable;
            continue;
        }

        let equals = skip_ascii_ws_server(source, after_variable);
        if source.as_bytes().get(equals) != Some(&b'=')
            || source
                .as_bytes()
                .get(equals + 1)
                .is_some_and(|byte| matches!(*byte, b'=' | b'>'))
        {
            offset = after_variable;
            continue;
        }

        let rhs_start = skip_ascii_ws_server(source, equals + 1);
        if let Some(rhs_end) = find_php_statement_end(source, rhs_start, search_end) {
            assignments.push(trim_source_range(source, rhs_start, rhs_end));
            offset = rhs_end + 1;
        } else {
            offset = after_variable;
        }
    }

    (!assignments.is_empty()).then_some(assignments)
}

fn simple_variable_array_append_values_before(
    source: &str,
    value_start: usize,
    variable_name: &str,
    file_symbols: &php_lsp_types::FileSymbols,
) -> Option<Vec<(usize, usize)>> {
    let scope_start = containing_callable_byte_range(source, value_start, file_symbols)
        .map(|range| range.0)
        .unwrap_or(0);
    let search_end = value_start.min(source.len());
    let needle = format!("${variable_name}");
    let mut append_values = Vec::new();
    let mut offset = scope_start;

    while offset < search_end {
        let Some(relative) = source.get(offset..search_end)?.find(&needle) else {
            break;
        };
        let variable_start = offset + relative;
        let after_variable = variable_start + needle.len();
        if source
            .as_bytes()
            .get(after_variable)
            .is_some_and(|byte| is_ident_byte(*byte))
        {
            offset = after_variable;
            continue;
        }

        let after_variable_ws = skip_ascii_ws_server(source, after_variable);
        if source.as_bytes().get(after_variable_ws) == Some(&b'=') {
            if source
                .as_bytes()
                .get(after_variable_ws + 1)
                .is_some_and(|byte| matches!(*byte, b'=' | b'>'))
            {
                offset = after_variable_ws + 1;
                continue;
            }

            append_values.clear();
            let rhs_start = skip_ascii_ws_server(source, after_variable_ws + 1);
            offset = find_php_statement_end(source, rhs_start, search_end)
                .map(|rhs_end| rhs_end + 1)
                .unwrap_or(after_variable_ws + 1);
            continue;
        }

        if source.as_bytes().get(after_variable_ws) != Some(&b'[') {
            offset = after_variable;
            continue;
        }
        let bracket_close = skip_ascii_ws_server(source, after_variable_ws + 1);
        if source.as_bytes().get(bracket_close) != Some(&b']') {
            offset = bracket_close;
            continue;
        }
        let equals = skip_ascii_ws_server(source, bracket_close + 1);
        if source.as_bytes().get(equals) != Some(&b'=')
            || source
                .as_bytes()
                .get(equals + 1)
                .is_some_and(|byte| matches!(*byte, b'=' | b'>'))
        {
            offset = equals;
            continue;
        }

        let rhs_start = skip_ascii_ws_server(source, equals + 1);
        if let Some(rhs_end) = find_php_statement_end(source, rhs_start, search_end) {
            append_values.push(trim_source_range(source, rhs_start, rhs_end));
            offset = rhs_end + 1;
        } else {
            offset = equals + 1;
        }
    }

    (!append_values.is_empty()).then_some(append_values)
}

fn merge_twig_context_type_texts(types: Vec<String>) -> Option<String> {
    let mut non_null = Vec::<String>::new();
    let mut saw_null = false;
    let mut saw_mixed = false;

    for type_text in types {
        for part in split_twig_context_union_type_text(&type_text) {
            if part.eq_ignore_ascii_case("mixed") {
                saw_mixed = true;
                continue;
            }
            if part.eq_ignore_ascii_case("null") {
                saw_null = true;
                continue;
            }

            if let Some(inner) = part.strip_prefix('?') {
                saw_null = true;
                if !inner.is_empty() && !non_null.iter().any(|existing| existing == inner) {
                    non_null.push(inner.to_string());
                }
                continue;
            }

            if !non_null.iter().any(|existing| existing == part) {
                non_null.push(part.to_string());
            }
        }
    }

    if non_null.is_empty() {
        return saw_null
            .then(|| "null".to_string())
            .or_else(|| saw_mixed.then(|| "mixed".to_string()));
    }

    if saw_null && non_null.len() == 1 {
        return Some(format!("?{}", non_null[0]));
    }

    let joined = non_null.join("|");
    Some(if saw_null {
        format!("{joined}|null")
    } else {
        joined
    })
}

fn twig_context_type_text_has_null(type_text: &str) -> bool {
    split_twig_context_union_type_text(type_text)
        .into_iter()
        .any(|part| part == "null" || part.starts_with('?'))
}

fn twig_context_type_text_with_null(type_text: &str) -> String {
    merge_twig_context_type_texts(vec![type_text.to_string(), "null".to_string()])
        .unwrap_or_else(|| type_text.to_string())
}

fn php_source_range_is_null_literal(source: &str, range: (usize, usize)) -> bool {
    let (start, end) = trim_source_range(source, range.0, range.1);
    source
        .get(start..end)
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("null"))
}

fn php_source_range_is_empty_array_literal(source: &str, range: (usize, usize)) -> bool {
    let Some((inner_start, inner_end)) = php_array_inner_range(source, range.0, range.1) else {
        return false;
    };
    let (inner_start, inner_end) = trim_source_range(source, inner_start, inner_end);
    inner_start >= inner_end
}

fn split_twig_context_union_type_text(type_text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut angle_depth = 0usize;

    for (offset, ch) in type_text.char_indices() {
        match ch {
            '<' => angle_depth += 1,
            '>' => angle_depth = angle_depth.saturating_sub(1),
            '|' if angle_depth == 0 => {
                let part = type_text[start..offset].trim();
                if !part.is_empty() {
                    parts.push(part);
                }
                start = offset + ch.len_utf8();
            }
            _ => {}
        }
    }

    let part = type_text[start..].trim();
    if !part.is_empty() {
        parts.push(part);
    }

    parts
}

fn containing_callable_byte_range(
    source: &str,
    offset: usize,
    file_symbols: &php_lsp_types::FileSymbols,
) -> Option<(usize, usize)> {
    let position = byte_line_col_at_offset(source, offset);
    file_symbols
        .symbols
        .iter()
        .filter(|symbol| {
            matches!(
                symbol.kind,
                php_lsp_types::PhpSymbolKind::Function | php_lsp_types::PhpSymbolKind::Method
            ) && byte_position_in_range(position, symbol.range)
        })
        .filter_map(|symbol| {
            let start = byte_offset_from_line_col(source, symbol.range.0, symbol.range.1)?;
            let end = byte_offset_from_line_col(source, symbol.range.2, symbol.range.3)?;
            Some((start, end))
        })
        .min_by_key(|(start, end)| end.saturating_sub(*start))
}

fn byte_offset_from_line_col(source: &str, line: u32, col: u32) -> Option<usize> {
    let mut current_line = 0u32;
    let mut line_start = 0usize;
    for (offset, byte) in source.bytes().enumerate() {
        if current_line == line {
            return Some((line_start + col as usize).min(source.len()));
        }
        if byte == b'\n' {
            current_line += 1;
            line_start = offset + 1;
        }
    }
    (current_line == line).then_some((line_start + col as usize).min(source.len()))
}

fn find_php_statement_end(source: &str, start: usize, limit: usize) -> Option<usize> {
    find_top_level_token(source, start, limit, b';')
}

fn find_top_level_token(source: &str, start: usize, end: usize, token: u8) -> Option<usize> {
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut offset = start;

    while offset < end {
        let ch = source[offset..end].chars().next()?;
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active_quote {
                quote = None;
            }
            offset += ch.len_utf8();
            continue;
        }

        match ch {
            '\'' | '"' => quote = Some(ch),
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            _ if ch.len_utf8() == 1
                && ch as u8 == token
                && paren_depth == 0
                && bracket_depth == 0
                && brace_depth == 0 =>
            {
                return Some(offset);
            }
            _ => {}
        }
        offset += ch.len_utf8();
    }

    None
}

#[derive(Debug, Clone, Copy)]
struct PhpMemberCallParts<'a> {
    object_range: (usize, usize),
    method_name: &'a str,
    args_range: (usize, usize),
}

#[derive(Debug, Clone, Copy)]
struct PhpMemberAccessParts<'a> {
    object_range: (usize, usize),
    member_name: &'a str,
}

fn infer_twig_paginated_source_item_type(
    source: &str,
    range: (usize, usize),
    file_symbols: &php_lsp_types::FileSymbols,
    index: Option<&WorkspaceIndex>,
    visited_variables: &mut HashSet<String>,
) -> Option<String> {
    let index = index?;
    let (start, end) = trim_source_range(source, range.0, range.1);
    let value = source.get(start..end)?.trim();

    if value.starts_with('[') || value.starts_with("array") {
        if let Some(class_name) = first_new_class_name(value) {
            return Some(resolve_twig_context_class_name(file_symbols, class_name));
        }
    }

    if let Some(variable_name) = simple_php_variable_name(value) {
        let assignment =
            latest_simple_variable_assignment_before(source, start, variable_name, file_symbols)?;
        if !visited_variables.insert(format!("paginate-source:{variable_name}")) {
            return None;
        }
        return infer_twig_paginated_source_item_type(
            source,
            assignment,
            file_symbols,
            Some(index),
            visited_variables,
        );
    }

    let call = php_member_call_parts(source, start, end)?;
    if call.method_name.eq_ignore_ascii_case("paginate") {
        if !twig_context_member_receiver_is_paginator(
            source,
            call.object_range,
            file_symbols,
            visited_variables,
        ) {
            return None;
        }
        let args = split_top_level_spans(
            source
                .get(call.args_range.0..call.args_range.1)
                .unwrap_or(""),
            call.args_range.0,
        );
        let first_arg = args.first().copied()?;
        return infer_twig_paginated_source_item_type(
            source,
            first_arg,
            file_symbols,
            Some(index),
            visited_variables,
        );
    }

    twig_context_repository_member_call_entity(source, call, file_symbols, index, visited_variables)
}

fn twig_context_member_receiver_is_paginator(
    source: &str,
    object_range: (usize, usize),
    file_symbols: &php_lsp_types::FileSymbols,
    visited_variables: &mut HashSet<String>,
) -> bool {
    twig_context_expression_type_text(source, object_range, file_symbols, None, visited_variables)
        .is_some_and(|type_text| type_text.to_ascii_lowercase().contains("paginator"))
}

fn twig_context_repository_member_call_entity(
    source: &str,
    call: PhpMemberCallParts<'_>,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    visited_variables: &mut HashSet<String>,
) -> Option<String> {
    let receiver_type = twig_context_expression_type_text(
        source,
        call.object_range,
        file_symbols,
        Some(index),
        visited_variables,
    )?;
    let repository_fqn = receiver_type
        .trim()
        .trim_start_matches('?')
        .trim_start_matches('\\');
    let entity_fqn = doctrine_repository_entity_from_type_text(index, repository_fqn)?;
    twig_context_repository_method_paginated_item_type(
        index,
        repository_fqn,
        call.method_name,
        file_symbols,
        &entity_fqn,
    )
}

fn twig_context_expression_type_text(
    source: &str,
    range: (usize, usize),
    file_symbols: &php_lsp_types::FileSymbols,
    index: Option<&WorkspaceIndex>,
    visited_variables: &mut HashSet<String>,
) -> Option<String> {
    let (start, end) = trim_source_range(source, range.0, range.1);
    let value = source.get(start..end)?.trim();
    if value == "$this" {
        return containing_class_fqn_at_offset(source, start, file_symbols);
    }
    if let Some(variable_name) = simple_php_variable_name(value) {
        return infer_twig_context_assignment_value_type(
            source,
            start,
            variable_name,
            file_symbols,
            index,
            visited_variables,
        )
        .or_else(|| infer_twig_context_variable_type(source, start, variable_name, file_symbols));
    }
    if let Some(type_text) = twig_context_member_call_type_text(
        source,
        (start, end),
        file_symbols,
        index,
        visited_variables,
    ) {
        return Some(type_text);
    }
    if let Some(type_text) = twig_context_member_access_type_text(
        source,
        (start, end),
        file_symbols,
        index,
        visited_variables,
    ) {
        return Some(type_text);
    }
    first_new_class_name(value)
        .map(|class_name| resolve_twig_context_class_name(file_symbols, class_name))
}

fn twig_context_member_call_type_text(
    source: &str,
    range: (usize, usize),
    file_symbols: &php_lsp_types::FileSymbols,
    index: Option<&WorkspaceIndex>,
    visited_variables: &mut HashSet<String>,
) -> Option<String> {
    let index = index?;
    let (start, end) = trim_source_range(source, range.0, range.1);
    let call = php_member_call_parts(source, start, end)?;
    let receiver_type = twig_context_expression_type_text(
        source,
        call.object_range,
        file_symbols,
        Some(index),
        visited_variables,
    )?;
    for receiver_fqn in twig_context_candidate_type_fqns(&receiver_type) {
        if let Some(type_text) = twig_context_repository_member_call_type_text(
            index,
            &receiver_fqn,
            call.method_name,
            file_symbols,
        ) {
            return Some(type_text);
        }

        let method_fqn = format!("{receiver_fqn}::{}", call.method_name);
        let Some(symbol) = index.resolve_member(&method_fqn) else {
            continue;
        };
        if !matches!(symbol.kind, php_lsp_types::PhpSymbolKind::Method) {
            continue;
        }
        if let Some(return_type) = symbol_effective_return_type(&symbol) {
            return twig_context_type_info_text_for_symbol(
                index,
                file_symbols,
                &symbol,
                &receiver_fqn,
                &return_type,
            );
        }
    }

    None
}

fn twig_context_member_access_type_text(
    source: &str,
    range: (usize, usize),
    file_symbols: &php_lsp_types::FileSymbols,
    index: Option<&WorkspaceIndex>,
    visited_variables: &mut HashSet<String>,
) -> Option<String> {
    let index = index?;
    let (start, end) = trim_source_range(source, range.0, range.1);
    let access = php_member_access_parts(source, start, end)?;
    let receiver_type = twig_context_expression_type_text(
        source,
        access.object_range,
        file_symbols,
        Some(index),
        visited_variables,
    )?;
    for receiver_fqn in twig_context_candidate_type_fqns(&receiver_type) {
        let property_fqn = format!("{receiver_fqn}::${}", access.member_name);
        let Some(symbol) = index.resolve_member(&property_fqn) else {
            continue;
        };
        if !matches!(symbol.kind, php_lsp_types::PhpSymbolKind::Property) {
            continue;
        }
        if let Some(property_type) = symbol_effective_return_type(&symbol) {
            return twig_context_type_info_text_for_symbol(
                index,
                file_symbols,
                &symbol,
                &receiver_fqn,
                &property_type,
            );
        }
    }

    None
}

fn php_member_call_parts<'a>(
    source: &'a str,
    start: usize,
    end: usize,
) -> Option<PhpMemberCallParts<'a>> {
    let mut latest = None;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut offset = start;

    while offset < end {
        let ch = source[offset..end].chars().next()?;
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active_quote {
                quote = None;
            }
            offset += ch.len_utf8();
            continue;
        }

        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && (source[offset..end].starts_with("->") || source[offset..end].starts_with("?->"))
        {
            let operator_len = if source[offset..end].starts_with("?->") {
                "?->".len()
            } else {
                "->".len()
            };
            let method_start = skip_ascii_ws_server(source, offset + operator_len);
            let method_end = scan_php_class_name_end(source, method_start);
            if method_end <= method_start {
                offset += operator_len;
                continue;
            }
            let open = skip_ascii_ws_server(source, method_end);
            if source.as_bytes().get(open) != Some(&b'(') {
                offset = method_end;
                continue;
            }
            let Some(close) = find_matching_delimiter(source, open, '(', ')') else {
                return latest;
            };
            if close > end {
                return latest;
            }
            latest = Some(PhpMemberCallParts {
                object_range: trim_source_range(source, start, offset),
                method_name: source.get(method_start..method_end)?,
                args_range: (open + 1, close),
            });
            offset = close + 1;
            continue;
        }

        match ch {
            '\'' | '"' => quote = Some(ch),
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }
        offset += ch.len_utf8();
    }

    latest
}

fn php_member_access_parts<'a>(
    source: &'a str,
    start: usize,
    end: usize,
) -> Option<PhpMemberAccessParts<'a>> {
    let mut latest = None;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut offset = start;

    while offset < end {
        let ch = source[offset..end].chars().next()?;
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active_quote {
                quote = None;
            }
            offset += ch.len_utf8();
            continue;
        }

        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && (source[offset..end].starts_with("->") || source[offset..end].starts_with("?->"))
        {
            let operator_len = if source[offset..end].starts_with("?->") {
                "?->".len()
            } else {
                "->".len()
            };
            let member_start = skip_ascii_ws_server(source, offset + operator_len);
            let member_end = scan_php_class_name_end(source, member_start);
            if member_end <= member_start {
                offset += operator_len;
                continue;
            }
            let after_member = skip_ascii_ws_server(source, member_end);
            if source.as_bytes().get(after_member) == Some(&b'(') {
                offset = member_end;
                continue;
            }
            latest = Some(PhpMemberAccessParts {
                object_range: trim_source_range(source, start, offset),
                member_name: source.get(member_start..member_end)?,
            });
            offset = member_end;
            continue;
        }

        match ch {
            '\'' | '"' => quote = Some(ch),
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }
        offset += ch.len_utf8();
    }

    latest
}

fn twig_context_candidate_type_fqns(type_text: &str) -> Vec<String> {
    split_twig_context_union_type_text(type_text)
        .into_iter()
        .filter_map(|part| {
            let candidate = part
                .trim()
                .trim_start_matches('?')
                .trim_start_matches('\\')
                .split_once('<')
                .map(|(base, _)| base)
                .unwrap_or_else(|| part.trim().trim_start_matches('?').trim_start_matches('\\'))
                .trim();
            let lower = candidate.to_ascii_lowercase();
            (!candidate.is_empty() && !twig_context_builtin_type_name(&lower))
                .then(|| candidate.to_string())
        })
        .collect()
}

fn containing_class_fqn_at_offset(
    source: &str,
    offset: usize,
    file_symbols: &php_lsp_types::FileSymbols,
) -> Option<String> {
    let position = byte_line_col_at_offset(source, offset);
    file_symbols
        .symbols
        .iter()
        .filter(|symbol| {
            matches!(
                symbol.kind,
                php_lsp_types::PhpSymbolKind::Class
                    | php_lsp_types::PhpSymbolKind::Interface
                    | php_lsp_types::PhpSymbolKind::Trait
                    | php_lsp_types::PhpSymbolKind::Enum
            ) && byte_position_in_range(position, symbol.range)
        })
        .min_by_key(|symbol| symbol_range_len(symbol.range))
        .map(|symbol| symbol.fqn.clone())
}

fn twig_context_repository_member_call_type_text(
    index: &WorkspaceIndex,
    repository_fqn: &str,
    method_name: &str,
    file_symbols: &php_lsp_types::FileSymbols,
) -> Option<String> {
    let repository_fqn = repository_fqn.trim_start_matches('\\');
    let entity_fqn = doctrine_repository_entity_from_type_text(index, repository_fqn)?;

    let method_fqn = format!("{repository_fqn}::{method_name}");
    if let Some(symbol) = index.resolve_member(&method_fqn) {
        if let Some(return_type) = symbol_effective_return_type(&symbol) {
            if let Some(value_type) = iterable_value_type_info(&return_type, None) {
                let item_type = twig_context_type_info_text_for_symbol(
                    index,
                    file_symbols,
                    &symbol,
                    repository_fqn,
                    &value_type,
                )?;
                return Some(format!("array<int, {item_type}>"));
            }
            return twig_context_type_info_text_for_symbol(
                index,
                file_symbols,
                &symbol,
                repository_fqn,
                &return_type,
            );
        }
    }

    let lower = method_name.to_ascii_lowercase();
    if lower == "find" || lower.starts_with("findoneby") {
        return Some(format!("?{entity_fqn}"));
    }
    if lower == "findall" || lower == "matching" || lower == "findby" || lower.starts_with("findby")
    {
        return Some(format!("array<int, {entity_fqn}>"));
    }

    None
}

fn doctrine_repository_entity_from_type_text(
    index: &WorkspaceIndex,
    repository_fqn: &str,
) -> Option<String> {
    let repository_fqn = repository_fqn.trim_start_matches('\\');
    let symbol = index.resolve_fqn(repository_fqn)?;
    for binding in &symbol.template_bindings {
        if binding.kind == php_lsp_types::TemplateBindingKind::Extends
            && is_doctrine_repository_base(&binding.target)
        {
            if let Some(entity_fqn) = binding.args.first().and_then(type_info_simple_fqn) {
                return Some(entity_fqn);
            }
        }
    }

    if let Some(entity_fqn) =
        doctrine_repository_entity_from_repository_class_binding(index, repository_fqn)
    {
        return Some(entity_fqn);
    }

    conventional_entity_fqn_for_repository(index, repository_fqn)
}

fn doctrine_repository_entity_from_repository_class_binding(
    index: &WorkspaceIndex,
    repository_fqn: &str,
) -> Option<String> {
    let repository_fqn = repository_fqn.trim_start_matches('\\');
    index.types.iter().find_map(|entry| {
        let symbol = entry.value();
        if !matches!(symbol.kind, php_lsp_types::PhpSymbolKind::Class) {
            return None;
        }

        symbol.template_bindings.iter().find_map(|binding| {
            if binding.kind != php_lsp_types::TemplateBindingKind::RepositoryClass {
                return None;
            }
            fqn_eq(&binding.target, repository_fqn).then(|| symbol.fqn.clone())
        })
    })
}

fn conventional_entity_fqn_for_repository(
    index: &WorkspaceIndex,
    repository_fqn: &str,
) -> Option<String> {
    let repository_short = repository_fqn.rsplit('\\').next()?;
    let entity_short = repository_short.strip_suffix("Repository")?;
    let direct_candidate = repository_fqn
        .replace("\\Repository\\", "\\Entity\\")
        .strip_suffix("Repository")
        .map(str::to_string);
    if let Some(candidate) = direct_candidate {
        if let Some(symbol) = index.resolve_fqn(&candidate) {
            if matches!(symbol.kind, php_lsp_types::PhpSymbolKind::Class) {
                return Some(symbol.fqn.clone());
            }
        }
    }

    let mut candidates = index.types.iter().filter_map(|entry| {
        let symbol = entry.value();
        (matches!(symbol.kind, php_lsp_types::PhpSymbolKind::Class)
            && symbol.name == entity_short
            && symbol.fqn.contains("\\Entity\\"))
        .then(|| symbol.fqn.clone())
    });
    let first = candidates.next()?;
    candidates.next().is_none().then_some(first)
}

fn twig_context_repository_method_paginated_item_type(
    index: &WorkspaceIndex,
    repository_fqn: &str,
    method_name: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    fallback_entity_fqn: &str,
) -> Option<String> {
    let method_fqn = format!("{repository_fqn}::{method_name}");
    if let Some(symbol) = index.resolve_fqn(&method_fqn) {
        if let Some(return_type) = symbol_effective_return_type(&symbol) {
            if let Some(value_type) = iterable_value_type_info(&return_type, None) {
                return twig_context_type_info_text_for_symbol(
                    index,
                    file_symbols,
                    &symbol,
                    repository_fqn,
                    &value_type,
                );
            }
            if twig_context_type_is_paginated_source(&return_type) {
                return Some(fallback_entity_fqn.to_string());
            }
            return None;
        }
    }

    let lower = method_name.to_ascii_lowercase();
    (matches!(
        lower.as_str(),
        "findall" | "findby" | "matching" | "createquerybuilder"
    ) || lower.ends_with("qb")
        || lower.ends_with("querybuilder"))
    .then(|| fallback_entity_fqn.to_string())
}

fn twig_context_type_info_text_for_symbol(
    index: &WorkspaceIndex,
    fallback_file_symbols: &php_lsp_types::FileSymbols,
    symbol: &php_lsp_types::SymbolInfo,
    fallback_owner_fqn: &str,
    type_info: &php_lsp_types::TypeInfo,
) -> Option<String> {
    let owner_fqn = symbol.parent_fqn.as_deref().unwrap_or(fallback_owner_fqn);
    if let Some(symbol_file_symbols) = index.file_symbols.get(&symbol.uri) {
        return twig_context_type_info_text(&symbol_file_symbols, owner_fqn, type_info);
    }

    twig_context_type_info_text(fallback_file_symbols, owner_fqn, type_info)
}

fn twig_context_type_is_paginated_source(type_info: &php_lsp_types::TypeInfo) -> bool {
    match type_info {
        php_lsp_types::TypeInfo::Nullable(inner) => twig_context_type_is_paginated_source(inner),
        php_lsp_types::TypeInfo::Union(types) | php_lsp_types::TypeInfo::Intersection(types) => {
            types.iter().any(twig_context_type_is_paginated_source)
        }
        php_lsp_types::TypeInfo::Generic { base, .. } => {
            let lower = base.trim_start_matches('\\').to_ascii_lowercase();
            matches!(lower.as_str(), "array" | "iterable" | "traversable")
        }
        php_lsp_types::TypeInfo::Simple(name) => {
            let lower = name.trim_start_matches('\\').to_ascii_lowercase();
            lower == "array"
                || lower == "iterable"
                || lower.ends_with("\\querybuilder")
                || lower.ends_with("\\query")
                || lower.ends_with("querybuilder")
        }
        _ => false,
    }
}

fn infer_twig_context_variable_type(
    source: &str,
    value_start: usize,
    variable_name: &str,
    file_symbols: &php_lsp_types::FileSymbols,
) -> Option<String> {
    let position = byte_line_col_at_offset(source, value_start);
    let signature_symbol = file_symbols
        .symbols
        .iter()
        .filter(|symbol| {
            matches!(
                symbol.kind,
                php_lsp_types::PhpSymbolKind::Function | php_lsp_types::PhpSymbolKind::Method
            ) && symbol.signature.is_some()
                && byte_position_in_range(position, symbol.range)
        })
        .min_by_key(|symbol| symbol_range_len(symbol.range))?;

    let signature = signature_symbol.signature.as_ref()?;
    let param = signature
        .params
        .iter()
        .find(|param| param.name.trim_start_matches('$') == variable_name)?;
    twig_context_type_info_text(
        file_symbols,
        signature_symbol.parent_fqn.as_deref().unwrap_or(""),
        param.type_info.as_ref()?,
    )
}

fn twig_context_type_info_text(
    file_symbols: &php_lsp_types::FileSymbols,
    owner_fqn: &str,
    type_info: &php_lsp_types::TypeInfo,
) -> Option<String> {
    match type_info {
        php_lsp_types::TypeInfo::Simple(name) => {
            twig_context_simple_type_text(file_symbols, owner_fqn, name)
        }
        php_lsp_types::TypeInfo::Nullable(inner) => {
            twig_context_type_info_text(file_symbols, owner_fqn, inner)
                .map(|inner| format!("?{inner}"))
        }
        php_lsp_types::TypeInfo::Union(types) => {
            let parts: Vec<_> = types
                .iter()
                .filter_map(|type_info| {
                    twig_context_type_info_text(file_symbols, owner_fqn, type_info)
                })
                .collect();
            (!parts.is_empty()).then(|| parts.join("|"))
        }
        php_lsp_types::TypeInfo::Intersection(types) => {
            let parts: Vec<_> = types
                .iter()
                .filter_map(|type_info| {
                    twig_context_type_info_text(file_symbols, owner_fqn, type_info)
                })
                .collect();
            (!parts.is_empty()).then(|| parts.join("&"))
        }
        php_lsp_types::TypeInfo::Generic { base, args } => {
            let base = twig_context_simple_type_text(file_symbols, owner_fqn, base)?;
            let args: Vec<_> = args
                .iter()
                .filter_map(|type_info| {
                    twig_context_type_info_text(file_symbols, owner_fqn, type_info)
                })
                .collect();
            Some(format!("{}<{}>", base, args.join(", ")))
        }
        php_lsp_types::TypeInfo::Conditional {
            if_type, else_type, ..
        } => {
            let parts: Vec<_> = [if_type.as_ref(), else_type.as_ref()]
                .into_iter()
                .filter_map(|type_info| {
                    twig_context_type_info_text(file_symbols, owner_fqn, type_info)
                })
                .collect();
            (!parts.is_empty()).then(|| parts.join("|"))
        }
        php_lsp_types::TypeInfo::Self_ | php_lsp_types::TypeInfo::Static_ => {
            (!owner_fqn.is_empty()).then(|| owner_fqn.to_string())
        }
        php_lsp_types::TypeInfo::Parent_ => Some("parent".to_string()),
        php_lsp_types::TypeInfo::ArrayShape(items) => {
            twig_context_shape_type_text(file_symbols, owner_fqn, "array", items)
        }
        php_lsp_types::TypeInfo::ObjectShape(items) => {
            twig_context_shape_type_text(file_symbols, owner_fqn, "object", items)
        }
        php_lsp_types::TypeInfo::Callable { .. }
        | php_lsp_types::TypeInfo::ClassString(_)
        | php_lsp_types::TypeInfo::LiteralString(_)
        | php_lsp_types::TypeInfo::LiteralInt(_)
        | php_lsp_types::TypeInfo::LiteralFloat(_)
        | php_lsp_types::TypeInfo::LiteralBool(_)
        | php_lsp_types::TypeInfo::LiteralNull
        | php_lsp_types::TypeInfo::Void
        | php_lsp_types::TypeInfo::Never
        | php_lsp_types::TypeInfo::Mixed => Some(type_info.to_string()),
    }
}

fn twig_context_shape_type_text(
    file_symbols: &php_lsp_types::FileSymbols,
    owner_fqn: &str,
    shape_kind: &str,
    items: &[php_lsp_types::ArrayShapeItem],
) -> Option<String> {
    let parts: Vec<_> = items
        .iter()
        .map(|item| {
            let value = twig_context_type_info_text(file_symbols, owner_fqn, &item.value)
                .unwrap_or_else(|| item.value.to_string());
            item.key
                .as_deref()
                .map(|key| {
                    let key = twig_context_shape_key_text(key);
                    if item.optional {
                        format!("{key}?: {value}")
                    } else {
                        format!("{key}: {value}")
                    }
                })
                .unwrap_or(value)
        })
        .collect();
    Some(format!("{shape_kind}{{{}}}", parts.join(", ")))
}

fn twig_context_shape_key_text(key: &str) -> String {
    let digits = key.strip_prefix('-').unwrap_or(key);
    if !digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit()) {
        return key.to_string();
    }

    let mut chars = key.chars();
    if let Some(first) = chars.next() {
        if (first == '_' || first.is_ascii_alphabetic())
            && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        {
            return key.to_string();
        }
    }

    let escaped = key.replace('\\', "\\\\").replace('\'', "\\'");
    format!("'{escaped}'")
}

fn twig_context_simple_type_text(
    file_symbols: &php_lsp_types::FileSymbols,
    owner_fqn: &str,
    name: &str,
) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let lower = name.trim_start_matches('\\').to_ascii_lowercase();
    if matches!(lower.as_str(), "self" | "static") {
        return (!owner_fqn.is_empty()).then(|| owner_fqn.to_string());
    }
    if lower == "parent" || twig_context_builtin_type_name(&lower) {
        return Some(name.trim_start_matches('\\').to_string());
    }
    Some(resolve_twig_context_class_name(file_symbols, name))
}

fn twig_context_builtin_type_name(lower: &str) -> bool {
    matches!(
        lower,
        "array"
            | "bool"
            | "boolean"
            | "callable"
            | "false"
            | "float"
            | "int"
            | "integer"
            | "iterable"
            | "iterator"
            | "iteratoraggregate"
            | "generator"
            | "list"
            | "non-empty-list"
            | "mixed"
            | "never"
            | "null"
            | "object"
            | "resource"
            | "self"
            | "static"
            | "string"
            | "true"
            | "void"
    )
}

fn byte_line_col_at_offset(source: &str, offset: usize) -> (u32, u32) {
    let bounded = offset.min(source.len());
    let prefix = &source[..bounded];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let line_start = prefix.rfind('\n').map_or(0, |idx| idx + 1);
    (line, bounded.saturating_sub(line_start) as u32)
}

fn byte_position_in_range(position: (u32, u32), range: (u32, u32, u32, u32)) -> bool {
    let start = (range.0, range.1);
    let end = (range.2, range.3);
    start <= position && position <= end
}

fn symbol_range_len(range: (u32, u32, u32, u32)) -> u64 {
    let start = u64::from(range.0) << 32 | u64::from(range.1);
    let end = u64::from(range.2) << 32 | u64::from(range.3);
    end.saturating_sub(start)
}

pub(in crate::server) fn first_new_class_name(value: &str) -> Option<&str> {
    let mut offset = 0usize;
    while let Some(relative) = value[offset..].find("new") {
        let start = offset + relative;
        let before_ok = start == 0
            || value
                .as_bytes()
                .get(start - 1)
                .map(|byte| !is_ident_byte(*byte))
                .unwrap_or(true);
        let after_new = start + "new".len();
        let after_ok = value
            .as_bytes()
            .get(after_new)
            .is_some_and(u8::is_ascii_whitespace);
        if before_ok && after_ok {
            let class_start = skip_ascii_ws_server(value, after_new);
            let class_end = scan_php_class_name_end(value, class_start);
            if class_end > class_start {
                return value.get(class_start..class_end);
            }
        }
        offset = after_new;
    }
    None
}

pub(in crate::server) fn resolve_twig_context_class_name(
    file_symbols: &php_lsp_types::FileSymbols,
    raw_name: &str,
) -> String {
    let raw_name = raw_name.trim_start_matches('\\');
    if raw_name.contains('\\') {
        return raw_name.to_string();
    }

    for use_statement in &file_symbols.use_statements {
        if use_statement.kind != php_lsp_types::UseKind::Class {
            continue;
        }
        let alias = use_statement.alias.as_deref().unwrap_or_else(|| {
            use_statement
                .fqn
                .rsplit('\\')
                .next()
                .unwrap_or(use_statement.fqn.as_str())
        });
        if alias == raw_name {
            return use_statement.fqn.clone();
        }
    }

    file_symbols
        .namespace
        .as_ref()
        .map(|namespace| format!("{namespace}\\{raw_name}"))
        .unwrap_or_else(|| raw_name.to_string())
}

pub(in crate::server) fn find_top_level_double_arrow(
    source: &str,
    start: usize,
    end: usize,
) -> Option<usize> {
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut offset = start;

    while offset < end {
        let ch = source[offset..end].chars().next()?;
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active_quote {
                quote = None;
            }
            offset += ch.len_utf8();
            continue;
        }

        match ch {
            '\'' | '"' => quote = Some(ch),
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            '=' if paren_depth == 0
                && bracket_depth == 0
                && brace_depth == 0
                && source[offset..end].starts_with("=>") =>
            {
                return Some(offset);
            }
            _ => {}
        }
        offset += ch.len_utf8();
    }
    None
}

pub(in crate::server) fn php_string_literal_value_at_range(
    source: &str,
    start: usize,
    end: usize,
) -> Option<String> {
    let text = source.get(start..end)?.trim();
    unquote_php_string_literal(text)
}

pub(in crate::server) fn trim_source_range(
    source: &str,
    mut start: usize,
    mut end: usize,
) -> (usize, usize) {
    while start < end
        && source
            .as_bytes()
            .get(start)
            .is_some_and(u8::is_ascii_whitespace)
    {
        start += 1;
    }
    while end > start
        && source
            .as_bytes()
            .get(end - 1)
            .is_some_and(u8::is_ascii_whitespace)
    {
        end -= 1;
    }
    (start, end)
}

pub(in crate::server) fn skip_ascii_ws_server(source: &str, mut offset: usize) -> usize {
    while offset < source.len()
        && source
            .as_bytes()
            .get(offset)
            .is_some_and(u8::is_ascii_whitespace)
    {
        offset += 1;
    }
    offset
}

pub(in crate::server) fn scan_php_class_name_end(source: &str, start: usize) -> usize {
    let mut end = start;
    while end < source.len() {
        let byte = source.as_bytes()[end];
        if is_ident_byte(byte) || byte == b'\\' {
            end += 1;
        } else {
            break;
        }
    }
    end
}

pub(in crate::server) fn normalize_twig_key(key: &str) -> String {
    key.trim_start_matches('/').replace('\\', "/")
}

pub(in crate::server) fn is_template_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

pub(in crate::server) fn map_goto_definition_response_for_template(
    current_uri: &str,
    template: &TemplateDocument,
    response: GotoDefinitionResponse,
) -> GotoDefinitionResponse {
    match response {
        GotoDefinitionResponse::Scalar(location) => GotoDefinitionResponse::Scalar(
            map_location_for_template(current_uri, template, location),
        ),
        GotoDefinitionResponse::Array(locations) => GotoDefinitionResponse::Array(
            locations
                .into_iter()
                .map(|location| map_location_for_template(current_uri, template, location))
                .collect(),
        ),
        GotoDefinitionResponse::Link(links) => GotoDefinitionResponse::Link(
            links
                .into_iter()
                .map(|mut link| {
                    if link.target_uri.as_str() == current_uri {
                        if let Some(range) =
                            template.map_virtual_range_to_original(link.target_range)
                        {
                            link.target_range = range;
                        }
                        if let Some(range) =
                            template.map_virtual_range_to_original(link.target_selection_range)
                        {
                            link.target_selection_range = range;
                        }
                    }
                    link
                })
                .collect(),
        ),
    }
}

pub(in crate::server) fn map_location_for_template(
    current_uri: &str,
    template: &TemplateDocument,
    mut location: Location,
) -> Location {
    if location.uri.as_str() == current_uri {
        if let Some(range) = template.map_virtual_range_to_original(location.range) {
            location.range = range;
        }
    }
    location
}

async fn cached_twig_context_file_variables_for_state(
    root: &Path,
    template_name: &str,
    index: Arc<WorkspaceIndex>,
    twig_context_disk_cache: &Arc<Mutex<TwigContextDiskCache>>,
) -> Vec<TwigContextFileVariables> {
    let key = TwigContextDiskCacheKey {
        root: root.to_path_buf(),
        template_name: template_name.to_string(),
    };
    if let Some(files) = twig_context_disk_cache.lock().await.get(&key) {
        return files;
    }

    let root = root.to_path_buf();
    let template_name = template_name.to_string();
    let path_label = format!("{} ({})", root.display(), template_name);
    let files = match run_file_io_blocking("twig context scan", path_label, move || {
        let mut result = Vec::new();
        for path in collect_twig_context_php_files(&root, TWIG_CONTEXT_PHP_FILE_SCAN_LIMIT) {
            let Ok(source_uri) = path_to_uri(&path) else {
                continue;
            };
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            let mut parser = FileParser::new();
            parser.parse_full(&source);
            let file_symbols = parser
                .tree()
                .map(|tree| extract_file_symbols(tree, &source, &source_uri))
                .unwrap_or_default();
            let mut variables = HashMap::new();
            let mut shape_definitions = HashMap::new();
            collect_twig_render_context_types(
                &template_name,
                &source,
                &source_uri,
                &file_symbols,
                Some(&index),
                &mut variables,
                &mut shape_definitions,
            );
            if variables.is_empty() {
                continue;
            }
            let mut variables: Vec<_> = variables
                .into_iter()
                .map(|(name, type_text)| TemplateVariableType {
                    shape_definitions: shape_definitions.remove(&name).unwrap_or_default(),
                    name,
                    type_text,
                })
                .collect();
            variables.sort_by(|left, right| left.name.cmp(&right.name));
            result.push(TwigContextFileVariables {
                uri: source_uri,
                variables,
            });
        }
        result
    })
    .await
    {
        Ok(files) => files,
        Err(message) => {
            tracing::warn!("{}", message);
            Vec::new()
        }
    };

    twig_context_disk_cache
        .lock()
        .await
        .insert(key, files.clone());
    files
}

async fn twig_variable_types_for_template_state(
    uri_str: &str,
    open_files: &Arc<DashMap<String, FileParser>>,
    index: &Arc<WorkspaceIndex>,
    workspace_roots: &[PathBuf],
    twig_context_disk_cache: &Arc<Mutex<TwigContextDiskCache>>,
) -> Vec<TemplateVariableType> {
    let Some(root) = workspace_root_for_template_context_uri(uri_str, workspace_roots) else {
        return Vec::new();
    };
    let Some(template_name) = twig_template_name_for_uri(uri_str, &root) else {
        return Vec::new();
    };

    let mut variables = HashMap::<String, String>::new();
    let mut shape_definitions = HashMap::<String, Vec<TemplateShapeKeyDefinition>>::new();
    let mut open_php_uris = HashSet::<String>::new();

    for entry in open_files.iter() {
        let source_uri = entry.key();
        if source_uri == uri_str
            || !source_uri.ends_with(".php")
            || is_blade_template_uri(source_uri.as_str())
        {
            continue;
        }
        open_php_uris.insert(source_uri.to_string());
        let source = entry.value().source();
        let file_symbols = index
            .file_symbols
            .get(source_uri.as_str())
            .map(|symbols| symbols.value().clone())
            .or_else(|| {
                entry
                    .value()
                    .tree()
                    .map(|tree| extract_file_symbols(tree, &source, source_uri.as_str()))
            })
            .unwrap_or_default();
        collect_twig_render_context_types(
            &template_name,
            &source,
            source_uri,
            &file_symbols,
            Some(index.as_ref()),
            &mut variables,
            &mut shape_definitions,
        );
    }

    for file in cached_twig_context_file_variables_for_state(
        &root,
        &template_name,
        index.clone(),
        twig_context_disk_cache,
    )
    .await
    {
        if open_php_uris.contains(&file.uri) {
            continue;
        }
        for variable in file.variables {
            merge_twig_context_variable_type(
                &mut variables,
                variable.name.clone(),
                variable.type_text,
            );
            merge_twig_context_variable_shape_definitions(
                &mut shape_definitions,
                variable.name,
                variable.shape_definitions,
            );
        }
    }

    let mut result: Vec<_> = variables
        .into_iter()
        .map(|(name, type_text)| TemplateVariableType {
            shape_definitions: shape_definitions.remove(&name).unwrap_or_default(),
            name,
            type_text,
        })
        .collect();
    result.sort_by(|left, right| left.name.cmp(&right.name));
    result
}

pub(in crate::server) async fn refresh_open_twig_contexts_for_state(
    open_files: &Arc<DashMap<String, FileParser>>,
    template_documents: &Arc<DashMap<String, TemplateDocument>>,
    index: &Arc<WorkspaceIndex>,
    workspace_roots: &[PathBuf],
    twig_context_disk_cache: &Arc<Mutex<TwigContextDiskCache>>,
    semantic_tokens_cache: &Arc<Mutex<SemanticTokensCache>>,
) -> Vec<String> {
    let mut candidates: Vec<_> = template_documents
        .iter()
        .filter_map(|entry| {
            (entry.value().kind() == TemplateKind::Twig).then(|| entry.key().clone())
        })
        .collect();
    candidates.sort();

    if candidates.len() > OPEN_TWIG_CONTEXT_REFRESH_LIMIT {
        tracing::warn!(
            "Skipping {} open Twig context refresh(es) over limit {}",
            candidates.len() - OPEN_TWIG_CONTEXT_REFRESH_LIMIT,
            OPEN_TWIG_CONTEXT_REFRESH_LIMIT
        );
        candidates.truncate(OPEN_TWIG_CONTEXT_REFRESH_LIMIT);
    }

    let mut refreshed = Vec::new();
    for uri_str in candidates {
        let Some(template) = template_documents
            .get(&uri_str)
            .map(|document| document.value().clone())
        else {
            continue;
        };
        if template.kind() != TemplateKind::Twig {
            continue;
        }

        let variable_types = twig_variable_types_for_template_state(
            &uri_str,
            open_files,
            index,
            workspace_roots,
            twig_context_disk_cache,
        )
        .await;
        let refreshed_template = template.with_twig_variable_types(&variable_types);
        let mut parser = FileParser::new();
        parser.parse_full(refreshed_template.virtual_source());
        template_documents.insert(uri_str.clone(), refreshed_template);
        open_files.insert(uri_str.clone(), parser);
        index.remove_file(&uri_str);
        semantic_tokens_cache.lock().await.remove(&uri_str);
        refreshed.push(uri_str);
    }

    refreshed
}

impl PhpLspBackend {
    pub(in crate::server) fn template_document(&self, uri_str: &str) -> Option<TemplateDocument> {
        self.template_documents
            .get(uri_str)
            .map(|document| document.value().clone())
    }

    pub(in crate::server) fn open_template_document(
        &self,
        uri_str: &str,
        text: &str,
        kind: TemplateKind,
        twig_variable_types: &[TemplateVariableType],
    ) -> FileParser {
        let template = match kind {
            TemplateKind::Blade => preprocess_blade_template(text),
            TemplateKind::Twig => preprocess_twig_template(text, twig_variable_types),
        };
        let mut parser = FileParser::new();
        parser.parse_full(template.virtual_source());
        self.template_documents
            .insert(uri_str.to_string(), template);
        parser
    }

    pub(in crate::server) async fn twig_variable_types_for_template(
        &self,
        uri_str: &str,
    ) -> Vec<TemplateVariableType> {
        let roots = self.current_workspace_roots().await;
        twig_variable_types_for_template_state(
            uri_str,
            &self.open_files,
            &self.index,
            &roots,
            &self.twig_context_disk_cache,
        )
        .await
    }

    pub(in crate::server) async fn refresh_open_twig_contexts(&self) -> Vec<String> {
        let roots = self.current_workspace_roots().await;
        refresh_open_twig_contexts_for_state(
            &self.open_files,
            &self.template_documents,
            &self.index,
            &roots,
            &self.twig_context_disk_cache,
            &self.semantic_tokens_cache,
        )
        .await
    }

    pub(in crate::server) async fn refresh_open_twig_contexts_and_republish_diagnostics(&self) {
        let refreshed_uris = self.refresh_open_twig_contexts().await;
        for uri_str in refreshed_uris {
            if let Ok(uri) = uri_str.parse::<Uri>() {
                self.publish_diagnostics(&uri).await;
            }
        }
    }

    pub(in crate::server) async fn twig_template_location(
        &self,
        uri_str: &str,
        key: &str,
    ) -> Option<Location> {
        let root = self.workspace_root_for_uri(uri_str).await?;
        let path = twig_template_path_for_key(&root, key)?;
        let uri = path_to_uri(&path).ok()?.parse::<Uri>().ok()?;
        Some(Location {
            uri,
            range: Range {
                start: Position::new(0, 0),
                end: Position::new(0, 0),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_nested_literal_array_shape_type_for_twig_context() {
        let source = concat!(
            "[\n",
            "    'encryption' => ['temp_dir_path' => '/tmp', 'enabled' => true],\n",
            "    'sftp' => ['host' => 'localhost', 'port' => 22],\n",
            "]",
        );
        let file_symbols = php_lsp_types::FileSymbols::default();

        let type_text =
            infer_twig_context_value_type(source, (0, source.len()), &file_symbols, None)
                .expect("literal array shape type");

        assert_eq!(
            type_text,
            "array{encryption: array{temp_dir_path: string, enabled: bool}, sftp: array{host: string, port: int}}"
        );
    }

    #[test]
    fn infers_array_append_shape_type_for_twig_context_assignment() {
        let source = concat!(
            "<?php\n",
            "$items = [];\n",
            "$items[] = ['nr' => 'NR-1', 'code' => 'ERR', 'description' => 'Failure'];\n",
            "$this->render('index.html.twig', ['items' => $items]);\n",
        );
        let value_start = source.rfind("$items").expect("render variable usage");
        let file_symbols = php_lsp_types::FileSymbols::default();
        let mut visited_variables = HashSet::new();

        let type_text = infer_twig_context_assignment_value_type(
            source,
            value_start,
            "items",
            &file_symbols,
            None,
            &mut visited_variables,
        )
        .expect("append array shape type");

        assert_eq!(
            type_text,
            "array<int, array{nr: string, code: string, description: string}>"
        );
    }

    #[test]
    fn infers_reused_variable_in_sibling_shape_values() {
        let source = concat!(
            "<?php\n",
            "$messageLog = new MessageLog();\n",
            "['first' => $messageLog, 'second' => $messageLog]\n",
        );
        let start = source.find("['first'").expect("shape start");
        let file_symbols = php_lsp_types::FileSymbols::default();

        let type_text =
            infer_twig_context_value_type(source, (start, source.len()), &file_symbols, None)
                .expect("reused variable shape type");

        assert_eq!(type_text, "array{first: MessageLog, second: MessageLog}");
    }

    #[test]
    fn infers_reused_variable_in_multiple_append_rows() {
        let source = concat!(
            "<?php\n",
            "$row = ['nr' => 'NR-1'];\n",
            "$items = [];\n",
            "$items[] = $row;\n",
            "$items[] = $row;\n",
            "$this->render('index.html.twig', ['items' => $items]);\n",
        );
        let value_start = source.rfind("$items").expect("render variable usage");
        let file_symbols = php_lsp_types::FileSymbols::default();
        let mut visited_variables = HashSet::new();

        let type_text = infer_twig_context_assignment_value_type(
            source,
            value_start,
            "items",
            &file_symbols,
            None,
            &mut visited_variables,
        )
        .expect("append array shape type");

        assert_eq!(type_text, "array<int, array{nr: string}>");
    }

    #[test]
    fn keeps_latest_non_empty_assignment_type_when_array_is_appended_later() {
        let source = concat!(
            "<?php\n",
            "$items = [new MessageLog()];\n",
            "$items[] = ['nr' => 'NR-1'];\n",
            "$this->render('index.html.twig', ['items' => $items]);\n",
        );
        let value_start = source.rfind("$items").expect("render variable usage");
        let file_symbols = php_lsp_types::FileSymbols::default();
        let mut visited_variables = HashSet::new();

        let type_text = infer_twig_context_assignment_value_type(
            source,
            value_start,
            "items",
            &file_symbols,
            None,
            &mut visited_variables,
        )
        .expect("latest non-empty assignment type");

        assert_eq!(type_text, "array<int, MessageLog>");
    }

    #[test]
    fn finds_phpdoc_list_shape_definition_after_non_ascii_text() {
        let comment = "/**\n * @return list<array{🇺🇸 中国 བོད note: string, npId: string}>\n */";
        let symbol = php_lsp_types::SymbolInfo {
            name: "fetchRows".to_string(),
            fqn: "App\\Repository\\MessageLogRepository::fetchRows".to_string(),
            kind: php_lsp_types::PhpSymbolKind::Method,
            uri: "file:///workspace/src/Repository/MessageLogRepository.php".to_string(),
            range: (12, 4, 15, 5),
            selection_range: (12, 20, 12, 29),
            visibility: php_lsp_types::Visibility::Public,
            modifiers: php_lsp_types::SymbolModifiers::default(),
            doc_comment: Some(comment.to_string()),
            signature: None,
            parent_fqn: Some("App\\Repository\\MessageLogRepository".to_string()),
            extends: Vec::new(),
            implements: Vec::new(),
            traits: Vec::new(),
            templates: Vec::new(),
            template_bindings: Vec::new(),
        };
        let type_info = php_lsp_types::TypeInfo::Generic {
            base: "list".to_string(),
            args: vec![php_lsp_types::TypeInfo::ArrayShape(vec![
                php_lsp_types::ArrayShapeItem {
                    key: Some("note".to_string()),
                    optional: false,
                    value: php_lsp_types::TypeInfo::Simple("string".to_string()),
                },
                php_lsp_types::ArrayShapeItem {
                    key: Some("npId".to_string()),
                    optional: false,
                    value: php_lsp_types::TypeInfo::Simple("string".to_string()),
                },
            ])],
        };
        let definitions = collect_symbol_type_shape_definitions(
            &WorkspaceIndex::new(),
            &php_lsp_types::FileSymbols::default(),
            &symbol,
            "App\\Repository\\MessageLogRepository",
            &type_info,
            TemplateShapeDefinitionTarget::Direct,
        );
        let definition = definitions
            .iter()
            .find(|definition| definition.path == vec!["npId".to_string()])
            .expect("npId shape definition");
        let np_id_offset = comment.find("npId").expect("npId in PHPDoc");
        let line_start = comment[..np_id_offset].rfind('\n').map_or(0, |idx| idx + 1);
        let expected_character = comment[line_start..np_id_offset].encode_utf16().count() as u32;

        assert_eq!(
            definition.target,
            TemplateShapeDefinitionTarget::IterableValue
        );
        assert_eq!(definition.range.0, 10);
        assert_eq!(definition.range.1, expected_character);
    }
}
