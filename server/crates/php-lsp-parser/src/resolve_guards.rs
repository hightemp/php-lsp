//! Conservative CST proofs for the continuation after an instanceof exit guard.
use super::{
    if_condition_node, positive_instanceof_expression_class_for_var, MAX_OBJECT_TYPE_RESOLVE_DEPTH,
};
use tree_sitter::Node;

const MAX_GUARD_NODES: usize = 4096;

struct Budget {
    remaining: usize,
}
impl Budget {
    fn new() -> Self {
        Self {
            remaining: MAX_GUARD_NODES,
        }
    }
    fn enter(&mut self, depth: usize) -> Option<()> {
        if depth >= MAX_OBJECT_TYPE_RESOLVE_DEPTH || self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        Some(())
    }
}

pub(super) fn post_guard_class(
    stmt: Node,
    variable: &str,
    usage: usize,
    source: &str,
) -> Option<String> {
    if stmt.has_error() {
        return None;
    }
    let condition = if_condition_node(stmt)?;
    let class = guaranteed_class(condition, variable, false, source, 0, &mut Budget::new())?;
    if !exiting_then_before_usage(stmt, usage, source) {
        return None;
    }
    // Restrict to a direct continuation in the same block: a nested loop use
    // could execute again after a later write, without rechecking this guard.
    let block = stmt.parent()?;
    if !matches!(
        block.kind(),
        "compound_statement" | "colon_block" | "program"
    ) {
        return None;
    }
    if block.named_child_count() > MAX_GUARD_NODES {
        return None;
    }
    let mut cursor = block.walk();
    let direct = block.named_children(&mut cursor).any(|node| {
        matches!(
            node.kind(),
            "expression_statement" | "return_statement" | "echo_statement"
        ) && node.start_byte() <= usage
            && usage <= node.end_byte()
    });
    if !direct || !stable_binding(stmt, variable, usage, source)? {
        return None;
    }
    Some(class)
}

/// A terminating arm cannot supply assignments to a later sibling statement.
/// Keep the containing-block check: a throw caught outside that block is not
/// proof that its writes cannot reach the outer continuation.
pub fn exiting_then_before_usage(stmt: Node, usage: usize, source: &str) -> bool {
    if stmt.kind() != "if_statement" || stmt.has_error() || stmt.end_byte() > usage {
        return false;
    }
    let Some(block) = stmt.parent() else {
        return false;
    };
    if !matches!(
        block.kind(),
        "compound_statement" | "colon_block" | "program"
    ) || usage > block.end_byte()
    {
        return false;
    }
    stmt.child_by_field_name("body").is_some_and(|body| {
        branch_has_jump(body, 0, &mut Budget::new()) == Some(false)
            && exits(body, source, 0, &mut Budget::new()) == Some(true)
    })
}

// A later return does not dominate a goto/break/continue, and a label permits
// entry past an earlier return. Nested callable/class labels belong elsewhere.
fn branch_has_jump(node: Node, depth: usize, budget: &mut Budget) -> Option<bool> {
    budget.enter(depth)?;
    if matches!(
        node.kind(),
        "goto_statement" | "named_label_statement" | "break_statement" | "continue_statement"
    ) {
        return Some(true);
    }
    if crate::variable_scope::is_scope_boundary(node) {
        return Some(false);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if branch_has_jump(child, depth + 1, budget)? {
            return Some(true);
        }
    }
    Some(false)
}

fn guaranteed_class(
    node: Node,
    variable: &str,
    outcome: bool,
    source: &str,
    depth: usize,
    budget: &mut Budget,
) -> Option<String> {
    budget.enter(depth)?;
    if node.has_error() || node.is_missing() {
        return None;
    }
    if node.kind() == "parenthesized_expression" {
        let child = super::first_expression_child(node)?;
        return guaranteed_class(child, variable, outcome, source, depth + 1, budget);
    }
    if node.kind() == "unary_op_expression" && node.child_by_field_name("operator")?.kind() == "!" {
        return guaranteed_class(
            node.child_by_field_name("argument")?,
            variable,
            !outcome,
            source,
            depth + 1,
            budget,
        );
    }
    if node.kind() == "binary_expression" {
        let operator = node.child_by_field_name("operator")?.kind();
        let conjunction = matches!(operator, "&&" | "and");
        let disjunction = matches!(operator, "||" | "or");
        if conjunction || disjunction {
            let left = guaranteed_class(
                node.child_by_field_name("left")?,
                variable,
                outcome,
                source,
                depth + 1,
                budget,
            );
            let right = guaranteed_class(
                node.child_by_field_name("right")?,
                variable,
                outcome,
                source,
                depth + 1,
                budget,
            );
            if conjunction == outcome {
                return left.or(right);
            }
            // Either operand can determine this outcome. Only a fact common
            // to both alternatives is guaranteed on every continuation.
            return left
                .zip(right)
                .and_then(|(left, right)| left.eq_ignore_ascii_case(&right).then_some(left));
        }
    }
    outcome
        .then(|| positive_instanceof_expression_class_for_var(node, source, variable))
        .flatten()
}

fn exits(node: Node, source: &str, depth: usize, budget: &mut Budget) -> Option<bool> {
    budget.enter(depth)?;
    if node.has_error() || node.is_missing() {
        return None;
    }
    match node.kind() {
        "return_statement" | "throw_expression" | "exit_statement" => Some(true),
        // The grammar represents die(...) as a name-based function call. PHP
        // reserves this intrinsic; qualified/dynamic names and first-class
        // callable creation do not establish a terminating invocation.
        "function_call_expression" => {
            let function = node.child_by_field_name("function")?;
            let arguments = node.child_by_field_name("arguments")?;
            let mut cursor = arguments.walk();
            Some(
                function.kind() == "name"
                    && source
                        .get(function.byte_range())
                        .is_some_and(|name| name.eq_ignore_ascii_case("die"))
                    && !arguments
                        .named_children(&mut cursor)
                        .any(|node| node.kind() == "variadic_placeholder"),
            )
        }
        "expression_statement" | "parenthesized_expression" => {
            let mut cursor = node.walk();
            let child = node
                .named_children(&mut cursor)
                .find(|node| !node.is_extra())?;
            exits(child, source, depth + 1, budget)
        }
        "compound_statement" | "colon_block" => {
            let mut cursor = node.walk();
            for child in node
                .named_children(&mut cursor)
                .filter(|node| !node.is_extra())
            {
                if exits(child, source, depth + 1, budget)? {
                    return Some(true);
                }
            }
            Some(false)
        }
        "if_statement" => {
            if !exits(node.child_by_field_name("body")?, source, depth + 1, budget)? {
                return Some(false);
            }
            let mut has_else = false;
            let mut cursor = node.walk();
            for clause in node
                .named_children(&mut cursor)
                .filter(|child| matches!(child.kind(), "else_clause" | "else_if_clause"))
            {
                if !exits(
                    clause.child_by_field_name("body")?,
                    source,
                    depth + 1,
                    budget,
                )? {
                    return Some(false);
                }
                has_else |= clause.kind() == "else_clause";
            }
            Some(has_else)
        }
        _ => Some(false),
    }
}

fn mentions(
    node: Node,
    variable: &str,
    source: &str,
    depth: usize,
    budget: &mut Budget,
) -> Option<bool> {
    budget.enter(depth)?;
    if node.kind() == "variable_name" && source.get(node.byte_range()) == Some(variable) {
        return Some(true);
    }
    if crate::variable_scope::is_scope_boundary(node) {
        return Some(false);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if mentions(child, variable, source, depth + 1, budget)? {
            return Some(true);
        }
    }
    Some(false)
}

fn stable_binding(stmt: Node, variable: &str, usage: usize, source: &str) -> Option<bool> {
    let scope = crate::variable_scope::lexical_scope(stmt);
    let condition = if_condition_node(stmt)?;
    let exiting_body = stmt.child_by_field_name("body")?;
    let mut pending = vec![(scope, 0)];
    let mut budget = Budget::new();
    while let Some((node, depth)) = pending.pop() {
        budget.enter(depth)?;
        // This arm was proved terminating and is never executed on the
        // continuation being narrowed; its writes/calls cannot invalidate it.
        if node.id() == exiting_body.id() {
            continue;
        }
        if node.id() != scope.id() && crate::variable_scope::is_scope_boundary(node) {
            if crate::variable_scope::capture_is_by_reference(node, variable, source) {
                return Some(false);
            }
            continue;
        }
        if matches!(
            node.kind(),
            "goto_statement"
                | "yield_expression"
                | "dynamic_variable_name"
                | "include_expression"
                | "include_once_expression"
                | "require_expression"
                | "require_once_expression"
                | "reference_assignment_expression"
                | "by_ref_assignment_expression"
                | "global_declaration"
                | "global_statement"
                | "function_static_declaration"
        ) {
            return Some(false);
        }
        if matches!(node.kind(), "reference_modifier" | "by_ref")
            && node.parent().is_some_and(|parent| {
                mentions(parent, variable, source, 0, &mut budget) != Some(false)
            })
        {
            return Some(false);
        }
        if node.start_byte() < usage {
            // Constructor argument lists are unnamed children in this grammar.
            // Check the arguments node itself for both calls and constructors.
            if node.kind() == "arguments" && mentions(node, variable, source, 0, &mut budget)? {
                return Some(false);
            }
            if node.kind() == "function_call_expression" {
                let function = node.child_by_field_name("function")?;
                if !matches!(function.kind(), "name" | "qualified_name") {
                    return Some(false);
                }
                let name = source
                    .get(function.byte_range())?
                    .rsplit('\\')
                    .next()?
                    .to_ascii_lowercase();
                if matches!(
                    name.as_str(),
                    "assert"
                        | "call_user_func"
                        | "call_user_func_array"
                        | "eval"
                        | "extract"
                        | "get_defined_vars"
                        | "parse_str"
                        | "mb_parse_str"
                ) {
                    return Some(false);
                }
            }
        }
        if condition.start_byte() <= node.start_byte() && node.start_byte() < usage {
            let target = match node.kind() {
                "assignment_expression" | "augmented_assignment_expression" => {
                    node.child_by_field_name("left")
                }
                "update_expression" | "unset_statement" | "foreach_statement" => Some(node),
                "catch_clause" => node.child_by_field_name("name"),
                _ => None,
            };
            if let Some(target) = target {
                if mentions(target, variable, source, 0, &mut budget)? {
                    return Some(false);
                }
            }
        }
        // Pending nodes also consume the remaining traversal budget.
        if pending.len().saturating_add(node.named_child_count()) > budget.remaining {
            return None;
        }
        let mut cursor = node.walk();
        pending.extend(
            node.named_children(&mut cursor)
                .map(|node| (node, depth + 1)),
        );
    }
    Some(true)
}
