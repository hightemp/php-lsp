//! Template-aware LSP helpers extracted from `server.rs`.

use crate::util::uri::path_to_uri;

use super::super::*;

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

pub(in crate::server) fn collect_twig_render_context_types(
    template_name: &str,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    variables: &mut HashMap<String, String>,
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
            {
                collect_twig_context_array_types(source, context_arg, file_symbols, variables);
            }
        }
        offset = close_paren + 1;
    }
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

pub(in crate::server) fn collect_twig_context_array_types(
    source: &str,
    range: (usize, usize),
    file_symbols: &php_lsp_types::FileSymbols,
    variables: &mut HashMap<String, String>,
) {
    let (start, end) = range;
    let Some((inner_start, inner_end)) = php_array_inner_range(source, start, end) else {
        return;
    };
    let spans = split_top_level_spans(
        source.get(inner_start..inner_end).unwrap_or(""),
        inner_start,
    );
    for span in spans {
        let Some(arrow) = find_top_level_double_arrow(source, span.0, span.1) else {
            continue;
        };
        let key_range = trim_source_range(source, span.0, arrow);
        let value_range = trim_source_range(source, arrow + 2, span.1);
        let Some(name) = php_string_literal_value_at_range(source, key_range.0, key_range.1) else {
            continue;
        };
        if !is_template_variable_name(&name) || variables.contains_key(&name) {
            continue;
        }
        if let Some(type_text) = infer_twig_context_value_type(source, value_range, file_symbols) {
            variables.insert(name, type_text);
        }
    }
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

pub(in crate::server) fn infer_twig_context_value_type(
    source: &str,
    range: (usize, usize),
    file_symbols: &php_lsp_types::FileSymbols,
) -> Option<String> {
    let (start, end) = trim_source_range(source, range.0, range.1);
    let value = source.get(start..end)?.trim();
    if value.starts_with('[') || value.starts_with("array") {
        if let Some(class_name) = first_new_class_name(value) {
            return Some(format!(
                "array<int, {}>",
                resolve_twig_context_class_name(file_symbols, class_name)
            ));
        }
    }

    first_new_class_name(value)
        .map(|class_name| resolve_twig_context_class_name(file_symbols, class_name))
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

    async fn cached_twig_context_file_variables(
        &self,
        root: &Path,
        template_name: &str,
    ) -> Vec<TwigContextFileVariables> {
        let key = TwigContextDiskCacheKey {
            root: root.to_path_buf(),
            template_name: template_name.to_string(),
        };
        if let Some(files) = self.twig_context_disk_cache.lock().await.get(&key) {
            return files;
        }

        let root = root.to_path_buf();
        let template_name = template_name.to_string();
        let path_label = format!("{} ({})", root.display(), template_name);
        let files = match run_file_io_blocking("twig context scan", path_label, move || {
            let mut result = Vec::new();
            for path in collect_twig_context_php_files(&root, 2048) {
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
                collect_twig_render_context_types(
                    &template_name,
                    &source,
                    &file_symbols,
                    &mut variables,
                );
                if variables.is_empty() {
                    continue;
                }
                let mut variables: Vec<_> = variables
                    .into_iter()
                    .map(|(name, type_text)| TemplateVariableType { name, type_text })
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

        self.twig_context_disk_cache
            .lock()
            .await
            .insert(key, files.clone());
        files
    }

    pub(in crate::server) async fn twig_variable_types_for_template(
        &self,
        uri_str: &str,
    ) -> Vec<TemplateVariableType> {
        let Some(root) = self.workspace_root_for_uri(uri_str).await else {
            return Vec::new();
        };
        let Some(template_name) = twig_template_name_for_uri(uri_str, &root) else {
            return Vec::new();
        };

        let mut variables = HashMap::<String, String>::new();
        let mut open_php_uris = HashSet::<String>::new();

        for entry in self.open_files.iter() {
            let source_uri = entry.key();
            if source_uri == uri_str || !source_uri.ends_with(".php") {
                continue;
            }
            open_php_uris.insert(source_uri.to_string());
            let source = entry.value().source();
            let file_symbols = self
                .index
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
                &file_symbols,
                &mut variables,
            );
        }

        for file in self
            .cached_twig_context_file_variables(&root, &template_name)
            .await
        {
            if open_php_uris.contains(&file.uri) {
                continue;
            }
            for variable in file.variables {
                variables.insert(variable.name, variable.type_text);
            }
        }

        let mut result: Vec<_> = variables
            .into_iter()
            .map(|(name, type_text)| TemplateVariableType { name, type_text })
            .collect();
        result.sort_by(|left, right| left.name.cmp(&right.name));
        result
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
