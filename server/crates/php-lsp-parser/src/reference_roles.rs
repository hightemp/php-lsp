//! Read/write roles for ephemeral CST reference locations.
use super::*;

pub(super) fn is_write(node: Node<'_>, source: &str) -> bool {
    if node.kind() == "variable_name" {
        if is_closure_capture_token(node) {
            return false;
        }
        let name = normalize_var_name(&source[node.byte_range()]);
        if is_variable_declaration(node, source, &name) {
            return true;
        }
    } else if is_by_ref_output_binding_variable(node, source) {
        return true;
    }
    let writable = |target: Node<'_>| {
        crate::cst::node_contains(target, node) && is_writable_target_variable(node, target)
    };
    let mut parent = node.parent();
    while let Some(owner) = parent {
        match owner.kind() {
            "assignment_expression"
            | "augmented_assignment_expression"
            | "by_ref_assignment_expression"
            | "reference_assignment_expression" => {
                return owner.child_by_field_name("left").is_some_and(writable);
            }
            "update_expression" => {
                return owner
                    .child_by_field_name("argument")
                    .or_else(|| owner.named_child(0))
                    .is_some_and(writable);
            }
            "foreach_statement" => {
                return foreach_targets(owner).is_some_and(|(key, value)| {
                    key.into_iter().chain(std::iter::once(value)).any(writable)
                });
            }
            "unset_statement" => {
                let mut cursor = owner.walk();
                return owner.named_children(&mut cursor).any(|argument| {
                    let mut cursor = argument.walk();
                    if argument.kind() == "arguments" {
                        argument
                            .named_children(&mut cursor)
                            .filter_map(argument_value_node)
                            .any(writable)
                    } else {
                        writable(argument)
                    }
                });
            }
            _ if is_scope_boundary(owner) || owner.kind() == "program" => return false,
            _ => parent = owner.parent(),
        }
    }
    false
}
