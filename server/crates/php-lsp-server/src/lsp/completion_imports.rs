//! Safe insertion spelling for symbol completions in the cursor's import scope.
use super::*;

pub(super) struct CompletionImport {
    pub insert_text: String,
    pub text_edit: CompletionTextEdit,
    pub additional_edit: Option<TextEdit>,
}

fn resolves_to(
    name: &str,
    file: &php_lsp_types::FileSymbols,
    symbol: &php_lsp_types::SymbolInfo,
) -> bool {
    let resolved = match import_kind_for_completion_symbol(symbol) {
        Some(ImportKind::Class) => php_lsp_parser::resolve::resolve_class_name(name, file),
        Some(ImportKind::Function) => {
            php_lsp_parser::resolve::resolve_function_name_pub(name, file)
        }
        Some(ImportKind::Constant) => {
            php_lsp_parser::resolve::resolve_constant_name_pub(name, file)
        }
        None => return false,
    };
    php_lsp_types::symbol_fqn_eq(&resolved, &symbol.fqn, symbol.kind)
}

pub(super) fn prepare_import(
    source: &str,
    tree: &tree_sitter::Tree,
    file: &php_lsp_types::FileSymbols,
    symbol: &php_lsp_types::SymbolInfo,
    prefix: &str,
    line: u32,
    byte_col: u32,
) -> Option<CompletionImport> {
    let offset = crate::util::lsp_text::byte_offset_for_line_col(source, line, byte_col)?;
    let start = offset.checked_sub(prefix.len())?;
    if source.get(start..offset)? != prefix {
        return None;
    }
    let tail = source
        .get(offset..)?
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .map(char::len_utf8)
        .sum::<usize>();
    if source.as_bytes().get(offset + tail) == Some(&b'\\') {
        return None;
    }
    let range = crate::util::lsp_text::range_from_byte_range(
        source,
        byte_offsets_to_range(source, start, offset + tail),
    );
    let mut additional_edit = None;
    let name = if let Some((qualifier, _)) = prefix.rsplit_once('\\') {
        // Preserve the user's actual qualifier. It may be absolute, relative,
        // namespace-relative, or an alias; none implies an automatic import.
        let name = format!("{qualifier}\\{}", symbol.name);
        if !resolves_to(&name, file, symbol) {
            return None;
        }
        name
    } else {
        let kind = import_kind_for_completion_symbol(symbol)?;
        let name = existing_import_for_fqn(file, &symbol.fqn, kind)
            .map(existing_use_alias)
            .unwrap_or_else(|| symbol.name.clone());
        if resolves_to(&name, file, symbol) {
            name
        } else {
            if used_import_aliases(file, kind).contains(&name) {
                return None;
            }
            additional_edit = build_completion_auto_import_edit(source, file, symbol)
                .filter(|edit| safe_import_location(source, tree, file, edit, range));
            if additional_edit.is_some() {
                name
            } else {
                format!("\\{}", symbol.fqn.trim_start_matches('\\'))
            }
        }
    };
    Some(CompletionImport {
        insert_text: name.clone(),
        text_edit: CompletionTextEdit::Edit(TextEdit {
            range,
            new_text: name,
        }),
        additional_edit,
    })
}

fn safe_import_location(
    source: &str,
    tree: &tree_sitter::Tree,
    file: &php_lsp_types::FileSymbols,
    edit: &TextEdit,
    replacement: Range,
) -> bool {
    let positions = Utf16LineIndex::new(source);
    let position = positions.position_to_point(edit.range.start.line, edit.range.start.character);
    // The existing line-oriented builder must not insert before the PHP tag,
    // into another namespace/body, or at the main completion replacement.
    if position == (0, 0)
        || positions
            .position_to_offset(edit.range.start.line, edit.range.start.character)
            .is_none()
    {
        return false;
    }
    if edit.range.start >= replacement.start && edit.range.start <= replacement.end {
        return false;
    }
    if file.namespace_scopes.first().is_some_and(|scope| {
        position < (scope.range.0, scope.range.1) || position >= (scope.range.2, scope.range.3)
    }) {
        return false;
    }
    let offset = positions
        .position_to_offset(edit.range.start.line, edit.range.start.character)
        .unwrap();
    if offset == 0 {
        return false;
    }
    let mut cursor = tree.root_node().walk();
    if tree
        .root_node()
        .named_children(&mut cursor)
        .any(|node| node.kind() == "declare_statement" && offset <= node.end_byte())
    {
        return false;
    }
    // Inspect the byte before the insertion, so a following function/class
    // declaration is not mistaken for an enclosing scope. Only program and
    // namespace bodies are legal here; comments and incomplete/group imports
    // cannot swallow a new use statement.
    let Some(mut owner) = tree
        .root_node()
        .descendant_for_byte_range(offset - 1, offset - 1)
    else {
        return false;
    };
    while owner.kind() != "program" {
        match owner.kind() {
            "namespace_definition" => {}
            "compound_statement"
                if owner
                    .parent()
                    .is_some_and(|parent| parent.kind() == "namespace_definition") => {}
            _ => return false,
        }
        let Some(parent) = owner.parent() else {
            return false;
        };
        owner = parent;
    }
    true
}
