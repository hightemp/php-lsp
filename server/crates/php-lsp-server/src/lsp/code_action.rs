//! Code Action LSP handlers extracted from `server.rs`.

use super::super::*;
use super::document_links::is_static_string_literal_node;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ImportKind {
    Class,
    Function,
    Constant,
}

pub(crate) fn code_action_kind_allowed(
    only: Option<&Vec<CodeActionKind>>,
    kind: &CodeActionKind,
) -> bool {
    only.map(|kinds| {
        kinds.is_empty()
            || kinds.iter().any(|requested| {
                requested == kind
                    || kind
                        .as_str()
                        .strip_prefix(requested.as_str())
                        .is_some_and(|suffix| suffix.starts_with('.'))
            })
    })
    .unwrap_or(true)
}

pub(crate) fn unknown_symbol_from_diagnostic(message: &str) -> Option<(ImportKind, String)> {
    if let Some(fqn) = message.strip_prefix("Unknown class: ") {
        return Some((ImportKind::Class, fqn.to_string()));
    }
    if let Some(fqn) = message.strip_prefix("Unknown function: ") {
        return Some((ImportKind::Function, fqn.to_string()));
    }
    None
}

pub(crate) fn short_name(fqn: &str) -> &str {
    fqn.trim_start_matches('\\')
        .rsplit('\\')
        .next()
        .unwrap_or(fqn)
}

pub(crate) fn use_kind_for_ref_kind(ref_kind: RefKind) -> Option<php_lsp_types::UseKind> {
    match ref_kind {
        RefKind::ClassName | RefKind::Constructor => Some(php_lsp_types::UseKind::Class),
        RefKind::FunctionCall => Some(php_lsp_types::UseKind::Function),
        RefKind::GlobalConstant => Some(php_lsp_types::UseKind::Constant),
        _ => None,
    }
}

pub(crate) fn import_target_fqn(sym: &SymbolAtPosition) -> &str {
    if sym.ref_kind == RefKind::Constructor {
        sym.fqn
            .strip_suffix("::__construct")
            .unwrap_or(sym.fqn.as_str())
    } else {
        sym.fqn.as_str()
    }
}

pub(crate) fn imported_use_statement_for_symbol<'a>(
    file_symbols: &'a php_lsp_types::FileSymbols,
    sym: &SymbolAtPosition,
) -> Option<&'a php_lsp_types::UseStatement> {
    let use_kind = use_kind_for_ref_kind(sym.ref_kind)?;
    let target_fqn = import_target_fqn(sym).trim_start_matches('\\');

    file_symbols.use_statements.iter().find(|use_stmt| {
        use_stmt.kind == use_kind && use_stmt.fqn.trim_start_matches('\\') == target_fqn
    })
}

pub(crate) fn is_builtin_type_name(name: &str) -> bool {
    matches!(
        name.trim_start_matches('\\').to_ascii_lowercase().as_str(),
        "int"
            | "float"
            | "string"
            | "bool"
            | "boolean"
            | "array"
            | "object"
            | "null"
            | "void"
            | "never"
            | "mixed"
            | "callable"
            | "iterable"
            | "true"
            | "false"
            | "resource"
    )
}

pub(crate) fn first_type_definition_fqn(
    type_info: &php_lsp_types::TypeInfo,
    file_symbols: &php_lsp_types::FileSymbols,
    current_class_fqn: Option<&str>,
) -> Option<String> {
    match type_info {
        php_lsp_types::TypeInfo::Simple(name) => {
            if is_builtin_type_name(name) {
                None
            } else {
                Some(resolve_class_name_pub(name, file_symbols))
            }
        }
        php_lsp_types::TypeInfo::Nullable(inner) => {
            first_type_definition_fqn(inner, file_symbols, current_class_fqn)
        }
        php_lsp_types::TypeInfo::Union(types) | php_lsp_types::TypeInfo::Intersection(types) => {
            types
                .iter()
                .find_map(|ty| first_type_definition_fqn(ty, file_symbols, current_class_fqn))
        }
        php_lsp_types::TypeInfo::Generic { base, args } => {
            if !is_builtin_type_name(base) {
                Some(resolve_class_name_pub(base, file_symbols))
            } else {
                args.iter()
                    .find_map(|ty| first_type_definition_fqn(ty, file_symbols, current_class_fqn))
            }
        }
        php_lsp_types::TypeInfo::ClassString(Some(inner)) => {
            first_type_definition_fqn(inner, file_symbols, current_class_fqn)
        }
        php_lsp_types::TypeInfo::Conditional {
            if_type, else_type, ..
        } => first_type_definition_fqn(if_type, file_symbols, current_class_fqn)
            .or_else(|| first_type_definition_fqn(else_type, file_symbols, current_class_fqn)),
        php_lsp_types::TypeInfo::ArrayShape(items)
        | php_lsp_types::TypeInfo::ObjectShape(items) => items.iter().find_map(|item| {
            first_type_definition_fqn(&item.value, file_symbols, current_class_fqn)
        }),
        php_lsp_types::TypeInfo::Callable {
            params,
            return_type,
        } => return_type
            .as_deref()
            .and_then(|ty| first_type_definition_fqn(ty, file_symbols, current_class_fqn))
            .or_else(|| {
                params
                    .iter()
                    .find_map(|ty| first_type_definition_fqn(ty, file_symbols, current_class_fqn))
            }),
        php_lsp_types::TypeInfo::Self_ | php_lsp_types::TypeInfo::Static_ => {
            current_class_fqn.map(str::to_string)
        }
        php_lsp_types::TypeInfo::Parent_ => current_class_fqn.and_then(|class_fqn| {
            file_symbols
                .symbols
                .iter()
                .find(|sym| sym.fqn == class_fqn)
                .and_then(|sym| sym.extends.first().cloned())
        }),
        php_lsp_types::TypeInfo::Void
        | php_lsp_types::TypeInfo::Never
        | php_lsp_types::TypeInfo::Mixed
        | php_lsp_types::TypeInfo::ClassString(None)
        | php_lsp_types::TypeInfo::LiteralString(_)
        | php_lsp_types::TypeInfo::LiteralInt(_)
        | php_lsp_types::TypeInfo::LiteralFloat(_)
        | php_lsp_types::TypeInfo::LiteralBool(_)
        | php_lsp_types::TypeInfo::LiteralNull => None,
    }
}

pub(crate) fn use_kind_matches(import_kind: ImportKind, use_kind: php_lsp_types::UseKind) -> bool {
    matches!(
        (import_kind, use_kind),
        (ImportKind::Class, php_lsp_types::UseKind::Class)
            | (ImportKind::Function, php_lsp_types::UseKind::Function)
            | (ImportKind::Constant, php_lsp_types::UseKind::Constant)
    )
}

pub(crate) fn import_kind_from_use_kind(use_kind: php_lsp_types::UseKind) -> ImportKind {
    match use_kind {
        php_lsp_types::UseKind::Class => ImportKind::Class,
        php_lsp_types::UseKind::Function => ImportKind::Function,
        php_lsp_types::UseKind::Constant => ImportKind::Constant,
    }
}

pub(crate) fn existing_use_alias(use_stmt: &php_lsp_types::UseStatement) -> String {
    use_stmt
        .alias
        .clone()
        .unwrap_or_else(|| short_name(&use_stmt.fqn).to_string())
}

pub(crate) fn used_import_aliases(
    file_symbols: &php_lsp_types::FileSymbols,
    import_kind: ImportKind,
) -> std::collections::HashSet<String> {
    let mut aliases = std::collections::HashSet::new();
    for use_stmt in &file_symbols.use_statements {
        if use_kind_matches(import_kind, use_stmt.kind) {
            aliases.insert(existing_use_alias(use_stmt));
        }
    }
    if import_kind == ImportKind::Class {
        for sym in &file_symbols.symbols {
            if matches!(
                sym.kind,
                php_lsp_types::PhpSymbolKind::Class
                    | php_lsp_types::PhpSymbolKind::Interface
                    | php_lsp_types::PhpSymbolKind::Trait
                    | php_lsp_types::PhpSymbolKind::Enum
            ) {
                aliases.insert(sym.name.clone());
            }
        }
    }
    aliases
}

pub(crate) fn unique_import_alias(base: &str, used: &std::collections::HashSet<String>) -> String {
    let mut candidate = format!("{}Import", base);
    let mut suffix = 2usize;
    while used.contains(&candidate) {
        candidate = format!("{}Import{}", base, suffix);
        suffix += 1;
    }
    candidate
}

pub(crate) fn existing_import_for_fqn<'a>(
    file_symbols: &'a php_lsp_types::FileSymbols,
    fqn: &str,
    import_kind: ImportKind,
) -> Option<&'a php_lsp_types::UseStatement> {
    file_symbols
        .use_statements
        .iter()
        .find(|use_stmt| use_kind_matches(import_kind, use_stmt.kind) && use_stmt.fqn == fqn)
}

pub(crate) fn line_is_blank(source: &str, line: u32) -> bool {
    source
        .lines()
        .nth(line as usize)
        .map(|line| line.trim().is_empty())
        .unwrap_or(false)
}

pub(crate) fn find_use_insert_line(source: &str, file_symbols: &php_lsp_types::FileSymbols) -> u32 {
    if let Some(last_use_line) = file_symbols
        .use_statements
        .iter()
        .map(|use_stmt| use_stmt.range.2)
        .max()
    {
        return last_use_line + 1;
    }

    for (idx, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("namespace ") && (trimmed.contains(';') || trimmed.contains('{')) {
            return idx as u32 + 1;
        }
    }

    if source
        .lines()
        .next()
        .is_some_and(|line| line.trim() == "<?php")
    {
        1
    } else {
        0
    }
}

pub(crate) fn build_use_statement(
    import_fqn: &str,
    import_kind: ImportKind,
    alias: Option<&str>,
) -> String {
    let import_fqn = import_fqn.trim_start_matches('\\');
    let prefix = match import_kind {
        ImportKind::Class => "use",
        ImportKind::Function => "use function",
        ImportKind::Constant => "use const",
    };
    match alias {
        Some(alias) => format!("{} {} as {};", prefix, import_fqn, alias),
        None => format!("{} {};", prefix, import_fqn),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct OrganizableImport {
    fqn: String,
    alias: Option<String>,
    kind: ImportKind,
}

pub(crate) fn import_kind_sort_key(kind: ImportKind) -> u8 {
    match kind {
        ImportKind::Class => 0,
        ImportKind::Function => 1,
        ImportKind::Constant => 2,
    }
}

pub(crate) fn source_line(source: &str, line: u32) -> Option<&str> {
    source.lines().nth(line as usize)
}

pub(crate) fn is_simple_use_statement_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with("use ")
        && trimmed.ends_with(';')
        && !trimmed.contains('{')
        && !trimmed.contains('}')
}

pub(crate) fn find_organizable_use_block(
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
) -> Option<(u32, u32)> {
    let start_line = file_symbols
        .use_statements
        .iter()
        .map(|use_stmt| use_stmt.range.0)
        .min()?;
    let end_line = file_symbols
        .use_statements
        .iter()
        .map(|use_stmt| use_stmt.range.2)
        .max()?
        + 1;

    for use_stmt in &file_symbols.use_statements {
        if use_stmt.range.0 != use_stmt.range.2 {
            return None;
        }
        let line = source_line(source, use_stmt.range.0)?;
        if !is_simple_use_statement_line(line) {
            return None;
        }
    }

    for line_idx in start_line..end_line {
        let line = source_line(source, line_idx)?;
        let trimmed = line.trim();
        if !trimmed.is_empty() && !is_simple_use_statement_line(line) {
            return None;
        }
    }

    Some((start_line, end_line))
}

pub(crate) fn import_alias(import: &OrganizableImport) -> &str {
    import
        .alias
        .as_deref()
        .unwrap_or_else(|| short_name(&import.fqn))
}

pub(crate) fn import_is_used(
    import: &OrganizableImport,
    file_symbols: &php_lsp_types::FileSymbols,
    references: &[php_lsp_types::SymbolReference],
) -> bool {
    import_is_used_by_references(import, references)
        || import_is_used_by_phpdoc_types(import, file_symbols)
}

fn import_is_used_by_references(
    import: &OrganizableImport,
    references: &[php_lsp_types::SymbolReference],
) -> bool {
    references
        .iter()
        .any(|reference| reference_matches_import(reference, import))
}

fn reference_matches_import(
    reference: &php_lsp_types::SymbolReference,
    import: &OrganizableImport,
) -> bool {
    let target_fqn = reference.target_fqn.trim_start_matches('\\');
    let import_fqn = import.fqn.trim_start_matches('\\');

    match import.kind {
        ImportKind::Class => {
            matches!(
                reference.target_kind,
                php_lsp_types::PhpSymbolKind::Class
                    | php_lsp_types::PhpSymbolKind::Interface
                    | php_lsp_types::PhpSymbolKind::Trait
                    | php_lsp_types::PhpSymbolKind::Enum
            ) && fqn_matches_import_or_prefix(target_fqn, import_fqn)
        }
        ImportKind::Function => {
            reference.target_kind == php_lsp_types::PhpSymbolKind::Function
                && target_fqn == import_fqn
        }
        ImportKind::Constant => {
            reference.target_kind == php_lsp_types::PhpSymbolKind::GlobalConstant
                && target_fqn == import_fqn
        }
    }
}

fn fqn_matches_import_or_prefix(target_fqn: &str, import_fqn: &str) -> bool {
    target_fqn == import_fqn
        || target_fqn
            .strip_prefix(import_fqn)
            .is_some_and(|rest| rest.starts_with('\\'))
}

fn import_is_used_by_phpdoc_types(
    import: &OrganizableImport,
    file_symbols: &php_lsp_types::FileSymbols,
) -> bool {
    if import.kind != ImportKind::Class {
        return false;
    }

    file_symbols
        .symbols
        .iter()
        .filter_map(|symbol| symbol.doc_comment.as_deref())
        .map(parse_phpdoc)
        .any(|phpdoc| phpdoc_uses_import(&phpdoc, import))
        || file_symbols
            .type_aliases
            .iter()
            .any(|alias| type_info_uses_import(&alias.type_info, import))
        || file_symbols
            .type_alias_imports
            .iter()
            .any(|alias_import| phpdoc_name_uses_import(&alias_import.source_type, import))
}

fn phpdoc_uses_import(phpdoc: &php_lsp_types::PhpDoc, import: &OrganizableImport) -> bool {
    phpdoc
        .params
        .iter()
        .filter_map(|param| param.type_info.as_ref())
        .any(|type_info| type_info_uses_import(type_info, import))
        || phpdoc
            .return_type
            .as_ref()
            .is_some_and(|type_info| type_info_uses_import(type_info, import))
        || phpdoc
            .var_type
            .as_ref()
            .is_some_and(|type_info| type_info_uses_import(type_info, import))
        || phpdoc
            .throws
            .iter()
            .any(|type_info| type_info_uses_import(type_info, import))
        || phpdoc.properties.iter().any(|property| {
            property
                .type_info
                .as_ref()
                .is_some_and(|type_info| type_info_uses_import(type_info, import))
        })
        || phpdoc.methods.iter().any(|method| {
            method
                .return_type
                .as_ref()
                .is_some_and(|type_info| type_info_uses_import(type_info, import))
                || method
                    .params
                    .iter()
                    .filter_map(|param| param.type_info.as_ref())
                    .any(|type_info| type_info_uses_import(type_info, import))
        })
        || phpdoc.templates.iter().any(|template| {
            template
                .bound
                .as_ref()
                .is_some_and(|type_info| type_info_uses_import(type_info, import))
        })
        || phpdoc.template_bindings.iter().any(|binding| {
            phpdoc_name_uses_import(&binding.target, import)
                || binding
                    .args
                    .iter()
                    .any(|type_info| type_info_uses_import(type_info, import))
        })
        || phpdoc
            .type_aliases
            .iter()
            .any(|alias| type_info_uses_import(&alias.type_info, import))
        || phpdoc
            .type_alias_imports
            .iter()
            .any(|alias_import| phpdoc_name_uses_import(&alias_import.source_type, import))
}

fn type_info_uses_import(type_info: &php_lsp_types::TypeInfo, import: &OrganizableImport) -> bool {
    match type_info {
        php_lsp_types::TypeInfo::Simple(name) => phpdoc_name_uses_import(name, import),
        php_lsp_types::TypeInfo::Generic { base, args } => {
            phpdoc_name_uses_import(base, import)
                || args
                    .iter()
                    .any(|type_info| type_info_uses_import(type_info, import))
        }
        php_lsp_types::TypeInfo::ArrayShape(items)
        | php_lsp_types::TypeInfo::ObjectShape(items) => items
            .iter()
            .any(|item| type_info_uses_import(&item.value, import)),
        php_lsp_types::TypeInfo::Callable {
            params,
            return_type,
        } => {
            params
                .iter()
                .any(|type_info| type_info_uses_import(type_info, import))
                || return_type
                    .as_deref()
                    .is_some_and(|type_info| type_info_uses_import(type_info, import))
        }
        php_lsp_types::TypeInfo::ClassString(inner) => inner
            .as_deref()
            .is_some_and(|type_info| type_info_uses_import(type_info, import)),
        php_lsp_types::TypeInfo::Conditional {
            target,
            if_type,
            else_type,
            ..
        } => {
            type_info_uses_import(target, import)
                || type_info_uses_import(if_type, import)
                || type_info_uses_import(else_type, import)
        }
        php_lsp_types::TypeInfo::Union(types) | php_lsp_types::TypeInfo::Intersection(types) => {
            types
                .iter()
                .any(|type_info| type_info_uses_import(type_info, import))
        }
        php_lsp_types::TypeInfo::Nullable(inner) => type_info_uses_import(inner, import),
        php_lsp_types::TypeInfo::LiteralString(_)
        | php_lsp_types::TypeInfo::LiteralInt(_)
        | php_lsp_types::TypeInfo::LiteralFloat(_)
        | php_lsp_types::TypeInfo::LiteralBool(_)
        | php_lsp_types::TypeInfo::LiteralNull
        | php_lsp_types::TypeInfo::Void
        | php_lsp_types::TypeInfo::Never
        | php_lsp_types::TypeInfo::Mixed
        | php_lsp_types::TypeInfo::Self_
        | php_lsp_types::TypeInfo::Static_
        | php_lsp_types::TypeInfo::Parent_ => false,
    }
}

fn phpdoc_name_uses_import(name: &str, import: &OrganizableImport) -> bool {
    let name = name.trim().trim_start_matches('\\');
    if name.is_empty() {
        return false;
    }

    let import_fqn = import.fqn.trim_start_matches('\\');
    if fqn_matches_import_or_prefix(name, import_fqn) {
        return true;
    }

    first_name_segment(name) == import_alias(import)
}

fn first_name_segment(name: &str) -> &str {
    name.trim_start_matches('\\')
        .split('\\')
        .next()
        .unwrap_or(name)
}

pub(crate) fn build_organize_imports_edit(
    uri: Uri,
    source: &str,
    tree: &tree_sitter::Tree,
    file_symbols: &php_lsp_types::FileSymbols,
) -> Option<WorkspaceEdit> {
    if file_symbols.use_statements.is_empty() {
        return None;
    }

    let (start_line, end_line) = find_organizable_use_block(source, file_symbols)?;
    let references = collect_symbol_references_in_file(tree, source, file_symbols);

    let mut imports: Vec<OrganizableImport> = file_symbols
        .use_statements
        .iter()
        .map(|use_stmt| OrganizableImport {
            fqn: use_stmt.fqn.trim_start_matches('\\').to_string(),
            alias: use_stmt.alias.clone(),
            kind: import_kind_from_use_kind(use_stmt.kind),
        })
        .filter(|import| import_is_used(import, file_symbols, &references))
        .collect();

    imports.sort_by(|a, b| {
        import_kind_sort_key(a.kind)
            .cmp(&import_kind_sort_key(b.kind))
            .then_with(|| a.fqn.to_lowercase().cmp(&b.fqn.to_lowercase()))
            .then_with(|| a.alias.cmp(&b.alias))
    });
    imports.dedup();

    let mut groups = Vec::new();
    for kind in [
        ImportKind::Class,
        ImportKind::Function,
        ImportKind::Constant,
    ] {
        let lines: Vec<String> = imports
            .iter()
            .filter(|import| import.kind == kind)
            .map(|import| build_use_statement(&import.fqn, import.kind, import.alias.as_deref()))
            .collect();
        if !lines.is_empty() {
            groups.push(lines.join("\n"));
        }
    }

    let mut new_text = groups.join("\n\n");
    if !new_text.is_empty() {
        new_text.push('\n');
        if !line_is_blank(source, end_line) {
            new_text.push('\n');
        }
    }

    let range = Range {
        start: Position::new(start_line, 0),
        end: Position::new(end_line, 0),
    };
    if text_at_lsp_range(source, range)
        .map(|old_text| old_text == new_text)
        .unwrap_or(false)
    {
        return None;
    }

    let mut changes = std::collections::HashMap::new();
    changes.insert(uri, vec![TextEdit { range, new_text }]);
    Some(WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    })
}

pub(crate) fn lsp_range_to_byte_range(source: &str, range: Range) -> (u32, u32, u32, u32) {
    (
        range.start.line,
        utf16_col_to_byte(source, range.start.line, range.start.character),
        range.end.line,
        utf16_col_to_byte(source, range.end.line, range.end.character),
    )
}

pub(crate) fn simple_return_type_hint_is_supported(
    name: &str,
    php_version: PhpVersion,
    in_union: bool,
) -> bool {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('$')
        || trimmed.contains(['<', '>', '[', ']', '(', ')', ',', ' '])
    {
        return false;
    }

    let lower = trimmed.trim_start_matches('\\').to_ascii_lowercase();
    match lower.as_str() {
        "void" => false,
        "never" => php_version.at_least(8, 1),
        "mixed" => php_version.at_least(8, 0),
        "static" => php_version.at_least(8, 0),
        "false" | "null" => {
            if in_union {
                php_version.at_least(8, 0)
            } else {
                php_version.at_least(8, 2)
            }
        }
        "true" => php_version.at_least(8, 2),
        "resource" => false,
        _ => true,
    }
}

pub(crate) fn is_intersection_member_type(type_info: &php_lsp_types::TypeInfo) -> bool {
    let php_lsp_types::TypeInfo::Simple(name) = type_info else {
        return false;
    };
    let lower = name.trim_start_matches('\\').to_ascii_lowercase();
    !matches!(
        lower.as_str(),
        "array"
            | "bool"
            | "callable"
            | "false"
            | "float"
            | "int"
            | "iterable"
            | "mixed"
            | "never"
            | "null"
            | "object"
            | "resource"
            | "string"
            | "true"
            | "void"
    ) && simple_return_type_hint_is_supported(name, PhpVersion::DEFAULT, false)
}

pub(crate) fn return_type_hint_is_supported(
    type_info: &php_lsp_types::TypeInfo,
    php_version: PhpVersion,
    in_union: bool,
) -> bool {
    match type_info {
        php_lsp_types::TypeInfo::Simple(name) => {
            simple_return_type_hint_is_supported(name, php_version, in_union)
        }
        php_lsp_types::TypeInfo::Union(types) => {
            php_version.at_least(8, 0)
                && types
                    .iter()
                    .all(|t| !matches!(t, php_lsp_types::TypeInfo::Void))
                && types
                    .iter()
                    .all(|t| return_type_hint_is_supported(t, php_version, true))
        }
        php_lsp_types::TypeInfo::Intersection(types) => {
            php_version.at_least(8, 1) && types.iter().all(is_intersection_member_type)
        }
        php_lsp_types::TypeInfo::Nullable(inner) => {
            php_version.at_least(7, 1)
                && !matches!(
                    inner.as_ref(),
                    php_lsp_types::TypeInfo::Mixed
                        | php_lsp_types::TypeInfo::Never
                        | php_lsp_types::TypeInfo::Void
                        | php_lsp_types::TypeInfo::Union(_)
                        | php_lsp_types::TypeInfo::Intersection(_)
                )
                && return_type_hint_is_supported(inner, php_version, false)
        }
        php_lsp_types::TypeInfo::Void => php_version.at_least(7, 1),
        php_lsp_types::TypeInfo::Never => php_version.at_least(8, 1),
        php_lsp_types::TypeInfo::Mixed => php_version.at_least(8, 0),
        php_lsp_types::TypeInfo::Self_ | php_lsp_types::TypeInfo::Parent_ => true,
        php_lsp_types::TypeInfo::Static_ => php_version.at_least(8, 0),
        php_lsp_types::TypeInfo::LiteralBool(value) => simple_return_type_hint_is_supported(
            if *value { "true" } else { "false" },
            php_version,
            in_union,
        ),
        php_lsp_types::TypeInfo::LiteralNull => {
            simple_return_type_hint_is_supported("null", php_version, in_union)
        }
        php_lsp_types::TypeInfo::Generic { .. }
        | php_lsp_types::TypeInfo::ArrayShape(_)
        | php_lsp_types::TypeInfo::ObjectShape(_)
        | php_lsp_types::TypeInfo::Callable { .. }
        | php_lsp_types::TypeInfo::ClassString(_)
        | php_lsp_types::TypeInfo::Conditional { .. }
        | php_lsp_types::TypeInfo::LiteralString(_)
        | php_lsp_types::TypeInfo::LiteralInt(_)
        | php_lsp_types::TypeInfo::LiteralFloat(_) => false,
    }
}

pub(crate) fn return_type_hint(
    type_info: &php_lsp_types::TypeInfo,
    php_version: PhpVersion,
) -> Option<String> {
    if return_type_hint_is_supported(type_info, php_version, false) {
        Some(type_info.to_string())
    } else {
        None
    }
}

pub(crate) fn build_add_return_type_action(
    uri: Uri,
    candidate: &MissingReturnTypeCandidate,
    php_version: PhpVersion,
    request_range: Range,
    document_version: Option<i32>,
) -> Option<CodeActionOrCommand> {
    let hint = return_type_hint(&candidate.return_type, php_version)?;
    let data = serde_json::to_value(CodeActionData {
        action_kind: CodeActionDataKind::AddReturnType,
        uri: uri.as_str().to_string(),
        range: request_range,
        document_version,
        extra: CodeActionDataExtra::AddReturnType {
            hint: hint.clone(),
            insert_position: CodeActionInsertPosition {
                line: candidate.insert_position.0,
                byte_character: candidate.insert_position.1,
            },
        },
    })
    .ok()?;

    Some(CodeActionOrCommand::CodeAction(CodeAction {
        title: format!("Add return type `{}`", hint),
        kind: Some(CodeActionKind::REFACTOR_REWRITE),
        diagnostics: None,
        edit: None,
        command: None,
        is_preferred: Some(false),
        disabled: None,
        data: Some(data),
    }))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum CodeActionDataKind {
    AddReturnType,
    ImplementMissingMethods,
    GenerateConstructor,
    GenerateAccessor,
    ChangeVisibility,
    PromoteConstructorParameter,
    UpdatePhpDoc,
    ExtractVariable,
    ExtractConstant,
    InlineVariable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CodeActionData {
    action_kind: CodeActionDataKind,
    uri: String,
    range: Range,
    document_version: Option<i32>,
    extra: CodeActionDataExtra,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(crate) enum CodeActionDataExtra {
    AddReturnType {
        hint: String,
        insert_position: CodeActionInsertPosition,
    },
    ImplementMissingMethods {
        class_fqn: String,
    },
    GenerateConstructor {
        class_fqn: String,
    },
    GenerateAccessor {
        property_fqn: String,
        accessor_kind: AccessorKind,
        method_name: String,
    },
    ChangeVisibility {
        symbol_fqn: String,
        target_visibility: php_lsp_types::Visibility,
    },
    PromoteConstructorParameter {
        property_fqn: String,
    },
    UpdatePhpDoc {
        symbol_fqn: String,
    },
    ExtractVariable {
        variable_name: String,
    },
    ExtractConstant {
        constant_name: String,
    },
    InlineVariable {
        variable_name: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum AccessorKind {
    Getter,
    Setter,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CodeActionInsertPosition {
    line: u32,
    byte_character: u32,
}

pub(crate) fn empty_workspace_edit() -> WorkspaceEdit {
    WorkspaceEdit {
        changes: Some(HashMap::new()),
        document_changes: None,
        change_annotations: None,
    }
}

pub(crate) fn add_return_type_edit(
    uri: Uri,
    source: &str,
    hint: &str,
    insert_position: CodeActionInsertPosition,
) -> WorkspaceEdit {
    let utf16_index = Utf16LineIndex::new(source);
    let position = Position::new(
        insert_position.line,
        utf16_index.byte_col_to_utf16(insert_position.line, insert_position.byte_character),
    );

    let mut changes = HashMap::new();
    changes.insert(
        uri,
        vec![TextEdit {
            range: Range {
                start: position,
                end: position,
            },
            new_text: format!(": {}", hint),
        }],
    );

    WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    }
}

pub(crate) fn build_implement_missing_methods_action(
    uri: Uri,
    class_sym: &php_lsp_types::SymbolInfo,
    missing_methods: &[Arc<php_lsp_types::SymbolInfo>],
    request_range: Range,
    document_version: Option<i32>,
) -> Option<CodeActionOrCommand> {
    if missing_methods.is_empty() {
        return None;
    }

    let data = serde_json::to_value(CodeActionData {
        action_kind: CodeActionDataKind::ImplementMissingMethods,
        uri: uri.as_str().to_string(),
        range: request_range,
        document_version,
        extra: CodeActionDataExtra::ImplementMissingMethods {
            class_fqn: class_sym.fqn.clone(),
        },
    })
    .ok()?;

    let title = if missing_methods.len() == 1 {
        format!("Implement missing method `{}`", missing_methods[0].name)
    } else {
        format!("Implement {} missing methods", missing_methods.len())
    };

    Some(CodeActionOrCommand::CodeAction(CodeAction {
        title,
        kind: Some(CodeActionKind::QUICKFIX),
        diagnostics: None,
        edit: None,
        command: None,
        is_preferred: Some(false),
        disabled: None,
        data: Some(data),
    }))
}

pub(crate) fn build_generate_constructor_action(
    uri: Uri,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    class_sym: &php_lsp_types::SymbolInfo,
    request_range: Range,
    document_version: Option<i32>,
) -> Option<CodeActionOrCommand> {
    if direct_method_name_exists(file_symbols, &class_sym.fqn, "__construct")
        || constructor_generation_properties(source, file_symbols, &class_sym.fqn).is_empty()
    {
        return None;
    }

    let data = serde_json::to_value(CodeActionData {
        action_kind: CodeActionDataKind::GenerateConstructor,
        uri: uri.as_str().to_string(),
        range: request_range,
        document_version,
        extra: CodeActionDataExtra::GenerateConstructor {
            class_fqn: class_sym.fqn.clone(),
        },
    })
    .ok()?;

    Some(CodeActionOrCommand::CodeAction(CodeAction {
        title: "Generate constructor".to_string(),
        kind: Some(CodeActionKind::REFACTOR_REWRITE),
        diagnostics: None,
        edit: None,
        command: None,
        is_preferred: Some(false),
        disabled: None,
        data: Some(data),
    }))
}

pub(crate) fn build_generate_accessor_action(
    uri: Uri,
    property: &php_lsp_types::SymbolInfo,
    accessor_kind: AccessorKind,
    method_name: String,
    request_range: Range,
    document_version: Option<i32>,
) -> Option<CodeActionOrCommand> {
    if accessor_kind == AccessorKind::Setter && property.modifiers.is_readonly {
        return None;
    }

    let data = serde_json::to_value(CodeActionData {
        action_kind: CodeActionDataKind::GenerateAccessor,
        uri: uri.as_str().to_string(),
        range: request_range,
        document_version,
        extra: CodeActionDataExtra::GenerateAccessor {
            property_fqn: property.fqn.clone(),
            accessor_kind,
            method_name: method_name.clone(),
        },
    })
    .ok()?;

    let accessor_label = match accessor_kind {
        AccessorKind::Getter => "getter",
        AccessorKind::Setter => "setter",
    };

    Some(CodeActionOrCommand::CodeAction(CodeAction {
        title: format!("Generate {} `{}`", accessor_label, method_name),
        kind: Some(CodeActionKind::REFACTOR_REWRITE),
        diagnostics: None,
        edit: None,
        command: None,
        is_preferred: Some(false),
        disabled: None,
        data: Some(data),
    }))
}

pub(crate) fn build_generate_accessor_actions(
    uri: Uri,
    index: &WorkspaceIndex,
    property: &php_lsp_types::SymbolInfo,
    request_range: Range,
    document_version: Option<i32>,
) -> Vec<CodeActionOrCommand> {
    let Some(class_fqn) = property.parent_fqn.as_deref() else {
        return Vec::new();
    };

    let mut actions = Vec::new();
    let getter = getter_name(property);
    if !member_method_name_exists(index, class_fqn, &getter) {
        if let Some(action) = build_generate_accessor_action(
            uri.clone(),
            property,
            AccessorKind::Getter,
            getter,
            request_range,
            document_version,
        ) {
            actions.push(action);
        }
    }

    let setter = setter_name(property);
    if !property.modifiers.is_readonly && !member_method_name_exists(index, class_fqn, &setter) {
        if let Some(action) = build_generate_accessor_action(
            uri,
            property,
            AccessorKind::Setter,
            setter,
            request_range,
            document_version,
        ) {
            actions.push(action);
        }
    }

    actions
}

pub(crate) fn visibility_text(visibility: php_lsp_types::Visibility) -> &'static str {
    match visibility {
        php_lsp_types::Visibility::Public => "public",
        php_lsp_types::Visibility::Protected => "protected",
        php_lsp_types::Visibility::Private => "private",
    }
}

pub(crate) fn member_symbol_at_range(
    file_symbols: &php_lsp_types::FileSymbols,
    range: (u32, u32, u32, u32),
) -> Option<&php_lsp_types::SymbolInfo> {
    file_symbols
        .symbols
        .iter()
        .filter(|sym| {
            matches!(
                sym.kind,
                php_lsp_types::PhpSymbolKind::Method
                    | php_lsp_types::PhpSymbolKind::Property
                    | php_lsp_types::PhpSymbolKind::ClassConstant
            )
        })
        .find(|sym| {
            byte_range_contains(sym.range, range) || byte_ranges_overlap(sym.selection_range, range)
        })
}

pub(crate) fn symbol_supports_visibility_change(symbol: &php_lsp_types::SymbolInfo) -> bool {
    matches!(
        symbol.kind,
        php_lsp_types::PhpSymbolKind::Method
            | php_lsp_types::PhpSymbolKind::Property
            | php_lsp_types::PhpSymbolKind::ClassConstant
    ) && !symbol.modifiers.is_builtin
}

pub(crate) fn visibility_rank(visibility: php_lsp_types::Visibility) -> u8 {
    match visibility {
        php_lsp_types::Visibility::Private => 0,
        php_lsp_types::Visibility::Protected => 1,
        php_lsp_types::Visibility::Public => 2,
    }
}

pub(crate) fn parent_symbol_for_member<'a>(
    file_symbols: &'a php_lsp_types::FileSymbols,
    symbol: &php_lsp_types::SymbolInfo,
) -> Option<&'a php_lsp_types::SymbolInfo> {
    let parent_fqn = symbol.parent_fqn.as_deref()?;
    file_symbols.symbols.iter().find(|candidate| {
        candidate.fqn == parent_fqn
            && matches!(
                candidate.kind,
                php_lsp_types::PhpSymbolKind::Class
                    | php_lsp_types::PhpSymbolKind::Interface
                    | php_lsp_types::PhpSymbolKind::Trait
            )
    })
}

pub(crate) fn collect_method_contract_visibilities(
    index: &WorkspaceIndex,
    type_fqn: &str,
    method_name: &str,
    out: &mut Vec<php_lsp_types::Visibility>,
    visited: &mut HashSet<String>,
) {
    let normalized_type = type_fqn.trim_start_matches('\\').to_string();
    if !visited.insert(normalized_type.clone()) {
        return;
    }

    let Some(type_sym) = index
        .types
        .get(&normalized_type)
        .map(|entry| entry.value().clone())
    else {
        return;
    };

    let wanted = normalized_method_name(method_name);
    for member in direct_member_symbols_from_index(index, &normalized_type) {
        if member.kind == php_lsp_types::PhpSymbolKind::Method
            && normalized_method_name(&member.name) == wanted
        {
            out.push(member.visibility);
        }
    }

    for trait_fqn in &type_sym.traits {
        collect_method_contract_visibilities(index, trait_fqn, method_name, out, visited);
    }
    for parent_fqn in &type_sym.extends {
        collect_method_contract_visibilities(index, parent_fqn, method_name, out, visited);
    }
    for iface_fqn in &type_sym.implements {
        collect_method_contract_visibilities(index, iface_fqn, method_name, out, visited);
    }
}

pub(crate) fn required_method_visibility(
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    method: &php_lsp_types::SymbolInfo,
) -> Option<php_lsp_types::Visibility> {
    let parent = parent_symbol_for_member(file_symbols, method)?;
    if parent.kind != php_lsp_types::PhpSymbolKind::Class {
        return None;
    }

    let mut visibilities = Vec::new();
    let mut visited = HashSet::new();
    for trait_fqn in &parent.traits {
        collect_method_contract_visibilities(
            index,
            trait_fqn,
            &method.name,
            &mut visibilities,
            &mut visited,
        );
    }
    for parent_fqn in &parent.extends {
        collect_method_contract_visibilities(
            index,
            parent_fqn,
            &method.name,
            &mut visibilities,
            &mut visited,
        );
    }
    for iface_fqn in &parent.implements {
        collect_method_contract_visibilities(
            index,
            iface_fqn,
            &method.name,
            &mut visibilities,
            &mut visited,
        );
    }

    visibilities
        .into_iter()
        .max_by_key(|visibility| visibility_rank(*visibility))
}

pub(crate) fn visibility_change_is_safe(
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    symbol: &php_lsp_types::SymbolInfo,
    target_visibility: php_lsp_types::Visibility,
) -> bool {
    if !symbol_supports_visibility_change(symbol) || symbol.visibility == target_visibility {
        return false;
    }

    if parent_symbol_for_member(file_symbols, symbol)
        .is_some_and(|parent| parent.kind == php_lsp_types::PhpSymbolKind::Interface)
    {
        return false;
    }

    if symbol.kind == php_lsp_types::PhpSymbolKind::Method {
        if symbol.modifiers.is_abstract {
            return false;
        }

        if let Some(required_visibility) = required_method_visibility(index, file_symbols, symbol) {
            return visibility_rank(target_visibility) >= visibility_rank(required_visibility);
        }
    }

    true
}

pub(crate) fn build_change_visibility_actions(
    uri: Uri,
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    symbol: &php_lsp_types::SymbolInfo,
    request_range: Range,
    document_version: Option<i32>,
) -> Vec<CodeActionOrCommand> {
    if !symbol_supports_visibility_change(symbol) {
        return Vec::new();
    }

    [
        php_lsp_types::Visibility::Public,
        php_lsp_types::Visibility::Protected,
        php_lsp_types::Visibility::Private,
    ]
    .into_iter()
    .filter(|target_visibility| {
        visibility_change_is_safe(index, file_symbols, symbol, *target_visibility)
    })
    .filter_map(|target_visibility| {
        let data = serde_json::to_value(CodeActionData {
            action_kind: CodeActionDataKind::ChangeVisibility,
            uri: uri.as_str().to_string(),
            range: request_range,
            document_version,
            extra: CodeActionDataExtra::ChangeVisibility {
                symbol_fqn: symbol.fqn.clone(),
                target_visibility,
            },
        })
        .ok()?;

        Some(CodeActionOrCommand::CodeAction(CodeAction {
            title: format!(
                "Change visibility to {}",
                visibility_text(target_visibility)
            ),
            kind: Some(CodeActionKind::REFACTOR_REWRITE),
            diagnostics: None,
            edit: None,
            command: None,
            is_preferred: Some(false),
            disabled: None,
            data: Some(data),
        }))
    })
    .collect()
}

pub(crate) fn concrete_class_symbol_at_range(
    file_symbols: &php_lsp_types::FileSymbols,
    range: (u32, u32, u32, u32),
) -> Option<&php_lsp_types::SymbolInfo> {
    file_symbols.symbols.iter().find(|sym| {
        sym.kind == php_lsp_types::PhpSymbolKind::Class
            && !sym.modifiers.is_abstract
            && byte_range_contains(sym.range, range)
    })
}

pub(crate) fn direct_method_symbols_from_file<'a>(
    file_symbols: &'a php_lsp_types::FileSymbols,
    type_fqn: &str,
) -> Vec<&'a php_lsp_types::SymbolInfo> {
    file_symbols
        .symbols
        .iter()
        .filter(|sym| {
            sym.kind == php_lsp_types::PhpSymbolKind::Method
                && sym.parent_fqn.as_deref() == Some(type_fqn)
        })
        .collect()
}

pub(crate) fn direct_member_symbols_from_index(
    index: &WorkspaceIndex,
    type_fqn: &str,
) -> Vec<Arc<php_lsp_types::SymbolInfo>> {
    let mut members = Vec::new();
    for entry in index.file_symbols.iter() {
        for sym in &entry.value().symbols {
            if sym.parent_fqn.as_deref() == Some(type_fqn) {
                members.push(Arc::new(sym.clone()));
            }
        }
    }
    members
}

pub(crate) fn normalized_method_name(name: &str) -> String {
    name.to_ascii_lowercase()
}

pub(crate) fn collect_concrete_methods_from_type(
    index: &WorkspaceIndex,
    type_fqn: &str,
    implemented: &mut HashSet<String>,
    visited: &mut HashSet<String>,
) {
    let normalized_type = type_fqn.trim_start_matches('\\').to_string();
    if !visited.insert(normalized_type.clone()) {
        return;
    }

    let Some(type_sym) = index
        .types
        .get(&normalized_type)
        .map(|entry| entry.value().clone())
    else {
        return;
    };

    for member in direct_member_symbols_from_index(index, &normalized_type) {
        if member.kind == php_lsp_types::PhpSymbolKind::Method && !member.modifiers.is_abstract {
            implemented.insert(normalized_method_name(&member.name));
        }
    }

    for trait_fqn in &type_sym.traits {
        collect_concrete_methods_from_type(index, trait_fqn, implemented, visited);
    }
    for parent_fqn in &type_sym.extends {
        collect_concrete_methods_from_type(index, parent_fqn, implemented, visited);
    }
}

pub(crate) fn collect_required_methods_from_type(
    index: &WorkspaceIndex,
    type_fqn: &str,
    required: &mut Vec<Arc<php_lsp_types::SymbolInfo>>,
    seen: &mut HashSet<String>,
    visited: &mut HashSet<String>,
) {
    let normalized_type = type_fqn.trim_start_matches('\\').to_string();
    if !visited.insert(normalized_type.clone()) {
        return;
    }

    let Some(type_sym) = index
        .types
        .get(&normalized_type)
        .map(|entry| entry.value().clone())
    else {
        return;
    };

    for member in direct_member_symbols_from_index(index, &normalized_type) {
        let required_method = match type_sym.kind {
            php_lsp_types::PhpSymbolKind::Interface => {
                member.kind == php_lsp_types::PhpSymbolKind::Method
            }
            php_lsp_types::PhpSymbolKind::Class | php_lsp_types::PhpSymbolKind::Trait => {
                member.kind == php_lsp_types::PhpSymbolKind::Method && member.modifiers.is_abstract
            }
            _ => false,
        };

        if required_method && seen.insert(normalized_method_name(&member.name)) {
            required.push(member);
        }
    }

    for trait_fqn in &type_sym.traits {
        collect_required_methods_from_type(index, trait_fqn, required, seen, visited);
    }
    for parent_fqn in &type_sym.extends {
        collect_required_methods_from_type(index, parent_fqn, required, seen, visited);
    }
    for iface_fqn in &type_sym.implements {
        collect_required_methods_from_type(index, iface_fqn, required, seen, visited);
    }
}

pub(crate) fn missing_implementation_methods(
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    class_sym: &php_lsp_types::SymbolInfo,
) -> Vec<Arc<php_lsp_types::SymbolInfo>> {
    if class_sym.kind != php_lsp_types::PhpSymbolKind::Class || class_sym.modifiers.is_abstract {
        return Vec::new();
    }

    let mut implemented = HashSet::new();
    for method in direct_method_symbols_from_file(file_symbols, &class_sym.fqn) {
        implemented.insert(normalized_method_name(&method.name));
    }

    let mut concrete_visited = HashSet::new();
    for trait_fqn in &class_sym.traits {
        collect_concrete_methods_from_type(
            index,
            trait_fqn,
            &mut implemented,
            &mut concrete_visited,
        );
    }
    for parent_fqn in &class_sym.extends {
        collect_concrete_methods_from_type(
            index,
            parent_fqn,
            &mut implemented,
            &mut concrete_visited,
        );
    }

    let mut required = Vec::new();
    let mut seen_required = HashSet::new();
    let mut required_visited = HashSet::new();
    for trait_fqn in &class_sym.traits {
        collect_required_methods_from_type(
            index,
            trait_fqn,
            &mut required,
            &mut seen_required,
            &mut required_visited,
        );
    }
    for parent_fqn in &class_sym.extends {
        collect_required_methods_from_type(
            index,
            parent_fqn,
            &mut required,
            &mut seen_required,
            &mut required_visited,
        );
    }
    for iface_fqn in &class_sym.implements {
        collect_required_methods_from_type(
            index,
            iface_fqn,
            &mut required,
            &mut seen_required,
            &mut required_visited,
        );
    }

    let mut missing = Vec::new();
    for method in required {
        let name = normalized_method_name(&method.name);
        if implemented.insert(name) {
            missing.push(method);
        }
    }

    missing.sort_by(|left, right| {
        normalized_method_name(&left.name)
            .cmp(&normalized_method_name(&right.name))
            .then_with(|| left.fqn.cmp(&right.fqn))
    });
    missing
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TypeHintPosition {
    Parameter,
    Return,
}

pub(crate) fn php_identifier_part_is_valid(part: &str) -> bool {
    let mut chars = part.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

pub(crate) fn simple_native_type_hint_text(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.contains('-') {
        return None;
    }

    let without_leading_slash = trimmed.trim_start_matches('\\');
    if without_leading_slash
        .split('\\')
        .all(php_identifier_part_is_valid)
    {
        Some(trimmed.to_string())
    } else {
        None
    }
}

pub(crate) fn native_type_hint_text(
    type_info: &php_lsp_types::TypeInfo,
    php_version: PhpVersion,
    position: TypeHintPosition,
) -> Option<String> {
    use php_lsp_types::TypeInfo;

    match type_info {
        TypeInfo::Simple(name) => simple_native_type_hint_text(name),
        TypeInfo::Self_ | TypeInfo::Parent_ => Some(type_info.to_string()),
        TypeInfo::Static_ if position == TypeHintPosition::Return && php_version.at_least(8, 0) => {
            Some("static".to_string())
        }
        TypeInfo::Mixed if php_version.at_least(8, 0) => Some("mixed".to_string()),
        TypeInfo::Void if position == TypeHintPosition::Return => Some("void".to_string()),
        TypeInfo::Never if position == TypeHintPosition::Return && php_version.at_least(8, 1) => {
            Some("never".to_string())
        }
        TypeInfo::Nullable(inner) => match inner.as_ref() {
            TypeInfo::Mixed | TypeInfo::Void | TypeInfo::Never | TypeInfo::Nullable(_) => None,
            _ => native_type_hint_text(inner, php_version, position)
                .map(|inner| format!("?{}", inner)),
        },
        TypeInfo::Union(types) if php_version.at_least(8, 0) => {
            let parts = types
                .iter()
                .map(|ty| native_type_hint_text(ty, php_version, position))
                .collect::<Option<Vec<_>>>()?;
            if parts.iter().any(|part| part == "void") {
                None
            } else {
                Some(parts.join("|"))
            }
        }
        TypeInfo::Intersection(types) if php_version.at_least(8, 1) => {
            let parts = types
                .iter()
                .map(|ty| match ty {
                    TypeInfo::Simple(_) | TypeInfo::Self_ | TypeInfo::Parent_ => {
                        native_type_hint_text(ty, php_version, position)
                    }
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()?;
            Some(parts.join("&"))
        }
        TypeInfo::LiteralNull if php_version.at_least(8, 2) => Some("null".to_string()),
        TypeInfo::LiteralBool(value)
            if position == TypeHintPosition::Return && php_version.at_least(8, 2) =>
        {
            Some(if *value { "true" } else { "false" }.to_string())
        }
        _ => None,
    }
}

pub(crate) fn render_method_param(
    param: &php_lsp_types::ParamInfo,
    php_version: PhpVersion,
) -> String {
    let mut text = String::new();
    if let Some(type_info) = &param.type_info {
        if let Some(type_text) = generated_member_native_type_hint_text(
            type_info,
            php_version,
            TypeHintPosition::Parameter,
        ) {
            text.push_str(&type_text);
            text.push(' ');
        }
    }
    if param.is_by_ref {
        text.push('&');
    }
    if param.is_variadic {
        text.push_str("...");
    }
    text.push('$');
    text.push_str(&param.name);
    if !param.is_variadic {
        if let Some(default_value) = param.default_value.as_deref() {
            text.push_str(" = ");
            text.push_str(default_value);
        }
    }
    text
}

#[derive(Debug, Clone, Default)]
pub(crate) struct MethodContractMetadata {
    doc_comment: Option<String>,
    attributes: Vec<String>,
}

pub(crate) fn method_attribute_bracket_delta(line: &str) -> isize {
    let mut delta = 0isize;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '#' if chars.peek() == Some(&'[') => {
                chars.next();
                delta += 1;
            }
            '[' => delta += 1,
            ']' => delta -= 1,
            _ => {}
        }
    }
    delta
}

pub(crate) fn collect_attribute_groups(text: &str) -> Vec<String> {
    let mut groups = Vec::new();
    let mut current = Vec::new();
    let mut depth = 0isize;

    for line in text.lines() {
        let trimmed = line.trim_start();
        if current.is_empty() {
            if !trimmed.starts_with("#[") {
                continue;
            }
            depth = 0;
        }

        current.push(trimmed.trim_end().to_string());
        depth += method_attribute_bracket_delta(trimmed);
        if depth <= 0 {
            groups.push(current.join("\n"));
            current.clear();
            depth = 0;
        }
    }

    groups
}

pub(crate) fn preceding_attribute_source(source: &str, method_start: usize) -> &str {
    let search_start = source[..method_start]
        .rfind("\n#[")
        .map(|idx| idx + 1)
        .or_else(|| source[..method_start].rfind("\n    #[").map(|idx| idx + 1))
        .unwrap_or(method_start);
    source.get(search_start..method_start).unwrap_or("")
}

pub(crate) fn method_contract_metadata(
    method: &php_lsp_types::SymbolInfo,
    declaration_source: Option<&str>,
) -> MethodContractMetadata {
    let Some(source) = declaration_source else {
        return MethodContractMetadata {
            doc_comment: method.doc_comment.clone(),
            attributes: Vec::new(),
        };
    };
    let Some(method_start) = byte_offset_for_line_col(source, method.range.0, method.range.1)
    else {
        return MethodContractMetadata {
            doc_comment: method.doc_comment.clone(),
            attributes: Vec::new(),
        };
    };
    let Some(method_end) = byte_offset_for_line_col(source, method.range.2, method.range.3) else {
        return MethodContractMetadata {
            doc_comment: method.doc_comment.clone(),
            attributes: Vec::new(),
        };
    };

    let mut attribute_source = String::new();
    if let Some((_, doc_end)) = symbol_doc_comment_span(source, method) {
        attribute_source.push_str(source.get(doc_end..method_start).unwrap_or(""));
    } else {
        attribute_source.push_str(preceding_attribute_source(source, method_start));
    }
    if let Some(method_text) = source.get(method_start..method_end) {
        if let Some(function_offset) = method_text.find("function") {
            attribute_source.push_str(method_text.get(..function_offset).unwrap_or(""));
        }
    }

    let mut attributes = collect_attribute_groups(&attribute_source);
    attributes.sort();
    attributes.dedup();

    MethodContractMetadata {
        doc_comment: method.doc_comment.clone(),
        attributes,
    }
}

pub(crate) fn render_reindented_block(block: &str, indent: &str) -> String {
    let mut text = String::new();
    for line in block.lines() {
        text.push_str(indent);
        text.push_str(line.trim_start());
        text.push('\n');
    }
    text
}

pub(crate) fn render_missing_method_stub(
    method: &php_lsp_types::SymbolInfo,
    metadata: Option<&MethodContractMetadata>,
    method_indent: &str,
    body_indent: &str,
    php_version: PhpVersion,
) -> String {
    let visibility = match method.visibility {
        php_lsp_types::Visibility::Public => "public",
        php_lsp_types::Visibility::Protected => "protected",
        php_lsp_types::Visibility::Private => "private",
    };

    let signature = method
        .signature
        .clone()
        .unwrap_or(php_lsp_types::Signature {
            params: Vec::new(),
            return_type: None,
        });
    let params = signature
        .params
        .iter()
        .map(|param| render_method_param(param, php_version))
        .collect::<Vec<_>>()
        .join(", ");

    let mut text = String::new();
    if let Some(metadata) = metadata {
        if let Some(doc_comment) = metadata.doc_comment.as_deref() {
            let content_lines = phpdoc_content_lines(doc_comment);
            if !content_lines.is_empty() {
                text.push_str(&render_phpdoc_comment(method_indent, &content_lines));
                text.push('\n');
            }
        }
        for attribute in &metadata.attributes {
            text.push_str(&render_reindented_block(attribute, method_indent));
        }
    }
    text.push_str(method_indent);
    text.push_str(visibility);
    text.push(' ');
    if method.modifiers.is_static {
        text.push_str("static ");
    }
    text.push_str("function ");
    text.push_str(&method.name);
    text.push('(');
    text.push_str(&params);
    text.push(')');
    if let Some(return_type) = signature.return_type.as_ref().and_then(|return_type| {
        native_type_hint_text(return_type, php_version, TypeHintPosition::Return)
    }) {
        text.push_str(": ");
        text.push_str(&return_type);
    }
    text.push('\n');
    text.push_str(method_indent);
    text.push_str("{\n");
    text.push_str(body_indent);
    text.push_str("throw new \\BadMethodCallException('Not implemented yet.');\n");
    text.push_str(method_indent);
    text.push_str("}\n");
    text
}

pub(crate) fn line_start_offsets(source: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    for (idx, byte) in source.bytes().enumerate() {
        if byte == b'\n' {
            offsets.push(idx + 1);
        }
    }
    offsets
}

pub(crate) fn byte_offset_for_line_col(source: &str, line: u32, byte_col: u32) -> Option<usize> {
    let offsets = line_start_offsets(source);
    let start = *offsets.get(line as usize)?;
    Some((start + byte_col as usize).min(source.len()))
}

pub(crate) fn line_col_for_byte_offset(source: &str, offset: usize) -> (u32, u32) {
    let offsets = line_start_offsets(source);
    let line_idx = offsets
        .partition_point(|line_start| *line_start <= offset)
        .saturating_sub(1);
    let line_start = offsets.get(line_idx).copied().unwrap_or(0);
    (line_idx as u32, offset.saturating_sub(line_start) as u32)
}

pub(crate) fn class_closing_brace_position(
    source: &str,
    class_sym: &php_lsp_types::SymbolInfo,
) -> Option<(u32, u32)> {
    let start = byte_offset_for_line_col(source, class_sym.range.0, class_sym.range.1)?;
    let end = byte_offset_for_line_col(source, class_sym.range.2, class_sym.range.3)?;
    let class_text = source.get(start..end)?;
    let closing_relative = class_text.rfind('}')?;
    Some(line_col_for_byte_offset(source, start + closing_relative))
}

pub(crate) fn line_text(source: &str, line: u32) -> &str {
    source.lines().nth(line as usize).unwrap_or("")
}

pub(crate) fn line_prefix_by_byte_col(line_text: &str, byte_col: u32) -> &str {
    let end = (byte_col as usize).min(line_text.len());
    line_text.get(..end).unwrap_or("")
}

pub(crate) fn leading_ascii_whitespace(text: &str) -> String {
    text.chars()
        .take_while(|ch| *ch == ' ' || *ch == '\t')
        .collect()
}

pub(crate) fn method_insertion_needs_leading_blank(
    source: &str,
    closing_line: u32,
    closing_col: u32,
) -> bool {
    let close_line_text = line_text(source, closing_line);
    if !line_prefix_by_byte_col(close_line_text, closing_col)
        .trim()
        .is_empty()
    {
        return true;
    }

    let lines = source.lines().collect::<Vec<_>>();
    for line in lines
        .get(..closing_line as usize)
        .unwrap_or(&[])
        .iter()
        .rev()
    {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        return !trimmed.ends_with('{');
    }

    false
}

pub(crate) struct ClassMethodInsertion {
    position: Position,
    method_indent: String,
    body_indent: String,
    needs_leading_blank: bool,
}

pub(crate) fn class_method_insertion(
    source: &str,
    class_sym: &php_lsp_types::SymbolInfo,
) -> Option<ClassMethodInsertion> {
    let (closing_line, closing_col) = class_closing_brace_position(source, class_sym)?;
    let utf16_index = Utf16LineIndex::new(source);
    let position = Position::new(
        closing_line,
        utf16_index.byte_col_to_utf16(closing_line, closing_col),
    );
    let close_line = line_text(source, closing_line);
    let close_indent = leading_ascii_whitespace(line_prefix_by_byte_col(close_line, closing_col));
    let method_indent = format!("{}    ", close_indent);
    let body_indent = format!("{}    ", method_indent);

    Some(ClassMethodInsertion {
        position,
        method_indent,
        body_indent,
        needs_leading_blank: method_insertion_needs_leading_blank(
            source,
            closing_line,
            closing_col,
        ),
    })
}

pub(crate) fn generated_methods_workspace_edit(
    uri: Uri,
    insertion: ClassMethodInsertion,
    rendered_methods: Vec<String>,
) -> WorkspaceEdit {
    let mut new_text = String::new();
    if insertion.needs_leading_blank {
        new_text.push('\n');
    }
    for (idx, method) in rendered_methods.into_iter().enumerate() {
        if idx > 0 {
            new_text.push('\n');
        }
        new_text.push_str(&method);
    }

    let mut changes = HashMap::new();
    changes.insert(
        uri,
        vec![TextEdit {
            range: Range {
                start: insertion.position,
                end: insertion.position,
            },
            new_text,
        }],
    );

    WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    }
}

pub(crate) fn implement_missing_methods_edit(
    uri: Uri,
    source: &str,
    class_sym: &php_lsp_types::SymbolInfo,
    missing_methods: &[Arc<php_lsp_types::SymbolInfo>],
    metadata_by_fqn: &HashMap<String, MethodContractMetadata>,
    php_version: PhpVersion,
) -> Option<WorkspaceEdit> {
    if missing_methods.is_empty() {
        return Some(empty_workspace_edit());
    }

    let insertion = class_method_insertion(source, class_sym)?;
    let rendered_methods = missing_methods
        .iter()
        .map(|method| {
            render_missing_method_stub(
                method,
                metadata_by_fqn.get(&method.fqn),
                &insertion.method_indent,
                &insertion.body_indent,
                php_version,
            )
        })
        .collect();

    Some(generated_methods_workspace_edit(
        uri,
        insertion,
        rendered_methods,
    ))
}

pub(crate) fn direct_property_symbols_from_file<'a>(
    file_symbols: &'a php_lsp_types::FileSymbols,
    type_fqn: &str,
) -> Vec<&'a php_lsp_types::SymbolInfo> {
    file_symbols
        .symbols
        .iter()
        .filter(|sym| {
            sym.kind == php_lsp_types::PhpSymbolKind::Property
                && sym.parent_fqn.as_deref() == Some(type_fqn)
        })
        .collect()
}

pub(crate) fn property_symbol_at_range(
    file_symbols: &php_lsp_types::FileSymbols,
    range: (u32, u32, u32, u32),
) -> Option<&php_lsp_types::SymbolInfo> {
    file_symbols
        .symbols
        .iter()
        .filter(|sym| sym.kind == php_lsp_types::PhpSymbolKind::Property)
        .find(|sym| {
            byte_range_contains(sym.range, range) || byte_ranges_overlap(sym.selection_range, range)
        })
}

pub(crate) fn direct_method_name_exists(
    file_symbols: &php_lsp_types::FileSymbols,
    class_fqn: &str,
    method_name: &str,
) -> bool {
    let wanted = normalized_method_name(method_name);
    direct_method_symbols_from_file(file_symbols, class_fqn)
        .iter()
        .any(|method| normalized_method_name(&method.name) == wanted)
}

pub(crate) fn member_method_name_exists(
    index: &WorkspaceIndex,
    class_fqn: &str,
    method_name: &str,
) -> bool {
    index
        .resolve_member(&format!("{}::{}", class_fqn, method_name))
        .is_some_and(|sym| sym.kind == php_lsp_types::PhpSymbolKind::Method)
}

pub(crate) fn property_type_info(
    property: &php_lsp_types::SymbolInfo,
) -> Option<&php_lsp_types::TypeInfo> {
    property
        .signature
        .as_ref()
        .and_then(|signature| signature.return_type.as_ref())
}

#[derive(Debug, Clone)]
pub(crate) struct PropertyDocType {
    type_info: Option<php_lsp_types::TypeInfo>,
    type_text: String,
    description: Option<String>,
}

pub(crate) fn property_doc_type(property: &php_lsp_types::SymbolInfo) -> Option<PropertyDocType> {
    let doc_comment = property.doc_comment.as_deref()?;
    let parsed = parse_phpdoc(doc_comment);

    for tag in ["@var", "@phpstan-var", "@psalm-var"] {
        for line in phpdoc_content_lines(doc_comment) {
            let Some(rest) = phpdoc_tag_rest(&line, tag) else {
                continue;
            };
            let Some(type_end) = consume_phpdoc_type_expr(rest) else {
                continue;
            };
            let type_text = rest[..type_end].trim();
            if type_text.is_empty() {
                continue;
            }

            let after_type = rest[type_end..].trim_start();
            let mut description = after_type;
            if let Some((name_start, name_end)) = find_phpdoc_variable_token_span(after_type) {
                let variable_text = after_type[name_start..name_end].trim();
                let Some(name) = phpdoc_variable_name_from_token(variable_text) else {
                    continue;
                };
                if name != property.name {
                    continue;
                }
                description = after_type[name_end..].trim_start();
            }

            return Some(PropertyDocType {
                type_info: (tag == "@var").then(|| parsed.var_type.clone()).flatten(),
                type_text: type_text.to_string(),
                description: (!description.is_empty()).then(|| description.to_string()),
            });
        }
    }

    None
}

pub(crate) fn generated_member_native_type_hint_text(
    type_info: &php_lsp_types::TypeInfo,
    php_version: PhpVersion,
    position: TypeHintPosition,
) -> Option<String> {
    use php_lsp_types::TypeInfo;

    if let Some(native) = native_type_hint_text(type_info, php_version, position) {
        return Some(native);
    }

    match type_info {
        TypeInfo::Generic { base, .. } => {
            let base_lower = base.to_ascii_lowercase();
            match base_lower.as_str() {
                "array" | "list" | "non-empty-array" | "non-empty-list" => {
                    Some("array".to_string())
                }
                "class-string" => Some("string".to_string()),
                _ => simple_native_type_hint_text(base),
            }
        }
        TypeInfo::ArrayShape(_) => Some("array".to_string()),
        TypeInfo::ObjectShape(_) => Some("object".to_string()),
        TypeInfo::Callable { .. } => Some("callable".to_string()),
        TypeInfo::ClassString(_) => Some("string".to_string()),
        TypeInfo::LiteralString(_) => Some("string".to_string()),
        TypeInfo::LiteralInt(_) => Some("int".to_string()),
        TypeInfo::LiteralFloat(_) => Some("float".to_string()),
        TypeInfo::LiteralBool(_) => Some("bool".to_string()),
        TypeInfo::Simple(name) => match name.to_ascii_lowercase().as_str() {
            "positive-int" | "negative-int" | "non-negative-int" | "non-positive-int"
            | "non-zero-int" => Some("int".to_string()),
            "non-empty-string" | "numeric-string" | "literal-string" | "lowercase-string"
            | "class-string" => Some("string".to_string()),
            "non-empty-array" | "list" | "non-empty-list" => Some("array".to_string()),
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn property_contract_type_text(
    property: &php_lsp_types::SymbolInfo,
) -> Option<PropertyDocType> {
    let doc_type = property_doc_type(property);
    let native_type = property_type_info(property);

    if let Some(doc_type) = doc_type {
        if let (Some(doc_info), Some(native)) = (doc_type.type_info.as_ref(), native_type) {
            if type_info_refines_native(doc_info, native) {
                return Some(doc_type);
            }
        }
        if native_type.map(ToString::to_string).as_deref() != Some(doc_type.type_text.as_str()) {
            return Some(doc_type);
        }
    }

    native_type.map(|type_info| PropertyDocType {
        type_info: Some(type_info.clone()),
        type_text: type_info.to_string(),
        description: None,
    })
}

pub(crate) fn property_doc_type_needed(
    property: &php_lsp_types::SymbolInfo,
    php_version: PhpVersion,
    position: TypeHintPosition,
) -> Option<PropertyDocType> {
    let contract = property_contract_type_text(property)?;
    let native_hint = contract.type_info.as_ref().and_then(|type_info| {
        generated_member_native_type_hint_text(type_info, php_version, position)
    });

    if native_hint.as_deref() == Some(contract.type_text.as_str()) {
        None
    } else {
        Some(contract)
    }
}

pub(crate) fn type_info_contains_bool(type_info: &php_lsp_types::TypeInfo) -> bool {
    use php_lsp_types::TypeInfo;

    match type_info {
        TypeInfo::Simple(name) => matches!(name.to_ascii_lowercase().as_str(), "bool" | "boolean"),
        TypeInfo::Nullable(inner) => type_info_contains_bool(inner),
        TypeInfo::Union(types) => types.iter().any(type_info_contains_bool),
        _ => false,
    }
}

pub(crate) fn property_is_bool(property: &php_lsp_types::SymbolInfo) -> bool {
    property_type_info(property).is_some_and(type_info_contains_bool)
}

pub(crate) fn studly_identifier(raw: &str) -> String {
    let mut result = String::new();
    for part in raw
        .trim_start_matches('$')
        .split(['_', '-'])
        .filter(|part| !part.is_empty())
    {
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            result.extend(first.to_uppercase());
            result.push_str(chars.as_str());
        }
    }

    if result.is_empty() {
        "Value".to_string()
    } else {
        result
    }
}

pub(crate) fn bool_getter_name(property_name: &str) -> String {
    let mut chars = property_name.chars();
    let starts_with_is = chars.next() == Some('i')
        && chars.next() == Some('s')
        && chars.next().is_some_and(|ch| ch.is_ascii_uppercase());
    if starts_with_is {
        property_name.to_string()
    } else {
        format!("is{}", studly_identifier(property_name))
    }
}

pub(crate) fn getter_name(property: &php_lsp_types::SymbolInfo) -> String {
    if property_is_bool(property) {
        bool_getter_name(&property.name)
    } else {
        format!("get{}", studly_identifier(&property.name))
    }
}

pub(crate) fn setter_name(property: &php_lsp_types::SymbolInfo) -> String {
    format!("set{}", studly_identifier(&property.name))
}

pub(crate) fn property_default_value(
    source: &str,
    property: &php_lsp_types::SymbolInfo,
) -> Option<String> {
    let start = byte_offset_for_line_col(source, property.range.0, property.range.1)?;
    let end = byte_offset_for_line_col(source, property.range.2, property.range.3)?;
    let declaration = source.get(start..end)?;
    let needle = format!("${}", property.name);
    let name_start = declaration.find(&needle)?;
    let after_name = declaration.get(name_start + needle.len()..)?;
    let equals_offset = after_name.find('=')?;
    let before_equals = after_name.get(..equals_offset)?;
    if before_equals.contains(',') || before_equals.contains(';') {
        return None;
    }

    let mut value = String::new();
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for ch in after_name[equals_offset + 1..].chars() {
        if let Some(active_quote) = quote {
            value.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active_quote {
                quote = None;
            }
            continue;
        }

        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                value.push(ch);
            }
            '(' => {
                paren_depth += 1;
                value.push(ch);
            }
            ')' => {
                paren_depth = paren_depth.saturating_sub(1);
                value.push(ch);
            }
            '[' => {
                bracket_depth += 1;
                value.push(ch);
            }
            ']' => {
                bracket_depth = bracket_depth.saturating_sub(1);
                value.push(ch);
            }
            '{' => {
                brace_depth += 1;
                value.push(ch);
            }
            '}' => {
                brace_depth = brace_depth.saturating_sub(1);
                value.push(ch);
            }
            ',' | ';' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => break,
            _ => value.push(ch),
        }
    }

    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub(crate) struct ConstructorProperty<'a> {
    symbol: &'a php_lsp_types::SymbolInfo,
    default_value: Option<String>,
    param_default: Option<String>,
}

pub(crate) fn constructor_generation_properties<'a>(
    source: &str,
    file_symbols: &'a php_lsp_types::FileSymbols,
    class_fqn: &str,
) -> Vec<ConstructorProperty<'a>> {
    let mut properties: Vec<_> = direct_property_symbols_from_file(file_symbols, class_fqn)
        .into_iter()
        .filter(|property| !property.modifiers.is_static)
        .map(|property| ConstructorProperty {
            symbol: property,
            default_value: property_default_value(source, property),
            param_default: None,
        })
        .collect();

    properties.sort_by_key(|property| property.symbol.selection_range);

    let mut has_later_required = false;
    for property in properties.iter_mut().rev() {
        if let Some(default_value) = property.default_value.clone() {
            if !has_later_required {
                property.param_default = Some(default_value);
            }
        } else {
            has_later_required = true;
        }
    }

    properties
}

pub(crate) fn render_constructor_param(
    property: &ConstructorProperty<'_>,
    php_version: PhpVersion,
) -> String {
    let mut text = String::new();
    let contract_type = property_contract_type_text(property.symbol);
    let type_info = contract_type
        .as_ref()
        .and_then(|contract| contract.type_info.as_ref())
        .or_else(|| property_type_info(property.symbol));
    if let Some(type_info) = type_info {
        if let Some(type_text) = generated_member_native_type_hint_text(
            type_info,
            php_version,
            TypeHintPosition::Parameter,
        ) {
            text.push_str(&type_text);
            text.push(' ');
        }
    }
    text.push('$');
    text.push_str(&property.symbol.name);
    if let Some(default_value) = property.param_default.as_deref() {
        text.push_str(" = ");
        text.push_str(default_value);
    }
    text
}

pub(crate) fn render_phpdoc_type_line(
    tag: &str,
    type_text: &str,
    variable_name: Option<&str>,
    description: Option<&str>,
) -> String {
    let mut line = format!("{} {}", tag, type_text.trim());
    if let Some(variable_name) = variable_name {
        line.push(' ');
        line.push('$');
        line.push_str(variable_name);
    }
    if let Some(description) = description.filter(|description| !description.is_empty()) {
        line.push(' ');
        line.push_str(description);
    }
    line
}

pub(crate) fn render_constructor_method(
    properties: &[ConstructorProperty<'_>],
    method_indent: &str,
    body_indent: &str,
    php_version: PhpVersion,
) -> String {
    let params = properties
        .iter()
        .map(|property| render_constructor_param(property, php_version))
        .collect::<Vec<_>>()
        .join(", ");

    let mut text = String::new();
    let doc_lines = properties
        .iter()
        .filter_map(|property| {
            let doc_type = property_doc_type_needed(
                property.symbol,
                php_version,
                TypeHintPosition::Parameter,
            )?;
            Some(render_phpdoc_type_line(
                "@param",
                &doc_type.type_text,
                Some(&property.symbol.name),
                doc_type.description.as_deref(),
            ))
        })
        .collect::<Vec<_>>();
    if !doc_lines.is_empty() {
        text.push_str(&render_phpdoc_comment(method_indent, &doc_lines));
        text.push('\n');
    }
    text.push_str(method_indent);
    text.push_str("public function __construct(");
    text.push_str(&params);
    text.push_str(")\n");
    text.push_str(method_indent);
    text.push_str("{\n");
    for property in properties {
        text.push_str(body_indent);
        text.push_str("$this->");
        text.push_str(&property.symbol.name);
        text.push_str(" = $");
        text.push_str(&property.symbol.name);
        text.push_str(";\n");
    }
    text.push_str(method_indent);
    text.push_str("}\n");
    text
}

pub(crate) fn render_accessor_method(
    property: &php_lsp_types::SymbolInfo,
    accessor_kind: AccessorKind,
    method_name: &str,
    method_indent: &str,
    body_indent: &str,
    php_version: PhpVersion,
) -> String {
    let is_static = property.modifiers.is_static;
    let contract_type = property_contract_type_text(property);
    let type_hint = contract_type
        .as_ref()
        .and_then(|contract| contract.type_info.as_ref())
        .or_else(|| property_type_info(property));
    let mut text = String::new();
    let doc_type = property_doc_type_needed(
        property,
        php_version,
        match accessor_kind {
            AccessorKind::Getter => TypeHintPosition::Return,
            AccessorKind::Setter => TypeHintPosition::Parameter,
        },
    );
    if let Some(doc_type) = doc_type.as_ref() {
        let line = match accessor_kind {
            AccessorKind::Getter => render_phpdoc_type_line(
                "@return",
                &doc_type.type_text,
                None,
                doc_type.description.as_deref(),
            ),
            AccessorKind::Setter => render_phpdoc_type_line(
                "@param",
                &doc_type.type_text,
                Some(&property.name),
                doc_type.description.as_deref(),
            ),
        };
        text.push_str(&render_phpdoc_comment(method_indent, &[line]));
        text.push('\n');
    }
    text.push_str(method_indent);
    text.push_str("public ");
    if is_static {
        text.push_str("static ");
    }
    text.push_str("function ");
    text.push_str(method_name);

    match accessor_kind {
        AccessorKind::Getter => {
            text.push_str("()");
            if let Some(return_type) = type_hint.and_then(|type_info| {
                generated_member_native_type_hint_text(
                    type_info,
                    php_version,
                    TypeHintPosition::Return,
                )
            }) {
                text.push_str(": ");
                text.push_str(&return_type);
            }
            text.push('\n');
            text.push_str(method_indent);
            text.push_str("{\n");
            text.push_str(body_indent);
            text.push_str("return ");
            if is_static {
                text.push_str("self::$");
            } else {
                text.push_str("$this->");
            }
            text.push_str(&property.name);
            text.push_str(";\n");
            text.push_str(method_indent);
            text.push_str("}\n");
        }
        AccessorKind::Setter => {
            text.push('(');
            if let Some(param_type) = type_hint.and_then(|type_info| {
                generated_member_native_type_hint_text(
                    type_info,
                    php_version,
                    TypeHintPosition::Parameter,
                )
            }) {
                text.push_str(&param_type);
                text.push(' ');
            }
            text.push('$');
            text.push_str(&property.name);
            text.push_str("): void\n");
            text.push_str(method_indent);
            text.push_str("{\n");
            text.push_str(body_indent);
            if is_static {
                text.push_str("self::$");
            } else {
                text.push_str("$this->");
            }
            text.push_str(&property.name);
            text.push_str(" = $");
            text.push_str(&property.name);
            text.push_str(";\n");
            text.push_str(method_indent);
            text.push_str("}\n");
        }
    }

    text
}

pub(crate) fn generate_constructor_edit(
    uri: Uri,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    class_sym: &php_lsp_types::SymbolInfo,
    php_version: PhpVersion,
) -> Option<WorkspaceEdit> {
    if direct_method_name_exists(file_symbols, &class_sym.fqn, "__construct") {
        return Some(empty_workspace_edit());
    }
    let properties = constructor_generation_properties(source, file_symbols, &class_sym.fqn);
    if properties.is_empty() {
        return Some(empty_workspace_edit());
    }

    let insertion = class_method_insertion(source, class_sym)?;
    let constructor = render_constructor_method(
        &properties,
        &insertion.method_indent,
        &insertion.body_indent,
        php_version,
    );
    Some(generated_methods_workspace_edit(
        uri,
        insertion,
        vec![constructor],
    ))
}

pub(crate) fn generate_accessor_edit(
    uri: Uri,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    property: &php_lsp_types::SymbolInfo,
    accessor_kind: AccessorKind,
    method_name: &str,
    php_version: PhpVersion,
) -> Option<WorkspaceEdit> {
    if accessor_kind == AccessorKind::Setter && property.modifiers.is_readonly {
        return Some(empty_workspace_edit());
    }

    let class_fqn = property.parent_fqn.as_deref()?;
    if direct_method_name_exists(file_symbols, class_fqn, method_name) {
        return Some(empty_workspace_edit());
    }

    let class_sym = file_symbols
        .symbols
        .iter()
        .find(|sym| sym.fqn == class_fqn && sym.kind == php_lsp_types::PhpSymbolKind::Class)?;
    let insertion = class_method_insertion(source, class_sym)?;
    let accessor = render_accessor_method(
        property,
        accessor_kind,
        method_name,
        &insertion.method_indent,
        &insertion.body_indent,
        php_version,
    );

    Some(generated_methods_workspace_edit(
        uri,
        insertion,
        vec![accessor],
    ))
}

pub(crate) fn lsp_range_for_byte_offsets(source: &str, start: usize, end: usize) -> Range {
    let (start_line, start_byte_col) = line_col_for_byte_offset(source, start);
    let (end_line, end_byte_col) = line_col_for_byte_offset(source, end);
    let utf16_index = Utf16LineIndex::new(source);
    Range {
        start: Position::new(
            start_line,
            utf16_index.byte_col_to_utf16(start_line, start_byte_col),
        ),
        end: Position::new(
            end_line,
            utf16_index.byte_col_to_utf16(end_line, end_byte_col),
        ),
    }
}

pub(crate) fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

pub(crate) fn find_visibility_token(
    source: &str,
    symbol: &php_lsp_types::SymbolInfo,
) -> Option<(usize, usize)> {
    let start = byte_offset_for_line_col(source, symbol.range.0, symbol.range.1)?;
    let end = byte_offset_for_line_col(source, symbol.range.2, symbol.range.3)?;
    let text = source.get(start..end)?;
    for keyword in ["public", "protected", "private"] {
        let mut search_offset = 0usize;
        while let Some(relative) = text.get(search_offset..)?.find(keyword) {
            let token_start = search_offset + relative;
            let token_end = token_start + keyword.len();
            let before = token_start
                .checked_sub(1)
                .and_then(|idx| text.as_bytes().get(idx))
                .copied();
            let after = text.as_bytes().get(token_end).copied();
            if before.is_none_or(|byte| !is_ident_byte(byte))
                && after.is_none_or(|byte| !is_ident_byte(byte))
            {
                return Some((start + token_start, start + token_end));
            }
            search_offset = token_end;
        }
    }
    None
}

pub(crate) fn change_visibility_edit(
    uri: Uri,
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    source: &str,
    symbol: &php_lsp_types::SymbolInfo,
    target_visibility: php_lsp_types::Visibility,
) -> Option<WorkspaceEdit> {
    if !visibility_change_is_safe(index, file_symbols, symbol, target_visibility) {
        return Some(empty_workspace_edit());
    }

    let (start, end, new_text) =
        if let Some((token_start, token_end)) = find_visibility_token(source, symbol) {
            (
                token_start,
                token_end,
                visibility_text(target_visibility).to_string(),
            )
        } else {
            let insert_at = byte_offset_for_line_col(source, symbol.range.0, symbol.range.1)?;
            (
                insert_at,
                insert_at,
                format!("{} ", visibility_text(target_visibility)),
            )
        };

    let mut changes = HashMap::new();
    changes.insert(
        uri,
        vec![TextEdit {
            range: lsp_range_for_byte_offsets(source, start, end),
            new_text,
        }],
    );

    Some(WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    })
}

pub(crate) fn line_full_span(source: &str, start: usize, end: usize) -> (usize, usize) {
    let line_start = source[..start].rfind('\n').map(|idx| idx + 1).unwrap_or(0);
    let line_end = source[end..]
        .find('\n')
        .map(|idx| end + idx + 1)
        .unwrap_or(source.len());
    (line_start, line_end)
}

pub(crate) fn find_matching_delimiter(
    text: &str,
    open_offset: usize,
    open: char,
    close: char,
) -> Option<usize> {
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (idx, ch) in text
        .char_indices()
        .skip_while(|(idx, _)| *idx < open_offset)
    {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active_quote {
                quote = None;
            }
            continue;
        }

        match ch {
            '\'' | '"' => quote = Some(ch),
            _ if ch == open => depth += 1,
            _ if ch == close => {
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

pub(crate) fn split_top_level_spans(text: &str, base_offset: usize) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for (idx, ch) in text.char_indices() {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active_quote {
                quote = None;
            }
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
            ',' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => {
                spans.push((base_offset + start, base_offset + idx));
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }

    spans.push((base_offset + start, base_offset + text.len()));
    spans
}

pub(crate) fn variable_name_in_parameter(param_text: &str) -> Option<String> {
    let bytes = param_text.as_bytes();
    let mut idx = 0usize;
    while idx < bytes.len() {
        if bytes[idx] == b'$' {
            let start = idx + 1;
            let mut end = start;
            while end < bytes.len() && is_ident_byte(bytes[end]) {
                end += 1;
            }
            if end > start {
                return Some(param_text[start..end].to_string());
            }
        }
        idx += 1;
    }
    None
}

pub(crate) fn constructor_symbol<'a>(
    file_symbols: &'a php_lsp_types::FileSymbols,
    class_fqn: &str,
) -> Option<&'a php_lsp_types::SymbolInfo> {
    direct_method_symbols_from_file(file_symbols, class_fqn)
        .into_iter()
        .find(|method| method.name.eq_ignore_ascii_case("__construct"))
}

#[derive(Clone)]
pub(crate) struct ConstructorParamSpan {
    name: String,
    start: usize,
    end: usize,
    text: String,
}

pub(crate) fn constructor_param_spans(
    source: &str,
    constructor: &php_lsp_types::SymbolInfo,
) -> Option<Vec<ConstructorParamSpan>> {
    let start = byte_offset_for_line_col(source, constructor.range.0, constructor.range.1)?;
    let end = byte_offset_for_line_col(source, constructor.range.2, constructor.range.3)?;
    let method_text = source.get(start..end)?;
    let open_relative = method_text.find('(')?;
    let close_relative = find_matching_delimiter(method_text, open_relative, '(', ')')?;
    let params_start = start + open_relative + 1;
    let params_end = start + close_relative;
    let params_text = source.get(params_start..params_end)?;

    Some(
        split_top_level_spans(params_text, params_start)
            .into_iter()
            .filter_map(|(span_start, span_end)| {
                let raw = source.get(span_start..span_end)?;
                let text = raw.trim();
                if text.is_empty() {
                    return None;
                }
                let leading_ws = raw.len().saturating_sub(raw.trim_start().len());
                let trailing_ws = raw.len().saturating_sub(raw.trim_end().len());
                let trimmed_start = span_start + leading_ws;
                let trimmed_end = span_end.saturating_sub(trailing_ws);
                Some(ConstructorParamSpan {
                    name: variable_name_in_parameter(text)?,
                    start: trimmed_start,
                    end: trimmed_end,
                    text: text.to_string(),
                })
            })
            .collect(),
    )
}

pub(crate) fn constructor_body_span(
    source: &str,
    constructor: &php_lsp_types::SymbolInfo,
) -> Option<(usize, usize)> {
    let start = byte_offset_for_line_col(source, constructor.range.0, constructor.range.1)?;
    let end = byte_offset_for_line_col(source, constructor.range.2, constructor.range.3)?;
    let method_text = source.get(start..end)?;
    let open_paren = method_text.find('(')?;
    let close_paren = find_matching_delimiter(method_text, open_paren, '(', ')')?;
    let after_params = method_text.get(close_paren..)?;
    let open_brace_relative = after_params.find('{')? + close_paren;
    let close_brace_relative = find_matching_delimiter(method_text, open_brace_relative, '{', '}')?;
    Some((
        start + open_brace_relative + 1,
        start + close_brace_relative,
    ))
}

pub(crate) fn property_declaration_is_safe_to_remove(
    source: &str,
    property: &php_lsp_types::SymbolInfo,
) -> bool {
    let Some(range_start) = byte_offset_for_line_col(source, property.range.0, property.range.1)
    else {
        return false;
    };
    let start = find_visibility_token(source, property)
        .map(|(token_start, _)| token_start)
        .unwrap_or(range_start);
    let Some(end) = byte_offset_for_line_col(source, property.range.2, property.range.3) else {
        return false;
    };
    let Some(text) = source.get(start..end) else {
        return false;
    };
    let before_semicolon = text
        .split_once(';')
        .map(|(before, _)| before)
        .unwrap_or(text);
    !before_semicolon.contains(',')
}

pub(crate) fn property_promotion_prefix(property: &php_lsp_types::SymbolInfo) -> String {
    let mut parts = vec![visibility_text(property.visibility)];
    if property.modifiers.is_readonly {
        parts.push("readonly");
    }
    parts.join(" ")
}

pub(crate) fn adjacent_attribute_start(source: &str, declaration_start: usize) -> Option<usize> {
    let mut current = line_start_offset(source, declaration_start);
    let mut first_attribute_start = None;

    while current > 0 {
        let previous_end = current.saturating_sub(1);
        let previous_start = source[..previous_end]
            .rfind('\n')
            .map(|idx| idx + 1)
            .unwrap_or(0);
        let line = source.get(previous_start..previous_end).unwrap_or("");
        let trimmed = line.trim();
        if trimmed.starts_with("#[") {
            first_attribute_start = Some(previous_start);
            current = previous_start;
            continue;
        }
        break;
    }

    first_attribute_start
}

pub(crate) struct PropertyPromotionMetadata {
    delete_start: usize,
    doc_comment: Option<String>,
    attributes: Vec<String>,
}

pub(crate) fn property_promotion_metadata(
    source: &str,
    property: &php_lsp_types::SymbolInfo,
) -> Option<PropertyPromotionMetadata> {
    let range_start = byte_offset_for_line_col(source, property.range.0, property.range.1)?;
    let declaration_start = find_visibility_token(source, property)
        .map(|(token_start, _)| token_start)
        .unwrap_or(range_start);
    let doc_span = symbol_doc_comment_span(source, property);
    let doc_comment = property.doc_comment.clone();
    let attribute_start = if let Some((_, doc_end)) = doc_span {
        doc_end
    } else if declaration_start > range_start {
        range_start
    } else {
        adjacent_attribute_start(source, declaration_start).unwrap_or(declaration_start)
    };
    let attributes = source
        .get(attribute_start..declaration_start)
        .map(collect_attribute_groups)
        .unwrap_or_default();
    let delete_start = doc_span
        .map(|(doc_start, _)| line_start_offset(source, doc_start))
        .or_else(|| {
            if declaration_start > range_start {
                Some(line_start_offset(source, range_start))
            } else {
                adjacent_attribute_start(source, declaration_start)
            }
        })
        .unwrap_or(declaration_start);

    Some(PropertyPromotionMetadata {
        delete_start,
        doc_comment,
        attributes,
    })
}

pub(crate) fn compact_phpdoc_comment(doc_comment: &str) -> Option<String> {
    let content = phpdoc_content_lines(doc_comment)
        .into_iter()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    (!content.is_empty()).then(|| format!("/** {} */", content))
}

pub(crate) fn compact_attribute(attribute: &str) -> String {
    attribute.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn promoted_parameter_text_with_metadata(
    source: &str,
    property: &php_lsp_types::SymbolInfo,
    param_start: usize,
    promoted_param: &str,
) -> String {
    let Some(metadata) = property_promotion_metadata(source, property) else {
        return promoted_param.to_string();
    };
    if metadata.doc_comment.is_none() && metadata.attributes.is_empty() {
        return promoted_param.to_string();
    }

    let line_start = line_start_offset(source, param_start);
    let before_param = source.get(line_start..param_start).unwrap_or("");
    if before_param.trim().is_empty() {
        let indent = before_param;
        let mut text = String::new();
        if let Some(doc_comment) = metadata.doc_comment.as_deref() {
            let content_lines = phpdoc_content_lines(doc_comment);
            if !content_lines.is_empty() {
                text.push_str(&render_phpdoc_comment(indent, &content_lines));
                text.push('\n');
            }
        }
        for attribute in &metadata.attributes {
            text.push_str(&render_reindented_block(attribute, indent));
        }
        text.push_str(indent);
        text.push_str(promoted_param);
        return text;
    }

    let mut parts = Vec::new();
    if let Some(doc_comment) = metadata
        .doc_comment
        .as_deref()
        .and_then(compact_phpdoc_comment)
    {
        parts.push(doc_comment);
    }
    parts.extend(
        metadata
            .attributes
            .iter()
            .map(|attr| compact_attribute(attr)),
    );
    parts.push(promoted_param.to_string());
    parts.join(" ")
}

pub(crate) fn parameter_is_already_promoted(param_text: &str) -> bool {
    let before_var = param_text.split('$').next().unwrap_or("");
    before_var
        .split_whitespace()
        .any(|part| matches!(part, "public" | "protected" | "private"))
}

pub(crate) fn find_constructor_assignment_line(
    source: &str,
    constructor: &php_lsp_types::SymbolInfo,
    property_name: &str,
) -> Option<(usize, usize)> {
    let (body_start, body_end) = constructor_body_span(source, constructor)?;
    let body = source.get(body_start..body_end)?;
    let expected = format!("$this->{} = ${};", property_name, property_name);
    let mut matches = Vec::new();
    let mut cursor = body_start;
    for line in body.split_inclusive('\n') {
        let line_start = cursor;
        let line_end = cursor + line.len();
        cursor = line_end;
        let trimmed = line.trim();
        if trimmed == expected {
            matches.push((line_start, line_end));
        } else if trimmed.contains(&format!("$this->{}", property_name)) && trimmed.contains('=') {
            return None;
        }
    }

    if matches.len() == 1 {
        matches.into_iter().next()
    } else {
        None
    }
}

pub(crate) struct PromoteConstructorParameterPlan {
    property_delete: (usize, usize),
    param_replace: (usize, usize, String),
    assignment_delete: (usize, usize),
}

pub(crate) fn promote_constructor_parameter_plan(
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    property: &php_lsp_types::SymbolInfo,
) -> Option<PromoteConstructorParameterPlan> {
    if property.kind != php_lsp_types::PhpSymbolKind::Property
        || property.modifiers.is_static
        || !property_declaration_is_safe_to_remove(source, property)
    {
        return None;
    }
    let class_fqn = property.parent_fqn.as_deref()?;
    let constructor = constructor_symbol(file_symbols, class_fqn)?;
    let param = constructor_param_spans(source, constructor)?
        .into_iter()
        .find(|param| param.name == property.name)?;
    if parameter_is_already_promoted(&param.text) {
        return None;
    }

    let property_end = byte_offset_for_line_col(source, property.range.2, property.range.3)?;
    let metadata = property_promotion_metadata(source, property)?;
    let property_delete = line_full_span(source, metadata.delete_start, property_end);
    let assignment_delete = find_constructor_assignment_line(source, constructor, &property.name)?;
    let promoted_param = format!("{} {}", property_promotion_prefix(property), param.text);
    let promoted_param =
        promoted_parameter_text_with_metadata(source, property, param.start, &promoted_param);

    Some(PromoteConstructorParameterPlan {
        property_delete,
        param_replace: (param.start, param.end, promoted_param),
        assignment_delete,
    })
}

pub(crate) fn property_for_constructor_param_at_range<'a>(
    source: &str,
    file_symbols: &'a php_lsp_types::FileSymbols,
    range: (u32, u32, u32, u32),
) -> Option<&'a php_lsp_types::SymbolInfo> {
    let point = byte_offset_for_line_col(source, range.0, range.1)?;
    for class_sym in file_symbols
        .symbols
        .iter()
        .filter(|sym| sym.kind == php_lsp_types::PhpSymbolKind::Class)
    {
        let Some(constructor) = constructor_symbol(file_symbols, &class_sym.fqn) else {
            continue;
        };
        let Some(param) = constructor_param_spans(source, constructor).and_then(|params| {
            params
                .into_iter()
                .find(|param| point >= param.start && point <= param.end)
        }) else {
            continue;
        };
        if let Some(property) = direct_property_symbols_from_file(file_symbols, &class_sym.fqn)
            .into_iter()
            .find(|property| property.name == param.name)
        {
            return Some(property);
        }
    }
    None
}

pub(crate) fn build_promote_constructor_parameter_action(
    uri: Uri,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    property: &php_lsp_types::SymbolInfo,
    request_range: Range,
    document_version: Option<i32>,
) -> Option<CodeActionOrCommand> {
    promote_constructor_parameter_plan(source, file_symbols, property)?;
    let data = serde_json::to_value(CodeActionData {
        action_kind: CodeActionDataKind::PromoteConstructorParameter,
        uri: uri.as_str().to_string(),
        range: request_range,
        document_version,
        extra: CodeActionDataExtra::PromoteConstructorParameter {
            property_fqn: property.fqn.clone(),
        },
    })
    .ok()?;

    Some(CodeActionOrCommand::CodeAction(CodeAction {
        title: format!("Promote constructor parameter `${}`", property.name),
        kind: Some(CodeActionKind::REFACTOR_REWRITE),
        diagnostics: None,
        edit: None,
        command: None,
        is_preferred: Some(false),
        disabled: None,
        data: Some(data),
    }))
}

pub(crate) fn promote_constructor_parameter_edit(
    uri: Uri,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    property: &php_lsp_types::SymbolInfo,
) -> Option<WorkspaceEdit> {
    let plan = promote_constructor_parameter_plan(source, file_symbols, property)?;
    let mut edits = vec![
        TextEdit {
            range: lsp_range_for_byte_offsets(source, plan.param_replace.0, plan.param_replace.1),
            new_text: plan.param_replace.2,
        },
        TextEdit {
            range: lsp_range_for_byte_offsets(
                source,
                plan.assignment_delete.0,
                plan.assignment_delete.1,
            ),
            new_text: String::new(),
        },
        TextEdit {
            range: lsp_range_for_byte_offsets(
                source,
                plan.property_delete.0,
                plan.property_delete.1,
            ),
            new_text: String::new(),
        },
    ];
    edits.sort_by(|left, right| {
        (right.range.start.line, right.range.start.character)
            .cmp(&(left.range.start.line, left.range.start.character))
    });

    let mut changes = HashMap::new();
    changes.insert(uri, edits);
    Some(WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    })
}

pub(crate) fn byte_offsets_for_range(
    source: &str,
    range: (u32, u32, u32, u32),
) -> Option<(usize, usize)> {
    let start = byte_offset_for_line_col(source, range.0, range.1)?;
    let end = byte_offset_for_line_col(source, range.2, range.3)?;
    Some((start.min(end), end.max(start)))
}

pub(crate) fn trimmed_byte_offsets(
    source: &str,
    start: usize,
    end: usize,
) -> Option<(usize, usize)> {
    let text = source.get(start..end)?;
    let leading = text.len().saturating_sub(text.trim_start().len());
    let trailing = text.len().saturating_sub(text.trim_end().len());
    let trimmed_start = start + leading;
    let trimmed_end = end.saturating_sub(trailing);
    (trimmed_start < trimmed_end).then_some((trimmed_start, trimmed_end))
}

pub(crate) fn selected_named_node_exact<'tree>(
    tree: &'tree tree_sitter::Tree,
    source: &str,
    range: (u32, u32, u32, u32),
) -> Option<tree_sitter::Node<'tree>> {
    let (start, end) = byte_offsets_for_range(source, range)?;
    let (start, end) = trimmed_byte_offsets(source, start, end)?;
    let root = tree.root_node();
    let mut node = root.descendant_for_byte_range(start, end)?;

    while !node.is_named() {
        node = node.parent()?;
    }

    let mut current = Some(node);
    while let Some(candidate) = current {
        if candidate.is_named() && candidate.start_byte() == start && candidate.end_byte() == end {
            return Some(candidate);
        }
        current = candidate.parent();
    }

    None
}

pub(crate) fn node_contains_node(outer: tree_sitter::Node, inner: tree_sitter::Node) -> bool {
    outer.start_byte() <= inner.start_byte() && outer.end_byte() >= inner.end_byte()
}

pub(crate) fn is_refactor_scope_boundary(node: tree_sitter::Node) -> bool {
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

pub(crate) fn nearest_local_refactor_scope<'tree>(
    mut node: tree_sitter::Node<'tree>,
) -> Option<tree_sitter::Node<'tree>> {
    loop {
        if matches!(
            node.kind(),
            "method_declaration"
                | "function_definition"
                | "arrow_function"
                | "anonymous_function"
                | "anonymous_function_creation_expression"
        ) {
            return Some(node);
        }
        node = node.parent()?;
    }
}

pub(crate) fn collect_variable_names_for_refactor(
    node: tree_sitter::Node,
    scope_id: usize,
    source: &str,
    names: &mut HashSet<String>,
) {
    if node.id() != scope_id && is_refactor_scope_boundary(node) {
        return;
    }

    if node.kind() == "variable_name" {
        let text = source.get(node.byte_range()).unwrap_or("").trim();
        if let Some(name) = text.strip_prefix('$') {
            names.insert(name.to_string());
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_variable_names_for_refactor(child, scope_id, source, names);
    }
}

pub(crate) fn unique_local_variable_name(
    tree: &tree_sitter::Tree,
    source: &str,
    selected_node: tree_sitter::Node,
) -> String {
    let root = tree.root_node();
    let scope = nearest_local_refactor_scope(selected_node).unwrap_or(root);
    let mut names = HashSet::new();
    collect_variable_names_for_refactor(scope, scope.id(), source, &mut names);

    let base = "extracted";
    if !names.contains(base) {
        return base.to_string();
    }

    for suffix in 2.. {
        let candidate = format!("{}{}", base, suffix);
        if !names.contains(&candidate) {
            return candidate;
        }
    }

    unreachable!("unbounded suffix search should always find a variable name")
}

pub(crate) fn requested_variable_name(raw: &str) -> Option<String> {
    let normalized = normalize_variable_new_name(raw)?;
    Some(normalized.trim_start_matches('$').to_string())
}

pub(crate) fn is_php_statement_node(node: tree_sitter::Node) -> bool {
    node.kind().ends_with("_statement")
}

pub(crate) fn enclosing_statement_for_refactor<'tree>(
    mut node: tree_sitter::Node<'tree>,
) -> Option<tree_sitter::Node<'tree>> {
    loop {
        if is_php_statement_node(node) {
            return Some(node);
        }
        if node.kind() == "program" {
            return None;
        }
        node = node.parent()?;
    }
}

pub(crate) fn statement_container_id(statement: tree_sitter::Node) -> Option<usize> {
    let parent = statement.parent()?;
    matches!(parent.kind(), "compound_statement" | "program").then_some(parent.id())
}

pub(crate) fn is_assignment_left_context(node: tree_sitter::Node) -> bool {
    let mut current = Some(node);
    while let Some(candidate) = current {
        let Some(parent) = candidate.parent() else {
            return false;
        };
        if matches!(
            parent.kind(),
            "assignment_expression" | "by_ref_assignment_expression"
        ) {
            return parent
                .child_by_field_name("left")
                .is_some_and(|left| node_contains_node(left, node));
        }
        if is_php_statement_node(parent) || is_refactor_scope_boundary(parent) {
            return false;
        }
        current = Some(parent);
    }
    false
}

pub(crate) fn is_extractable_expression_node(node: tree_sitter::Node) -> bool {
    matches!(
        node.kind(),
        "array_creation_expression"
            | "binary_expression"
            | "boolean"
            | "cast_expression"
            | "class_constant_access_expression"
            | "conditional_expression"
            | "encapsed_string"
            | "false"
            | "float"
            | "function_call_expression"
            | "integer"
            | "member_access_expression"
            | "member_call_expression"
            | "name"
            | "null"
            | "object_creation_expression"
            | "parenthesized_expression"
            | "qualified_name"
            | "scoped_call_expression"
            | "scoped_property_access_expression"
            | "string"
            | "subscript_expression"
            | "true"
            | "unary_op_expression"
            | "variable_name"
    )
}

pub(crate) fn selected_extract_expression<'tree>(
    tree: &'tree tree_sitter::Tree,
    source: &str,
    range: (u32, u32, u32, u32),
) -> Option<tree_sitter::Node<'tree>> {
    let node = selected_named_node_exact(tree, source, range)?;
    if is_extractable_expression_node(node) && !is_assignment_left_context(node) {
        Some(node)
    } else {
        None
    }
}

pub(crate) struct ExtractVariablePlan {
    variable_name: String,
    assignment_insert: usize,
    assignment_text: String,
    expression_start: usize,
    expression_end: usize,
}

pub(crate) fn extract_variable_plan(
    tree: &tree_sitter::Tree,
    source: &str,
    range: (u32, u32, u32, u32),
    variable_name: Option<&str>,
) -> Option<ExtractVariablePlan> {
    let expression = selected_extract_expression(tree, source, range)?;
    let statement = enclosing_statement_for_refactor(expression)?;
    statement_container_id(statement)?;

    let expression_text = source
        .get(expression.start_byte()..expression.end_byte())?
        .trim();
    if expression_text.is_empty() || expression_text.contains(['\n', '\r']) {
        return None;
    }

    let variable_name = match variable_name {
        Some(name) => requested_variable_name(name)?,
        None => unique_local_variable_name(tree, source, expression),
    };
    let assignment_insert = line_start_offset(source, statement.start_byte());
    let indent = line_indent_at_offset(source, statement.start_byte());
    let assignment_text = format!("{}${} = {};\n", indent, variable_name, expression_text);

    Some(ExtractVariablePlan {
        variable_name,
        assignment_insert,
        assignment_text,
        expression_start: expression.start_byte(),
        expression_end: expression.end_byte(),
    })
}

pub(crate) fn build_extract_variable_action(
    uri: Uri,
    tree: &tree_sitter::Tree,
    source: &str,
    request_range: Range,
    document_version: Option<i32>,
) -> Option<CodeActionOrCommand> {
    let range = lsp_range_to_byte_range(source, request_range);
    let plan = extract_variable_plan(tree, source, range, None)?;
    let data = serde_json::to_value(CodeActionData {
        action_kind: CodeActionDataKind::ExtractVariable,
        uri: uri.as_str().to_string(),
        range: request_range,
        document_version,
        extra: CodeActionDataExtra::ExtractVariable {
            variable_name: plan.variable_name.clone(),
        },
    })
    .ok()?;

    Some(CodeActionOrCommand::CodeAction(CodeAction {
        title: format!("Extract variable `${}`", plan.variable_name),
        kind: Some(CodeActionKind::REFACTOR_EXTRACT),
        diagnostics: None,
        edit: None,
        command: None,
        is_preferred: Some(false),
        disabled: None,
        data: Some(data),
    }))
}

pub(crate) fn workspace_edit_from_text_edits(uri: Uri, mut edits: Vec<TextEdit>) -> WorkspaceEdit {
    edits.sort_by(|left, right| {
        (right.range.start.line, right.range.start.character)
            .cmp(&(left.range.start.line, left.range.start.character))
    });

    let mut changes = HashMap::new();
    changes.insert(uri, edits);
    WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    }
}

pub(crate) fn extract_variable_edit(
    uri: Uri,
    tree: &tree_sitter::Tree,
    source: &str,
    range: (u32, u32, u32, u32),
    variable_name: &str,
) -> Option<WorkspaceEdit> {
    let plan = extract_variable_plan(tree, source, range, Some(variable_name))?;
    Some(workspace_edit_from_text_edits(
        uri,
        vec![
            TextEdit {
                range: lsp_range_for_byte_offsets(
                    source,
                    plan.assignment_insert,
                    plan.assignment_insert,
                ),
                new_text: plan.assignment_text,
            },
            TextEdit {
                range: lsp_range_for_byte_offsets(
                    source,
                    plan.expression_start,
                    plan.expression_end,
                ),
                new_text: format!("${}", plan.variable_name),
            },
        ],
    ))
}

pub(crate) fn class_symbol_at_range(
    file_symbols: &php_lsp_types::FileSymbols,
    range: (u32, u32, u32, u32),
) -> Option<&php_lsp_types::SymbolInfo> {
    file_symbols
        .symbols
        .iter()
        .filter(|sym| sym.kind == php_lsp_types::PhpSymbolKind::Class)
        .filter(|sym| byte_range_contains(sym.range, range))
        .min_by_key(|sym| {
            (
                sym.range.2.saturating_sub(sym.range.0),
                sym.range.3.saturating_sub(sym.range.1),
            )
        })
}

pub(crate) fn direct_class_constant_name_exists(
    file_symbols: &php_lsp_types::FileSymbols,
    class_fqn: &str,
    constant_name: &str,
) -> bool {
    file_symbols.symbols.iter().any(|sym| {
        sym.kind == php_lsp_types::PhpSymbolKind::ClassConstant
            && sym.parent_fqn.as_deref() == Some(class_fqn)
            && sym.name.eq_ignore_ascii_case(constant_name)
    })
}

pub(crate) fn unique_class_constant_name(
    file_symbols: &php_lsp_types::FileSymbols,
    class_fqn: &str,
) -> String {
    let base = "EXTRACTED";
    if !direct_class_constant_name_exists(file_symbols, class_fqn, base) {
        return base.to_string();
    }

    for suffix in 2.. {
        let candidate = format!("{}{}", base, suffix);
        if !direct_class_constant_name_exists(file_symbols, class_fqn, &candidate) {
            return candidate;
        }
    }

    unreachable!("unbounded suffix search should always find a constant name")
}

pub(crate) fn is_numeric_literal_text(text: &str) -> bool {
    let normalized = text.trim().trim_start_matches(['+', '-']).replace('_', "");
    !normalized.is_empty()
        && (normalized.parse::<i64>().is_ok() || normalized.parse::<f64>().is_ok())
}

pub(crate) fn is_extract_constant_literal_node(source: &str, node: tree_sitter::Node) -> bool {
    let raw = source.get(node.byte_range()).unwrap_or("").trim();
    let lower = raw.to_ascii_lowercase();
    matches!(lower.as_str(), "true" | "false" | "null")
        || is_numeric_literal_text(raw)
        || is_static_string_literal_node(node)
}

pub(crate) struct ExtractConstantPlan {
    constant_name: String,
    insert_position: Position,
    insert_text: String,
    expression_start: usize,
    expression_end: usize,
}

pub(crate) struct ClassConstantInsertion {
    position: Position,
    member_indent: String,
    needs_leading_newline: bool,
    needs_trailing_blank: bool,
}

pub(crate) fn class_constant_insertion(
    source: &str,
    class_sym: &php_lsp_types::SymbolInfo,
) -> Option<ClassConstantInsertion> {
    let start = byte_offset_for_line_col(source, class_sym.range.0, class_sym.range.1)?;
    let end = byte_offset_for_line_col(source, class_sym.range.2, class_sym.range.3)?;
    let class_text = source.get(start..end)?;
    let open_brace = start + class_text.find('{')?;
    let open_line_end = line_end_offset(source, open_brace);
    let has_open_line_break = source.as_bytes().get(open_line_end) == Some(&b'\n');
    let insert_byte = if has_open_line_break {
        open_line_end + 1
    } else {
        open_brace + 1
    };
    let (line, byte_col) = line_col_for_byte_offset(source, insert_byte);
    let utf16_index = Utf16LineIndex::new(source);
    let position = Position::new(line, utf16_index.byte_col_to_utf16(line, byte_col));

    let open_line = line_text(source, line_col_for_byte_offset(source, open_brace).0);
    let open_col = line_col_for_byte_offset(source, open_brace).1;
    let class_indent = leading_ascii_whitespace(line_prefix_by_byte_col(open_line, open_col));
    let member_indent = format!("{}    ", class_indent);
    let needs_trailing_blank = source
        .get(insert_byte..end)
        .map(|after| !after.trim_start().starts_with('}'))
        .unwrap_or(false);

    Some(ClassConstantInsertion {
        position,
        member_indent,
        needs_leading_newline: !has_open_line_break,
        needs_trailing_blank,
    })
}

pub(crate) fn valid_constant_name(raw: &str) -> Option<String> {
    let name = raw.trim().to_ascii_uppercase();
    if name.is_empty() {
        return None;
    }
    let mut chars = name.chars();
    let first = chars.next()?;
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return None;
    }
    if !chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric()) {
        return None;
    }
    Some(name)
}

pub(crate) fn extract_constant_plan(
    tree: &tree_sitter::Tree,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    range: (u32, u32, u32, u32),
    constant_name: Option<&str>,
) -> Option<ExtractConstantPlan> {
    let literal = selected_named_node_exact(tree, source, range)?;
    if !is_extract_constant_literal_node(source, literal) {
        return None;
    }
    let class_sym = class_symbol_at_range(file_symbols, node_range_node(literal))?;
    let constant_name = match constant_name {
        Some(name) => valid_constant_name(name)?,
        None => unique_class_constant_name(file_symbols, &class_sym.fqn),
    };
    if direct_class_constant_name_exists(file_symbols, &class_sym.fqn, &constant_name) {
        return None;
    }

    let literal_text = source.get(literal.byte_range())?.trim();
    if literal_text.contains(['\n', '\r']) {
        return None;
    }

    let insertion = class_constant_insertion(source, class_sym)?;
    let mut insert_text = String::new();
    if insertion.needs_leading_newline {
        insert_text.push('\n');
    }
    insert_text.push_str(&insertion.member_indent);
    insert_text.push_str("private const ");
    insert_text.push_str(&constant_name);
    insert_text.push_str(" = ");
    insert_text.push_str(literal_text);
    insert_text.push_str(";\n");
    if insertion.needs_trailing_blank {
        insert_text.push('\n');
    }

    Some(ExtractConstantPlan {
        constant_name,
        insert_position: insertion.position,
        insert_text,
        expression_start: literal.start_byte(),
        expression_end: literal.end_byte(),
    })
}

pub(crate) fn build_extract_constant_action(
    uri: Uri,
    tree: &tree_sitter::Tree,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    request_range: Range,
    document_version: Option<i32>,
) -> Option<CodeActionOrCommand> {
    let range = lsp_range_to_byte_range(source, request_range);
    let plan = extract_constant_plan(tree, source, file_symbols, range, None)?;
    let data = serde_json::to_value(CodeActionData {
        action_kind: CodeActionDataKind::ExtractConstant,
        uri: uri.as_str().to_string(),
        range: request_range,
        document_version,
        extra: CodeActionDataExtra::ExtractConstant {
            constant_name: plan.constant_name.clone(),
        },
    })
    .ok()?;

    Some(CodeActionOrCommand::CodeAction(CodeAction {
        title: format!("Extract constant `{}`", plan.constant_name),
        kind: Some(CodeActionKind::REFACTOR_EXTRACT),
        diagnostics: None,
        edit: None,
        command: None,
        is_preferred: Some(false),
        disabled: None,
        data: Some(data),
    }))
}

pub(crate) fn extract_constant_edit(
    uri: Uri,
    tree: &tree_sitter::Tree,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    range: (u32, u32, u32, u32),
    constant_name: &str,
) -> Option<WorkspaceEdit> {
    let plan = extract_constant_plan(tree, source, file_symbols, range, Some(constant_name))?;
    Some(workspace_edit_from_text_edits(
        uri,
        vec![
            TextEdit {
                range: Range {
                    start: plan.insert_position,
                    end: plan.insert_position,
                },
                new_text: plan.insert_text,
            },
            TextEdit {
                range: lsp_range_for_byte_offsets(
                    source,
                    plan.expression_start,
                    plan.expression_end,
                ),
                new_text: format!("self::{}", plan.constant_name),
            },
        ],
    ))
}

pub(crate) fn variable_name_node_at_range<'tree>(
    tree: &'tree tree_sitter::Tree,
    source: &str,
    range: (u32, u32, u32, u32),
) -> Option<tree_sitter::Node<'tree>> {
    let (start, end) = byte_offsets_for_range(source, range)?;
    let root = tree.root_node();
    let mut node = if start == end {
        let point = tree_sitter::Point::new(range.0 as usize, range.1 as usize);
        root.descendant_for_point_range(point, point)?
    } else {
        root.descendant_for_byte_range(start, end)?
    };

    while !node.is_named() {
        node = node.parent()?;
    }

    let mut current = Some(node);
    while let Some(candidate) = current {
        if candidate.kind() == "variable_name" {
            return Some(candidate);
        }
        current = candidate.parent();
    }
    None
}

pub(crate) fn variable_text_for_node(source: &str, node: tree_sitter::Node) -> Option<String> {
    let text = source.get(node.byte_range())?.trim();
    text.starts_with('$').then(|| text.to_string())
}

pub(crate) struct InlineAssignment {
    statement_start: usize,
    statement_end: usize,
    statement_container_id: usize,
    rhs_start: usize,
    rhs_end: usize,
}

pub(crate) struct InlineRead {
    start: usize,
    end: usize,
    statement_start: usize,
    statement_container_id: usize,
}

pub(crate) fn simple_inline_assignment_from_statement(
    statement: tree_sitter::Node,
    source: &str,
    variable_name: &str,
) -> Option<InlineAssignment> {
    if statement.kind() != "expression_statement" {
        return None;
    }
    let expression = statement.named_child(0)?;
    if expression.kind() != "assignment_expression" {
        return None;
    }
    let left = expression.child_by_field_name("left")?;
    let right = expression.child_by_field_name("right")?;
    if left.kind() != "variable_name" || variable_text_for_node(source, left)? != variable_name {
        return None;
    }
    let operator_text = source.get(left.end_byte()..right.start_byte())?.trim();
    if operator_text != "=" {
        return None;
    }

    Some(InlineAssignment {
        statement_start: statement.start_byte(),
        statement_end: statement.end_byte(),
        statement_container_id: statement_container_id(statement)?,
        rhs_start: right.start_byte(),
        rhs_end: right.end_byte(),
    })
}

pub(crate) fn collect_inline_assignments(
    node: tree_sitter::Node,
    scope_id: usize,
    source: &str,
    variable_name: &str,
    assignments: &mut Vec<InlineAssignment>,
) {
    if node.id() != scope_id && is_refactor_scope_boundary(node) {
        return;
    }
    if let Some(assignment) = simple_inline_assignment_from_statement(node, source, variable_name) {
        assignments.push(assignment);
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_inline_assignments(child, scope_id, source, variable_name, assignments);
    }
}

pub(crate) fn collect_inline_reads(
    node: tree_sitter::Node,
    scope_id: usize,
    source: &str,
    variable_name: &str,
    reads: &mut Vec<InlineRead>,
) {
    if node.id() != scope_id && is_refactor_scope_boundary(node) {
        return;
    }

    if node.kind() == "variable_name"
        && variable_text_for_node(source, node).as_deref() == Some(variable_name)
        && !is_assignment_left_context(node)
    {
        if let Some(statement) = enclosing_statement_for_refactor(node) {
            if let Some(container_id) = statement_container_id(statement) {
                reads.push(InlineRead {
                    start: node.start_byte(),
                    end: node.end_byte(),
                    statement_start: statement.start_byte(),
                    statement_container_id: container_id,
                });
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_inline_reads(child, scope_id, source, variable_name, reads);
    }
}

pub(crate) fn inline_replacement_is_atomic(node: tree_sitter::Node, source: &str) -> bool {
    let raw = source.get(node.byte_range()).unwrap_or("").trim();
    let lower = raw.to_ascii_lowercase();
    matches!(lower.as_str(), "true" | "false" | "null")
        || is_numeric_literal_text(raw)
        || matches!(
            node.kind(),
            "array_creation_expression"
                | "boolean"
                | "class_constant_access_expression"
                | "encapsed_string"
                | "float"
                | "function_call_expression"
                | "integer"
                | "member_access_expression"
                | "member_call_expression"
                | "name"
                | "null"
                | "object_creation_expression"
                | "parenthesized_expression"
                | "qualified_name"
                | "scoped_call_expression"
                | "scoped_property_access_expression"
                | "string"
                | "subscript_expression"
                | "variable_name"
        )
}

pub(crate) fn inline_replacement_text_for_node(
    source: &str,
    rhs: tree_sitter::Node,
) -> Option<String> {
    let raw = source.get(rhs.byte_range())?.trim();
    if raw.is_empty() || raw.contains(['\n', '\r']) {
        return None;
    }
    if inline_replacement_is_atomic(rhs, source) {
        Some(raw.to_string())
    } else {
        Some(format!("({})", raw))
    }
}

pub(crate) struct InlineVariablePlan {
    variable_name: String,
    assignment_delete: (usize, usize),
    usage_replacements: Vec<(usize, usize, String)>,
}

pub(crate) fn inline_variable_plan(
    tree: &tree_sitter::Tree,
    source: &str,
    range: (u32, u32, u32, u32),
    variable_name: Option<&str>,
) -> Option<InlineVariablePlan> {
    let selected_variable = variable_name_node_at_range(tree, source, range)?;
    let selected_name = variable_text_for_node(source, selected_variable)?;
    if !is_renameable_variable(&selected_name) {
        return None;
    }
    if let Some(requested) = variable_name {
        let requested = normalize_variable_new_name(requested)?;
        if requested != selected_name {
            return None;
        }
    }

    let root = tree.root_node();
    let scope = nearest_local_refactor_scope(selected_variable).unwrap_or(root);
    let mut assignments = Vec::new();
    collect_inline_assignments(scope, scope.id(), source, &selected_name, &mut assignments);
    if assignments.len() != 1 {
        return None;
    }

    let mut reads = Vec::new();
    collect_inline_reads(scope, scope.id(), source, &selected_name, &mut reads);
    if reads.is_empty() {
        return None;
    }

    let assignment = assignments.into_iter().next()?;
    if !reads.iter().all(|read| {
        assignment.statement_container_id == read.statement_container_id
            && assignment.statement_end <= read.statement_start
    }) {
        return None;
    }

    let rhs_node = tree
        .root_node()
        .descendant_for_byte_range(assignment.rhs_start, assignment.rhs_end)?;
    if source
        .get(assignment.rhs_start..assignment.rhs_end)?
        .contains(&selected_name)
    {
        return None;
    }
    let replacement = inline_replacement_text_for_node(source, rhs_node)?;
    let assignment_delete =
        line_full_span(source, assignment.statement_start, assignment.statement_end);
    let usage_replacements = reads
        .into_iter()
        .map(|read| (read.start, read.end, replacement.clone()))
        .collect();

    Some(InlineVariablePlan {
        variable_name: selected_name,
        assignment_delete,
        usage_replacements,
    })
}

pub(crate) fn build_inline_variable_action(
    uri: Uri,
    tree: &tree_sitter::Tree,
    source: &str,
    request_range: Range,
    document_version: Option<i32>,
) -> Option<CodeActionOrCommand> {
    let range = lsp_range_to_byte_range(source, request_range);
    let plan = inline_variable_plan(tree, source, range, None)?;
    let data = serde_json::to_value(CodeActionData {
        action_kind: CodeActionDataKind::InlineVariable,
        uri: uri.as_str().to_string(),
        range: request_range,
        document_version,
        extra: CodeActionDataExtra::InlineVariable {
            variable_name: plan.variable_name.clone(),
        },
    })
    .ok()?;

    Some(CodeActionOrCommand::CodeAction(CodeAction {
        title: format!("Inline variable `{}`", plan.variable_name),
        kind: Some(CodeActionKind::REFACTOR_INLINE),
        diagnostics: None,
        edit: None,
        command: None,
        is_preferred: Some(false),
        disabled: None,
        data: Some(data),
    }))
}

pub(crate) fn inline_variable_edit(
    uri: Uri,
    tree: &tree_sitter::Tree,
    source: &str,
    range: (u32, u32, u32, u32),
    variable_name: &str,
) -> Option<WorkspaceEdit> {
    let plan = inline_variable_plan(tree, source, range, Some(variable_name))?;
    let mut edits = plan
        .usage_replacements
        .into_iter()
        .map(|(start, end, replacement)| TextEdit {
            range: lsp_range_for_byte_offsets(source, start, end),
            new_text: replacement,
        })
        .collect::<Vec<_>>();
    edits.push(TextEdit {
        range: lsp_range_for_byte_offsets(
            source,
            plan.assignment_delete.0,
            plan.assignment_delete.1,
        ),
        new_text: String::new(),
    });

    Some(workspace_edit_from_text_edits(uri, edits))
}

pub(crate) fn callable_symbol_at_range(
    file_symbols: &php_lsp_types::FileSymbols,
    range: (u32, u32, u32, u32),
) -> Option<&php_lsp_types::SymbolInfo> {
    file_symbols
        .symbols
        .iter()
        .filter(|sym| {
            matches!(
                sym.kind,
                php_lsp_types::PhpSymbolKind::Function | php_lsp_types::PhpSymbolKind::Method
            ) && !sym.modifiers.is_builtin
        })
        .find(|sym| {
            byte_range_contains(sym.range, range) || byte_ranges_overlap(sym.selection_range, range)
        })
}

#[derive(Clone)]
pub(crate) struct DesiredPhpDocParam {
    name: String,
    type_text: String,
    variable_text: String,
    description: Option<String>,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct DesiredPhpDocReturn {
    type_text: String,
    description: Option<String>,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum PhpDocReturnUpdate {
    Preserve,
    Remove,
    Replace(DesiredPhpDocReturn),
}

pub(crate) struct UpdatePhpDocPlan {
    start: usize,
    end: usize,
    new_text: String,
}

pub(crate) fn phpdoc_line_starts_with_tag(line: &str, tag: &str) -> bool {
    let trimmed = line.trim_start();
    let Some(rest) = trimmed.strip_prefix(tag) else {
        return false;
    };
    rest.is_empty() || rest.chars().next().is_some_and(|ch| ch.is_whitespace())
}

pub(crate) fn phpdoc_line_is_tag(line: &str) -> bool {
    line.trim_start().starts_with('@')
}

pub(crate) fn phpdoc_content_lines(doc_comment: &str) -> Vec<String> {
    let raw_lines: Vec<&str> = doc_comment.lines().collect();
    let mut lines = Vec::new();

    for raw in raw_lines.iter() {
        let trimmed_start = raw.trim_start();
        if let Some(rest) = trimmed_start.strip_prefix("/**") {
            let rest = rest.trim_start();
            let rest = rest.strip_suffix("*/").map(str::trim_end).unwrap_or(rest);
            if !rest.is_empty() {
                lines.push(rest.to_string());
            }
            continue;
        }

        if trimmed_start.starts_with("*/") {
            continue;
        }

        if let Some(rest) = trimmed_start.strip_prefix('*') {
            lines.push(
                rest.strip_prefix(' ')
                    .unwrap_or(rest)
                    .trim_end()
                    .to_string(),
            );
        } else {
            lines.push(trimmed_start.trim_end().to_string());
        }
    }

    lines
}

pub(crate) fn next_non_whitespace(text: &str, start: usize) -> Option<char> {
    text.get(start..)?.chars().find(|ch| !ch.is_whitespace())
}

pub(crate) fn consume_phpdoc_type_expr(rest: &str) -> Option<usize> {
    let mut paren_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut last_significant: Option<char> = None;
    let mut end = 0usize;

    for (idx, ch) in rest.char_indices() {
        let ch_end = idx + ch.len_utf8();

        if let Some(quote_ch) = quote {
            end = ch_end;
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
            last_significant = Some(ch);
            end = ch_end;
            continue;
        }

        let nested = paren_depth > 0 || angle_depth > 0 || bracket_depth > 0 || brace_depth > 0;
        if ch.is_whitespace() && !nested {
            let next = next_non_whitespace(rest, ch_end);
            if matches!(next, Some('|') | Some('&'))
                || matches!(last_significant, Some('|') | Some('&') | Some(':'))
            {
                end = ch_end;
                continue;
            }
            break;
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

        if !ch.is_whitespace() {
            last_significant = Some(ch);
        }
        end = ch_end;
    }

    (end > 0).then_some(end)
}

pub(crate) fn find_phpdoc_variable_token_span(rest: &str) -> Option<(usize, usize)> {
    let mut paren_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for (idx, ch) in rest.char_indices() {
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
        if ch == '$' && !nested {
            let mut name_end = idx + ch.len_utf8();
            let mut has_name = false;
            for (offset, name_ch) in rest[name_end..].char_indices() {
                if name_ch.is_ascii_alphanumeric() || name_ch == '_' {
                    has_name = true;
                    name_end = idx + ch.len_utf8() + offset + name_ch.len_utf8();
                } else {
                    break;
                }
            }

            if !has_name {
                continue;
            }

            let prefix = &rest[..idx];
            let name_start = if prefix.ends_with("&...") {
                idx - 4
            } else if prefix.ends_with("...") {
                idx - 3
            } else if prefix.ends_with('&') {
                idx - 1
            } else {
                idx
            };

            return Some((name_start, name_end));
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

pub(crate) fn phpdoc_tag_rest<'a>(line: &'a str, tag: &str) -> Option<&'a str> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix(tag)?;
    if rest.is_empty() || rest.chars().next().is_some_and(|ch| ch.is_whitespace()) {
        Some(rest.trim_start())
    } else {
        None
    }
}

pub(crate) fn phpdoc_variable_name_from_token(token: &str) -> Option<String> {
    let token = token
        .trim()
        .strip_prefix("&...")
        .or_else(|| token.trim().strip_prefix("..."))
        .or_else(|| token.trim().strip_prefix('&'))
        .unwrap_or_else(|| token.trim());
    let name = token.strip_prefix('$')?;
    if name.is_empty()
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return None;
    }
    Some(name.to_string())
}

pub(crate) fn existing_phpdoc_param_variable_texts(
    doc_comment: Option<&str>,
) -> HashMap<String, String> {
    let mut variables = HashMap::new();
    let Some(doc_comment) = doc_comment else {
        return variables;
    };

    for line in phpdoc_content_lines(doc_comment) {
        let Some(rest) = phpdoc_tag_rest(&line, "@param") else {
            continue;
        };
        let Some((start, end)) = find_phpdoc_variable_token_span(rest) else {
            continue;
        };
        let variable_text = rest[start..end].trim().to_string();
        if let Some(name) = phpdoc_variable_name_from_token(&variable_text) {
            variables.entry(name).or_insert(variable_text);
        }
    }

    variables
}

pub(crate) fn existing_phpdoc_return_description(doc_comment: Option<&str>) -> Option<String> {
    let doc_comment = doc_comment?;
    for line in phpdoc_content_lines(doc_comment) {
        let Some(rest) = phpdoc_tag_rest(&line, "@return") else {
            continue;
        };
        let end = consume_phpdoc_type_expr(rest)?;
        let description = rest[end..].trim();
        return (!description.is_empty()).then(|| description.to_string());
    }

    None
}

pub(crate) fn normalize_phpdoc_content_lines(lines: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut previous_blank = true;

    for line in lines {
        let line = line.trim_end().to_string();
        let is_blank = line.trim().is_empty();
        if is_blank {
            if !previous_blank {
                out.push(String::new());
            }
            previous_blank = true;
        } else {
            out.push(line);
            previous_blank = false;
        }
    }

    while out.last().is_some_and(|line| line.trim().is_empty()) {
        out.pop();
    }

    out
}

pub(crate) fn phpdoc_managed_insert_index(lines: &[String]) -> usize {
    lines
        .iter()
        .position(|line| phpdoc_line_is_tag(line))
        .unwrap_or(lines.len())
}

pub(crate) fn render_phpdoc_param_line(param: &DesiredPhpDocParam) -> String {
    let mut line = format!(
        "@param {} {}",
        param.type_text.trim(),
        param.variable_text.trim()
    );
    if let Some(description) = param.description.as_deref().filter(|desc| !desc.is_empty()) {
        line.push(' ');
        line.push_str(description);
    }
    line
}

pub(crate) fn render_managed_phpdoc_lines(
    params: &[DesiredPhpDocParam],
    return_update: &PhpDocReturnUpdate,
) -> Vec<String> {
    let mut lines = params
        .iter()
        .map(render_phpdoc_param_line)
        .collect::<Vec<_>>();
    if let PhpDocReturnUpdate::Replace(return_doc) = return_update {
        let mut line = format!("@return {}", return_doc.type_text.trim());
        if let Some(description) = return_doc
            .description
            .as_deref()
            .filter(|desc| !desc.is_empty())
        {
            line.push(' ');
            line.push_str(description);
        }
        lines.push(line);
    }
    lines
}

pub(crate) fn update_phpdoc_content_lines(
    existing_lines: Vec<String>,
    managed_lines: Vec<String>,
    manage_return: bool,
) -> Vec<String> {
    let mut filtered = Vec::new();
    let mut insert_at = None;

    for line in existing_lines {
        let managed = phpdoc_line_starts_with_tag(&line, "@param")
            || (manage_return && phpdoc_line_starts_with_tag(&line, "@return"));
        if managed {
            if insert_at.is_none() {
                insert_at = Some(filtered.len());
            }
            continue;
        }
        filtered.push(line);
    }

    let insert_at = insert_at.unwrap_or_else(|| phpdoc_managed_insert_index(&filtered));
    let mut out = Vec::new();
    out.extend(filtered[..insert_at].iter().cloned());
    if !managed_lines.is_empty() {
        if out.last().is_some_and(|line| !line.trim().is_empty()) {
            out.push(String::new());
        }
        out.extend(managed_lines);
    }
    out.extend(filtered[insert_at..].iter().cloned());

    normalize_phpdoc_content_lines(out)
}

pub(crate) fn render_phpdoc_comment(indent: &str, content_lines: &[String]) -> String {
    let mut text = String::new();
    text.push_str(indent);
    text.push_str("/**\n");
    for line in content_lines {
        text.push_str(indent);
        if line.trim().is_empty() {
            text.push_str(" *\n");
        } else {
            text.push_str(" * ");
            text.push_str(line);
            text.push('\n');
        }
    }
    text.push_str(indent);
    text.push_str(" */");
    text
}

pub(crate) fn line_start_offset(source: &str, offset: usize) -> usize {
    source[..offset].rfind('\n').map(|idx| idx + 1).unwrap_or(0)
}

pub(crate) fn line_end_offset(source: &str, offset: usize) -> usize {
    source[offset..]
        .find('\n')
        .map(|idx| offset + idx)
        .unwrap_or(source.len())
}

pub(crate) fn line_indent_at_offset(source: &str, offset: usize) -> String {
    let line_start = line_start_offset(source, offset);
    let line_end = line_end_offset(source, line_start);
    leading_ascii_whitespace(source.get(line_start..line_end).unwrap_or(""))
}

pub(crate) fn symbol_doc_comment_span(
    source: &str,
    symbol: &php_lsp_types::SymbolInfo,
) -> Option<(usize, usize)> {
    let doc_comment = symbol.doc_comment.as_deref()?;
    let declaration_start = byte_offset_for_line_col(source, symbol.range.0, symbol.range.1)?;
    let search = source.get(..declaration_start)?;
    let start = search.rfind(doc_comment)?;
    Some((start, start + doc_comment.len()))
}

pub(crate) fn symbol_has_native_return_type(
    source: &str,
    symbol: &php_lsp_types::SymbolInfo,
) -> bool {
    if !matches!(
        symbol.kind,
        php_lsp_types::PhpSymbolKind::Function | php_lsp_types::PhpSymbolKind::Method
    ) {
        return false;
    }

    let Some(start) = byte_offset_for_line_col(source, symbol.range.0, symbol.range.1) else {
        return false;
    };
    let Some(end) = byte_offset_for_line_col(source, symbol.range.2, symbol.range.3) else {
        return false;
    };
    let Some(text) = source.get(start..end) else {
        return false;
    };
    let Some(open_paren) = text.find('(') else {
        return false;
    };
    let Some(close_paren) = find_matching_delimiter(text, open_paren, '(', ')') else {
        return false;
    };
    text.get(close_paren + 1..)
        .is_some_and(|after_params| after_params.trim_start().starts_with(':'))
}

pub(crate) fn type_name_eq(left: &str, right: &str) -> bool {
    left.trim_start_matches('\\')
        .eq_ignore_ascii_case(right.trim_start_matches('\\'))
}

pub(crate) fn type_info_refines_native(
    phpdoc_type: &php_lsp_types::TypeInfo,
    native_type: &php_lsp_types::TypeInfo,
) -> bool {
    use php_lsp_types::TypeInfo;

    if phpdoc_type == native_type {
        return true;
    }

    match (phpdoc_type, native_type) {
        (_, TypeInfo::Mixed) => true,
        (TypeInfo::Simple(phpdoc), TypeInfo::Simple(native)) => {
            let phpdoc = phpdoc.trim_start_matches('\\').to_ascii_lowercase();
            let native = native.trim_start_matches('\\').to_ascii_lowercase();
            phpdoc == native
                || matches!(
                    (phpdoc.as_str(), native.as_str()),
                    (
                        "positive-int"
                            | "negative-int"
                            | "non-negative-int"
                            | "non-positive-int"
                            | "non-zero-int",
                        "int"
                    ) | (
                        "non-empty-string"
                            | "numeric-string"
                            | "literal-string"
                            | "lowercase-string"
                            | "class-string",
                        "string"
                    ) | ("non-empty-array" | "list" | "non-empty-list", "array")
                )
        }
        (TypeInfo::Generic { base, .. }, TypeInfo::Simple(native)) => {
            type_name_eq(base, native)
                || (native.eq_ignore_ascii_case("array")
                    && matches!(
                        base.to_ascii_lowercase().as_str(),
                        "list" | "non-empty-list" | "non-empty-array"
                    ))
        }
        (TypeInfo::ArrayShape(_), TypeInfo::Simple(native)) => native.eq_ignore_ascii_case("array"),
        (TypeInfo::ObjectShape(_), TypeInfo::Simple(native)) => {
            native.eq_ignore_ascii_case("object")
        }
        (TypeInfo::Callable { .. }, TypeInfo::Simple(native)) => {
            native.eq_ignore_ascii_case("callable")
        }
        (TypeInfo::ClassString(_), TypeInfo::Simple(native)) => {
            native.eq_ignore_ascii_case("string") || native.eq_ignore_ascii_case("class-string")
        }
        (TypeInfo::Nullable(phpdoc_inner), TypeInfo::Nullable(native_inner)) => {
            type_info_refines_native(phpdoc_inner, native_inner)
        }
        (TypeInfo::Union(phpdoc_parts), TypeInfo::Nullable(native_inner)) => {
            let mut has_null = false;
            let mut has_refined_inner = false;
            for part in phpdoc_parts {
                match part {
                    TypeInfo::LiteralNull => has_null = true,
                    other if type_info_refines_native(other, native_inner) => {
                        has_refined_inner = true;
                    }
                    _ => return false,
                }
            }
            has_null && has_refined_inner
        }
        (TypeInfo::Nullable(phpdoc_inner), TypeInfo::Union(native_parts)) => {
            native_parts
                .iter()
                .any(|part| matches!(part, TypeInfo::LiteralNull))
                && native_parts
                    .iter()
                    .any(|part| type_info_refines_native(phpdoc_inner, part))
        }
        (TypeInfo::Union(phpdoc_parts), TypeInfo::Union(native_parts)) => {
            phpdoc_parts.iter().all(|phpdoc_part| {
                native_parts
                    .iter()
                    .any(|native_part| type_info_refines_native(phpdoc_part, native_part))
            })
        }
        _ => false,
    }
}

pub(crate) fn preferred_phpdoc_type_text(
    native_type: Option<&php_lsp_types::TypeInfo>,
    existing_phpdoc_type: Option<&php_lsp_types::TypeInfo>,
) -> Option<String> {
    match (native_type, existing_phpdoc_type) {
        (Some(native), Some(existing)) if type_info_refines_native(existing, native) => {
            Some(existing.to_string())
        }
        (Some(native), _) => Some(native.to_string()),
        (None, Some(existing)) => Some(existing.to_string()),
        (None, None) => None,
    }
}

pub(crate) fn phpdoc_return_update(
    source: &str,
    symbol: &php_lsp_types::SymbolInfo,
    existing_doc: Option<&php_lsp_types::PhpDoc>,
    existing_description: Option<String>,
) -> PhpDocReturnUpdate {
    if !symbol_has_native_return_type(source, symbol) {
        return PhpDocReturnUpdate::Preserve;
    }

    match symbol
        .signature
        .as_ref()
        .and_then(|sig| sig.return_type.as_ref())
    {
        Some(php_lsp_types::TypeInfo::Void) => PhpDocReturnUpdate::Remove,
        Some(return_type) => {
            let type_text = preferred_phpdoc_type_text(
                Some(return_type),
                existing_doc.and_then(|doc| doc.return_type.as_ref()),
            )
            .unwrap_or_else(|| return_type.to_string());
            PhpDocReturnUpdate::Replace(DesiredPhpDocReturn {
                type_text,
                description: existing_description,
            })
        }
        None => PhpDocReturnUpdate::Preserve,
    }
}

pub(crate) fn phpdoc_param_variable_text(param: &php_lsp_types::ParamInfo) -> String {
    let mut text = String::new();
    if param.is_by_ref {
        text.push('&');
    }
    if param.is_variadic {
        text.push_str("...");
    }
    text.push('$');
    text.push_str(&param.name);
    text
}

pub(crate) fn desired_phpdoc_params(
    signature: &php_lsp_types::Signature,
    existing_doc: Option<&php_lsp_types::PhpDoc>,
) -> Vec<DesiredPhpDocParam> {
    let has_native_param_types = signature
        .params
        .iter()
        .any(|param| param.type_info.is_some());
    let has_existing_param_tags = existing_doc.is_some_and(|doc| !doc.params.is_empty());
    if !has_existing_param_tags && !has_native_param_types {
        return Vec::new();
    }

    let mut existing_by_name = HashMap::new();
    if let Some(doc) = existing_doc {
        for param in &doc.params {
            existing_by_name.entry(param.name.clone()).or_insert(param);
        }
    }

    signature
        .params
        .iter()
        .map(|param| {
            let existing = existing_by_name.get(&param.name).copied();
            let type_text = preferred_phpdoc_type_text(
                param.type_info.as_ref(),
                existing.and_then(|doc_param| doc_param.type_info.as_ref()),
            )
            .unwrap_or_else(|| "mixed".to_string());

            DesiredPhpDocParam {
                name: param.name.clone(),
                type_text,
                variable_text: phpdoc_param_variable_text(param),
                description: existing.and_then(|doc_param| doc_param.description.clone()),
            }
        })
        .collect()
}

pub(crate) fn phpdoc_params_need_update(
    existing_doc: Option<&php_lsp_types::PhpDoc>,
    desired_params: &[DesiredPhpDocParam],
    existing_variable_texts: &HashMap<String, String>,
) -> bool {
    let Some(existing_doc) = existing_doc else {
        return !desired_params.is_empty();
    };
    if existing_doc.params.len() != desired_params.len() {
        return true;
    }

    existing_doc
        .params
        .iter()
        .zip(desired_params.iter())
        .any(|(existing, desired)| {
            existing.name != desired.name
                || existing_variable_texts
                    .get(&existing.name)
                    .is_none_or(|variable_text| variable_text != &desired.variable_text)
                || existing
                    .type_info
                    .as_ref()
                    .map(ToString::to_string)
                    .as_deref()
                    != Some(desired.type_text.as_str())
        })
}

pub(crate) fn phpdoc_return_needs_update(
    existing_doc: Option<&php_lsp_types::PhpDoc>,
    return_update: &PhpDocReturnUpdate,
) -> bool {
    match return_update {
        PhpDocReturnUpdate::Preserve => false,
        PhpDocReturnUpdate::Remove => existing_doc.is_some_and(|doc| doc.return_type.is_some()),
        PhpDocReturnUpdate::Replace(return_doc) => {
            existing_doc
                .and_then(|doc| doc.return_type.as_ref())
                .map(ToString::to_string)
                .as_deref()
                != Some(return_doc.type_text.as_str())
        }
    }
}

pub(crate) fn update_phpdoc_existing_plan(
    source: &str,
    symbol: &php_lsp_types::SymbolInfo,
    desired_params: &[DesiredPhpDocParam],
    return_update: &PhpDocReturnUpdate,
) -> Option<UpdatePhpDocPlan> {
    let doc_comment = symbol.doc_comment.as_deref()?;
    let (doc_start, doc_end) = symbol_doc_comment_span(source, symbol)?;
    let manage_return = !matches!(return_update, PhpDocReturnUpdate::Preserve);
    let managed_lines = render_managed_phpdoc_lines(desired_params, return_update);
    let content_lines = update_phpdoc_content_lines(
        phpdoc_content_lines(doc_comment),
        managed_lines,
        manage_return,
    );

    let line_start = line_start_offset(source, doc_start);
    let line_end = line_end_offset(source, doc_end);
    let line_prefix = source.get(line_start..doc_start).unwrap_or("");
    let line_suffix = source.get(doc_end..line_end).unwrap_or("");
    let starts_standalone = line_prefix.trim().is_empty();
    let ends_standalone = line_suffix.trim().is_empty();

    if content_lines.is_empty() {
        let (start, end) = if starts_standalone && ends_standalone {
            line_full_span(source, doc_start, doc_end)
        } else {
            (doc_start, doc_end)
        };
        return Some(UpdatePhpDocPlan {
            start,
            end,
            new_text: String::new(),
        });
    }

    let start = if starts_standalone {
        line_start
    } else {
        doc_start
    };
    let indent = if starts_standalone { line_prefix } else { "" };

    Some(UpdatePhpDocPlan {
        start,
        end: doc_end,
        new_text: render_phpdoc_comment(indent, &content_lines),
    })
}

pub(crate) fn update_phpdoc_create_plan(
    source: &str,
    symbol: &php_lsp_types::SymbolInfo,
    desired_params: &[DesiredPhpDocParam],
    return_update: &PhpDocReturnUpdate,
) -> Option<UpdatePhpDocPlan> {
    let managed_lines = render_managed_phpdoc_lines(desired_params, return_update);
    if managed_lines.is_empty() {
        return None;
    }

    let declaration_start = byte_offset_for_line_col(source, symbol.range.0, symbol.range.1)?;
    let insert_at = line_start_offset(source, declaration_start);
    let indent = line_indent_at_offset(source, declaration_start);
    let mut new_text = render_phpdoc_comment(&indent, &managed_lines);
    new_text.push('\n');

    Some(UpdatePhpDocPlan {
        start: insert_at,
        end: insert_at,
        new_text,
    })
}

pub(crate) fn update_phpdoc_from_signature_plan(
    source: &str,
    symbol: &php_lsp_types::SymbolInfo,
) -> Option<UpdatePhpDocPlan> {
    if !matches!(
        symbol.kind,
        php_lsp_types::PhpSymbolKind::Function | php_lsp_types::PhpSymbolKind::Method
    ) || symbol.modifiers.is_builtin
    {
        return None;
    }

    let signature = symbol.signature.as_ref()?;
    let existing_doc = symbol.doc_comment.as_deref().map(parse_phpdoc);
    let existing_variable_texts =
        existing_phpdoc_param_variable_texts(symbol.doc_comment.as_deref());
    let existing_return_description =
        existing_phpdoc_return_description(symbol.doc_comment.as_deref());
    let desired_params = desired_phpdoc_params(signature, existing_doc.as_ref());
    let return_update = phpdoc_return_update(
        source,
        symbol,
        existing_doc.as_ref(),
        existing_return_description,
    );
    let params_need_update = phpdoc_params_need_update(
        existing_doc.as_ref(),
        &desired_params,
        &existing_variable_texts,
    );
    let return_needs_update = phpdoc_return_needs_update(existing_doc.as_ref(), &return_update);

    if !params_need_update && !return_needs_update {
        return None;
    }

    if symbol.doc_comment.is_some() {
        update_phpdoc_existing_plan(source, symbol, &desired_params, &return_update)
    } else {
        update_phpdoc_create_plan(source, symbol, &desired_params, &return_update)
    }
}

pub(crate) fn build_update_phpdoc_from_signature_action(
    uri: Uri,
    source: &str,
    symbol: &php_lsp_types::SymbolInfo,
    request_range: Range,
    document_version: Option<i32>,
) -> Option<CodeActionOrCommand> {
    update_phpdoc_from_signature_plan(source, symbol)?;
    let data = serde_json::to_value(CodeActionData {
        action_kind: CodeActionDataKind::UpdatePhpDoc,
        uri: uri.as_str().to_string(),
        range: request_range,
        document_version,
        extra: CodeActionDataExtra::UpdatePhpDoc {
            symbol_fqn: symbol.fqn.clone(),
        },
    })
    .ok()?;

    Some(CodeActionOrCommand::CodeAction(CodeAction {
        title: "Update PHPDoc from signature".to_string(),
        kind: Some(CodeActionKind::REFACTOR_REWRITE),
        diagnostics: None,
        edit: None,
        command: None,
        is_preferred: Some(false),
        disabled: None,
        data: Some(data),
    }))
}

pub(crate) fn update_phpdoc_from_signature_edit(
    uri: Uri,
    source: &str,
    symbol: &php_lsp_types::SymbolInfo,
) -> Option<WorkspaceEdit> {
    let plan = update_phpdoc_from_signature_plan(source, symbol)?;
    let mut changes = HashMap::new();
    changes.insert(
        uri,
        vec![TextEdit {
            range: lsp_range_for_byte_offsets(source, plan.start, plan.end),
            new_text: plan.new_text,
        }],
    );

    Some(WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    })
}

pub(crate) fn build_add_import_edit(
    uri: Uri,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    import_fqn: &str,
    import_kind: ImportKind,
    diagnostic_range: Range,
) -> Option<(WorkspaceEdit, Option<String>)> {
    if let Some(existing) = existing_import_for_fqn(file_symbols, import_fqn, import_kind) {
        if let Some(alias) = existing.alias.clone() {
            let edit = TextEdit {
                range: diagnostic_range,
                new_text: alias.clone(),
            };
            let mut changes = std::collections::HashMap::new();
            changes.insert(uri, vec![edit]);
            return Some((
                WorkspaceEdit {
                    changes: Some(changes),
                    document_changes: None,
                    change_annotations: None,
                },
                Some(alias),
            ));
        }
        return None;
    }

    let import_short_name = short_name(import_fqn);
    let used_aliases = used_import_aliases(file_symbols, import_kind);
    let alias = if used_aliases.contains(import_short_name) {
        Some(unique_import_alias(import_short_name, &used_aliases))
    } else {
        None
    };

    let insert_line = find_use_insert_line(source, file_symbols);
    let needs_spacing =
        file_symbols.use_statements.is_empty() && !line_is_blank(source, insert_line);
    let mut import_text = build_use_statement(import_fqn, import_kind, alias.as_deref());
    import_text.push('\n');
    if needs_spacing {
        import_text.push('\n');
    }

    let mut edits = vec![TextEdit {
        range: Range {
            start: Position::new(insert_line, 0),
            end: Position::new(insert_line, 0),
        },
        new_text: import_text,
    }];

    let replacement_name = alias.as_deref().unwrap_or(import_short_name);
    if alias.is_some()
        || text_at_lsp_range(source, diagnostic_range)
            .map(|text| text.trim_start_matches('\\') != replacement_name)
            .unwrap_or(false)
    {
        edits.push(TextEdit {
            range: diagnostic_range,
            new_text: replacement_name.to_string(),
        });
    }

    let mut changes = std::collections::HashMap::new();
    changes.insert(uri, edits);
    Some((
        WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        },
        alias,
    ))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiagnosticDataEnvelope {
    #[serde(rename = "phpLsp")]
    php_lsp: Option<PhpLspDiagnosticData>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhpLspDiagnosticData {
    replacement: Option<DiagnosticReplacement>,
    #[serde(default)]
    analyzer_fixes: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiagnosticReplacement {
    new_text: String,
    title: Option<String>,
    range: Option<Range>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(crate) enum ExternalAnalyzerFix {
    AddThrows {
        exception: String,
    },
    AddIterableValueType {
        variable: String,
        #[serde(rename = "typeText")]
        type_text: String,
    },
    ReplacePrefixedClassName {
        replacement: String,
        range: Option<Range>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExternalAnalyzer {
    PhpStan,
    Psalm,
}

impl ExternalAnalyzer {
    fn display_name(self) -> &'static str {
        match self {
            ExternalAnalyzer::PhpStan => "PHPStan",
            ExternalAnalyzer::Psalm => "Psalm",
        }
    }
}

pub(crate) fn diagnostic_code_str(diagnostic: &Diagnostic) -> Option<&str> {
    match diagnostic.code.as_ref()? {
        NumberOrString::String(value) => Some(value.as_str()),
        NumberOrString::Number(_) => None,
    }
}

pub(crate) fn diagnostic_data(diagnostic: &Diagnostic) -> Option<PhpLspDiagnosticData> {
    let data = diagnostic.data.clone()?;
    if let Ok(envelope) = serde_json::from_value::<DiagnosticDataEnvelope>(data.clone()) {
        if let Some(php_lsp) = envelope.php_lsp {
            return Some(php_lsp);
        }
    }
    serde_json::from_value::<PhpLspDiagnosticData>(data).ok()
}

pub(crate) fn diagnostic_external_analyzer(diagnostic: &Diagnostic) -> Option<ExternalAnalyzer> {
    match diagnostic
        .source
        .as_deref()
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("phpstan") => Some(ExternalAnalyzer::PhpStan),
        Some("psalm") => Some(ExternalAnalyzer::Psalm),
        _ => None,
    }
}

pub(crate) fn is_unused_import_diagnostic(diagnostic: &Diagnostic) -> bool {
    diagnostic.source.as_deref() == Some("php-lsp")
        && (diagnostic_code_str(diagnostic) == Some("php-lsp.unusedImport")
            || diagnostic.message.starts_with("Unused import: "))
}

pub(crate) fn diagnostic_range_byte_offsets(source: &str, range: Range) -> Option<(usize, usize)> {
    let start = lsp_position_to_byte(source, range.start)?;
    let end = lsp_position_to_byte(source, range.end)?;
    Some((start.min(source.len()), end.min(source.len())))
}

pub(crate) fn remove_unused_import_edit(
    uri: Uri,
    source: &str,
    range: Range,
) -> Option<WorkspaceEdit> {
    let (start, end) = diagnostic_range_byte_offsets(source, range)?;
    let (start, end) = line_full_span(source, start, end);
    Some(workspace_edit_from_text_edits(
        uri,
        vec![TextEdit {
            range: lsp_range_for_byte_offsets(source, start, end),
            new_text: String::new(),
        }],
    ))
}

pub(crate) fn build_remove_unused_import_action(
    uri: Uri,
    source: &str,
    diagnostic: &Diagnostic,
    is_preferred: bool,
) -> Option<CodeActionOrCommand> {
    Some(CodeActionOrCommand::CodeAction(CodeAction {
        title: "Remove unused import".to_string(),
        kind: Some(CodeActionKind::QUICKFIX),
        diagnostics: Some(vec![diagnostic.clone()]),
        edit: Some(remove_unused_import_edit(uri, source, diagnostic.range)?),
        command: None,
        is_preferred: Some(is_preferred),
        disabled: None,
        data: None,
    }))
}

pub(crate) fn build_remove_all_unused_imports_action(
    organize_imports_edit: Option<&WorkspaceEdit>,
    diagnostics: &[Diagnostic],
) -> Option<CodeActionOrCommand> {
    let unused_diagnostics = diagnostics
        .iter()
        .filter(|diagnostic| is_unused_import_diagnostic(diagnostic))
        .cloned()
        .collect::<Vec<_>>();
    if unused_diagnostics.is_empty() {
        return None;
    }

    Some(CodeActionOrCommand::CodeAction(CodeAction {
        title: "Remove all unused imports".to_string(),
        kind: Some(CodeActionKind::QUICKFIX),
        diagnostics: Some(unused_diagnostics),
        edit: Some(organize_imports_edit.cloned()?),
        command: None,
        is_preferred: Some(false),
        disabled: None,
        data: None,
    }))
}

pub(crate) fn build_diagnostic_replacement_action(
    uri: Uri,
    source: &str,
    diagnostic: &Diagnostic,
    replacement: &DiagnosticReplacement,
    is_preferred: bool,
) -> Option<CodeActionOrCommand> {
    if replacement.new_text.trim().is_empty() {
        return None;
    }

    let range = replacement.range.unwrap_or(diagnostic.range);
    let title = replacement.title.clone().unwrap_or_else(|| {
        format!(
            "Replace with `{}`",
            replacement.new_text.trim().trim_end_matches("()")
        )
    });

    Some(CodeActionOrCommand::CodeAction(CodeAction {
        title,
        kind: Some(CodeActionKind::QUICKFIX),
        diagnostics: Some(vec![diagnostic.clone()]),
        edit: Some(workspace_edit_from_text_edits(
            uri,
            vec![TextEdit {
                range: lsp_range_for_byte_offsets(
                    source,
                    diagnostic_range_byte_offsets(source, range)?.0,
                    diagnostic_range_byte_offsets(source, range)?.1,
                ),
                new_text: replacement.new_text.clone(),
            }],
        )),
        command: None,
        is_preferred: Some(is_preferred),
        disabled: None,
        data: None,
    }))
}

pub(crate) fn line_insert_position(line: u32) -> Range {
    Range {
        start: Position::new(line, 0),
        end: Position::new(line, 0),
    }
}

pub(crate) fn analyzer_ignore_comment(
    source: &str,
    diagnostic: &Diagnostic,
    analyzer: ExternalAnalyzer,
) -> Option<String> {
    let line = diagnostic.range.start.line;
    let indent = leading_ascii_whitespace(line_text(source, line));
    match analyzer {
        ExternalAnalyzer::PhpStan => Some(format!("{indent}// @phpstan-ignore-next-line\n")),
        ExternalAnalyzer::Psalm => {
            let code = diagnostic_code_str(diagnostic)?.trim();
            if code.is_empty() {
                return None;
            }
            Some(format!("{indent}/** @psalm-suppress {code} */\n"))
        }
    }
}

pub(crate) fn build_ignore_external_analyzer_action(
    uri: Uri,
    source: &str,
    diagnostic: &Diagnostic,
    analyzer: ExternalAnalyzer,
) -> Option<CodeActionOrCommand> {
    Some(CodeActionOrCommand::CodeAction(CodeAction {
        title: format!("Ignore {} diagnostic locally", analyzer.display_name()),
        kind: Some(CodeActionKind::QUICKFIX),
        diagnostics: Some(vec![diagnostic.clone()]),
        edit: Some(workspace_edit_from_text_edits(
            uri,
            vec![TextEdit {
                range: line_insert_position(diagnostic.range.start.line),
                new_text: analyzer_ignore_comment(source, diagnostic, analyzer)?,
            }],
        )),
        command: None,
        is_preferred: Some(false),
        disabled: None,
        data: None,
    }))
}

pub(crate) fn callable_symbol_containing_range(
    file_symbols: &php_lsp_types::FileSymbols,
    range: (u32, u32, u32, u32),
) -> Option<&php_lsp_types::SymbolInfo> {
    let start_line = range.0;
    file_symbols
        .symbols
        .iter()
        .filter(|symbol| {
            matches!(
                symbol.kind,
                php_lsp_types::PhpSymbolKind::Function | php_lsp_types::PhpSymbolKind::Method
            )
        })
        .find(|symbol| {
            byte_range_contains(symbol.range, range)
                || byte_ranges_overlap(symbol.range, range)
                || byte_ranges_overlap(symbol.selection_range, range)
                || (symbol.selection_range.0 <= start_line && start_line <= symbol.range.2)
        })
}

pub(crate) fn render_existing_phpdoc_plan(
    source: &str,
    symbol: &php_lsp_types::SymbolInfo,
    content_lines: Vec<String>,
) -> Option<UpdatePhpDocPlan> {
    let (doc_start, doc_end) = symbol_doc_comment_span(source, symbol)?;
    let line_start = line_start_offset(source, doc_start);
    let line_end = line_end_offset(source, doc_end);
    let line_prefix = source.get(line_start..doc_start).unwrap_or("");
    let line_suffix = source.get(doc_end..line_end).unwrap_or("");
    let starts_standalone = line_prefix.trim().is_empty();
    let ends_standalone = line_suffix.trim().is_empty();
    let start = if starts_standalone {
        line_start
    } else {
        doc_start
    };
    let indent = if starts_standalone { line_prefix } else { "" };
    let mut new_text =
        render_phpdoc_comment(indent, &normalize_phpdoc_content_lines(content_lines));
    if starts_standalone && !ends_standalone {
        new_text.push('\n');
    }

    Some(UpdatePhpDocPlan {
        start,
        end: doc_end,
        new_text,
    })
}

pub(crate) fn render_created_phpdoc_plan(
    source: &str,
    symbol: &php_lsp_types::SymbolInfo,
    content_lines: Vec<String>,
) -> Option<UpdatePhpDocPlan> {
    let declaration_start = byte_offset_for_line_col(source, symbol.range.0, symbol.range.1)?;
    let insert_at = line_start_offset(source, declaration_start);
    let indent = line_indent_at_offset(source, declaration_start);
    let mut new_text =
        render_phpdoc_comment(&indent, &normalize_phpdoc_content_lines(content_lines));
    new_text.push('\n');
    Some(UpdatePhpDocPlan {
        start: insert_at,
        end: insert_at,
        new_text,
    })
}

pub(crate) fn add_throws_phpdoc_edit(
    uri: Uri,
    source: &str,
    symbol: &php_lsp_types::SymbolInfo,
    exception: &str,
) -> Option<WorkspaceEdit> {
    let exception = exception.trim();
    if exception.is_empty() {
        return None;
    }
    let throws_line = format!("@throws {exception}");
    let plan = if let Some(doc_comment) = symbol.doc_comment.as_deref() {
        let mut lines = phpdoc_content_lines(doc_comment);
        if lines.iter().any(|line| line.trim() == throws_line) {
            return None;
        }
        let insert_at = lines
            .iter()
            .rposition(|line| phpdoc_line_is_tag(line))
            .map(|idx| idx + 1)
            .unwrap_or_else(|| phpdoc_managed_insert_index(&lines));
        lines.insert(insert_at, throws_line);
        render_existing_phpdoc_plan(source, symbol, lines)?
    } else {
        render_created_phpdoc_plan(source, symbol, vec![throws_line])?
    };

    Some(workspace_edit_from_text_edits(
        uri,
        vec![TextEdit {
            range: lsp_range_for_byte_offsets(source, plan.start, plan.end),
            new_text: plan.new_text,
        }],
    ))
}

pub(crate) fn normalize_phpdoc_variable_name(variable: &str) -> Option<String> {
    let name = variable.trim().trim_start_matches('$');
    if name.is_empty()
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return None;
    }
    Some(name.to_string())
}

pub(crate) fn phpdoc_param_insert_index(lines: &[String]) -> usize {
    lines
        .iter()
        .position(|line| phpdoc_line_starts_with_tag(line, "@return"))
        .or_else(|| {
            lines
                .iter()
                .rposition(|line| phpdoc_line_starts_with_tag(line, "@param"))
                .map(|idx| idx + 1)
        })
        .unwrap_or_else(|| phpdoc_managed_insert_index(lines))
}

pub(crate) fn update_param_phpdoc_lines(
    mut lines: Vec<String>,
    variable: &str,
    type_text: &str,
) -> Vec<String> {
    let variable_token = format!("${variable}");
    for line in &mut lines {
        let Some(rest) = phpdoc_tag_rest(line, "@param") else {
            continue;
        };
        let Some(type_end) = consume_phpdoc_type_expr(rest) else {
            continue;
        };
        let after_type = rest[type_end..].trim_start();
        let Some((variable_start, variable_end)) = find_phpdoc_variable_token_span(after_type)
        else {
            continue;
        };
        let existing_variable_text = after_type[variable_start..variable_end].trim();
        if phpdoc_variable_name_from_token(existing_variable_text).as_deref() != Some(variable) {
            continue;
        }

        let description = after_type[variable_end..].trim();
        let mut updated = format!("@param {} {}", type_text.trim(), existing_variable_text);
        if !description.is_empty() {
            updated.push(' ');
            updated.push_str(description);
        }
        *line = updated;
        return lines;
    }

    let insert_at = phpdoc_param_insert_index(&lines);
    lines.insert(
        insert_at,
        format!("@param {} {}", type_text.trim(), variable_token),
    );
    lines
}

pub(crate) fn add_iterable_value_type_phpdoc_edit(
    uri: Uri,
    source: &str,
    symbol: &php_lsp_types::SymbolInfo,
    variable: &str,
    type_text: &str,
) -> Option<WorkspaceEdit> {
    let variable = normalize_phpdoc_variable_name(variable)?;
    let type_text = type_text.trim();
    if type_text.is_empty() {
        return None;
    }

    let plan = if let Some(doc_comment) = symbol.doc_comment.as_deref() {
        let lines =
            update_param_phpdoc_lines(phpdoc_content_lines(doc_comment), &variable, type_text);
        render_existing_phpdoc_plan(source, symbol, lines)?
    } else {
        render_created_phpdoc_plan(
            source,
            symbol,
            vec![format!("@param {} ${}", type_text, variable)],
        )?
    };

    Some(workspace_edit_from_text_edits(
        uri,
        vec![TextEdit {
            range: lsp_range_for_byte_offsets(source, plan.start, plan.end),
            new_text: plan.new_text,
        }],
    ))
}

pub(crate) fn build_external_analyzer_fix_actions(
    uri: Uri,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    diagnostic: &Diagnostic,
    analyzer: ExternalAnalyzer,
    data: Option<&PhpLspDiagnosticData>,
) -> Vec<CodeActionOrCommand> {
    let mut actions = Vec::new();
    if let Some(action) =
        build_ignore_external_analyzer_action(uri.clone(), source, diagnostic, analyzer)
    {
        actions.push(action);
    }

    let range = lsp_range_to_byte_range(source, diagnostic.range);
    let callable = callable_symbol_containing_range(file_symbols, range);
    let fixes = data
        .into_iter()
        .flat_map(|data| data.analyzer_fixes.iter())
        .filter_map(|value| serde_json::from_value::<ExternalAnalyzerFix>(value.clone()).ok());

    for fix in fixes {
        match fix {
            ExternalAnalyzerFix::AddThrows { exception } => {
                let Some(symbol) = callable else {
                    continue;
                };
                if let Some(edit) =
                    add_throws_phpdoc_edit(uri.clone(), source, symbol, exception.as_str())
                {
                    actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                        title: format!("Add @throws {}", exception.trim()),
                        kind: Some(CodeActionKind::QUICKFIX),
                        diagnostics: Some(vec![diagnostic.clone()]),
                        edit: Some(edit),
                        command: None,
                        is_preferred: Some(false),
                        disabled: None,
                        data: None,
                    }));
                }
            }
            ExternalAnalyzerFix::AddIterableValueType {
                variable,
                type_text,
            } => {
                let Some(symbol) = callable else {
                    continue;
                };
                if let Some(edit) = add_iterable_value_type_phpdoc_edit(
                    uri.clone(),
                    source,
                    symbol,
                    variable.as_str(),
                    type_text.as_str(),
                ) {
                    let variable = normalize_phpdoc_variable_name(&variable)
                        .map(|name| format!("${name}"))
                        .unwrap_or(variable);
                    actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                        title: format!("Add PHPDoc iterable value type for `{variable}`"),
                        kind: Some(CodeActionKind::QUICKFIX),
                        diagnostics: Some(vec![diagnostic.clone()]),
                        edit: Some(edit),
                        command: None,
                        is_preferred: Some(false),
                        disabled: None,
                        data: None,
                    }));
                }
            }
            ExternalAnalyzerFix::ReplacePrefixedClassName { replacement, range } => {
                if replacement.trim().is_empty() {
                    continue;
                }
                let range = range.unwrap_or(diagnostic.range);
                let Some((start, end)) = diagnostic_range_byte_offsets(source, range) else {
                    continue;
                };
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: format!("Replace class name with `{}`", replacement.trim()),
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: Some(vec![diagnostic.clone()]),
                    edit: Some(workspace_edit_from_text_edits(
                        uri.clone(),
                        vec![TextEdit {
                            range: lsp_range_for_byte_offsets(source, start, end),
                            new_text: replacement,
                        }],
                    )),
                    command: None,
                    is_preferred: Some(false),
                    disabled: None,
                    data: None,
                }));
            }
        }
    }

    actions
}

pub(crate) fn range_overlaps(a: Range, b: Range) -> bool {
    a.start <= b.end && b.start <= a.end
}

pub(crate) fn byte_ranges_overlap(left: (u32, u32, u32, u32), right: (u32, u32, u32, u32)) -> bool {
    (left.0, left.1) <= (right.2, right.3) && (right.0, right.1) <= (left.2, left.3)
}

impl PhpLspBackend {
    pub(crate) async fn lsp_code_action(
        &self,
        params: CodeActionParams,
    ) -> Result<Option<CodeActionResponse>> {
        let wants_quickfix =
            code_action_kind_allowed(params.context.only.as_ref(), &CodeActionKind::QUICKFIX);
        let wants_organize_imports = code_action_kind_allowed(
            params.context.only.as_ref(),
            &CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
        );
        let wants_add_return_type = code_action_kind_allowed(
            params.context.only.as_ref(),
            &CodeActionKind::REFACTOR_REWRITE,
        );
        let wants_generate_members = code_action_kind_allowed(
            params.context.only.as_ref(),
            &CodeActionKind::REFACTOR_REWRITE,
        );
        let wants_refactor_extract = code_action_kind_allowed(
            params.context.only.as_ref(),
            &CodeActionKind::REFACTOR_EXTRACT,
        );
        let wants_refactor_inline = code_action_kind_allowed(
            params.context.only.as_ref(),
            &CodeActionKind::REFACTOR_INLINE,
        );
        let wants_implement_missing_methods =
            code_action_kind_allowed(params.context.only.as_ref(), &CodeActionKind::QUICKFIX);

        if !wants_quickfix
            && !wants_organize_imports
            && !wants_add_return_type
            && !wants_generate_members
            && !wants_refactor_extract
            && !wants_refactor_inline
            && !wants_implement_missing_methods
        {
            return Ok(Some(vec![]));
        }

        let uri = params.text_document.uri;
        let uri_str = uri.as_str().to_string();
        let php_version = *self.php_version.lock().await;
        let analyzer_code_actions = *self.analyzer_code_actions.lock().await;
        let document_version = self.current_document_version(&uri_str);

        let (
            source,
            file_symbols,
            organize_imports_edit,
            add_return_type_actions,
            generate_member_actions,
            refactor_extract_actions,
            refactor_inline_actions,
            implement_missing_methods_actions,
        ) = {
            let parser = match self.open_files.get(&uri_str) {
                Some(p) => p,
                None => return Ok(Some(vec![])),
            };
            let tree = match parser.tree() {
                Some(t) => t,
                None => return Ok(Some(vec![])),
            };
            let source = parser.source();
            let file_symbols = self
                .index
                .file_symbols
                .get(&uri_str)
                .map(|entry| entry.value().clone())
                .unwrap_or_else(|| extract_file_symbols(tree, &source, &uri_str));
            let organize_imports_edit = if wants_organize_imports || wants_quickfix {
                build_organize_imports_edit(uri.clone(), &source, tree, &file_symbols)
            } else {
                None
            };
            let add_return_type_actions = if wants_add_return_type {
                let range = lsp_range_to_byte_range(&source, params.range);
                find_missing_return_type_candidates(tree, &source, range)
                    .into_iter()
                    .filter_map(|candidate| {
                        build_add_return_type_action(
                            uri.clone(),
                            &candidate,
                            php_version,
                            params.range,
                            document_version,
                        )
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let generate_member_actions = if wants_generate_members {
                let range = lsp_range_to_byte_range(&source, params.range);
                let mut actions = Vec::new();
                let visibility_symbol = property_symbol_at_range(&file_symbols, range)
                    .or_else(|| member_symbol_at_range(&file_symbols, range));
                if let Some(symbol) = visibility_symbol {
                    actions.extend(build_change_visibility_actions(
                        uri.clone(),
                        &self.index,
                        &file_symbols,
                        symbol,
                        params.range,
                        document_version,
                    ));
                }
                if let Some(class_sym) = concrete_class_symbol_at_range(&file_symbols, range) {
                    if let Some(action) = build_generate_constructor_action(
                        uri.clone(),
                        &source,
                        &file_symbols,
                        class_sym,
                        params.range,
                        document_version,
                    ) {
                        actions.push(action);
                    }
                }
                if let Some(property) = property_symbol_at_range(&file_symbols, range) {
                    let parent_is_class =
                        property.parent_fqn.as_deref().is_some_and(|parent_fqn| {
                            file_symbols.symbols.iter().any(|sym| {
                                sym.fqn == parent_fqn
                                    && sym.kind == php_lsp_types::PhpSymbolKind::Class
                            })
                        });
                    if parent_is_class {
                        actions.extend(build_generate_accessor_actions(
                            uri.clone(),
                            &self.index,
                            property,
                            params.range,
                            document_version,
                        ));
                    }
                }
                let promote_property =
                    property_symbol_at_range(&file_symbols, range).or_else(|| {
                        property_for_constructor_param_at_range(&source, &file_symbols, range)
                    });
                if let Some(property) = promote_property {
                    if let Some(action) = build_promote_constructor_parameter_action(
                        uri.clone(),
                        &source,
                        &file_symbols,
                        property,
                        params.range,
                        document_version,
                    ) {
                        actions.push(action);
                    }
                }
                if let Some(symbol) = callable_symbol_at_range(&file_symbols, range) {
                    if let Some(action) = build_update_phpdoc_from_signature_action(
                        uri.clone(),
                        &source,
                        symbol,
                        params.range,
                        document_version,
                    ) {
                        actions.push(action);
                    }
                }
                actions
            } else {
                Vec::new()
            };
            let refactor_extract_actions = if wants_refactor_extract {
                let mut actions = Vec::new();
                if let Some(action) = build_extract_variable_action(
                    uri.clone(),
                    tree,
                    &source,
                    params.range,
                    document_version,
                ) {
                    actions.push(action);
                }
                if let Some(action) = build_extract_constant_action(
                    uri.clone(),
                    tree,
                    &source,
                    &file_symbols,
                    params.range,
                    document_version,
                ) {
                    actions.push(action);
                }
                actions
            } else {
                Vec::new()
            };
            let refactor_inline_actions = if wants_refactor_inline {
                build_inline_variable_action(
                    uri.clone(),
                    tree,
                    &source,
                    params.range,
                    document_version,
                )
                .into_iter()
                .collect()
            } else {
                Vec::new()
            };
            let implement_missing_methods_actions = if wants_implement_missing_methods {
                let range = lsp_range_to_byte_range(&source, params.range);
                concrete_class_symbol_at_range(&file_symbols, range)
                    .and_then(|class_sym| {
                        let missing_methods =
                            missing_implementation_methods(&self.index, &file_symbols, class_sym);
                        build_implement_missing_methods_action(
                            uri.clone(),
                            class_sym,
                            &missing_methods,
                            params.range,
                            document_version,
                        )
                    })
                    .into_iter()
                    .collect()
            } else {
                Vec::new()
            };
            (
                source,
                file_symbols,
                organize_imports_edit,
                add_return_type_actions,
                generate_member_actions,
                refactor_extract_actions,
                refactor_inline_actions,
                implement_missing_methods_actions,
            )
        };

        let mut actions = Vec::new();
        actions.extend(add_return_type_actions);
        actions.extend(generate_member_actions);
        actions.extend(refactor_extract_actions);
        actions.extend(refactor_inline_actions);
        actions.extend(implement_missing_methods_actions);

        if wants_organize_imports {
            if let Some(edit) = organize_imports_edit.clone() {
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: "Organize imports".to_string(),
                    kind: Some(CodeActionKind::SOURCE_ORGANIZE_IMPORTS),
                    diagnostics: None,
                    edit: Some(edit),
                    command: None,
                    is_preferred: Some(false),
                    disabled: None,
                    data: None,
                }));
            }
        }

        if !wants_quickfix {
            return Ok(Some(actions));
        }

        let diagnostics = if params.context.diagnostics.is_empty() {
            let parser = match self.open_files.get(&uri_str) {
                Some(p) => p,
                None => return Ok(Some(vec![])),
            };
            let diagnostics_mode = *self.diagnostics_mode.lock().await;
            let diagnostic_severity = *self.diagnostic_severity.lock().await;
            let diagnostic_budget = *self.diagnostic_budget.lock().await;
            compute_diagnostics_with_config_for_version(
                &uri_str,
                &parser,
                &self.index,
                DiagnosticsRuntimeConfig {
                    mode: diagnostics_mode,
                    severity: diagnostic_severity,
                    budget: diagnostic_budget,
                    php_version,
                },
                self.current_document_version(&uri_str),
            )
            .into_iter()
            .filter(|diag| range_overlaps(diag.range, params.range))
            .collect()
        } else {
            params.context.diagnostics
        };

        let all_quickfix_diagnostics = diagnostics.clone();
        let mut quickfix_count = 0usize;

        for diagnostic in diagnostics {
            let data = diagnostic_data(&diagnostic);
            let analyzer = diagnostic_external_analyzer(&diagnostic);

            if analyzer.is_none_or(|_| analyzer_code_actions.enabled) {
                if let Some(replacement) = data.as_ref().and_then(|data| data.replacement.as_ref())
                {
                    if let Some(action) = build_diagnostic_replacement_action(
                        uri.clone(),
                        &source,
                        &diagnostic,
                        replacement,
                        quickfix_count == 0,
                    ) {
                        actions.push(action);
                        quickfix_count += 1;
                    }
                }
            }

            if is_unused_import_diagnostic(&diagnostic) {
                if let Some(action) = build_remove_unused_import_action(
                    uri.clone(),
                    &source,
                    &diagnostic,
                    quickfix_count == 0,
                ) {
                    actions.push(action);
                    quickfix_count += 1;
                }
            }

            if analyzer_code_actions.enabled {
                if let Some(analyzer) = analyzer {
                    let analyzer_actions = build_external_analyzer_fix_actions(
                        uri.clone(),
                        &source,
                        &file_symbols,
                        &diagnostic,
                        analyzer,
                        data.as_ref(),
                    );
                    quickfix_count += analyzer_actions.len();
                    actions.extend(analyzer_actions);
                }
            }

            let Some((import_kind, unresolved_fqn)) =
                unknown_symbol_from_diagnostic(&diagnostic.message)
            else {
                continue;
            };
            let unresolved_short = short_name(&unresolved_fqn);

            let mut candidates: Vec<std::sync::Arc<php_lsp_types::SymbolInfo>> = match import_kind {
                ImportKind::Class => self
                    .index
                    .types
                    .iter()
                    .filter(|entry| {
                        let sym = entry.value();
                        !sym.modifiers.is_builtin
                            && (sym.name == unresolved_short
                                || short_name(&sym.fqn) == unresolved_short)
                    })
                    .map(|entry| entry.value().clone())
                    .collect(),
                ImportKind::Function => self
                    .index
                    .functions
                    .iter()
                    .filter(|entry| {
                        let sym = entry.value();
                        !sym.modifiers.is_builtin
                            && (sym.name == unresolved_short
                                || short_name(&sym.fqn) == unresolved_short)
                    })
                    .map(|entry| entry.value().clone())
                    .collect(),
                ImportKind::Constant => Vec::new(),
            };
            candidates.sort_by(|a, b| a.fqn.cmp(&b.fqn));
            candidates.dedup_by(|a, b| a.fqn == b.fqn);
            candidates.truncate(5);

            for candidate in candidates {
                let Some((edit, alias)) = build_add_import_edit(
                    uri.clone(),
                    &source,
                    &file_symbols,
                    &candidate.fqn,
                    import_kind,
                    diagnostic.range,
                ) else {
                    continue;
                };

                let title = if let Some(alias) = alias {
                    format!("Import {} as {}", candidate.fqn, alias)
                } else {
                    format!("Import {}", candidate.fqn)
                };

                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title,
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: Some(vec![diagnostic.clone()]),
                    edit: Some(edit),
                    command: None,
                    is_preferred: Some(quickfix_count == 0),
                    disabled: None,
                    data: None,
                }));
                quickfix_count += 1;
            }
        }

        if let Some(action) = build_remove_all_unused_imports_action(
            organize_imports_edit.as_ref(),
            &all_quickfix_diagnostics,
        ) {
            actions.push(action);
        }

        Ok(Some(actions))
    }

    pub(crate) async fn lsp_code_action_resolve(
        &self,
        mut params: CodeAction,
    ) -> Result<CodeAction> {
        let Some(data_value) = params.data.clone() else {
            return Ok(params);
        };
        let Ok(data) = serde_json::from_value::<CodeActionData>(data_value) else {
            return Ok(params);
        };

        let CodeActionData {
            action_kind,
            uri,
            range: requested_range,
            document_version,
            extra,
        } = data;

        match (action_kind, extra) {
            (
                CodeActionDataKind::AddReturnType,
                CodeActionDataExtra::AddReturnType {
                    hint,
                    insert_position,
                },
            ) => {
                if self.current_document_version(&uri) != document_version {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                }

                let Ok(uri_value) = uri.parse::<Uri>() else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };

                let source = match self.open_files.get(&uri) {
                    Some(parser) => parser.source(),
                    None => {
                        params.edit = Some(empty_workspace_edit());
                        return Ok(params);
                    }
                };

                params.edit = Some(add_return_type_edit(
                    uri_value,
                    &source,
                    &hint,
                    insert_position,
                ));
            }
            (
                CodeActionDataKind::ImplementMissingMethods,
                CodeActionDataExtra::ImplementMissingMethods { class_fqn },
            ) => {
                if self.current_document_version(&uri) != document_version {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                }

                let Ok(uri_value) = uri.parse::<Uri>() else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };

                let (source, file_symbols) = match self.open_files.get(&uri) {
                    Some(parser) => {
                        let source = parser.source();
                        let file_symbols = match parser.tree() {
                            Some(tree) => self
                                .index
                                .file_symbols
                                .get(&uri)
                                .map(|entry| entry.value().clone())
                                .unwrap_or_else(|| extract_file_symbols(tree, &source, &uri)),
                            None => {
                                params.edit = Some(empty_workspace_edit());
                                return Ok(params);
                            }
                        };
                        (source, file_symbols)
                    }
                    None => {
                        params.edit = Some(empty_workspace_edit());
                        return Ok(params);
                    }
                };

                let Some(class_sym) = file_symbols.symbols.iter().find(|sym| {
                    sym.fqn == class_fqn && sym.kind == php_lsp_types::PhpSymbolKind::Class
                }) else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };

                let php_version = *self.php_version.lock().await;
                let missing_methods =
                    missing_implementation_methods(&self.index, &file_symbols, class_sym);
                let mut metadata_by_fqn = HashMap::new();
                for method in &missing_methods {
                    let declaration_source = self
                        .source_for_uri(&method.uri, "implement missing methods source read")
                        .await;
                    metadata_by_fqn.insert(
                        method.fqn.clone(),
                        method_contract_metadata(method, declaration_source.as_deref()),
                    );
                }
                params.edit = implement_missing_methods_edit(
                    uri_value,
                    &source,
                    class_sym,
                    &missing_methods,
                    &metadata_by_fqn,
                    php_version,
                )
                .or_else(|| Some(empty_workspace_edit()));
            }
            (
                CodeActionDataKind::GenerateConstructor,
                CodeActionDataExtra::GenerateConstructor { class_fqn },
            ) => {
                if self.current_document_version(&uri) != document_version {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                }

                let Ok(uri_value) = uri.parse::<Uri>() else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };

                let (source, file_symbols) = match self.open_files.get(&uri) {
                    Some(parser) => {
                        let source = parser.source();
                        let file_symbols = match parser.tree() {
                            Some(tree) => self
                                .index
                                .file_symbols
                                .get(&uri)
                                .map(|entry| entry.value().clone())
                                .unwrap_or_else(|| extract_file_symbols(tree, &source, &uri)),
                            None => {
                                params.edit = Some(empty_workspace_edit());
                                return Ok(params);
                            }
                        };
                        (source, file_symbols)
                    }
                    None => {
                        params.edit = Some(empty_workspace_edit());
                        return Ok(params);
                    }
                };

                let Some(class_sym) = file_symbols.symbols.iter().find(|sym| {
                    sym.fqn == class_fqn && sym.kind == php_lsp_types::PhpSymbolKind::Class
                }) else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };

                let php_version = *self.php_version.lock().await;
                params.edit = generate_constructor_edit(
                    uri_value,
                    &source,
                    &file_symbols,
                    class_sym,
                    php_version,
                )
                .or_else(|| Some(empty_workspace_edit()));
            }
            (
                CodeActionDataKind::GenerateAccessor,
                CodeActionDataExtra::GenerateAccessor {
                    property_fqn,
                    accessor_kind,
                    method_name,
                },
            ) => {
                if self.current_document_version(&uri) != document_version {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                }

                let Ok(uri_value) = uri.parse::<Uri>() else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };

                let (source, file_symbols) = match self.open_files.get(&uri) {
                    Some(parser) => {
                        let source = parser.source();
                        let file_symbols = match parser.tree() {
                            Some(tree) => self
                                .index
                                .file_symbols
                                .get(&uri)
                                .map(|entry| entry.value().clone())
                                .unwrap_or_else(|| extract_file_symbols(tree, &source, &uri)),
                            None => {
                                params.edit = Some(empty_workspace_edit());
                                return Ok(params);
                            }
                        };
                        (source, file_symbols)
                    }
                    None => {
                        params.edit = Some(empty_workspace_edit());
                        return Ok(params);
                    }
                };

                let Some(property) = file_symbols.symbols.iter().find(|sym| {
                    sym.fqn == property_fqn && sym.kind == php_lsp_types::PhpSymbolKind::Property
                }) else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };

                let php_version = *self.php_version.lock().await;
                params.edit = generate_accessor_edit(
                    uri_value,
                    &source,
                    &file_symbols,
                    property,
                    accessor_kind,
                    &method_name,
                    php_version,
                )
                .or_else(|| Some(empty_workspace_edit()));
            }
            (
                CodeActionDataKind::ChangeVisibility,
                CodeActionDataExtra::ChangeVisibility {
                    symbol_fqn,
                    target_visibility,
                },
            ) => {
                if self.current_document_version(&uri) != document_version {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                }

                let Ok(uri_value) = uri.parse::<Uri>() else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };

                let (source, file_symbols) = match self.open_files.get(&uri) {
                    Some(parser) => {
                        let source = parser.source();
                        let file_symbols = match parser.tree() {
                            Some(tree) => self
                                .index
                                .file_symbols
                                .get(&uri)
                                .map(|entry| entry.value().clone())
                                .unwrap_or_else(|| extract_file_symbols(tree, &source, &uri)),
                            None => {
                                params.edit = Some(empty_workspace_edit());
                                return Ok(params);
                            }
                        };
                        (source, file_symbols)
                    }
                    None => {
                        params.edit = Some(empty_workspace_edit());
                        return Ok(params);
                    }
                };

                let Some(symbol) = file_symbols
                    .symbols
                    .iter()
                    .find(|sym| sym.fqn == symbol_fqn)
                else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };

                params.edit = change_visibility_edit(
                    uri_value,
                    &self.index,
                    &file_symbols,
                    &source,
                    symbol,
                    target_visibility,
                )
                .or_else(|| Some(empty_workspace_edit()));
            }
            (
                CodeActionDataKind::PromoteConstructorParameter,
                CodeActionDataExtra::PromoteConstructorParameter { property_fqn },
            ) => {
                if self.current_document_version(&uri) != document_version {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                }

                let Ok(uri_value) = uri.parse::<Uri>() else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };

                let (source, file_symbols) = match self.open_files.get(&uri) {
                    Some(parser) => {
                        let source = parser.source();
                        let file_symbols = match parser.tree() {
                            Some(tree) => self
                                .index
                                .file_symbols
                                .get(&uri)
                                .map(|entry| entry.value().clone())
                                .unwrap_or_else(|| extract_file_symbols(tree, &source, &uri)),
                            None => {
                                params.edit = Some(empty_workspace_edit());
                                return Ok(params);
                            }
                        };
                        (source, file_symbols)
                    }
                    None => {
                        params.edit = Some(empty_workspace_edit());
                        return Ok(params);
                    }
                };

                let Some(property) = file_symbols.symbols.iter().find(|sym| {
                    sym.fqn == property_fqn && sym.kind == php_lsp_types::PhpSymbolKind::Property
                }) else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };

                params.edit =
                    promote_constructor_parameter_edit(uri_value, &source, &file_symbols, property)
                        .or_else(|| Some(empty_workspace_edit()));
            }
            (
                CodeActionDataKind::UpdatePhpDoc,
                CodeActionDataExtra::UpdatePhpDoc { symbol_fqn },
            ) => {
                if self.current_document_version(&uri) != document_version {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                }

                let Ok(uri_value) = uri.parse::<Uri>() else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };

                let (source, file_symbols) = match self.open_files.get(&uri) {
                    Some(parser) => {
                        let source = parser.source();
                        let file_symbols = match parser.tree() {
                            Some(tree) => self
                                .index
                                .file_symbols
                                .get(&uri)
                                .map(|entry| entry.value().clone())
                                .unwrap_or_else(|| extract_file_symbols(tree, &source, &uri)),
                            None => {
                                params.edit = Some(empty_workspace_edit());
                                return Ok(params);
                            }
                        };
                        (source, file_symbols)
                    }
                    None => {
                        params.edit = Some(empty_workspace_edit());
                        return Ok(params);
                    }
                };

                let Some(symbol) = file_symbols.symbols.iter().find(|sym| {
                    sym.fqn == symbol_fqn
                        && matches!(
                            sym.kind,
                            php_lsp_types::PhpSymbolKind::Function
                                | php_lsp_types::PhpSymbolKind::Method
                        )
                }) else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };

                params.edit = update_phpdoc_from_signature_edit(uri_value, &source, symbol)
                    .or_else(|| Some(empty_workspace_edit()));
            }
            (
                CodeActionDataKind::ExtractVariable,
                CodeActionDataExtra::ExtractVariable { variable_name },
            ) => {
                if self.current_document_version(&uri) != document_version {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                }

                let Ok(uri_value) = uri.parse::<Uri>() else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };

                let Some(parser) = self.open_files.get(&uri) else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };
                let source = parser.source();
                let Some(tree) = parser.tree() else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };
                let range = lsp_range_to_byte_range(&source, requested_range);
                params.edit =
                    extract_variable_edit(uri_value, tree, &source, range, &variable_name)
                        .or_else(|| Some(empty_workspace_edit()));
            }
            (
                CodeActionDataKind::ExtractConstant,
                CodeActionDataExtra::ExtractConstant { constant_name },
            ) => {
                if self.current_document_version(&uri) != document_version {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                }

                let Ok(uri_value) = uri.parse::<Uri>() else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };

                let Some(parser) = self.open_files.get(&uri) else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };
                let source = parser.source();
                let Some(tree) = parser.tree() else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };
                let file_symbols = self
                    .index
                    .file_symbols
                    .get(&uri)
                    .map(|entry| entry.value().clone())
                    .unwrap_or_else(|| extract_file_symbols(tree, &source, &uri));
                let range = lsp_range_to_byte_range(&source, requested_range);
                params.edit = extract_constant_edit(
                    uri_value,
                    tree,
                    &source,
                    &file_symbols,
                    range,
                    &constant_name,
                )
                .or_else(|| Some(empty_workspace_edit()));
            }
            (
                CodeActionDataKind::InlineVariable,
                CodeActionDataExtra::InlineVariable { variable_name },
            ) => {
                if self.current_document_version(&uri) != document_version {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                }

                let Ok(uri_value) = uri.parse::<Uri>() else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };

                let Some(parser) = self.open_files.get(&uri) else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };
                let source = parser.source();
                let Some(tree) = parser.tree() else {
                    params.edit = Some(empty_workspace_edit());
                    return Ok(params);
                };
                let range = lsp_range_to_byte_range(&source, requested_range);
                params.edit = inline_variable_edit(uri_value, tree, &source, range, &variable_name)
                    .or_else(|| Some(empty_workspace_edit()));
            }
            _ => {
                params.edit = Some(empty_workspace_edit());
            }
        }

        Ok(params)
    }
}
