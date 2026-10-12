//! Bounded per-callable CFG and forward must/may-defined dataflow.
use super::*;
use std::collections::{BTreeMap, VecDeque};
use tree_sitter::Node;

const MAX_BLOCKS: usize = 16_384;
const MAX_VARIABLES: usize = 4096;
const MAX_DEPTH: usize = 128;
const MAX_UPDATES: usize = 262_144;

#[derive(Clone, PartialEq, Eq)]
struct State {
    must: Vec<u64>,
    may: Vec<u64>,
    unknown: bool,
}
impl State {
    fn new(count: usize) -> Self {
        Self {
            must: vec![0; count.div_ceil(64)],
            may: vec![0; count.div_ceil(64)],
            unknown: false,
        }
    }
    fn define(&mut self, id: usize) {
        self.must[id / 64] |= 1 << (id % 64);
        self.may[id / 64] |= 1 << (id % 64);
    }
    fn kill(&mut self, id: usize) {
        self.must[id / 64] &= !(1 << (id % 64));
        self.may[id / 64] &= !(1 << (id % 64));
    }
    fn defined(&self, id: usize) -> bool {
        self.must[id / 64] & (1 << (id % 64)) != 0
    }
    fn merge(&mut self, other: &Self) {
        for (a, b) in self.must.iter_mut().zip(&other.must) {
            *a &= b;
        }
        for (a, b) in self.may.iter_mut().zip(&other.may) {
            *a |= b;
        }
        self.unknown |= other.unknown;
    }
}
#[derive(Clone, Copy)]
enum Event {
    Nop,
    Read(usize),
    Define(usize),
    Kill(usize),
    Unknown,
}
struct Block {
    event: Event,
    next: Vec<usize>,
}
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Jump {
    Return,
    Throw,
    Halt,
    Break(usize),
    Continue(usize),
}
struct Builder<'a> {
    source: &'a str,
    occurrences: &'a [VariableOccurrence],
    names: HashMap<String, usize>,
    at: HashMap<usize, Vec<usize>>,
    blocks: Vec<Block>,
    abrupt: Vec<(Jump, usize)>,
    failed: bool,
    visits: usize,
    scope_id: usize,
    by_start: Vec<usize>,
}

/// None means unsupported flow or a spent budget, never a textual fallback.
pub(super) fn undefined_reads(
    scope: Node,
    source: &str,
    occurrences: &[VariableOccurrence],
) -> Option<HashSet<usize>> {
    let mut names = HashMap::new();
    let mut at: HashMap<usize, Vec<usize>> = HashMap::new();
    for (index, item) in occurrences.iter().enumerate() {
        let next = names.len();
        names.entry(item.name.clone()).or_insert(next);
        if names.len() > MAX_VARIABLES {
            return None;
        }
        at.entry(item.start_byte).or_default().push(index);
    }
    let mut by_start = (0..occurrences.len()).collect::<Vec<_>>();
    by_start.sort_by_key(|&index| occurrences[index].start_byte);
    if !within_ast_budget(scope) {
        return None;
    }
    let mut builder = Builder {
        source,
        occurrences,
        names,
        at,
        blocks: Vec::new(),
        abrupt: Vec::new(),
        failed: false,
        visits: 0,
        scope_id: scope.id(),
        by_start,
    };
    let entry = builder.block(Event::Nop, None);
    let mut initial = State::new(builder.names.len());
    for item in occurrences {
        if matches!(
            item.declaration_kind,
            Some(
                VariableDeclarationKind::Parameter
                    | VariableDeclarationKind::PromotedProperty
                    | VariableDeclarationKind::ClosureUse
            )
        ) {
            initial.define(builder.names[&item.name]);
        }
    }
    if scope.kind() == "program" {
        builder.stmt(scope, Some(entry), 0);
    } else if let Some(body) = scope.child_by_field_name("body") {
        if scope.kind() == "arrow_function" {
            builder.expr(body, Some(entry), false, 0);
        } else {
            builder.stmt(body, Some(entry), 0);
        }
    }
    if builder.failed {
        return None;
    }
    builder.solve(initial)
}

impl Builder<'_> {
    fn block(&mut self, event: Event, from: Option<usize>) -> usize {
        if self.blocks.len() >= MAX_BLOCKS {
            self.failed = true;
            return 0;
        }
        let id = self.blocks.len();
        self.blocks.push(Block {
            event,
            next: Vec::new(),
        });
        if let Some(from) = from {
            self.blocks[from].next.push(id);
        }
        id
    }
    fn join(&mut self, inputs: impl IntoIterator<Item = Option<usize>>) -> Option<usize> {
        let inputs = inputs.into_iter().flatten().collect::<Vec<_>>();
        if inputs.is_empty() {
            return None;
        }
        let id = self.block(Event::Nop, None);
        for from in inputs {
            self.blocks[from].next.push(id);
        }
        Some(id)
    }
    fn budget(&mut self, depth: usize) -> bool {
        self.visits += 1;
        if depth > MAX_DEPTH || self.visits > MAX_UPDATES || self.failed {
            self.failed = true;
            false
        } else {
            true
        }
    }
    fn children(node: Node) -> Vec<Node> {
        let mut cursor = node.walk();
        node.named_children(&mut cursor)
            .filter(|n| !n.is_extra())
            .collect()
    }
    fn event(&mut self, event: Event, from: Option<usize>) -> Option<usize> {
        from.map(|from| self.block(event, Some(from)))
    }
    fn implicit_throw(&mut self, end: Option<usize>) -> Option<usize> {
        if let Some(end) = end {
            self.abrupt.push((Jump::Throw, end));
        }
        end
    }
    fn variable(
        &mut self,
        node: Node,
        from: Option<usize>,
        write: bool,
        probe: bool,
    ) -> Option<usize> {
        let name = normalize_var_name(&self.source[node.byte_range()]);
        let Some(&id) = self.names.get(&name) else {
            return from;
        };
        if write {
            return self.event(Event::Define(id), from);
        }
        if probe {
            return from;
        }
        let mut end = from;
        for index in self.at.get(&node.start_byte()).cloned().unwrap_or_default() {
            if !self.occurrences[index].implicit_capture {
                end = self.event(Event::Read(index), end);
            }
        }
        end
    }
    fn captures(&mut self, node: Node, mut end: Option<usize>) -> Option<usize> {
        let start = self
            .by_start
            .partition_point(|&index| self.occurrences[index].start_byte < node.start_byte());
        let finish = self
            .by_start
            .partition_point(|&index| self.occurrences[index].start_byte < node.end_byte());
        for position in start..finish {
            let index = self.by_start[position];
            let item = &self.occurrences[index];
            if item.start_byte >= node.start_byte() && item.start_byte < node.end_byte() {
                if item.explicit_by_ref_capture && !item.implicit_capture {
                    end = self.event(Event::Define(self.names[&item.name]), end);
                } else if (item.implicit_capture || item.declaration_kind.is_none())
                    && !item.null_coalesce_probe
                {
                    end = self.event(Event::Read(index), end);
                }
            }
        }
        end
    }
    fn target(&mut self, node: Node, mut end: Option<usize>, depth: usize) -> Option<usize> {
        if !self.budget(depth) {
            return end;
        }
        match node.kind() {
            "variable_name" => self.variable(node, end, true, false),
            "subscript_expression" => {
                let children = Self::children(node);
                if let Some(base) = children.first() {
                    for key in children.iter().skip(1) {
                        end = self.expr(*key, end, false, depth + 1);
                    }
                    self.target(*base, end, depth + 1)
                } else {
                    end
                }
            }
            "member_access_expression"
            | "nullsafe_member_access_expression"
            | "scoped_property_access_expression" => self.expr(node, end, false, depth + 1),
            "dynamic_variable_name" => {
                end = self.expr(node, end, false, depth + 1);
                self.event(Event::Unknown, end)
            }
            _ => {
                for child in Self::children(node) {
                    let mut vars = Vec::new();
                    collect_vars(child, &mut vars);
                    let writable = vars
                        .iter()
                        .any(|var| crate::variable_scope::is_writable_target_variable(*var, node));
                    end = if writable {
                        self.target(child, end, depth + 1)
                    } else {
                        self.expr(child, end, false, depth + 1)
                    };
                }
                end
            }
        }
    }
    fn expr(
        &mut self,
        node: Node,
        mut end: Option<usize>,
        probe: bool,
        depth: usize,
    ) -> Option<usize> {
        if end.is_none() || !self.budget(depth) {
            return end;
        }
        if node.id() != self.scope_id && is_variable_scope(node) {
            return self.captures(node, end);
        }
        match node.kind() {
            "variable_name" => self.variable(node, end, false, probe),
            "assignment_expression"
            | "by_ref_assignment_expression"
            | "reference_assignment_expression" => {
                let left = node.child_by_field_name("left")?;
                let right = node.child_by_field_name("right")?;
                end = if node.kind() != "assignment_expression" && right.kind() == "variable_name" {
                    self.target(right, end, depth + 1)
                } else {
                    self.expr(right, end, false, depth + 1)
                };
                self.target(left, end, depth + 1)
            }
            "augmented_assignment_expression" => {
                let left = node.child_by_field_name("left")?;
                let right = node.child_by_field_name("right")?;
                let coalesce = node
                    .child_by_field_name("operator")
                    .is_some_and(|op| &self.source[op.byte_range()] == "??=");
                end = self.expr(left, end, coalesce, depth + 1);
                if coalesce {
                    let yes = self.assume_defined(left, end, depth + 1);
                    let no = self.expr(right, end, false, depth + 1);
                    let no = self.target(left, no, depth + 1);
                    self.join([yes, no])
                } else {
                    end = self.expr(right, end, false, depth + 1);
                    self.target(left, end, depth + 1)
                }
            }
            "binary_expression" => {
                let op = node
                    .child_by_field_name("operator")
                    .map(|n| self.source[n.byte_range()].to_ascii_lowercase())
                    .unwrap_or_default();
                if matches!(op.as_str(), "&&" | "and" | "||" | "or") {
                    let (yes, no) = self.condition(node, end, depth + 1);
                    return self.join([yes, no]);
                }
                let left = node.child_by_field_name("left")?;
                let right = node.child_by_field_name("right")?;
                end = self.expr(left, end, op == "??", depth + 1);
                if op == "??" {
                    let yes = self.assume_defined(left, end, depth + 1);
                    let no = self.expr(right, end, false, depth + 1);
                    self.join([yes, no])
                } else {
                    let done = self.expr(right, end, false, depth + 1);
                    if matches!(
                        op.as_str(),
                        "+" | "-" | "*" | "/" | "%" | "**" | "<<" | ">>" | "&" | "|" | "^"
                    ) && self.binary_can_throw(left, right, &op)
                    {
                        self.implicit_throw(done)
                    } else {
                        done
                    }
                }
            }
            "conditional_expression" => {
                let (yes, no) =
                    self.condition(node.child_by_field_name("condition")?, end, depth + 1);
                let yes = node
                    .child_by_field_name("body")
                    .map_or(yes, |body| self.expr(body, yes, false, depth + 1));
                let no = self.expr(
                    node.child_by_field_name("alternative")?,
                    no,
                    false,
                    depth + 1,
                );
                self.join([yes, no])
            }
            "throw_expression" => {
                for child in Self::children(node) {
                    end = self.expr(child, end, false, depth + 1);
                }
                if let Some(end) = end {
                    self.abrupt.push((Jump::Throw, end));
                }
                None
            }
            "update_expression" => {
                for child in Self::children(node) {
                    end = self.expr(child, end, false, depth + 1);
                    if child.kind() == "variable_name" {
                        end = self.target(child, end, depth + 1);
                    }
                }
                end
            }
            "subscript_expression"
            | "member_access_expression"
            | "nullsafe_member_access_expression"
            | "member_call_expression"
            | "nullsafe_member_call_expression" => {
                let (active, skip) = self.chain(node, end, probe, depth + 1);
                self.join([active, skip])
            }
            "parenthesized_expression" => {
                for child in Self::children(node) {
                    end = self.expr(child, end, probe, depth + 1);
                }
                end
            }
            "function_call_expression" => {
                let function = node.child_by_field_name("function")?;
                let name = self.source[function.byte_range()]
                    .trim_start_matches('\\')
                    .to_ascii_lowercase();
                let builtin = matches!(name.as_str(), "isset" | "empty" | "unset")
                    && !self.source[function.byte_range()]
                        .trim_start_matches('\\')
                        .contains('\\');
                if builtin && matches!(name.as_str(), "isset" | "empty") {
                    let (yes, no) = self.probe_condition(node, end, depth + 1, name == "empty");
                    return self.join([yes, no]);
                }
                end = self.expr(function, end, false, depth + 1);
                if let Some(args) = node.child_by_field_name("arguments") {
                    for arg in Self::children(args) {
                        let value = crate::variable_scope::argument_value_node(arg).unwrap_or(arg);
                        if builtin && name == "unset" {
                            end = self.expr(value, end, true, depth + 1);
                            if value.kind() == "variable_name" {
                                if let Some(id) =
                                    self.names.get(&self.source[value.byte_range()]).copied()
                                {
                                    end = self.event(Event::Kill(id), end);
                                }
                            }
                        } else {
                            let mut vars = Vec::new();
                            collect_vars(value, &mut vars);
                            if vars
                                .iter()
                                .any(|var| is_by_ref_output_argument_variable(*var, self.source))
                            {
                                end = self.target(value, end, depth + 1);
                            } else {
                                end = self.expr(value, end, builtin, depth + 1);
                            }
                        }
                    }
                }
                for index in self
                    .at
                    .get(&node.end_byte().saturating_sub(1))
                    .cloned()
                    .unwrap_or_default()
                {
                    if !self.occurrences[index].capture_eligible {
                        end = self.event(Event::Read(index), end);
                    }
                }
                if !builtin && !matches!(name.as_str(), "exit" | "die") {
                    end = self.implicit_throw(end);
                }
                if matches!(name.as_str(), "exit" | "die") {
                    if let Some(end) = end {
                        self.abrupt.push((Jump::Halt, end));
                    }
                    None
                } else if matches!(
                    name.as_str(),
                    "eval" | "extract" | "parse_str" | "include" | "require"
                ) {
                    self.event(Event::Unknown, end)
                } else {
                    end
                }
            }
            "match_expression" => self.selection(node, end, depth + 1, true),
            "scoped_call_expression" | "object_creation_expression" => {
                for child in Self::children(node) {
                    end = self.expr(child, end, false, depth + 1);
                }
                self.implicit_throw(end)
            }
            "class_declaration"
            | "interface_declaration"
            | "trait_declaration"
            | "enum_declaration"
            | "declaration_list" => end,
            "include_expression"
            | "require_expression"
            | "include_once_expression"
            | "require_once_expression" => {
                for child in Self::children(node) {
                    end = self.expr(child, end, false, depth + 1);
                }
                self.event(Event::Unknown, end)
            }
            _ => {
                for child in Self::children(node) {
                    end = self.expr(child, end, false, depth + 1);
                }
                end
            }
        }
    }
    fn assume_defined(&mut self, node: Node, end: Option<usize>, depth: usize) -> Option<usize> {
        if !self.budget(depth) {
            return end;
        }
        match node.kind() {
            "variable_name" => self.variable(node, end, true, false),
            "parenthesized_expression"
            | "subscript_expression"
            | "member_access_expression"
            | "nullsafe_member_access_expression" => Self::children(node)
                .first()
                .map_or(end, |base| self.assume_defined(*base, end, depth + 1)),
            _ => end,
        }
    }
    fn condition(
        &mut self,
        node: Node,
        end: Option<usize>,
        depth: usize,
    ) -> (Option<usize>, Option<usize>) {
        if end.is_none() || !self.budget(depth) {
            return (None, None);
        }
        if node.kind() == "function_call_expression" {
            if let Some(function) = node.child_by_field_name("function") {
                let name = self.source[function.byte_range()]
                    .trim_start_matches('\\')
                    .to_ascii_lowercase();
                if matches!(name.as_str(), "isset" | "empty") {
                    return self.probe_condition(node, end, depth + 1, name == "empty");
                }
            }
        }
        let children = Self::children(node);
        if node.kind() == "parenthesized_expression" {
            return children
                .first()
                .map_or((end, end), |child| self.condition(*child, end, depth + 1));
        }
        if node.kind() == "unary_op_expression"
            && self.source[node.byte_range()].trim_start().starts_with('!')
        {
            if let Some(child) = children.last() {
                let (yes, no) = self.condition(*child, end, depth + 1);
                return (no, yes);
            }
        }
        if node.kind() == "binary_expression" {
            let op = node
                .child_by_field_name("operator")
                .map(|n| self.source[n.byte_range()].to_ascii_lowercase())
                .unwrap_or_default();
            if let (Some(left), Some(right)) = (
                node.child_by_field_name("left"),
                node.child_by_field_name("right"),
            ) {
                if matches!(op.as_str(), "&&" | "and") {
                    let (yes, no) = self.condition(left, end, depth + 1);
                    let (a, b) = self.condition(right, yes, depth + 1);
                    return (a, self.join([no, b]));
                }
                if matches!(op.as_str(), "||" | "or") {
                    let (yes, no) = self.condition(left, end, depth + 1);
                    let (a, b) = self.condition(right, no, depth + 1);
                    return (self.join([yes, a]), b);
                }
            }
        }
        let done = self.expr(node, end, false, depth + 1);
        let raw = self.source[node.byte_range()].trim();
        if raw.eq_ignore_ascii_case("true") {
            return (done, None);
        }
        if raw.eq_ignore_ascii_case("false") || raw.eq_ignore_ascii_case("null") {
            return (None, done);
        }
        let yes = done.map(|done| self.block(Event::Nop, Some(done)));
        let no = done.map(|done| self.block(Event::Nop, Some(done)));
        (yes, no)
    }
    fn probe_condition(
        &mut self,
        node: Node,
        mut yes: Option<usize>,
        depth: usize,
        empty: bool,
    ) -> (Option<usize>, Option<usize>) {
        if !self.budget(depth) {
            return (None, None);
        }
        let mut failures = Vec::new();
        if let Some(args) = node.child_by_field_name("arguments") {
            for arg in Self::children(args) {
                let value = crate::variable_scope::argument_value_node(arg).unwrap_or(arg);
                let done = self.expr(value, yes, true, depth + 1);
                failures.push(done.map(|p| self.block(Event::Nop, Some(p))));
                let next = done.map(|p| self.block(Event::Nop, Some(p)));
                yes = self.assume_defined(value, next, depth + 1);
            }
        }
        let no = self.join(failures);
        if empty {
            (no, yes)
        } else {
            (yes, no)
        }
    }
    fn binary_can_throw(&self, left: Node, right: Node, operator: &str) -> bool {
        let a = self.source[left.byte_range()].trim().parse::<i64>();
        let b = self.source[right.byte_range()].trim().parse::<i64>();
        match (a, b) {
            (Ok(a), Ok(b)) => match operator {
                "/" | "%" => b == 0,
                "<<" | ">>" => b < 0,
                "**" => a == 0 && b < 0,
                _ => false,
            },
            _ => true,
        }
    }
    fn stmt(&mut self, node: Node, mut end: Option<usize>, depth: usize) -> Option<usize> {
        if end.is_none() || !self.budget(depth) {
            return end;
        }
        match node.kind() {
            "if_statement" | "else_if_clause" => {
                let (yes, mut no) =
                    self.condition(node.child_by_field_name("condition")?, end, depth + 1);
                let yes = self.stmt(node.child_by_field_name("body")?, yes, depth + 1);
                let mut results = vec![yes];
                for clause in Self::children(node)
                    .into_iter()
                    .filter(|n| matches!(n.kind(), "else_if_clause" | "else_clause"))
                {
                    if clause.kind() == "else_clause" {
                        no = self.stmt(clause.child_by_field_name("body")?, no, depth + 1);
                        results.push(no);
                        no = None;
                    } else {
                        let (a, b) =
                            self.condition(clause.child_by_field_name("condition")?, no, depth + 1);
                        results.push(self.stmt(clause.child_by_field_name("body")?, a, depth + 1));
                        no = b;
                    }
                }
                results.push(no);
                self.join(results)
            }
            "while_statement" | "do_statement" | "for_statement" | "foreach_statement" => {
                self.loop_statement(node, end, depth + 1)
            }
            "try_statement" => self.try_statement(node, end, depth + 1),
            "switch_statement" => self.selection(node, end, depth + 1, false),
            "return_statement" | "throw_statement" | "break_statement" | "continue_statement"
            | "exit_statement" => {
                for child in Self::children(node) {
                    end = self.expr(child, end, false, depth + 1);
                }
                let levels = Self::children(node)
                    .first()
                    .map(|n| self.source[n.byte_range()].parse::<usize>())
                    .transpose();
                let jump = match node.kind() {
                    "return_statement" => Jump::Return,
                    "exit_statement" => Jump::Halt,
                    "throw_statement" => Jump::Throw,
                    "break_statement" | "continue_statement" => {
                        let Ok(levels) = levels else {
                            self.failed = true;
                            return None;
                        };
                        let level = levels.unwrap_or(1);
                        if level == 0 {
                            self.failed = true;
                            return None;
                        }
                        if node.kind() == "break_statement" {
                            Jump::Break(level)
                        } else {
                            Jump::Continue(level)
                        }
                    }
                    _ => unreachable!(),
                };
                if let Some(end) = end {
                    self.abrupt.push((jump, end));
                }
                None
            }
            "unset_statement" => {
                for child in Self::children(node) {
                    end = self.expr(child, end, true, depth + 1);
                    if child.kind() == "variable_name" {
                        if let Some(id) = self.names.get(&self.source[child.byte_range()]).copied()
                        {
                            end = self.event(Event::Kill(id), end);
                        }
                    }
                }
                end
            }
            "static_variable_declaration" => {
                if let Some(value) = node.child_by_field_name("value") {
                    end = self.expr(value, end, false, depth + 1);
                }
                node.child_by_field_name("name")
                    .map_or(end, |name| self.target(name, end, depth + 1))
            }
            "function_static_declaration" => {
                for child in Self::children(node) {
                    end = self.stmt(child, end, depth + 1);
                }
                end
            }
            "global_declaration" => {
                for child in Self::children(node) {
                    end = self.target(child, end, depth + 1);
                }
                end
            }
            "goto_statement" | "named_label_statement" => {
                self.failed = true;
                end
            }
            "program"
            | "compound_statement"
            | "colon_block"
            | "else_clause"
            | "namespace_definition"
            | "declare_statement" => {
                for child in Self::children(node) {
                    end = self.stmt(child, end, depth + 1);
                }
                end
            }
            _ => self.expr(node, end, false, depth + 1),
        }
    }
    fn loop_statement(
        &mut self,
        node: Node,
        mut end: Option<usize>,
        depth: usize,
    ) -> Option<usize> {
        if let Some(init) = node.child_by_field_name("initialize") {
            end = self.expr(init, end, false, depth + 1);
        }
        let foreach = node.kind() == "foreach_statement";
        if foreach {
            if let Some(iterable) = Self::children(node).first() {
                end = self.expr(*iterable, end, false, depth + 1);
            }
        }
        let head = self.block(Event::Nop, end);
        let before = self.abrupt.len();
        if foreach {
            self.implicit_throw(Some(head));
        }
        let mut after = None;
        let mut cursor = node.walk();
        let bodies = node
            .children_by_field_name("body", &mut cursor)
            .collect::<Vec<_>>();
        let (body_entry, false_exit) = if node.kind() == "do_statement" {
            (Some(head), None)
        } else if foreach {
            (
                Some(self.block(Event::Nop, Some(head))),
                Some(self.block(Event::Nop, Some(head))),
            )
        } else if let Some(condition) = node.child_by_field_name("condition") {
            self.condition(condition, Some(head), depth + 1)
        } else {
            (Some(head), None)
        };
        let mut body_end = body_entry;
        if foreach {
            if let Some(target) = crate::variable_scope::foreach_binding_target(node) {
                body_end = self.target(target, body_end, depth + 1);
            }
        }
        for body in bodies {
            body_end = self.stmt(body, body_end, depth + 1);
        }
        let abrupt = self.abrupt.split_off(before);
        let mut back = vec![body_end];
        let mut exits = vec![false_exit];
        for (jump, block) in abrupt {
            match jump {
                Jump::Break(1) => exits.push(Some(block)),
                Jump::Continue(1) => back.push(Some(block)),
                Jump::Break(n) => self.abrupt.push((Jump::Break(n - 1), block)),
                Jump::Continue(n) => self.abrupt.push((Jump::Continue(n - 1), block)),
                _ => self.abrupt.push((jump, block)),
            }
        }
        let mut back = self.join(back);
        if let Some(update) = node.child_by_field_name("update") {
            back = self.expr(update, back, false, depth + 1);
        }
        if node.kind() == "do_statement" {
            if let Some(condition) = node.child_by_field_name("condition") {
                let (yes, no) = self.condition(condition, back, depth + 1);
                back = yes;
                after = no;
            }
        }
        if let Some(back) = back {
            self.blocks[back].next.push(head);
        }
        exits.push(after);
        self.join(exits)
    }
    fn try_statement(&mut self, node: Node, end: Option<usize>, depth: usize) -> Option<usize> {
        let before = self.abrupt.len();
        let body = node.child_by_field_name("body")?;
        let normal = self.stmt(body, end, depth + 1);
        let mut try_abrupt = self.abrupt.split_off(before);
        let mut normals = vec![normal];
        let mut finally = None;
        let catches = Self::children(node)
            .into_iter()
            .filter(|n| n.kind() == "catch_clause")
            .collect::<Vec<_>>();
        for catch in &catches {
            let throws = try_abrupt
                .iter()
                .filter(|(jump, _)| *jump == Jump::Throw)
                .map(|(_, block)| Some(*block));
            let mut start = self.join(throws);
            if let Some(name) = catch.child_by_field_name("name") {
                start = self.target(name, start, depth + 1);
            }
            normals.push(self.stmt(catch.child_by_field_name("body")?, start, depth + 1));
        }
        if !catches.is_empty() {
            try_abrupt.retain(|(jump, _)| *jump != Jump::Throw);
        }
        try_abrupt.extend(self.abrupt.split_off(before));
        for child in Self::children(node) {
            if child.kind() == "finally_clause" {
                finally = child.child_by_field_name("body");
            }
        }
        let mut normal = self.join(normals);
        if let Some(finally) = finally {
            normal = self.stmt(finally, normal, depth + 1);
            let mut grouped: BTreeMap<Jump, Vec<Option<usize>>> = BTreeMap::new();
            for (jump, block) in try_abrupt {
                grouped.entry(jump).or_default().push(Some(block));
            }
            for (jump, blocks) in grouped {
                let start = self.join(blocks);
                let done = if jump == Jump::Halt {
                    start
                } else {
                    self.stmt(finally, start, depth + 1)
                };
                if let Some(done) = done {
                    self.abrupt.push((jump, done));
                }
            }
        } else {
            self.abrupt.extend(try_abrupt);
        }
        normal
    }
    fn selection(
        &mut self,
        node: Node,
        end: Option<usize>,
        depth: usize,
        is_match: bool,
    ) -> Option<usize> {
        let condition = node.child_by_field_name("condition")?;
        let selector = if is_match {
            self.literal(condition)
        } else {
            None
        };
        let end = self.expr(condition, end, false, depth + 1);
        let body = node.child_by_field_name("body")?;
        let arms = Self::children(body);
        let before = self.abrupt.len();
        let mut fallback = None;
        let mut pending = end;
        let mut entries = Vec::new();
        for arm in &arms {
            if matches!(arm.kind(), "default_statement" | "match_default_expression") {
                fallback = Some(*arm);
                entries.push(None);
                continue;
            }
            let value = if is_match {
                arm.child_by_field_name("conditional_expressions")
            } else {
                arm.child_by_field_name("value")
            };
            let values = if is_match {
                value.map(Self::children).unwrap_or_default()
            } else {
                value.into_iter().collect()
            };
            let mut matched = Vec::new();
            for value in values {
                pending = self.expr(value, pending, false, depth + 1);
                let equal = selector
                    .as_ref()
                    .zip(self.literal(value).as_ref())
                    .map(|(a, b)| a == b);
                matched.push(if equal == Some(false) {
                    None
                } else {
                    pending.map(|p| self.block(Event::Nop, Some(p)))
                });
                pending = if equal == Some(true) {
                    None
                } else {
                    pending.map(|p| self.block(Event::Nop, Some(p)))
                };
            }
            entries.push(self.join(matched));
        }
        let mut previous = None;
        let mut results = Vec::new();
        for (arm, entry) in arms.into_iter().zip(entries) {
            let entry = if Some(arm) == fallback {
                pending
            } else {
                entry
            };
            let mut start = if is_match {
                entry
            } else {
                self.join([entry, previous])
            };
            if is_match {
                if let Some(value) = arm.child_by_field_name("return_expression") {
                    start = self.expr(value, start, false, depth + 1);
                }
                results.push(start);
            } else {
                let value = arm.child_by_field_name("value").map(|n| n.id());
                for child in Self::children(arm) {
                    if Some(child.id()) != value {
                        start = self.stmt(child, start, depth + 1);
                    }
                }
                previous = start;
            }
        }
        if !is_match {
            results.push(previous);
            if fallback.is_none() {
                results.push(pending);
            }
        } else if fallback.is_none() {
            if let Some(unmatched) = pending {
                self.abrupt.push((Jump::Throw, unmatched));
            }
        }
        for (jump, block) in self.abrupt.split_off(before) {
            match jump {
                Jump::Break(1) | Jump::Continue(1) if !is_match => results.push(Some(block)),
                Jump::Break(n) if !is_match => self.abrupt.push((Jump::Break(n - 1), block)),
                Jump::Continue(n) if !is_match => self.abrupt.push((Jump::Continue(n - 1), block)),
                _ => self.abrupt.push((jump, block)),
            }
        }
        self.join(results)
    }
    fn literal(&self, mut node: Node) -> Option<(u8, String)> {
        while node.kind() == "parenthesized_expression" {
            node = node.named_child(0)?;
        }
        let raw = self.source[node.byte_range()].trim();
        if raw.eq_ignore_ascii_case("true") || raw.eq_ignore_ascii_case("false") {
            return Some((0, raw.to_ascii_lowercase()));
        }
        if raw.eq_ignore_ascii_case("null") {
            return Some((1, String::new()));
        }
        if let Ok(value) = raw.parse::<i64>() {
            return Some((2, value.to_string()));
        }
        if node.kind() == "string" || node.kind() == "encapsed_string" && !raw.contains('$') {
            return php_lsp_types::decode_shape_key_token(raw).map(|value| (3, value));
        }
        None
    }
    fn chain(
        &mut self,
        node: Node,
        end: Option<usize>,
        probe: bool,
        depth: usize,
    ) -> (Option<usize>, Option<usize>) {
        if !self.budget(depth) {
            return (end, None);
        }
        let children = Self::children(node);
        let Some(&base) = children.first() else {
            return (end, None);
        };
        let chain = matches!(
            base.kind(),
            "member_call_expression"
                | "nullsafe_member_call_expression"
                | "member_access_expression"
                | "nullsafe_member_access_expression"
                | "subscript_expression"
        );
        let (mut active, mut skip) = if chain {
            self.chain(base, end, probe, depth + 1)
        } else {
            (self.expr(base, end, probe, depth + 1), None)
        };
        if node.kind().starts_with("nullsafe_") {
            let extra = active.map(|p| self.block(Event::Nop, Some(p)));
            skip = self.join([skip, extra]);
            active = active.map(|p| self.block(Event::Nop, Some(p)));
        }
        for child in children.into_iter().skip(1) {
            active = self.expr(child, active, false, depth + 1);
        }
        if !probe || node.kind().ends_with("call_expression") {
            active = self.implicit_throw(active);
        }
        (active, skip)
    }
    fn solve(&self, initial: State) -> Option<HashSet<usize>> {
        let mut states = vec![None; self.blocks.len()];
        states[0] = Some(initial);
        let mut queue = VecDeque::from([0]);
        let mut queued = vec![false; self.blocks.len()];
        queued[0] = true;
        let mut updates = 0;
        while let Some(id) = queue.pop_front() {
            queued[id] = false;
            updates += 1;
            if updates > MAX_UPDATES {
                return None;
            }
            let mut state = states[id].clone()?;
            match self.blocks[id].event {
                Event::Define(id) => state.define(id),
                Event::Kill(id) => state.kill(id),
                Event::Unknown => state.unknown = true,
                _ => {}
            }
            for &next in &self.blocks[id].next {
                let mut merged = state.clone();
                if let Some(previous) = &states[next] {
                    merged.merge(previous);
                }
                if states[next].as_ref() != Some(&merged) {
                    states[next] = Some(merged);
                    if !queued[next] {
                        queue.push_back(next);
                        queued[next] = true;
                    }
                }
            }
        }
        let mut undefined = HashSet::new();
        for (block, state) in self.blocks.iter().zip(states) {
            if let (Event::Read(index), Some(state)) = (block.event, state) {
                if !state.unknown && !state.defined(self.names[&self.occurrences[index].name]) {
                    undefined.insert(index);
                }
            }
        }
        Some(undefined)
    }
}
fn collect_vars<'tree>(node: Node<'tree>, out: &mut Vec<Node<'tree>>) {
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        if node.kind() == "variable_name" {
            out.push(node);
            continue;
        }
        if is_variable_scope(node) {
            continue;
        }
        pending.extend(Builder::children(node));
    }
}
fn within_ast_budget(root: Node) -> bool {
    let mut cursor = root.walk();
    let mut count = 0;
    let mut depth = 0;
    loop {
        count += 1;
        if count > MAX_BLOCKS || depth > MAX_DEPTH {
            return false;
        }
        if cursor.goto_first_child() {
            depth += 1;
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return true;
            }
            depth -= 1;
        }
    }
}
