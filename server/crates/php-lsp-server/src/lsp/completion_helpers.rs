//! Shared hover/completion/definition helpers extracted from `server.rs`.

use super::super::*;
use php_lsp_types::normalize_shape_key_text;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::server) enum PhpDocVirtualMemberKind {
    Property,
    Method,
}

#[derive(Debug, Clone)]
pub(in crate::server) struct PhpDocVirtualMember {
    pub(in crate::server) owner: Arc<php_lsp_types::SymbolInfo>,
    pub(in crate::server) name: String,
    pub(in crate::server) kind: PhpDocVirtualMemberKind,
    pub(in crate::server) type_info: Option<php_lsp_types::TypeInfo>,
    pub(in crate::server) access: Option<php_lsp_types::PhpDocPropertyAccess>,
    pub(in crate::server) return_type: Option<php_lsp_types::TypeInfo>,
    pub(in crate::server) params: Vec<php_lsp_types::ParamInfo>,
    pub(in crate::server) description: Option<String>,
    pub(in crate::server) is_static: bool,
}

#[derive(Debug, Clone)]
pub(in crate::server) struct FrameworkStringKeyAtPosition {
    pub(in crate::server) domain: &'static str,
    pub(in crate::server) prefix: String,
    pub(in crate::server) key: String,
}

pub(in crate::server) fn phpdoc_virtual_member_for_symbol(
    index: &WorkspaceIndex,
    sym: &SymbolAtPosition,
) -> Option<PhpDocVirtualMember> {
    let kind = match sym.ref_kind {
        RefKind::PropertyAccess | RefKind::StaticPropertyAccess => {
            PhpDocVirtualMemberKind::Property
        }
        RefKind::MethodCall => PhpDocVirtualMemberKind::Method,
        _ => return None,
    };
    let (class_fqn, member_name) = sym.fqn.rsplit_once("::")?;
    let member_name = member_name.trim_start_matches('$');
    phpdoc_virtual_member(index, class_fqn, member_name, kind)
}

pub(in crate::server) fn phpdoc_virtual_member(
    index: &WorkspaceIndex,
    class_fqn: &str,
    member_name: &str,
    kind: PhpDocVirtualMemberKind,
) -> Option<PhpDocVirtualMember> {
    for owner in index.get_type_hierarchy_symbols(class_fqn) {
        let Some(ref doc_comment) = owner.doc_comment else {
            continue;
        };
        let phpdoc = parse_phpdoc(doc_comment);
        match kind {
            PhpDocVirtualMemberKind::Property => {
                if let Some(property) = phpdoc
                    .properties
                    .into_iter()
                    .find(|property| property.name == member_name)
                {
                    return Some(PhpDocVirtualMember {
                        owner,
                        name: property.name,
                        kind,
                        type_info: property.type_info,
                        access: Some(property.access),
                        return_type: None,
                        params: Vec::new(),
                        description: property.description,
                        is_static: false,
                    });
                }
            }
            PhpDocVirtualMemberKind::Method => {
                if let Some(method) = phpdoc
                    .methods
                    .into_iter()
                    .find(|method| method.name == member_name)
                {
                    return Some(PhpDocVirtualMember {
                        owner,
                        name: method.name,
                        kind,
                        type_info: None,
                        access: None,
                        return_type: method.return_type,
                        params: method.params,
                        description: method.description,
                        is_static: method.is_static,
                    });
                }
            }
        }
    }

    None
}

pub(in crate::server) fn phpdoc_virtual_property_type_fqn(
    index: &WorkspaceIndex,
    class_fqn: &str,
    member_name: &str,
) -> Option<String> {
    let member_name = member_name.trim_start_matches('$');
    let member = phpdoc_virtual_member(
        index,
        class_fqn,
        member_name,
        PhpDocVirtualMemberKind::Property,
    )?;
    let type_info = member.type_info.as_ref()?;
    type_info_fqn_from_index(index, class_fqn, &member.owner.uri, type_info)
}

pub(in crate::server) fn framework_virtual_member_for_symbol(
    index: &WorkspaceIndex,
    sym: &SymbolAtPosition,
    source_uri: Option<&str>,
    file_symbols: Option<&php_lsp_types::FileSymbols>,
    source: Option<&str>,
) -> Option<crate::framework::VirtualMember> {
    let (class_fqn, member_name) = sym.fqn.rsplit_once("::")?;
    let query =
        crate::framework::VirtualMemberQuery::from_ref_kind(class_fqn, member_name, sym.ref_kind)?;
    let ctx = crate::framework::FrameworkProviderContext::new(index)
        .with_source_uri(source_uri)
        .with_file(file_symbols, source)
        .with_relevant_files(&[]);
    let registry = crate::framework::default_framework_provider_registry();
    let cache = crate::framework::FrameworkProviderCache::default();
    cache
        .virtual_members(&registry, &ctx, &query)
        .into_iter()
        .next()
}

pub(in crate::server) fn framework_virtual_member_candidates(
    index: &WorkspaceIndex,
    class_fqn: &str,
    source_uri: Option<&str>,
    file_symbols: Option<&php_lsp_types::FileSymbols>,
    source: Option<&str>,
    kind: Option<crate::framework::VirtualMemberKind>,
) -> Vec<crate::framework::VirtualMember> {
    let ctx = crate::framework::FrameworkProviderContext::new(index)
        .with_source_uri(source_uri)
        .with_file(file_symbols, source)
        .with_relevant_files(&[]);
    let registry = crate::framework::default_framework_provider_registry();
    registry.virtual_member_candidates(&ctx, class_fqn, kind)
}

pub(in crate::server) fn framework_virtual_member_type_fqn(
    index: &WorkspaceIndex,
    class_fqn: &str,
    member_name: &str,
    source_uri: Option<&str>,
    file_symbols: Option<&php_lsp_types::FileSymbols>,
    source: Option<&str>,
) -> Option<String> {
    let kind = if member_name.starts_with('$') {
        crate::framework::VirtualMemberKind::Property
    } else {
        crate::framework::VirtualMemberKind::Method
    };
    let query = crate::framework::VirtualMemberQuery {
        owner_fqn: class_fqn.to_string(),
        member_name: member_name.to_string(),
        kind,
    };
    let ctx = crate::framework::FrameworkProviderContext::new(index)
        .with_source_uri(source_uri)
        .with_file(file_symbols, source)
        .with_relevant_files(&[]);
    let registry = crate::framework::default_framework_provider_registry();
    let cache = crate::framework::FrameworkProviderCache::default();
    let member = cache
        .virtual_members(&registry, &ctx, &query)
        .into_iter()
        .next()?;
    let type_info = member.type_info.as_ref()?;
    let uri = file_symbols
        .and_then(|symbols| symbols.symbols.first())
        .map(|symbol| symbol.uri.as_str())
        .or(source_uri)
        .unwrap_or("");
    type_info_fqn_from_index(index, class_fqn, uri, type_info)
}

pub(in crate::server) fn phpdoc_property_tag(
    access: php_lsp_types::PhpDocPropertyAccess,
) -> &'static str {
    match access {
        php_lsp_types::PhpDocPropertyAccess::ReadWrite => "@property",
        php_lsp_types::PhpDocPropertyAccess::ReadOnly => "@property-read",
        php_lsp_types::PhpDocPropertyAccess::WriteOnly => "@property-write",
    }
}

pub(in crate::server) fn phpdoc_virtual_completion_data(
    item: &CompletionItem,
) -> Option<(&str, &str, &str)> {
    let data = item.data.as_ref()?;
    if data.get("kind")?.as_str()? != "phpdoc-virtual-member" {
        return None;
    }
    Some((
        data.get("ownerFqn")?.as_str()?,
        data.get("memberKind")?.as_str()?,
        data.get("memberName")?.as_str()?,
    ))
}

pub(in crate::server) fn framework_virtual_completion_data(
    item: &CompletionItem,
) -> Option<(&str, &str, &str)> {
    let data = item.data.as_ref()?;
    if data.get("kind")?.as_str()? != "framework-virtual-member" {
        return None;
    }
    Some((
        data.get("ownerFqn")?.as_str()?,
        data.get("memberKind")?.as_str()?,
        data.get("memberName")?.as_str()?,
    ))
}

pub(in crate::server) fn phpdoc_virtual_member_markdown(member: &PhpDocVirtualMember) -> String {
    let mut content = String::new();
    content.push_str("```php\n");
    match member.kind {
        PhpDocVirtualMemberKind::Property => {
            let access = member
                .access
                .map(phpdoc_property_tag)
                .unwrap_or("@property");
            content.push_str(access);
            if let Some(ref type_info) = member.type_info {
                content.push(' ');
                content.push_str(&type_info.to_string());
            }
            content.push_str(" $");
            content.push_str(&member.name);
        }
        PhpDocVirtualMemberKind::Method => {
            content.push_str("@method ");
            if member.is_static {
                content.push_str("static ");
            }
            if let Some(ref return_type) = member.return_type {
                content.push_str(&return_type.to_string());
                content.push(' ');
            }
            content.push_str(&member.name);
            content.push('(');
            content.push_str(&format_phpdoc_params(&member.params));
            content.push(')');
        }
    }
    content.push_str("\n```\n");
    if let Some(ref description) = member.description {
        content.push_str("\n---\n\n");
        content.push_str(description);
        content.push('\n');
    }
    content
}

pub(in crate::server) fn phpdoc_virtual_member_markdown_with_links(
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    member: &PhpDocVirtualMember,
) -> String {
    let mut content = phpdoc_virtual_member_markdown(member);
    let owner_fqn = member.owner.fqn.as_str();
    let uri = member.owner.uri.as_str();
    append_class_fqn_link_line(&mut content, "Declared in", index, owner_fqn, owner_fqn);
    match member.kind {
        PhpDocVirtualMemberKind::Property => {
            if let Some(ref type_info) = member.type_info {
                append_type_link_line(
                    &mut content,
                    "Type",
                    index,
                    file_symbols,
                    owner_fqn,
                    uri,
                    type_info,
                );
            }
        }
        PhpDocVirtualMemberKind::Method => {
            if let Some(ref return_type) = member.return_type {
                append_type_link_line(
                    &mut content,
                    "Returns",
                    index,
                    file_symbols,
                    owner_fqn,
                    uri,
                    return_type,
                );
            }
            append_signature_parameter_lines(
                &mut content,
                index,
                file_symbols,
                owner_fqn,
                uri,
                &member.params,
                &[],
            );
        }
    }
    content
}

fn format_phpdoc_params(params: &[php_lsp_types::ParamInfo]) -> String {
    params
        .iter()
        .map(format_phpdoc_param)
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_phpdoc_param(param: &php_lsp_types::ParamInfo) -> String {
    let mut out = String::new();
    if let Some(ref type_info) = param.type_info {
        out.push_str(&type_info.to_string());
        out.push(' ');
    }
    if param.is_by_ref {
        out.push('&');
    }
    if param.is_variadic {
        out.push_str("...");
    }
    out.push('$');
    out.push_str(&param.name);
    if let Some(ref default) = param.default_value {
        out.push_str(" = ");
        out.push_str(default);
    }
    out
}

pub(in crate::server) fn framework_virtual_member_detail(
    member: &crate::framework::VirtualMember,
) -> String {
    match member.kind {
        crate::framework::VirtualMemberKind::Property
        | crate::framework::VirtualMemberKind::StaticProperty => {
            let access = member
                .access
                .map(phpdoc_property_tag)
                .unwrap_or("@property");
            match member.type_info.as_ref() {
                Some(type_info) => format!("{} {}", access, type_info),
                None => access.to_string(),
            }
        }
        crate::framework::VirtualMemberKind::Method => member
            .type_info
            .as_ref()
            .map(|type_info| format!("(): {}", type_info))
            .unwrap_or_else(|| "()".to_string()),
        crate::framework::VirtualMemberKind::ClassConstant => "class constant".to_string(),
    }
}

pub(in crate::server) fn framework_virtual_member_markdown(
    member: &crate::framework::VirtualMember,
) -> String {
    let mut content = String::new();
    content.push_str("```php\n");
    match member.kind {
        crate::framework::VirtualMemberKind::Property
        | crate::framework::VirtualMemberKind::StaticProperty => {
            let access = member
                .access
                .map(phpdoc_property_tag)
                .unwrap_or("@property");
            content.push_str(access);
            if let Some(ref type_info) = member.type_info {
                content.push(' ');
                content.push_str(&type_info.to_string());
            }
            content.push_str(" $");
            content.push_str(member.name.trim_start_matches('$'));
        }
        crate::framework::VirtualMemberKind::Method => {
            content.push_str("@method ");
            if let Some(ref type_info) = member.type_info {
                content.push_str(&type_info.to_string());
                content.push(' ');
            }
            content.push_str(&member.name);
            content.push_str("()");
        }
        crate::framework::VirtualMemberKind::ClassConstant => {
            content.push_str("const ");
            content.push_str(&member.name);
        }
    }
    content.push_str("\n```\n");
    if let Some(ref detail) = member.detail {
        content.push_str("\n---\n\n");
        content.push_str(detail);
        content.push('\n');
    }
    content
}

pub(in crate::server) fn framework_virtual_member_markdown_with_links(
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    member: &crate::framework::VirtualMember,
) -> String {
    let mut content = framework_virtual_member_markdown(member);
    let uri = index
        .resolve_fqn(member.owner_fqn.trim_start_matches('\\'))
        .map(|symbol| symbol.uri.clone())
        .unwrap_or_default();
    append_class_fqn_link_line(
        &mut content,
        "Declared in",
        index,
        &member.owner_fqn,
        &member.owner_fqn,
    );
    if let Some(ref type_info) = member.type_info {
        let label = match member.kind {
            crate::framework::VirtualMemberKind::Method => "Returns",
            crate::framework::VirtualMemberKind::Property
            | crate::framework::VirtualMemberKind::StaticProperty
            | crate::framework::VirtualMemberKind::ClassConstant => "Type",
        };
        append_type_link_line(
            &mut content,
            label,
            index,
            file_symbols,
            &member.owner_fqn,
            &uri,
            type_info,
        );
    }
    content
}

pub(in crate::server) fn framework_virtual_completion_item(
    member: &crate::framework::VirtualMember,
    member_prefix: &str,
) -> lsp_types::CompletionItem {
    let label = member.name.trim_start_matches('$').to_string();
    let rank = if label.starts_with(member_prefix) {
        "0"
    } else if label
        .to_ascii_lowercase()
        .contains(&member_prefix.to_ascii_lowercase())
    {
        "1"
    } else {
        "2"
    };

    lsp_types::CompletionItem {
        label: label.clone(),
        kind: Some(match member.kind {
            crate::framework::VirtualMemberKind::Method => lsp_types::CompletionItemKind::METHOD,
            crate::framework::VirtualMemberKind::Property
            | crate::framework::VirtualMemberKind::StaticProperty => {
                lsp_types::CompletionItemKind::PROPERTY
            }
            crate::framework::VirtualMemberKind::ClassConstant => {
                lsp_types::CompletionItemKind::CONSTANT
            }
        }),
        detail: Some(framework_virtual_member_detail(member)),
        documentation: Some(lsp_types::Documentation::MarkupContent(
            lsp_types::MarkupContent {
                kind: lsp_types::MarkupKind::Markdown,
                value: framework_virtual_member_markdown(member),
            },
        )),
        sort_text: Some(format!("2_{}_{}", rank, label.to_ascii_lowercase())),
        filter_text: Some(format!("{} {}", label, member.fqn)),
        data: Some(serde_json::json!({
            "kind": "framework-virtual-member",
            "ownerFqn": member.owner_fqn.as_str(),
            "memberKind": match member.kind {
                crate::framework::VirtualMemberKind::Method => "method",
                crate::framework::VirtualMemberKind::Property
                    | crate::framework::VirtualMemberKind::StaticProperty => "property",
                crate::framework::VirtualMemberKind::ClassConstant => "constant",
            },
            "memberName": member.name.as_str(),
        })),
        commit_characters: Some(match member.kind {
            crate::framework::VirtualMemberKind::Method => vec!["(".to_string()],
            _ => vec![";".to_string(), ",".to_string()],
        }),
        ..Default::default()
    }
}

pub(in crate::server) fn framework_string_key_completion_item(
    key: &crate::framework::FrameworkStringKey,
    prefix: &str,
) -> lsp_types::CompletionItem {
    let insert_text = key
        .key
        .strip_prefix(prefix)
        .unwrap_or(key.key.as_str())
        .to_string();
    lsp_types::CompletionItem {
        label: key.key.clone(),
        kind: Some(lsp_types::CompletionItemKind::VALUE),
        detail: key.detail.clone(),
        insert_text: Some(insert_text),
        sort_text: Some(format!("1_{}", key.key.to_ascii_lowercase())),
        filter_text: Some(key.key.clone()),
        data: Some(serde_json::json!({
            "kind": "framework-string-key",
            "domain": key.provider_ids.first().copied().unwrap_or("framework"),
            "key": key.key.as_str(),
        })),
        ..Default::default()
    }
}

pub(in crate::server) fn framework_string_key_completion_item_to_ls(
    mut item: lsp_types::CompletionItem,
) -> CompletionItem {
    CompletionItem {
        label: item.label,
        kind: item.kind.map(lsp_completion_kind_to_ls),
        detail: item.detail,
        sort_text: item.sort_text,
        filter_text: item.filter_text,
        insert_text: item.insert_text,
        insert_text_format: item.insert_text_format.map(lsp_insert_text_format_to_ls),
        commit_characters: item.commit_characters,
        tags: item.tags.take().map(|tags| {
            tags.into_iter()
                .filter_map(|tag| {
                    if tag == lsp_types::CompletionItemTag::DEPRECATED {
                        Some(CompletionItemTag::DEPRECATED)
                    } else {
                        None
                    }
                })
                .collect()
        }),
        data: item.data,
        ..Default::default()
    }
}

pub(in crate::server) fn framework_string_key_source_byte_range(
    key: &crate::framework::FrameworkStringKey,
) -> Option<(String, (u32, u32, u32, u32))> {
    let (uri, range) = key.sources.iter().find_map(|source| match source {
        crate::framework::VirtualMemberSource::SourceRange { uri, range } => {
            Some((uri.clone(), *range))
        }
        crate::framework::VirtualMemberSource::Synthetic { .. } => None,
    })?;
    Some((uri, range))
}

pub(in crate::server) fn framework_string_key_context_at_position(
    source: &str,
    line: u32,
    byte_col: u32,
) -> Option<FrameworkStringKeyAtPosition> {
    let offset = byte_offset_for_line_col(source, line, byte_col)?;
    let bounds = string_literal_bounds_at_offset(source, offset)?;
    let domain = framework_string_key_domain_before_string(source, bounds.quote_start)?;
    let prefix = source.get(bounds.content_start..offset)?.to_string();
    let key = source
        .get(bounds.content_start..bounds.content_end)
        .unwrap_or(prefix.as_str())
        .to_string();
    Some(FrameworkStringKeyAtPosition {
        domain,
        prefix,
        key,
    })
}

pub(in crate::server) fn twig_static_template_path_context_at_position(
    source: &str,
    line: u32,
    byte_col: u32,
) -> Option<FrameworkStringKeyAtPosition> {
    let offset = line_col_to_byte_offset(source, line, byte_col)?;
    let bounds = string_literal_bounds_at_offset(source, offset)?;
    let prefix = source.get(bounds.content_start..offset)?.to_string();
    let key = source
        .get(bounds.content_start..bounds.content_end)
        .unwrap_or(prefix.as_str())
        .to_string();
    Some(FrameworkStringKeyAtPosition {
        domain: "twig",
        prefix,
        key,
    })
}

pub(in crate::server) fn twig_route_key_context_at_position(
    source: &str,
    line: u32,
    byte_col: u32,
) -> Option<FrameworkStringKeyAtPosition> {
    let offset = line_col_to_byte_offset(source, line, byte_col)?;
    let bounds = string_literal_bounds_at_offset(source, offset)?;
    let (expression_start, expression_end) =
        twig_delimiter_bounds_containing(source, bounds.quote_start)?;
    if bounds.content_end > expression_end {
        return None;
    }

    let open_paren = previous_non_ws_char(source, bounds.quote_start)?;
    if open_paren < expression_start || source.as_bytes().get(open_paren).copied()? != b'(' {
        return None;
    }
    if !source
        .get(open_paren + 1..bounds.quote_start)?
        .trim()
        .is_empty()
    {
        return None;
    }

    let name_end = previous_non_ws_char(source, open_paren)?;
    if name_end < expression_start {
        return None;
    }
    let name_start = scan_identifier_start(source, name_end + 1).max(expression_start);
    let name = source.get(name_start..=name_end)?;
    if !matches!(name, "path" | "url") {
        return None;
    }

    let prefix = source.get(bounds.content_start..offset)?.to_string();
    let key = source
        .get(bounds.content_start..bounds.content_end)
        .unwrap_or(prefix.as_str())
        .to_string();
    Some(FrameworkStringKeyAtPosition {
        domain: "route",
        prefix,
        key,
    })
}

#[derive(Debug, Clone, Copy)]
pub(in crate::server) struct StringLiteralBounds {
    quote_start: usize,
    content_start: usize,
    content_end: usize,
}

pub(in crate::server) fn string_literal_bounds_at_offset(
    source: &str,
    offset: usize,
) -> Option<StringLiteralBounds> {
    let mut quote: Option<(char, usize)> = None;
    let mut escaped = false;
    for (idx, ch) in source.char_indices() {
        if idx >= offset {
            break;
        }
        if let Some((active_quote, _)) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active_quote {
                quote = None;
            }
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some((ch, idx));
        }
    }

    let (quote_char, quote_start) = quote?;
    let content_start = quote_start + quote_char.len_utf8();
    if offset < content_start {
        return None;
    }
    let content_end = find_unescaped_quote(source, offset, quote_char)
        .unwrap_or_else(|| line_end_offset(source, offset));
    Some(StringLiteralBounds {
        quote_start,
        content_start,
        content_end,
    })
}

fn twig_delimiter_bounds_containing(source: &str, offset: usize) -> Option<(usize, usize)> {
    let before = source.get(..offset)?;
    let echo_open = before.rfind("{{").map(|idx| (idx, "}}"));
    let tag_open = before.rfind("{%").map(|idx| (idx, "%}"));
    let (open, close_token) = match (echo_open, tag_open) {
        (Some(left), Some(right)) => {
            if left.0 > right.0 {
                left
            } else {
                right
            }
        }
        (Some(open), None) | (None, Some(open)) => open,
        (None, None) => return None,
    };

    let last_echo_close = before.rfind("}}");
    let last_tag_close = before.rfind("%}");
    let last_close = match (last_echo_close, last_tag_close) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(close), None) | (None, Some(close)) => Some(close),
        (None, None) => None,
    };
    if last_close.is_some_and(|close| close > open) {
        return None;
    }

    let close = source
        .get(offset..)?
        .find(close_token)
        .map(|relative| offset + relative)?;
    Some((open + 2, close))
}

pub(in crate::server) fn find_unescaped_quote(
    source: &str,
    start: usize,
    quote: char,
) -> Option<usize> {
    let mut escaped = false;
    for (relative, ch) in source.get(start..)?.char_indices() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == quote {
            return Some(start + relative);
        } else if ch == '\n' {
            return None;
        }
    }
    None
}

pub(in crate::server) fn framework_string_key_domain_before_string(
    source: &str,
    quote_start: usize,
) -> Option<&'static str> {
    let open_paren = previous_non_ws_char(source, quote_start)?;
    if source.as_bytes().get(open_paren).copied()? != b'(' {
        return None;
    }

    let name_end = previous_non_ws_char(source, open_paren)?;
    let name_start = scan_identifier_start(source, name_end + 1);
    let raw_name = source.get(name_start..=name_end)?.trim_start_matches('\\');
    let before_name = source.get(..name_start)?.trim_end();

    match raw_name {
        "config" => Some("config"),
        "route" => Some("route"),
        "view" => Some("view"),
        "render" | "renderView" => Some("twig"),
        "__" | "trans" | "trans_choice" => Some("translation"),
        "name" if before_name.ends_with("->") => Some("route"),
        "get" if before_name.ends_with("Lang::") => Some("translation"),
        "make" if before_name.ends_with("View::") => Some("view"),
        _ => None,
    }
}

pub(in crate::server) fn previous_non_ws_char(source: &str, before: usize) -> Option<usize> {
    source
        .get(..before)?
        .char_indices()
        .rev()
        .find_map(|(idx, ch)| (!ch.is_whitespace()).then_some(idx))
}

pub(in crate::server) fn scan_identifier_start(source: &str, end_exclusive: usize) -> usize {
    let mut start = end_exclusive;
    for (idx, ch) in source
        .get(..end_exclusive)
        .unwrap_or("")
        .char_indices()
        .rev()
    {
        if ch.is_alphanumeric() || ch == '_' || ch == '\\' {
            start = idx;
        } else {
            break;
        }
    }
    start
}

pub(in crate::server) fn phpdoc_extra_markdown_sections(
    phpdoc: &php_lsp_types::PhpDoc,
) -> Vec<String> {
    let mut sections = Vec::new();

    if let Some(ref var_type) = phpdoc.var_type {
        sections.push(format!("**@var** `{}`", var_type));
    }

    if !phpdoc.throws.is_empty() {
        let throws = phpdoc
            .throws
            .iter()
            .map(|throw_type| format!("- `{}`", throw_type))
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(format!("**Throws:**\n\n{}", throws));
    }

    if !phpdoc.properties.is_empty() {
        let properties = phpdoc
            .properties
            .iter()
            .map(|property| {
                let access = phpdoc_property_tag(property.access);
                let type_info = property
                    .type_info
                    .as_ref()
                    .map(|type_info| format!(" {}", type_info))
                    .unwrap_or_default();
                let description = property
                    .description
                    .as_ref()
                    .map(|description| format!(" - {}", description))
                    .unwrap_or_default();
                format!("- `{access}{type_info} ${}`{description}", property.name)
            })
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(format!("**PHPDoc properties:**\n\n{}", properties));
    }

    if !phpdoc.methods.is_empty() {
        let methods = phpdoc
            .methods
            .iter()
            .map(|method| {
                let static_part = if method.is_static { "static " } else { "" };
                let return_type = method
                    .return_type
                    .as_ref()
                    .map(|return_type| format!("{} ", return_type))
                    .unwrap_or_default();
                let description = method
                    .description
                    .as_ref()
                    .map(|description| format!(" - {}", description))
                    .unwrap_or_default();
                let params = format_phpdoc_params(&method.params);
                format!(
                    "- `@method {static_part}{return_type}{}({params})`{description}",
                    method.name
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(format!("**PHPDoc methods:**\n\n{}", methods));
    }

    sections
}

pub(in crate::server) fn phpdoc_extra_markdown_sections_with_links(
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    owner_fqn: &str,
    uri: &str,
    phpdoc: &php_lsp_types::PhpDoc,
) -> Vec<String> {
    let mut sections = Vec::new();

    if let Some(ref var_type) = phpdoc.var_type {
        sections.push(format!(
            "**@var** {}",
            type_info_raw_with_links(index, file_symbols, owner_fqn, uri, var_type)
        ));
    }

    if !phpdoc.throws.is_empty() {
        let throws = phpdoc
            .throws
            .iter()
            .map(|throw_type| {
                format!(
                    "- {}",
                    type_info_raw_with_links(index, file_symbols, owner_fqn, uri, throw_type)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(format!("**Throws:**\n\n{}", throws));
    }

    if !phpdoc.properties.is_empty() {
        let properties = phpdoc
            .properties
            .iter()
            .map(|property| {
                let access = phpdoc_property_tag(property.access);
                let type_info = property
                    .type_info
                    .as_ref()
                    .map(|type_info| format!(" {}", type_info))
                    .unwrap_or_default();
                let description = property
                    .description
                    .as_ref()
                    .map(|description| format!(" - {}", description))
                    .unwrap_or_default();
                let mut line = format!("- `{access}{type_info} ${}`{description}", property.name);
                if let Some(ref type_info) = property.type_info {
                    append_inline_type_links(
                        &mut line,
                        "Type",
                        index,
                        file_symbols,
                        owner_fqn,
                        uri,
                        type_info,
                    );
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(format!("**PHPDoc properties:**\n\n{}", properties));
    }

    if !phpdoc.methods.is_empty() {
        let methods = phpdoc
            .methods
            .iter()
            .map(|method| {
                let static_part = if method.is_static { "static " } else { "" };
                let return_type = method
                    .return_type
                    .as_ref()
                    .map(|return_type| format!("{} ", return_type))
                    .unwrap_or_default();
                let description = method
                    .description
                    .as_ref()
                    .map(|description| format!(" - {}", description))
                    .unwrap_or_default();
                let params = format_phpdoc_params(&method.params);
                let mut line = format!(
                    "- `@method {static_part}{return_type}{}({params})`{description}",
                    method.name
                );
                if let Some(ref return_type) = method.return_type {
                    append_inline_type_links(
                        &mut line,
                        "Returns",
                        index,
                        file_symbols,
                        owner_fqn,
                        uri,
                        return_type,
                    );
                }
                append_inline_param_type_links(
                    &mut line,
                    index,
                    file_symbols,
                    owner_fqn,
                    uri,
                    &method.params,
                );
                line
            })
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(format!("**PHPDoc methods:**\n\n{}", methods));
    }

    sections
}

pub(in crate::server) fn type_info_raw_with_links(
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    owner_fqn: &str,
    uri: &str,
    type_info: &php_lsp_types::TypeInfo,
) -> String {
    let raw = markdown_code_span(&type_info.to_string());
    match markdown_type_info_class_links(index, file_symbols, owner_fqn, uri, type_info) {
        Some(links) => format!("{raw} — {links}"),
        None => raw,
    }
}

pub(in crate::server) fn append_type_link_line(
    content: &mut String,
    label: &str,
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    owner_fqn: &str,
    uri: &str,
    type_info: &php_lsp_types::TypeInfo,
) {
    let Some(links) =
        markdown_type_info_class_links(index, file_symbols, owner_fqn, uri, type_info)
    else {
        return;
    };
    content.push('\n');
    content.push_str("**");
    content.push_str(label);
    content.push_str(":** ");
    content.push_str(&links);
    content.push('\n');
}

pub(in crate::server) fn append_class_fqn_link_line(
    content: &mut String,
    label: &str,
    index: &WorkspaceIndex,
    display: &str,
    target_fqn: &str,
) {
    let Some(symbol) = index.resolve_fqn(target_fqn.trim_start_matches('\\')) else {
        return;
    };
    if !matches!(
        symbol.kind,
        php_lsp_types::PhpSymbolKind::Class
            | php_lsp_types::PhpSymbolKind::Interface
            | php_lsp_types::PhpSymbolKind::Trait
            | php_lsp_types::PhpSymbolKind::Enum
    ) {
        return;
    }
    let destination = markdown_file_location_destination(&symbol);
    content.push('\n');
    content.push_str("**");
    content.push_str(label);
    content.push_str(":** ");
    content.push_str(&format!(
        "[{}](<{}>)",
        markdown_code_span(display),
        destination
    ));
    content.push('\n');
}

pub(in crate::server) fn append_signature_parameter_lines(
    content: &mut String,
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    owner_fqn: &str,
    uri: &str,
    params: &[php_lsp_types::ParamInfo],
    phpdoc_params: &[php_lsp_types::PhpDocParam],
) {
    let mut lines = Vec::new();
    let mut seen_names = Vec::new();

    for param in params {
        let phpdoc_param = phpdoc_params
            .iter()
            .find(|candidate| same_param_name(&candidate.name, &param.name));
        lines.push(signature_parameter_markdown_line(
            index,
            file_symbols,
            owner_fqn,
            uri,
            param,
            phpdoc_param,
        ));
        seen_names.push(normalized_param_name(&param.name));
    }

    for phpdoc_param in phpdoc_params {
        let name = normalized_param_name(&phpdoc_param.name);
        if seen_names.iter().any(|seen| seen == &name) {
            continue;
        }
        lines.push(phpdoc_parameter_markdown_line(
            index,
            file_symbols,
            owner_fqn,
            uri,
            phpdoc_param,
        ));
    }

    if lines.is_empty() {
        return;
    }
    content.push_str("\n**Parameters:**\n\n");
    content.push_str(&lines.join("\n"));
    content.push('\n');
}

fn signature_parameter_markdown_line(
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    owner_fqn: &str,
    uri: &str,
    param: &php_lsp_types::ParamInfo,
    phpdoc_param: Option<&php_lsp_types::PhpDocParam>,
) -> String {
    let mut line = format!("- `{}`: ", format_signature_param(param));
    let type_info = param
        .type_info
        .as_ref()
        .or_else(|| phpdoc_param.and_then(|doc| doc.type_info.as_ref()));
    line.push_str(&parameter_type_markdown(
        index,
        file_symbols,
        owner_fqn,
        uri,
        type_info,
    ));
    if let Some(description) = phpdoc_param.and_then(|doc| doc.description.as_deref()) {
        line.push_str(" — ");
        line.push_str(description);
    }
    line
}

fn phpdoc_parameter_markdown_line(
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    owner_fqn: &str,
    uri: &str,
    param: &php_lsp_types::PhpDocParam,
) -> String {
    let mut line = format!("- `${}`: ", normalized_param_name(&param.name));
    line.push_str(&parameter_type_markdown(
        index,
        file_symbols,
        owner_fqn,
        uri,
        param.type_info.as_ref(),
    ));
    if let Some(description) = param.description.as_deref() {
        line.push_str(" — ");
        line.push_str(description);
    }
    line
}

fn parameter_type_markdown(
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    owner_fqn: &str,
    uri: &str,
    type_info: Option<&php_lsp_types::TypeInfo>,
) -> String {
    let Some(type_info) = type_info else {
        return markdown_code_span("untyped");
    };
    markdown_type_info_class_links(index, file_symbols, owner_fqn, uri, type_info)
        .unwrap_or_else(|| markdown_code_span(&type_info.to_string()))
}

fn same_param_name(left: &str, right: &str) -> bool {
    normalized_param_name(left) == normalized_param_name(right)
}

fn normalized_param_name(name: &str) -> String {
    name.trim_start_matches('$').to_string()
}

fn append_inline_type_links(
    line: &mut String,
    label: &str,
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    owner_fqn: &str,
    uri: &str,
    type_info: &php_lsp_types::TypeInfo,
) {
    let Some(links) =
        markdown_type_info_class_links(index, file_symbols, owner_fqn, uri, type_info)
    else {
        return;
    };
    line.push_str(" — ");
    line.push_str(label);
    line.push_str(": ");
    line.push_str(&links);
}

fn append_inline_param_type_links(
    line: &mut String,
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    owner_fqn: &str,
    uri: &str,
    params: &[php_lsp_types::ParamInfo],
) {
    let parts = params
        .iter()
        .filter_map(|param| {
            let type_info = param.type_info.as_ref()?;
            let links =
                markdown_type_info_class_links(index, file_symbols, owner_fqn, uri, type_info)?;
            Some(format!("`${}`: {}", param.name, links))
        })
        .collect::<Vec<_>>();
    if parts.is_empty() {
        return;
    }
    line.push_str(" — Params: ");
    line.push_str(&parts.join(", "));
}

pub(in crate::server) fn phpdoc_virtual_member_range(
    source: &str,
    doc_comment: &str,
    doc_start: usize,
    member: &PhpDocVirtualMember,
) -> Option<(u32, u32, u32, u32)> {
    let needle = match member.kind {
        PhpDocVirtualMemberKind::Property => format!("${}", member.name),
        PhpDocVirtualMemberKind::Method => format!("{}(", member.name),
    };
    let tag = match member.kind {
        PhpDocVirtualMemberKind::Property => "@property",
        PhpDocVirtualMemberKind::Method => "@method",
    };

    let mut line_offset = 0usize;
    for line in doc_comment.split_inclusive('\n') {
        if line.contains(tag) {
            if let Some(local_start) = line.find(&needle) {
                let name_start = if member.kind == PhpDocVirtualMemberKind::Method {
                    local_start
                } else {
                    local_start + 1
                };
                let name_end = name_start + member.name.len();
                let absolute_start = doc_start + line_offset + name_start;
                let absolute_end = doc_start + line_offset + name_end;
                return Some(byte_offsets_to_range(source, absolute_start, absolute_end));
            }
        }
        line_offset += line.len();
    }

    Some(byte_offsets_to_range(
        source,
        doc_start,
        doc_start + doc_comment.len().min(3),
    ))
}

pub(in crate::server) fn byte_offsets_to_range(
    source: &str,
    start: usize,
    end: usize,
) -> (u32, u32, u32, u32) {
    let (start_line, start_col) = byte_offset_to_line_col(source, start);
    let (end_line, end_col) = byte_offset_to_line_col(source, end);
    (start_line, start_col, end_line, end_col)
}

pub(in crate::server) fn byte_offset_to_line_col(source: &str, byte_offset: usize) -> (u32, u32) {
    let mut line = 0u32;
    let mut line_start = 0usize;

    for (idx, ch) in source.char_indices() {
        if idx >= byte_offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            line_start = idx + ch.len_utf8();
        }
    }

    (line, byte_offset.saturating_sub(line_start) as u32)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::server) enum ShapeCompletionKind {
    ArrayKey,
    ObjectProperty,
    TwigAttribute,
}

pub(in crate::server) fn shape_completion_items_from_type_info(
    type_info: &php_lsp_types::TypeInfo,
    kind: ShapeCompletionKind,
    prefix: &str,
    quote: Option<char>,
) -> Vec<lsp_types::CompletionItem> {
    let mut shape_items = Vec::new();
    let mut seen = HashSet::new();
    collect_shape_completion_items(type_info, kind, &mut seen, &mut shape_items);

    let prefix_lower = prefix.to_ascii_lowercase();
    let mut completion_items = shape_items
        .into_iter()
        .filter(|item| {
            item.key
                .as_deref()
                .is_some_and(|key| key.to_ascii_lowercase().starts_with(&prefix_lower))
        })
        .filter_map(|item| {
            let key = item.key?;
            let detail = match kind {
                ShapeCompletionKind::ArrayKey => {
                    if item.optional {
                        format!("optional array shape key: {}", item.value)
                    } else {
                        format!("array shape key: {}", item.value)
                    }
                }
                ShapeCompletionKind::ObjectProperty => {
                    if item.optional {
                        format!("optional object shape property: {}", item.value)
                    } else {
                        format!("object shape property: {}", item.value)
                    }
                }
                ShapeCompletionKind::TwigAttribute => {
                    if item.optional {
                        format!("optional Twig shape attribute: {}", item.value)
                    } else {
                        format!("Twig shape attribute: {}", item.value)
                    }
                }
            };
            let insert_text = match (kind, quote) {
                (ShapeCompletionKind::ArrayKey, None) => Some(format!("'{key}'")),
                _ => Some(key.clone()),
            };

            Some(lsp_types::CompletionItem {
                label: key.clone(),
                kind: Some(match kind {
                    ShapeCompletionKind::ArrayKey => lsp_types::CompletionItemKind::FIELD,
                    ShapeCompletionKind::ObjectProperty | ShapeCompletionKind::TwigAttribute => {
                        lsp_types::CompletionItemKind::PROPERTY
                    }
                }),
                detail: Some(detail),
                sort_text: Some(format!(
                    "01_{}_{}",
                    completion_prefix_rank_for_text(&key, prefix),
                    key.to_ascii_lowercase()
                )),
                filter_text: Some(key.clone()),
                insert_text,
                commit_characters: Some(match kind {
                    ShapeCompletionKind::ArrayKey => vec!["'".to_string(), "\"".to_string()],
                    ShapeCompletionKind::ObjectProperty | ShapeCompletionKind::TwigAttribute => {
                        vec!["(".to_string(), ";".to_string(), ",".to_string()]
                    }
                }),
                ..Default::default()
            })
        })
        .collect::<Vec<_>>();
    completion_items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text).then(a.label.cmp(&b.label)));
    completion_items
}

pub(in crate::server) fn collect_shape_completion_items(
    type_info: &php_lsp_types::TypeInfo,
    kind: ShapeCompletionKind,
    seen: &mut HashSet<String>,
    out: &mut Vec<php_lsp_types::ArrayShapeItem>,
) {
    match type_info {
        php_lsp_types::TypeInfo::Nullable(inner) => {
            collect_shape_completion_items(inner, kind, seen, out);
        }
        php_lsp_types::TypeInfo::Union(types) | php_lsp_types::TypeInfo::Intersection(types) => {
            for ty in types {
                collect_shape_completion_items(ty, kind, seen, out);
            }
        }
        php_lsp_types::TypeInfo::Conditional {
            if_type, else_type, ..
        } => {
            collect_shape_completion_items(if_type, kind, seen, out);
            collect_shape_completion_items(else_type, kind, seen, out);
        }
        php_lsp_types::TypeInfo::ArrayShape(items)
            if matches!(
                kind,
                ShapeCompletionKind::ArrayKey | ShapeCompletionKind::TwigAttribute
            ) =>
        {
            collect_named_shape_items(items, seen, out);
        }
        php_lsp_types::TypeInfo::ObjectShape(items)
            if matches!(
                kind,
                ShapeCompletionKind::ObjectProperty | ShapeCompletionKind::TwigAttribute
            ) =>
        {
            collect_named_shape_items(items, seen, out);
        }
        _ => {}
    }
}

pub(in crate::server) fn collect_named_shape_items(
    items: &[php_lsp_types::ArrayShapeItem],
    seen: &mut HashSet<String>,
    out: &mut Vec<php_lsp_types::ArrayShapeItem>,
) {
    for item in items {
        let Some(key) = item.key.as_ref() else {
            continue;
        };
        if seen.insert(normalize_shape_key_text(key)) {
            out.push(item.clone());
        }
    }
}

pub(in crate::server) fn completion_prefix_rank_for_text(label: &str, prefix: &str) -> u8 {
    if prefix.is_empty() {
        return 0;
    }
    let label = label.to_ascii_lowercase();
    let prefix = prefix.to_ascii_lowercase();
    if label == prefix {
        0
    } else if label.starts_with(&prefix) {
        1
    } else {
        2
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::server) enum ShapeDefinitionKind {
    Array,
    Object,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::server) struct ShapePathSegment {
    pub(in crate::server) key: String,
    pub(in crate::server) kind: ShapeDefinitionKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::server) struct ShapeDefinitionAccess {
    pub(in crate::server) root_var: String,
    pub(in crate::server) segments: Vec<ShapePathSegment>,
}

pub(in crate::server) fn shape_definition_at_position(
    source: &str,
    line: u32,
    byte_col: u32,
) -> Option<(u32, u32, u32, u32)> {
    let usage_byte = line_col_to_byte_offset(source, line, byte_col)?;
    let access = array_shape_key_access_at_position(source, line, byte_col)
        .or_else(|| object_shape_property_access_at_position(source, line, byte_col))?;
    phpdoc_shape_key_range_before_usage(source, &access, usage_byte)
        .or_else(|| literal_array_shape_key_range_before_usage(source, &access, usage_byte))
        .map(|(start, end)| byte_offsets_to_range(source, start, end))
}

pub(in crate::server) fn line_col_to_byte_offset(
    source: &str,
    line: u32,
    byte_col: u32,
) -> Option<usize> {
    let mut offset = 0usize;
    for (idx, row) in source.split_inclusive('\n').enumerate() {
        let row_without_newline = row.trim_end_matches('\n');
        if idx == line as usize {
            return Some(offset + (byte_col as usize).min(row_without_newline.len()));
        }
        offset += row.len();
    }
    (line as usize == source.lines().count()).then_some(source.len())
}

pub(in crate::server) fn line_bounds_at(source: &str, line: u32) -> Option<(usize, usize)> {
    let mut offset = 0usize;
    for (idx, row) in source.split_inclusive('\n').enumerate() {
        let end = offset + row.trim_end_matches('\n').len();
        if idx == line as usize {
            return Some((offset, end));
        }
        offset += row.len();
    }
    None
}

pub(in crate::server) fn array_shape_key_access_at_position(
    source: &str,
    line: u32,
    byte_col: u32,
) -> Option<ShapeDefinitionAccess> {
    let (line_start, line_end) = line_bounds_at(source, line)?;
    let offset = line_start + byte_col as usize;
    if offset > line_end {
        return None;
    }
    let line_text = &source[line_start..line_end];
    let rel = offset.saturating_sub(line_start);
    let quote_start = line_text[..rel].rfind(['\'', '"'])?;
    let quote = line_text.as_bytes().get(quote_start).copied()? as char;
    let before_quote = &line_text[..quote_start];
    let bracket = before_quote.rfind('[')?;
    if !before_quote[bracket + 1..].trim().is_empty() {
        return None;
    }

    let key_start = quote_start + quote.len_utf8();
    let key_end = line_text[key_start..]
        .find(quote)
        .map(|idx| key_start + idx)
        .unwrap_or(rel);
    if rel < key_start || rel > key_end {
        return None;
    }
    let key = normalize_shape_key_text(&line_text[key_start..key_end]);
    if key.is_empty() {
        return None;
    }

    let array_expr = extract_shape_base_expr(&line_text[..bracket])?;
    let (root_var, mut segments) = shape_array_expr_segments(&array_expr)
        .unwrap_or_else(|| (normalize_shape_root_var(&array_expr), Vec::new()));
    if !root_var.starts_with('$') {
        return None;
    }
    segments.push(ShapePathSegment {
        key,
        kind: ShapeDefinitionKind::Array,
    });
    Some(ShapeDefinitionAccess { root_var, segments })
}

pub(in crate::server) fn object_shape_property_access_at_position(
    source: &str,
    line: u32,
    byte_col: u32,
) -> Option<ShapeDefinitionAccess> {
    let (line_start, line_end) = line_bounds_at(source, line)?;
    let offset = line_start + byte_col as usize;
    if offset > line_end {
        return None;
    }
    let line_text = &source[line_start..line_end];
    let rel = offset.saturating_sub(line_start);
    let (name_start, name_end) = identifier_bounds_at(line_text, rel)?;
    let name = &line_text[name_start..name_end];
    if name.is_empty() {
        return None;
    }

    let before_name = line_text[..name_start].trim_end();
    let (object_text, arrow_len) = if let Some(object_text) = before_name.strip_suffix("?->") {
        (object_text, 3)
    } else if let Some(object_text) = before_name.strip_suffix("->") {
        (object_text, 2)
    } else {
        return None;
    };
    if arrow_len == 0 {
        return None;
    }

    let object_expr = extract_shape_base_expr(object_text)?;
    let (root_var, mut segments) = shape_array_expr_segments(&object_expr)
        .unwrap_or_else(|| (normalize_shape_root_var(&object_expr), Vec::new()));
    if !root_var.starts_with('$') {
        return None;
    }
    segments.push(ShapePathSegment {
        key: name.to_string(),
        kind: ShapeDefinitionKind::Object,
    });
    Some(ShapeDefinitionAccess { root_var, segments })
}

pub(in crate::server) fn identifier_bounds_at(text: &str, offset: usize) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    if offset > bytes.len() {
        return None;
    }
    let mut start = offset.min(bytes.len());
    while start > 0 {
        let ch = bytes[start - 1] as char;
        if ch.is_ascii_alphanumeric() || ch == '_' {
            start -= 1;
        } else {
            break;
        }
    }
    let mut end = offset.min(bytes.len());
    while end < bytes.len() {
        let ch = bytes[end] as char;
        if ch.is_ascii_alphanumeric() || ch == '_' {
            end += 1;
        } else {
            break;
        }
    }
    (start < end).then_some((start, end))
}

pub(in crate::server) fn extract_shape_base_expr(text: &str) -> Option<String> {
    let trimmed = text.trim_end();
    let mut start = trimmed.len();
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    for (idx, ch) in trimmed.char_indices().rev() {
        match ch {
            ')' => {
                paren_depth += 1;
                start = idx;
                continue;
            }
            '(' if paren_depth > 0 => {
                paren_depth -= 1;
                start = idx;
                continue;
            }
            ']' => {
                bracket_depth += 1;
                start = idx;
                continue;
            }
            '[' if bracket_depth > 0 => {
                bracket_depth -= 1;
                start = idx;
                continue;
            }
            _ if paren_depth > 0 || bracket_depth > 0 => {
                start = idx;
                continue;
            }
            _ => {}
        }

        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '$' | '\\' | '-' | '>' | '?') {
            start = idx;
        } else {
            break;
        }
    }

    let expr = trimmed[start..].trim();
    (!expr.is_empty()).then(|| expr.to_string())
}

pub(in crate::server) fn shape_array_expr_segments(
    expr: &str,
) -> Option<(String, Vec<ShapePathSegment>)> {
    let expr = expr.trim();
    let bracket = expr.find('[')?;
    let root_var = normalize_shape_root_var(expr[..bracket].trim());
    if !root_var.starts_with('$') {
        return None;
    }

    let mut segments = Vec::new();
    let mut idx = bracket;
    while idx < expr.len() {
        while idx < expr.len() && expr.as_bytes()[idx].is_ascii_whitespace() {
            idx += 1;
        }
        if idx >= expr.len() || expr.as_bytes()[idx] != b'[' {
            break;
        }
        let close = find_matching_pair(expr, idx, '[', ']').unwrap_or(expr.len());
        let key_text = expr[idx + 1..close].trim();
        let key = normalize_shape_key_text(key_text);
        if !key.is_empty() {
            segments.push(ShapePathSegment {
                key,
                kind: ShapeDefinitionKind::Array,
            });
        }
        if close >= expr.len() {
            break;
        }
        idx = close + 1;
    }

    Some((root_var, segments))
}

pub(in crate::server) fn normalize_shape_root_var(expr: &str) -> String {
    let expr = expr.trim();
    if expr.starts_with('$') {
        expr.to_string()
    } else {
        format!("${expr}")
    }
}

pub(in crate::server) fn phpdoc_shape_key_range_before_usage(
    source: &str,
    access: &ShapeDefinitionAccess,
    usage_byte: usize,
) -> Option<(usize, usize)> {
    let mut search_end = usage_byte.min(source.len());
    while let Some(open) = source[..search_end].rfind("/**") {
        let Some(close_rel) = source[open..].find("*/") else {
            break;
        };
        let close = open + close_rel + 2;
        if close > usage_byte {
            search_end = open;
            continue;
        }
        let comment = &source[open..close];
        if comment.contains("@var") && comment.contains(&access.root_var) {
            if let Some(range) = find_shape_path_range_in_text(comment, open, &access.segments) {
                return Some(range);
            }
        }
        search_end = open;
    }

    None
}

pub(in crate::server) fn literal_array_shape_key_range_before_usage(
    source: &str,
    access: &ShapeDefinitionAccess,
    usage_byte: usize,
) -> Option<(usize, usize)> {
    if access
        .segments
        .iter()
        .any(|segment| segment.kind != ShapeDefinitionKind::Array)
    {
        return None;
    }

    let mut search_end = usage_byte.min(source.len());
    while let Some(var_pos) = source[..search_end].rfind(&access.root_var) {
        if let Some(array_start) = assignment_array_literal_start(source, var_pos, &access.root_var)
        {
            if let Some(range) =
                find_literal_array_path_range(source, array_start, &access.segments)
            {
                return Some(range);
            }
        }
        search_end = var_pos;
    }

    None
}

pub(in crate::server) fn assignment_array_literal_start(
    source: &str,
    var_pos: usize,
    var_name: &str,
) -> Option<usize> {
    let mut idx = var_pos + var_name.len();
    idx = skip_ascii_whitespace(source, idx);
    if source.as_bytes().get(idx).copied()? != b'='
        || source.as_bytes().get(idx + 1).copied() == Some(b'=')
    {
        return None;
    }
    idx = skip_ascii_whitespace(source, idx + 1);
    if source[idx..].starts_with('[') || source[idx..].starts_with("array(") {
        Some(idx)
    } else {
        None
    }
}

pub(in crate::server) fn skip_ascii_whitespace(source: &str, mut idx: usize) -> usize {
    while idx < source.len() && source.as_bytes()[idx].is_ascii_whitespace() {
        idx += 1;
    }
    idx
}

pub(in crate::server) fn find_literal_array_path_range(
    source: &str,
    array_start: usize,
    segments: &[ShapePathSegment],
) -> Option<(usize, usize)> {
    let (body_start, body_end) = literal_array_body_range(source, array_start)?;
    find_literal_array_key_range_in_body(source, body_start, body_end, segments)
}

pub(in crate::server) fn literal_array_body_range(
    source: &str,
    array_start: usize,
) -> Option<(usize, usize)> {
    if source[array_start..].starts_with('[') {
        let close = find_matching_pair(source, array_start, '[', ']')?;
        return Some((array_start + 1, close));
    }
    if source[array_start..].starts_with("array(") {
        let open = array_start + "array".len();
        let close = find_matching_pair(source, open, '(', ')')?;
        return Some((open + 1, close));
    }
    None
}

pub(in crate::server) fn find_literal_array_key_range_in_body(
    source: &str,
    body_start: usize,
    body_end: usize,
    segments: &[ShapePathSegment],
) -> Option<(usize, usize)> {
    let segment = segments.first()?;
    for (item_start, item_end) in split_top_level_ranges(source, body_start, body_end, ',') {
        let arrow = find_top_level_needle(source, item_start, item_end, "=>")?;
        let (key, key_start, key_end) = shape_key_from_raw_range(source, item_start, arrow)?;
        if key != segment.key {
            continue;
        }
        if segments.len() == 1 {
            return Some((key_start, key_end));
        }
        let value_start = skip_ascii_whitespace(source, arrow + 2);
        if let Some(range) = find_literal_array_path_range(source, value_start, &segments[1..]) {
            return Some(range);
        }
    }

    None
}

pub(in crate::server) fn find_shape_path_range_in_text(
    text: &str,
    text_abs_start: usize,
    segments: &[ShapePathSegment],
) -> Option<(usize, usize)> {
    let segment = segments.first()?;
    let prefix = match segment.kind {
        ShapeDefinitionKind::Array => "array{",
        ShapeDefinitionKind::Object => "object{",
    };
    let mut search_start = 0usize;
    while let Some(prefix_rel) = text[search_start..].find(prefix) {
        let shape_start = search_start + prefix_rel;
        let open = shape_start + prefix.len() - 1;
        let Some(close) = find_matching_pair(text, open, '{', '}') else {
            search_start = shape_start + prefix.len();
            continue;
        };
        if let Some(range) =
            find_shape_key_range_in_body(text, text_abs_start, open + 1, close, segments)
        {
            return Some(range);
        }
        search_start = close + 1;
    }

    None
}

pub(in crate::server) fn find_shape_key_range_in_body(
    text: &str,
    text_abs_start: usize,
    body_start: usize,
    body_end: usize,
    segments: &[ShapePathSegment],
) -> Option<(usize, usize)> {
    let segment = segments.first()?;
    for (item_start, item_end) in split_top_level_ranges(text, body_start, body_end, ',') {
        let Some(colon) = find_top_level_char_in_range(text, item_start, item_end, ':') else {
            continue;
        };
        let (key, key_start, key_end) = shape_key_from_raw_range(text, item_start, colon)?;
        if key != segment.key {
            continue;
        }
        if segments.len() == 1 {
            return Some((text_abs_start + key_start, text_abs_start + key_end));
        }
        if let Some(range) = find_shape_path_range_in_text(
            &text[colon + 1..item_end],
            text_abs_start + colon + 1,
            &segments[1..],
        ) {
            return Some(range);
        }
    }

    None
}

pub(in crate::server) fn shape_key_from_raw_range(
    text: &str,
    start: usize,
    end: usize,
) -> Option<(String, usize, usize)> {
    let mut key_start = start;
    let mut key_end = end;
    while key_start < key_end && text.as_bytes()[key_start].is_ascii_whitespace() {
        key_start += 1;
    }
    while key_end > key_start && text.as_bytes()[key_end - 1].is_ascii_whitespace() {
        key_end -= 1;
    }
    if key_end > key_start && text.as_bytes()[key_end - 1] == b'?' {
        key_end -= 1;
        while key_end > key_start && text.as_bytes()[key_end - 1].is_ascii_whitespace() {
            key_end -= 1;
        }
    }
    if key_end > key_start + 1 {
        let first = text.as_bytes()[key_start];
        let last = text.as_bytes()[key_end - 1];
        if (first == b'\'' && last == b'\'') || (first == b'"' && last == b'"') {
            key_start += 1;
            key_end -= 1;
        }
    }
    let key = normalize_shape_key_text(&text[key_start..key_end]);
    (!key.is_empty()).then_some((key, key_start, key_end))
}

pub(in crate::server) fn split_top_level_ranges(
    text: &str,
    start: usize,
    end: usize,
    delimiter: char,
) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut item_start = start;
    let mut paren_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for (rel, ch) in text[start..end].char_indices() {
        let idx = start + rel;
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

        if ch == delimiter
            && paren_depth == 0
            && angle_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
        {
            ranges.push((item_start, idx));
            item_start = idx + ch.len_utf8();
        }
    }
    ranges.push((item_start, end));
    ranges
}

pub(in crate::server) fn find_top_level_char_in_range(
    text: &str,
    start: usize,
    end: usize,
    needle: char,
) -> Option<usize> {
    let mut paren_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (rel, ch) in text[start..end].char_indices() {
        let idx = start + rel;
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
        if ch == needle
            && paren_depth == 0
            && angle_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
        {
            return Some(idx);
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

pub(in crate::server) fn find_top_level_needle(
    text: &str,
    start: usize,
    end: usize,
    needle: &str,
) -> Option<usize> {
    let mut paren_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (rel, ch) in text[start..end].char_indices() {
        let idx = start + rel;
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
        if paren_depth == 0
            && angle_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && text[idx..end].starts_with(needle)
        {
            return Some(idx);
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

pub(in crate::server) fn find_matching_pair(
    text: &str,
    open: usize,
    open_ch: char,
    close_ch: char,
) -> Option<usize> {
    if !text[open..].starts_with(open_ch) {
        return None;
    }
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut depth = 0usize;
    for (rel, ch) in text[open..].char_indices() {
        let idx = open + rel;
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
        if ch == open_ch {
            depth += 1;
        } else if ch == close_ch {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some(idx);
            }
        }
    }
    None
}

pub(in crate::server) fn add_local_variable_completion_items(
    items: &mut Vec<lsp_types::CompletionItem>,
    tree: &tree_sitter::Tree,
    source: &str,
    line: u32,
    byte_col: u32,
    prefix: &str,
) {
    let prefix_lower = prefix.to_ascii_lowercase();
    let mut seen: HashSet<String> = items.iter().map(|item| item.label.clone()).collect();

    for var_name in local_variable_names_at_position(tree, source, line, byte_col) {
        let name_without_dollar = var_name.trim_start_matches('$');
        if !name_without_dollar
            .to_ascii_lowercase()
            .starts_with(&prefix_lower)
        {
            continue;
        }
        if !seen.insert(var_name.clone()) {
            continue;
        }

        items.push(lsp_types::CompletionItem {
            label: var_name.clone(),
            kind: Some(lsp_types::CompletionItemKind::VARIABLE),
            sort_text: Some(format!("0102_{}", name_without_dollar.to_ascii_lowercase())),
            filter_text: Some(format!("{} {}", var_name, name_without_dollar)),
            ..Default::default()
        });
    }
}

pub(in crate::server) fn infer_new_expression_type(
    expr: &str,
    file_symbols: &php_lsp_types::FileSymbols,
) -> Option<String> {
    let expr = trim_balanced_outer_parens(expr.trim());
    let rest = expr.strip_prefix("new")?;
    if !rest.chars().next().is_some_and(char::is_whitespace) {
        return None;
    }

    let rest = rest.trim_start();
    let end = rest
        .char_indices()
        .find_map(|(idx, ch)| (!ch.is_alphanumeric() && ch != '_' && ch != '\\').then_some(idx))
        .unwrap_or(rest.len());
    let class_name = rest[..end].trim();
    if class_name.is_empty() || class_name == "class" {
        return None;
    }

    Some(
        resolve_class_name_pub(class_name, file_symbols)
            .trim_start_matches('\\')
            .to_string(),
    )
}

pub(in crate::server) fn infer_static_call_expression_type<F>(
    expr: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    source: &str,
    context_node: tree_sitter::Node<'_>,
    mut resolver: F,
) -> Option<String>
where
    F: FnMut(&str, &str) -> Option<String>,
{
    let expr = trim_balanced_outer_parens(expr.trim());
    let (class_expr, after_scope) = expr.split_once("::")?;
    let class_name = class_expr.trim();
    if class_name.is_empty() {
        return None;
    }

    let method_name_end = after_scope
        .char_indices()
        .find_map(|(idx, ch)| (!ch.is_alphanumeric() && ch != '_').then_some(idx))
        .unwrap_or(after_scope.len());
    let method_name = after_scope[..method_name_end].trim();
    if method_name.is_empty() || after_scope[method_name_end..].trim_start().chars().next()? != '('
    {
        return None;
    }

    let class_fqn = php_lsp_parser::resolve::resolve_scope_class_name_pub(
        class_name,
        context_node,
        source,
        file_symbols,
    )
    .trim_start_matches('\\')
    .to_string();
    if class_fqn.is_empty() || matches!(class_fqn.as_str(), "self" | "static" | "parent") {
        return None;
    }
    resolver(&class_fqn, method_name)
}

pub(in crate::server) fn trim_balanced_outer_parens(mut text: &str) -> &str {
    loop {
        let trimmed = text.trim();
        if !trimmed.starts_with('(') || !trimmed.ends_with(')') {
            return trimmed;
        }

        let mut depth = 0usize;
        let mut encloses_whole_expr = false;
        for (idx, ch) in trimmed.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        encloses_whole_expr = idx + ch.len_utf8() == trimmed.len();
                        break;
                    }
                }
                _ => {}
            }
        }

        if !encloses_whole_expr {
            return trimmed;
        }
        text = &trimmed[1..trimmed.len() - 1];
    }
}

pub(in crate::server) fn resolve_member_type_from_index(
    index: &WorkspaceIndex,
    class_fqn: &str,
    member_name: &str,
) -> Option<String> {
    if class_fqn.is_empty() {
        return resolve_function_return_type_from_index(index, member_name);
    }

    let member_fqn = format!("{}::{}", class_fqn, member_name);
    tracing::debug!("resolve_member_type: looking up {}", member_fqn);

    let sym = match index.resolve_fqn(&member_fqn) {
        Some(s) => s,
        None => {
            tracing::debug!("resolve_member_type: {} not found in index", member_fqn);
            return None;
        }
    };

    symbol_return_type_fqn(index, class_fqn, &sym)
        .or_else(|| symbol_effective_return_type(&sym).map(|type_info| type_info.to_string()))
}

pub(in crate::server) fn twig_property_accessor_method_for_alias(
    index: &WorkspaceIndex,
    class_fqn: &str,
    property_name: &str,
) -> Option<Arc<php_lsp_types::SymbolInfo>> {
    let property_name = property_name.trim_start_matches('$');
    if property_name.is_empty() {
        return None;
    }

    let mut chars = property_name.chars();
    let first = chars.next()?;
    let mut suffix = String::new();
    suffix.push(first.to_ascii_uppercase());
    suffix.push_str(chars.as_str());

    ["get", "is", "has"].into_iter().find_map(|prefix| {
        let method_fqn = format!("{class_fqn}::{prefix}{suffix}");
        let symbol = index.resolve_fqn(&method_fqn)?;
        twig_method_symbol_can_be_property_accessor(&symbol).then_some(symbol)
    })
}

pub(in crate::server) fn twig_property_accessor_method_for_symbol(
    index: &WorkspaceIndex,
    sym_at_pos: &SymbolAtPosition,
) -> Option<Arc<php_lsp_types::SymbolInfo>> {
    if sym_at_pos.ref_kind != RefKind::PropertyAccess {
        return None;
    }
    let (class_fqn, property_name) = sym_at_pos.fqn.rsplit_once("::$")?;
    twig_property_accessor_method_for_alias(index, class_fqn, property_name)
}

pub(in crate::server) fn twig_method_symbol_can_be_property_accessor(
    symbol: &php_lsp_types::SymbolInfo,
) -> bool {
    if symbol.kind != php_lsp_types::PhpSymbolKind::Method {
        return false;
    }

    let Some(signature) = &symbol.signature else {
        return false;
    };
    signature
        .params
        .iter()
        .all(|param| param.default_value.is_some() || param.is_variadic)
}

pub(in crate::server) fn resolve_function_return_type_from_index(
    index: &WorkspaceIndex,
    function_fqn: &str,
) -> Option<String> {
    let sym = index.resolve_fqn(function_fqn).or_else(|| {
        function_fqn
            .rsplit_once('\\')
            .and_then(|(_, short_name)| index.resolve_fqn(short_name))
    })?;
    if sym.kind != php_lsp_types::PhpSymbolKind::Function {
        return None;
    }
    symbol_return_type_fqn(index, "", &sym)
}

pub(in crate::server) fn symbol_return_type_fqn(
    index: &WorkspaceIndex,
    owner_fqn: &str,
    sym: &php_lsp_types::SymbolInfo,
) -> Option<String> {
    let ret = symbol_effective_return_type(sym)?;
    tracing::debug!("resolve_member_type: {} -> return type '{}'", sym.fqn, ret);

    type_info_fqn_from_index(index, owner_fqn, &sym.uri, &ret)
}

pub(in crate::server) fn type_info_fqn_from_index(
    index: &WorkspaceIndex,
    owner_fqn: &str,
    uri: &str,
    type_info: &php_lsp_types::TypeInfo,
) -> Option<String> {
    match type_info {
        php_lsp_types::TypeInfo::Simple(name) => {
            if owner_fqn.is_empty() {
                let raw = name.trim().trim_start_matches('\\');
                if !raw.is_empty() && index.resolve_fqn(raw).is_some() {
                    return Some(raw.to_string());
                }
            }
            simple_type_fqn_from_owner_or_index(index, owner_fqn, uri, name)
        }
        php_lsp_types::TypeInfo::Nullable(inner) => {
            type_info_fqn_from_index(index, owner_fqn, uri, inner)
        }
        php_lsp_types::TypeInfo::Self_ | php_lsp_types::TypeInfo::Static_ => {
            Some(owner_fqn.to_string())
        }
        php_lsp_types::TypeInfo::Generic { base, .. } if !is_builtin_type_name(base) => {
            simple_type_fqn_from_owner_or_index(index, owner_fqn, uri, base)
        }
        php_lsp_types::TypeInfo::Union(types) | php_lsp_types::TypeInfo::Intersection(types) => {
            types
                .iter()
                .find_map(|type_info| type_info_fqn_from_index(index, owner_fqn, uri, type_info))
        }
        php_lsp_types::TypeInfo::ClassString(Some(inner)) => {
            type_info_fqn_from_index(index, owner_fqn, uri, inner)
        }
        _ => None,
    }
}

pub(in crate::server) fn simple_type_fqn_from_index(
    index: &WorkspaceIndex,
    uri: &str,
    type_name: &str,
) -> Option<String> {
    let type_name = type_name.trim();
    if type_name.is_empty() || type_name == "mixed" || is_builtin_type_name(type_name) {
        return None;
    }
    if type_name.contains(['|', '&', '<', '>', '{', '}', '(', ')', ',', ' ']) {
        return None;
    }
    if type_name.contains('\\') {
        return Some(type_name.trim_start_matches('\\').to_string());
    }

    if let Some(file_syms) = index.file_symbols.get(uri) {
        Some(php_lsp_parser::resolve::resolve_class_name(
            type_name, &file_syms,
        ))
    } else {
        Some(type_name.to_string())
    }
}

pub(in crate::server) fn format_signature_param(param: &php_lsp_types::ParamInfo) -> String {
    let mut label = String::new();
    if let Some(ref type_info) = param.type_info {
        label.push_str(&type_info.to_string());
        label.push(' ');
    }
    if param.is_variadic {
        label.push_str("...");
    }
    if param.is_by_ref {
        label.push('&');
    }
    if param.name.starts_with('$') {
        label.push_str(&param.name);
    } else {
        label.push('$');
        label.push_str(&param.name);
    }
    if let Some(ref default) = param.default_value {
        label.push_str(" = ");
        label.push_str(default);
    }
    label
}

pub(in crate::server) fn build_signature_help(
    sym: &php_lsp_types::SymbolInfo,
    active_parameter: usize,
) -> Option<SignatureHelp> {
    let sig = sym.signature.as_ref()?;
    let param_labels: Vec<String> = sig.params.iter().map(format_signature_param).collect();

    let mut label = String::new();
    label.push_str(&sym.fqn);
    label.push('(');
    label.push_str(&param_labels.join(", "));
    label.push(')');
    if let Some(ref ret) = sig.return_type {
        label.push_str(": ");
        label.push_str(&ret.to_string());
    }

    let phpdoc = sym.doc_comment.as_ref().map(|doc| parse_phpdoc(doc));
    let documentation = phpdoc.as_ref().and_then(|doc| {
        let mut parts = Vec::new();
        if let Some(ref summary) = doc.summary {
            parts.push(summary.clone());
        }
        if let Some(ref ret) = doc.return_type {
            parts.push(format!("@return `{}`", ret));
        }
        if parts.is_empty() {
            None
        } else {
            Some(Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: parts.join("\n\n"),
            }))
        }
    });

    let parameters: Vec<ParameterInformation> = sig
        .params
        .iter()
        .zip(param_labels.iter())
        .map(|(param, label)| {
            let documentation = phpdoc.as_ref().and_then(|doc| {
                doc.params
                    .iter()
                    .find(|p| p.name == param.name)
                    .and_then(|p| {
                        let mut parts = Vec::new();
                        if let Some(ref type_info) = p.type_info {
                            parts.push(format!("`{}`", type_info));
                        }
                        if let Some(ref desc) = p.description {
                            parts.push(desc.clone());
                        }
                        if parts.is_empty() {
                            None
                        } else {
                            Some(Documentation::MarkupContent(MarkupContent {
                                kind: MarkupKind::Markdown,
                                value: parts.join(" — "),
                            }))
                        }
                    })
            });

            ParameterInformation {
                label: ParameterLabel::Simple(label.clone()),
                documentation,
            }
        })
        .collect();

    let active_parameter = if sig.params.is_empty() {
        None
    } else {
        Some(active_parameter.min(sig.params.len() - 1) as u32)
    };

    Some(SignatureHelp {
        signatures: vec![SignatureInformation {
            label,
            documentation,
            parameters: Some(parameters),
            active_parameter,
        }],
        active_signature: Some(0),
        active_parameter,
    })
}

pub(in crate::server) fn import_kind_for_completion_symbol(
    sym: &php_lsp_types::SymbolInfo,
) -> Option<ImportKind> {
    match sym.kind {
        php_lsp_types::PhpSymbolKind::Class
        | php_lsp_types::PhpSymbolKind::Interface
        | php_lsp_types::PhpSymbolKind::Trait
        | php_lsp_types::PhpSymbolKind::Enum => Some(ImportKind::Class),
        php_lsp_types::PhpSymbolKind::Function => Some(ImportKind::Function),
        php_lsp_types::PhpSymbolKind::GlobalConstant => Some(ImportKind::Constant),
        _ => None,
    }
}

pub(in crate::server) fn symbol_is_in_current_namespace(
    file_symbols: &php_lsp_types::FileSymbols,
    fqn: &str,
) -> bool {
    let Some(namespace) = file_symbols.namespace.as_deref() else {
        return false;
    };
    fqn.rsplit_once('\\')
        .map(|(symbol_namespace, _)| symbol_namespace == namespace)
        .unwrap_or(false)
}

pub(in crate::server) fn build_completion_auto_import_edit(
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    sym: &php_lsp_types::SymbolInfo,
) -> Option<TextEdit> {
    if sym.modifiers.is_builtin || !sym.fqn.contains('\\') {
        return None;
    }
    if symbol_is_in_current_namespace(file_symbols, &sym.fqn) {
        return None;
    }

    let import_kind = import_kind_for_completion_symbol(sym)?;
    if existing_import_for_fqn(file_symbols, &sym.fqn, import_kind).is_some() {
        return None;
    }

    let import_short_name = short_name(&sym.fqn);
    let used_aliases = used_import_aliases(file_symbols, import_kind);
    if used_aliases.contains(import_short_name) {
        return None;
    }

    let insert_line = find_use_insert_line(source, file_symbols);
    let needs_spacing =
        file_symbols.use_statements.is_empty() && !line_is_blank(source, insert_line);
    let mut new_text = build_use_statement(&sym.fqn, import_kind, None);
    new_text.push('\n');
    if needs_spacing {
        new_text.push('\n');
    }

    Some(TextEdit {
        range: Range {
            start: Position::new(insert_line, 0),
            end: Position::new(insert_line, 0),
        },
        new_text,
    })
}
