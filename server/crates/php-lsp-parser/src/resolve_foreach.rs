//! Shared foreach binding identity and element paths for parser/server inference.
use super::*;

pub struct ForeachBinding<'tree> {
    pub variable: Node<'tree>,
    pub path: Vec<Option<String>>,
    pub is_key: bool,
}

pub fn foreach_binding<'tree>(
    statement: Node<'tree>,
    variable: &str,
    before: usize,
    source: &str,
) -> Option<ForeachBinding<'tree>> {
    super::within_type_scan_budget(|| foreach_binding_inner(statement, variable, before, source))
}

fn foreach_binding_inner<'tree>(
    statement: Node<'tree>,
    variable: &str,
    before: usize,
    source: &str,
) -> Option<ForeachBinding<'tree>> {
    let (key, value) = crate::variable_scope::foreach_targets(statement)?;
    // PHP writes the key last if key and value reuse the same local name.
    for (target, is_key) in key
        .into_iter()
        .map(|key| (key, true))
        .chain(std::iter::once((value, false)))
    {
        if let Some((binding, path)) = crate::variable_scope::destructuring_binding_with_budget(
            target,
            variable,
            source,
            budget::consume,
        ) {
            if binding.start_byte() <= before {
                return Some(ForeachBinding {
                    variable: binding,
                    path,
                    is_key,
                });
            }
        }
    }
    None
}

pub fn foreach_binding_type_info(
    iterable: &TypeInfo,
    binding: &ForeachBinding<'_>,
) -> Option<TypeInfo> {
    let mut ty = if binding.is_key {
        iterable_key_type_info(iterable)?
    } else {
        iterable_value_type_info(iterable, None)?
    };
    for key in &binding.path {
        ty = iterable_value_type_info(
            &ty,
            Some(&php_lsp_types::quote_shape_key_text(key.as_deref()?)),
        )?;
    }
    Some(ty)
}

/// The reaching foreach binding in this lexical/capture component. A later
/// local write wins; reference captures cannot freeze the parent's old value.
pub fn enclosing_foreach_binding<'tree>(
    variable_node: Node<'tree>,
    source: &str,
) -> Option<(Node<'tree>, ForeachBinding<'tree>)> {
    super::within_type_scan_budget(|| {
        let name = source.get(variable_node.byte_range())?;
        if variable_node.kind() != "variable_name" {
            return None;
        }
        let mut origin = variable_node;
        let mut scope = crate::variable_scope::lexical_scope(origin);
        let mut before = variable_node.start_byte();
        loop {
            let mut current = Some(origin);
            while let Some(node) = current {
                if !budget::consume() {
                    return None;
                }
                if node.kind() == "foreach_statement" {
                    if let Some(binding) = foreach_binding(node, name, before, source) {
                        if !written_between(
                            scope,
                            name,
                            binding.variable.start_byte(),
                            before,
                            source,
                        )? {
                            return Some((node, binding));
                        }
                        return None;
                    }
                }
                if node.id() == scope.id() {
                    break;
                }
                current = node.parent();
            }
            if written_between(scope, name, scope.start_byte(), before, source)?
                || crate::variable_scope::capture_is_by_reference(scope, name, source)
            {
                return None;
            }
            let (parent, creation) = crate::variable_scope::captured_parent(scope, source, name)?;
            origin = scope;
            scope = parent;
            before = creation;
        }
    })
}

fn written_between(
    scope: Node,
    variable: &str,
    after: usize,
    before: usize,
    source: &str,
) -> Option<bool> {
    let mut cursor = scope.walk();
    loop {
        if !budget::consume() {
            return None;
        }
        let node = cursor.node();
        let mut descend = node.start_byte() < before && node.end_byte() > after;
        if node.id() != scope.id() && crate::variable_scope::is_scope_boundary(node) {
            if descend && crate::variable_scope::capture_is_by_reference(node, variable, source) {
                return Some(true);
            }
            descend = false;
        }
        if descend {
            if node.start_byte() > after
                && super::invalidating_write_for_var(node, variable, before, source)
            {
                return Some(true);
            }
            if node.kind() == "foreach_statement"
                && foreach_binding(node, variable, before, source)
                    .is_some_and(|binding| binding.variable.start_byte() > after)
            {
                return Some(true);
            }
            if node.end_byte() <= before {
                let target = match node.kind() {
                    "assignment_expression"
                    | "reference_assignment_expression"
                    | "by_ref_assignment_expression"
                    | "augmented_assignment_expression" => node.child_by_field_name("left"),
                    "update_expression" => {
                        let mut children = node.walk();
                        let target = node.named_children(&mut children).find(|n| !n.is_extra());
                        target
                    }
                    _ => None,
                };
                if target
                    .and_then(|target| {
                        crate::variable_scope::destructuring_binding_before_with_budget(
                            target,
                            variable,
                            before,
                            source,
                            budget::consume,
                        )
                    })
                    .is_some_and(|(binding, _)| binding.start_byte() > after)
                {
                    return Some(true);
                }
                if node.kind() == "unset_statement" {
                    let mut children = node.walk();
                    if node.named_children(&mut children).any(|child| {
                        child.kind() == "variable_name"
                            && source.get(child.byte_range()) == Some(variable)
                    }) {
                        return Some(true);
                    }
                }
            }
        }
        if descend && cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return Some(false);
            }
        }
    }
}
