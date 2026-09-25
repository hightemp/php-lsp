//! Typed receiver algebra shared by completion, navigation and diagnostics.
use super::super::*;
use php_lsp_parser::resolve::{
    is_builtin_non_object_type as is_builtin_type_name, ResolvedFunctionType,
};
use php_lsp_types::{FileSymbols, PhpSymbolKind, SymbolInfo, TypeInfo};
use std::collections::BTreeMap;

#[cfg(test)]
#[path = "composite_receivers_tests.rs"]
mod tests;

#[derive(Clone)]
pub(super) struct Member {
    pub declarations: Vec<Arc<SymbolInfo>>,
    pub virtual_properties: Vec<PhpDocVirtualMember>,
    pub shape: Option<php_lsp_types::ArrayShapeItem>,
    pub result: Option<TypeInfo>,
}

pub(super) fn composite(ty: &TypeInfo) -> bool {
    match ty {
        TypeInfo::Union(_) | TypeInfo::Intersection(_) => true,
        TypeInfo::Nullable(inner) => composite(inner),
        _ => false,
    }
}

fn without_null(ty: TypeInfo) -> TypeInfo {
    php_lsp_parser::resolve::type_info_without_null(&ty).unwrap_or(TypeInfo::Never)
}

fn absolute_type(ty: TypeInfo) -> TypeInfo {
    php_lsp_parser::resolve::map_receiver_type_names(&ty, &|name| {
        (!is_builtin_type_name(name)).then(|| format!("\\{}", name.trim_start_matches('\\')))
    })
}

fn special_return_type(
    ty: TypeInfo,
    declaring: &str,
    receiver: &str,
    index: &WorkspaceIndex,
) -> TypeInfo {
    php_lsp_parser::resolve::map_receiver_type_names(&ty, &|name| match name {
        "self" => Some(format!("\\{declaring}")),
        "static" | "$this" => Some(format!("\\{receiver}")),
        "parent" => index
            .get_type(declaring)
            .and_then(|symbol| symbol.extends.first().cloned())
            .map(|parent| format!("\\{parent}")),
        _ => None,
    })
}

fn key(symbol: &SymbolInfo) -> String {
    let name = symbol.name.trim_start_matches('$');
    if symbol.kind == PhpSymbolKind::Method {
        format!("method:{}", name.to_ascii_lowercase())
    } else {
        format!("property:{name}")
    }
}

fn joined(types: Vec<TypeInfo>, union: bool) -> Option<TypeInfo> {
    let mut unique = Vec::new();
    for ty in types {
        if ty == TypeInfo::Mixed {
            if union {
                return Some(TypeInfo::Mixed);
            }
            continue;
        }
        if !unique.contains(&ty) {
            unique.push(ty);
        }
    }
    match unique.len() {
        0 => None,
        1 => unique.pop(),
        _ if union => Some(TypeInfo::Union(unique)),
        _ => Some(TypeInfo::Intersection(unique)),
    }
}

/// Keep the receiver tree intact: unions intersect keys; intersections union keys.
pub(super) fn members(
    index: &WorkspaceIndex,
    symbols: &FileSymbols,
    ty: &TypeInfo,
    range: Option<(u32, u32, u32, u32)>,
) -> BTreeMap<String, Member> {
    members_for_access(
        index,
        symbols,
        ty,
        range,
        php_lsp_completion::context::MemberAccessMode::Read,
    )
}

pub(super) fn members_for_access(
    index: &WorkspaceIndex,
    symbols: &FileSymbols,
    ty: &TypeInfo,
    range: Option<(u32, u32, u32, u32)>,
    mode: php_lsp_completion::context::MemberAccessMode,
) -> BTreeMap<String, Member> {
    match ty {
        TypeInfo::Union(parts) | TypeInfo::Intersection(parts) => {
            let union = matches!(ty, TypeInfo::Union(_));
            let mut maps = parts
                .iter()
                .map(|part| members_for_access(index, symbols, part, range, mode));
            let Some(mut result) = maps.next() else {
                return BTreeMap::new();
            };
            for next in maps {
                if union {
                    result.retain(|key, _| next.contains_key(key));
                }
                for (key, other) in next {
                    if let Some(current) = result.get_mut(&key) {
                        current.result = match (&current.result, &other.result) {
                            (Some(a), Some(b)) => joined(vec![a.clone(), b.clone()], union),
                            (Some(known), None) | (None, Some(known)) if !union => {
                                Some(known.clone())
                            }
                            _ => None,
                        };
                        for declaration in other.declarations {
                            if !current.declarations.iter().any(|old| {
                                old.uri == declaration.uri
                                    && old.selection_range == declaration.selection_range
                            }) {
                                current.declarations.push(declaration);
                            }
                        }
                        for declaration in other.virtual_properties {
                            if !current.virtual_properties.iter().any(|old| {
                                old.owner.uri == declaration.owner.uri
                                    && old.owner.selection_range
                                        == declaration.owner.selection_range
                                    && old.name == declaration.name
                            }) {
                                current.virtual_properties.push(declaration);
                            }
                        }
                    } else if !union {
                        result.insert(key, other);
                    }
                }
            }
            result
        }
        TypeInfo::Nullable(inner) => members_for_access(index, symbols, inner, range, mode),
        TypeInfo::ObjectShape(items) => items
            .iter()
            .filter(|item| !item.optional)
            .filter_map(|item| {
                let name = item.key.as_ref()?;
                Some((
                    format!("property:{}", php_lsp_types::normalize_shape_key_text(name)),
                    Member {
                        declarations: Vec::new(),
                        virtual_properties: Vec::new(),
                        shape: Some(item.clone()),
                        result: Some(item.value.clone()),
                    },
                ))
            })
            .collect(),
        TypeInfo::Simple(name) | TypeInfo::Generic { base: name, .. }
            if !is_builtin_type_name(name) =>
        {
            let fqn = if name.starts_with('\\') {
                name.trim_start_matches('\\').to_string()
            } else {
                resolve_class_name_pub(name, symbols)
            };
            let mut result = BTreeMap::new();
            for symbol in index.get_members(&fqn) {
                if !matches!(symbol.kind, PhpSymbolKind::Method | PhpSymbolKind::Property)
                    || symbol.modifiers.is_static
                    || range.is_some_and(|range| {
                        visibility_violation_message(index, &symbol, symbols, range).is_some()
                    })
                {
                    continue;
                }
                if php_lsp_completion::provider::phpdoc_property_access_for_symbol(&symbol)
                    .is_some_and(|access| {
                        !php_lsp_completion::provider::phpdoc_property_matches_access(access, mode)
                    })
                {
                    continue;
                }
                let target = format!("{fqn}::{}", symbol.name);
                let substitutions = receiver_template_substitutions_from_index(
                    index,
                    symbols,
                    &CallableParameterContext {
                        target_fqn: &target,
                        argument_index: 0,
                        argument_name: None,
                        parameter_index: 0,
                        parameter_name: "",
                        receiver_type: Some(ty),
                        argument_types: &[],
                    },
                );
                let result_type = symbol_effective_return_type(&symbol)
                    .map(|ty| substitute_call_site_type_info(&ty, &substitutions))
                    .map(|ty| {
                        special_return_type(
                            ty,
                            symbol.parent_fqn.as_deref().unwrap_or(&fqn),
                            &fqn,
                            index,
                        )
                    })
                    .map(|ty| {
                        let declaring = index
                            .read()
                            .file_symbols()
                            .get(&symbol.uri)
                            .map(|entry| entry.value().clone())
                            .unwrap_or_default();
                        php_lsp_parser::resolve::resolve_type_info_relative_to_symbol(
                            &ty, &symbol, &declaring,
                        )
                    })
                    .map(absolute_type);
                result.entry(key(&symbol)).or_insert(Member {
                    declarations: vec![symbol],
                    virtual_properties: Vec::new(),
                    shape: None,
                    result: result_type,
                });
            }
            for owner in index.get_type_hierarchy_symbols(&fqn) {
                let Some(doc) = &owner.doc_comment else {
                    continue;
                };
                for property in parse_phpdoc(doc).properties {
                    if !php_lsp_completion::provider::phpdoc_property_matches_access(
                        property.access,
                        mode,
                    ) {
                        continue;
                    }
                    let target = format!("{fqn}::${}", property.name);
                    let substitutions = receiver_template_substitutions_from_index(
                        index,
                        symbols,
                        &CallableParameterContext {
                            target_fqn: &target,
                            argument_index: 0,
                            argument_name: None,
                            parameter_index: 0,
                            parameter_name: "",
                            receiver_type: Some(ty),
                            argument_types: &[],
                        },
                    );
                    let result_type = property.type_info.as_ref().map(|ty| {
                        let declaring = index
                            .read()
                            .file_symbols()
                            .get(&owner.uri)
                            .map(|entry| entry.value().clone())
                            .unwrap_or_default();
                        let ty = substitute_call_site_type_info(ty, &substitutions);
                        let ty = special_return_type(ty, &owner.fqn, &fqn, index);
                        absolute_type(
                            php_lsp_parser::resolve::resolve_type_info_relative_to_symbol(
                                &ty, &owner, &declaring,
                            ),
                        )
                    });
                    result
                        .entry(format!("property:{}", property.name))
                        .or_insert(Member {
                            declarations: Vec::new(),
                            shape: None,
                            virtual_properties: vec![PhpDocVirtualMember {
                                owner: owner.clone(),
                                name: property.name,
                                kind: PhpDocVirtualMemberKind::Property,
                                type_info: property.type_info,
                                access: Some(property.access),
                                return_type: None,
                                params: Vec::new(),
                                description: property.description,
                                is_static: false,
                            }],
                            result: result_type,
                        });
                }
            }
            result
        }
        _ => BTreeMap::new(),
    }
}

pub(super) fn member(
    index: &WorkspaceIndex,
    symbols: &FileSymbols,
    ty: &TypeInfo,
    name: &str,
    property: bool,
    range: Option<(u32, u32, u32, u32)>,
) -> Option<Member> {
    let key = if property {
        format!("property:{}", name.trim_start_matches('$'))
    } else {
        format!("method:{}", name.to_ascii_lowercase())
    };
    members(index, symbols, ty, range).remove(&key)
}

pub(super) fn selected(
    index: &WorkspaceIndex,
    symbols: &FileSymbols,
    source: &str,
    node: tree_sitter::Node,
    receiver: &TypeInfo,
) -> Option<Member> {
    let name = node.child_by_field_name("name")?;
    let scoped = symbols.scoped_at_byte_position(
        node.start_position().row as u32,
        node.start_position().column as u32,
    );
    let property = node.kind().ends_with("access_expression");
    let key = if property {
        format!("property:{}", &source[name.byte_range()])
    } else {
        format!("method:{}", source[name.byte_range()].to_ascii_lowercase())
    };
    let mode =
        php_lsp_completion::context::member_access_mode_after_cursor(&source[name.end_byte()..]);
    members_for_access(index, &scoped, receiver, Some(node_range_node(node)), mode).remove(&key)
}

pub(super) fn completion_item(member: Member, prefix: &str) -> Option<lsp_types::CompletionItem> {
    let mut item = if let Some(symbol) = member.declarations.first() {
        let mut symbol = symbol.as_ref().clone();
        if let Some(signature) = &mut symbol.signature {
            signature.return_type = member.result.clone();
        }
        php_lsp_completion::provider::symbol_to_completion_item(&symbol, false, Some(prefix))
    } else if let Some(property) = member.virtual_properties.first() {
        php_lsp_completion::provider::phpdoc_property_completion_item(
            &property.owner.fqn,
            &php_lsp_types::PhpDocProperty {
                name: property.name.clone(),
                type_info: member.result.clone(),
                access: property
                    .access
                    .unwrap_or(php_lsp_types::PhpDocPropertyAccess::ReadWrite),
                description: property.description.clone(),
            },
            prefix,
        )
    } else {
        let mut shape = member
            .shape
            .expect("structural member has a shape declaration");
        shape.value = member.result.clone().unwrap_or(TypeInfo::Mixed);
        shape_completion_items_from_type_info(
            &TypeInfo::ObjectShape(vec![shape]),
            ShapeCompletionKind::ObjectProperty,
            "",
            None,
        )
        .into_iter()
        .next()?
    };
    // Composite details are already materialized; resolve must not reload one branch.
    item.data = None;
    if let Some(result) = member.result {
        item.detail = Some(format!(
            "{}: {}",
            item.label,
            php_lsp_parser::resolve::receiver_type_text(&result)
        ));
    }
    Some(item)
}

/// Full expression inference for receiver chains, never a first-object projection.
pub(super) fn expression_type(
    _tree: &tree_sitter::Tree,
    source: &str,
    symbols: &FileSymbols,
    index: &WorkspaceIndex,
    node: tree_sitter::Node,
) -> Option<TypeInfo> {
    expression_resolution(source, symbols, index, node).map(|(ty, _)| ty)
}

fn expression_resolution(
    source: &str,
    symbols: &FileSymbols,
    index: &WorkspaceIndex,
    node: tree_sitter::Node,
) -> Option<(TypeInfo, bool)> {
    php_lsp_parser::resolve::within_type_resolution_budget(|| {
        expression_resolution_inner(source, symbols, index, node)
    })
}

fn expression_resolution_inner(
    source: &str,
    symbols: &FileSymbols,
    index: &WorkspaceIndex,
    node: tree_sitter::Node,
) -> Option<(TypeInfo, bool)> {
    let node = normalized_expression_node(node);
    let scoped = symbols.scoped_at_byte_position(
        node.start_position().row as u32,
        node.start_position().column as u32,
    );
    let symbols = scoped.as_ref();
    if matches!(
        node.kind(),
        "member_call_expression"
            | "nullsafe_member_call_expression"
            | "member_access_expression"
            | "nullsafe_member_access_expression"
    ) {
        let object = node.child_by_field_name("object")?;
        let name = node.child_by_field_name("name")?;
        let (receiver, origin) = expression_resolution(source, symbols, index, object)?;
        let nullable = node.kind().starts_with("nullsafe_")
            && php_lsp_parser::resolve::type_info_without_null(&receiver).as_ref()
                != Some(&receiver);
        let receiver = if node.kind().starts_with("nullsafe_") {
            without_null(receiver)
        } else {
            receiver
        };
        let property = node.kind().ends_with("access_expression");
        let result = member(
            index,
            symbols,
            &receiver,
            &source[name.byte_range()],
            property,
            Some(node_range_node(node)),
        )?
        .result?;
        let result = if nullable {
            TypeInfo::Nullable(Box::new(result))
        } else {
            result
        };
        let origin = origin || composite(&result);
        return Some((result, origin));
    }
    let resolver = |owner: &str, name: &str| resolve_member_type_from_index(index, owner, name);
    let callable = |ctx: CallableParameterContext<'_>| {
        resolve_callable_parameter_type_from_index(index, symbols, ctx)
    };
    let function = |name: &str| {
        resolve_function_return_type_from_index(index, name).map(ResolvedFunctionType::new)
    };
    let ty = php_lsp_parser::resolve::infer_expression_type_info_with_function_resolver(
        node,
        source,
        symbols,
        Some(&resolver),
        Some(&callable),
        Some(&function),
    )?;
    let ty = php_lsp_parser::resolve::qualify_receiver_type(&ty, node, source, symbols);
    let origin = composite(&ty);
    Some((ty, origin))
}

pub(super) fn receiver_at<'a>(
    tree: &'a tree_sitter::Tree,
    source: &str,
    symbols: &FileSymbols,
    index: &WorkspaceIndex,
    line: u32,
    col: u32,
) -> Option<(tree_sitter::Node<'a>, TypeInfo)> {
    let node = access_at(tree, line, col)?;
    let (ty, origin) =
        expression_resolution(source, symbols, index, node.child_by_field_name("object")?)?;
    origin.then(|| {
        (
            node,
            if node.kind().starts_with("nullsafe_") {
                without_null(ty)
            } else {
                ty
            },
        )
    })
}

pub(super) fn access_at(
    tree: &tree_sitter::Tree,
    line: u32,
    col: u32,
) -> Option<tree_sitter::Node<'_>> {
    let point = tree_sitter::Point::new(line as usize, col as usize);
    let mut node = tree.root_node().descendant_for_point_range(point, point)?;
    loop {
        if matches!(
            node.kind(),
            "member_call_expression"
                | "nullsafe_member_call_expression"
                | "member_access_expression"
                | "nullsafe_member_access_expression"
        ) {
            let name = node.child_by_field_name("name")?;
            if name.kind() != "name" {
                return None;
            }
            if name.start_position() <= point && point <= name.end_position() {
                return Some(node);
            }
        }
        node = node.parent()?;
    }
}

pub(super) fn expression_at(
    tree: &tree_sitter::Tree,
    line: u32,
    col: u32,
) -> Option<tree_sitter::Node<'_>> {
    let point = tree_sitter::Point::new(line as usize, col as usize);
    let mut node = tree.root_node().descendant_for_point_range(point, point)?;
    loop {
        if matches!(
            node.kind(),
            "variable_name"
                | "member_call_expression"
                | "member_access_expression"
                | "nullsafe_member_call_expression"
                | "nullsafe_member_access_expression"
        ) {
            return Some(node);
        }
        node = node.parent()?;
    }
}

pub(super) fn type_targets(ty: &TypeInfo) -> Vec<String> {
    match ty {
        TypeInfo::Simple(name) | TypeInfo::Generic { base: name, .. }
            if !is_builtin_type_name(name) =>
        {
            vec![name.trim_start_matches('\\').to_string()]
        }
        TypeInfo::Union(parts) | TypeInfo::Intersection(parts) => {
            parts.iter().flat_map(type_targets).collect()
        }
        TypeInfo::Nullable(inner) => type_targets(inner),
        _ => Vec::new(),
    }
}

pub(super) fn completion_node<'a>(
    tree: &'a tree_sitter::Tree,
    source: &str,
    line: u32,
    col: u32,
    expression: &str,
) -> Option<tree_sitter::Node<'a>> {
    let offset = byte_offset_for_line_col(source, line, col)?;
    let start = source.get(..offset)?.rfind(expression)?;
    tree.root_node()
        .descendant_for_byte_range(start, start + expression.len())
}

fn class_leaves(ty: &TypeInfo, names: &mut Vec<String>) {
    match ty {
        TypeInfo::Simple(name) if !is_builtin_type_name(name) => {
            names.push(name.trim_start_matches('\\').to_string())
        }
        TypeInfo::Generic { base, args } => {
            if !is_builtin_type_name(base) {
                names.push(base.trim_start_matches('\\').to_string());
            }
            for arg in args {
                class_leaves(arg, names);
            }
        }
        TypeInfo::Union(parts) | TypeInfo::Intersection(parts) => {
            for part in parts {
                class_leaves(part, names);
            }
        }
        TypeInfo::Nullable(inner) => class_leaves(inner, names),
        _ => {}
    }
}

/// Load receiver/dependency leaves in expression order, using existing vendor leases.
pub(super) async fn preload<'a>(
    tree: &'a tree_sitter::Tree,
    source: &str,
    symbols: &FileSymbols,
    context: &VendorLazyIndexContext,
    nodes: Vec<tree_sitter::Node<'a>>,
) {
    let mut stack: Vec<_> = nodes.into_iter().map(|node| (node, false)).collect();
    let mut visited = HashSet::new();
    let mut loaded = HashSet::new();
    while let Some((node, ready)) = stack.pop() {
        if !ready {
            if !visited.insert(node.id()) {
                continue;
            }
            stack.push((node, true));
            if let Some(object) = node.child_by_field_name("object") {
                stack.push((object, false));
            } else if let Some(operand) = php_lsp_parser::resolve::first_expression_child(node) {
                stack.push((operand, false));
            } else if node.kind() == "variable_name" {
                if let Some((_, rhs)) = latest_assignment_rhs_before_usage(
                    local_variable_scope_node(node),
                    &source[node.byte_range()],
                    node.start_byte(),
                    source,
                ) {
                    stack.push((rhs, false));
                }
            } else if matches!(node.kind(), "subscript_expression") {
                if let Some(child) = node.named_child(0) {
                    stack.push((child, false));
                }
            }
            continue;
        }
        if let Some(ty) = expression_type(tree, source, symbols, &context.index, node) {
            let mut names = Vec::new();
            class_leaves(&ty, &mut names);
            for name in names {
                if loaded.insert(name.clone()) {
                    lazy_index_class_dependencies_with_context(context, &name).await;
                }
            }
        }
    }
}

pub(super) fn known(index: &WorkspaceIndex, ty: &TypeInfo) -> bool {
    let mut names = Vec::new();
    class_leaves(ty, &mut names);
    names.into_iter().all(|name| index.contains_type(&name))
}

pub(super) fn completion_type(
    tree: &tree_sitter::Tree,
    source: &str,
    symbols: &FileSymbols,
    index: &WorkspaceIndex,
    line: u32,
    col: u32,
    expression: &str,
) -> Option<TypeInfo> {
    let offset = byte_offset_for_line_col(source, line, col)?;
    let start = source.get(..offset)?.rfind(expression)?;
    let node = tree
        .root_node()
        .descendant_for_byte_range(start, start + expression.len())?;
    let scoped = symbols.scoped_at_byte_position(line, col);
    let (ty, origin) = expression_resolution(source, &scoped, index, node)?;
    origin.then(|| {
        if source
            .get(start + expression.len()..offset)
            .is_some_and(|tail| tail.trim_start().starts_with("?->"))
        {
            without_null(ty)
        } else {
            ty
        }
    })
}
