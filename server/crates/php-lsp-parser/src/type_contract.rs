//! Declaration-scoped PHPDoc/native compatibility over one parsed file.
use crate::resolve::{
    expand_file_type_aliases, map_receiver_type_names, resolve_type_info_relative_to_symbol,
};
use php_lsp_types::type_refinement::{phpdoc_refines_native_with_templates, TypeRefinement};
use php_lsp_types::*;

pub fn refinement_for_symbol(
    symbol: &SymbolInfo,
    doc: &TypeInfo,
    native: &TypeInfo,
    file: &FileSymbols,
) -> TypeRefinement {
    let scoped = file.scoped_at_byte_position(symbol.range.0, symbol.range.1);
    let mut templates = symbol.templates.clone();
    if let Some(owner) = symbol.parent_fqn.as_ref().and_then(|owner| {
        file.symbols
            .iter()
            .find(|ty| ty.fqn.eq_ignore_ascii_case(owner))
    }) {
        templates.extend(owner.templates.clone());
    }
    let doc = expand_file_type_aliases(doc, &scoped, &mut Vec::new());
    let doc = map_receiver_type_names(&doc, &|name| {
        if matches!(name, "self" | "static" | "parent")
            || templates.iter().any(|template| template.name == name)
        {
            return None;
        }
        match resolve_type_info_relative_to_symbol(
            &TypeInfo::Simple(name.to_string()),
            symbol,
            file,
        ) {
            TypeInfo::Simple(name) => Some(name),
            _ => None,
        }
    });
    let native = resolve_type_info_relative_to_symbol(native, symbol, file);
    phpdoc_refines_native_with_templates(
        &doc,
        &native,
        &|d, n| class_relation(symbol, d, n, file),
        &templates,
    )
}

fn class_relation(
    symbol: &SymbolInfo,
    doc: &str,
    native: &str,
    file: &FileSymbols,
) -> Option<bool> {
    let qualify = |name: &str| {
        if matches!(name, "self" | "static") {
            return symbol.parent_fqn.clone();
        }
        if name == "parent" {
            return symbol
                .parent_fqn
                .as_ref()
                .and_then(|owner| {
                    file.symbols
                        .iter()
                        .find(|ty| ty.fqn.eq_ignore_ascii_case(owner))
                })
                .and_then(|owner| owner.extends.first().cloned());
        }
        match resolve_type_info_relative_to_symbol(
            &TypeInfo::Simple(name.to_string()),
            symbol,
            file,
        ) {
            TypeInfo::Simple(name) => Some(name),
            _ => None,
        }
    };
    let (doc, native) = (qualify(doc)?, qualify(native)?);
    let same = |a: &str, b: &str| {
        a.trim_start_matches('\\')
            .eq_ignore_ascii_case(b.trim_start_matches('\\'))
    };
    let mut pending = vec![doc];
    let mut visited = std::collections::HashSet::new();
    let mut incomplete = false;
    while let Some(name) = pending.pop() {
        if same(&name, &native) {
            return Some(true);
        }
        if !visited.insert(name.to_ascii_lowercase()) {
            continue;
        }
        if visited.len() > 64 {
            return None;
        }
        let Some(ty) = file.symbols.iter().find(|ty| {
            matches!(
                ty.kind,
                PhpSymbolKind::Class | PhpSymbolKind::Interface | PhpSymbolKind::Enum
            ) && same(&ty.fqn, &name)
        }) else {
            incomplete = true;
            continue;
        };
        pending.extend(ty.extends.iter().chain(&ty.implements).cloned());
    }
    if incomplete {
        None
    } else {
        Some(false)
    }
}

pub(crate) fn select_signature_types(symbol: &mut SymbolInfo, file: &FileSymbols) {
    let Some(mut signature) = symbol.signature.take() else {
        return;
    };
    let scoped = file.scoped_at_byte_position(symbol.range.0, symbol.range.1);
    for param in &mut signature.params {
        if let Some(doc) = param.phpdoc_type_info.as_ref() {
            let native = param.declared_native_type().cloned();
            param.type_info = if native.as_ref().is_none_or(|native| {
                refinement_for_symbol(symbol, doc, native, file) == TypeRefinement::Compatible
            }) {
                Some(expand_file_type_aliases(doc, &scoped, &mut Vec::new()))
            } else {
                native
            };
        }
    }
    if let Some(doc) = signature.phpdoc_return_type.as_ref() {
        let native = signature.declared_native_return_type();
        signature.return_type = if native.is_none_or(|native| {
            refinement_for_symbol(symbol, doc, native, file) == TypeRefinement::Compatible
        }) {
            Some(expand_file_type_aliases(doc, &scoped, &mut Vec::new()))
        } else {
            native.cloned()
        };
    }
    symbol.signature = Some(signature);
}
