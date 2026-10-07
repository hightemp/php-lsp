//! Signature help call-site detection.
//!
//! Finds the callable expression that owns the argument list at a cursor
//! position and resolves it using the existing symbol resolver.

use crate::resolve::{
    resolve_class_name_pub, resolve_function_name_pub, resolve_scope_class_name_pub,
    symbol_at_position_with_resolver, try_resolve_object_type,
    unqualified_name_allows_global_fallback, MemberTypeResolver, RefKind, SymbolAtPosition,
};
use php_lsp_types::{FileSymbols, UseKind};
use tree_sitter::{Node, Tree};

/// Information needed by the LSP server to build `SignatureHelp`.
#[derive(Debug, Clone)]
pub struct SignatureHelpContext {
    /// Resolved callable symbol for the active call expression.
    pub symbol: SymbolAtPosition,
    /// Zero-based active parameter index.
    pub active_parameter: usize,
}

/// Find signature-help context at a source position.
///
/// `character` is a byte column, matching tree-sitter `Point.column`.
pub fn signature_help_context_at_position(
    tree: &Tree,
    source: &str,
    line: u32,
    character: u32,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
) -> Option<SignatureHelpContext> {
    let root = tree.root_node();
    let offset = position_to_byte(source, line, character);
    let mut node = node_before_cursor(root, offset);

    loop {
        if is_call_node(node.kind()) {
            if let Some(arguments) = arguments_node(node) {
                if arguments_own_cursor(arguments, source, offset) {
                    let target = call_target_node(node)?;
                    let target_pos = target.start_position();
                    let symbol = symbol_at_position_with_resolver(
                        tree,
                        source,
                        target_pos.row as u32,
                        target_pos.column as u32,
                        file_symbols,
                        resolver,
                    )?;
                    return Some(SignatureHelpContext {
                        symbol,
                        active_parameter: active_parameter_index(arguments, offset),
                    });
                }
            }
        }

        if node.is_error() {
            if let Some(context) = recover_error_call(node, source, offset, file_symbols, resolver)
            {
                return Some(context);
            }
        }
        if node.kind() == "comment" {
            let mut previous = node.prev_sibling();
            while previous
                .is_some_and(|node| (node.is_extra() && !node.is_error()) || node.is_missing())
            {
                previous = previous.and_then(|node| node.prev_sibling());
            }
            if let Some(error) = previous.filter(|node| node.is_error()) {
                if let Some(context) =
                    recover_error_call(error, source, offset, file_symbols, resolver)
                {
                    return Some(context);
                }
            }
        }

        node = node.parent()?;
    }
}

fn is_call_node(kind: &str) -> bool {
    matches!(
        kind,
        "function_call_expression"
            | "member_call_expression"
            | "nullsafe_member_call_expression"
            | "scoped_call_expression"
            | "object_creation_expression"
    )
}

fn arguments_node(call: Node) -> Option<Node> {
    call.child_by_field_name("arguments").or_else(|| {
        (0..call.child_count())
            .filter_map(|i| call.child(i))
            .find(|child| child.kind() == "arguments")
    })
}

fn call_target_node(call: Node) -> Option<Node> {
    if let Some(node) = call.child_by_field_name("function") {
        return Some(node);
    }
    if let Some(node) = call.child_by_field_name("name") {
        return Some(node);
    }

    match call.kind() {
        "object_creation_expression" => (0..call.named_child_count())
            .filter_map(|i| call.named_child(i))
            .find(|child| matches!(child.kind(), "name" | "qualified_name" | "namespace_name")),
        "function_call_expression" => call.named_child(0),
        _ => None,
    }
}

fn arguments_own_cursor(arguments: Node, source: &str, offset: usize) -> bool {
    let mut cursor = arguments.walk();
    let mut opened = false;
    for child in arguments
        .children(&mut cursor)
        .filter(|child| !child.is_missing())
    {
        if child.kind() == "(" {
            opened = child.end_byte() <= offset;
        }
        if child.kind() == ")" {
            return opened && offset <= child.start_byte();
        }
    }
    opened
        && (offset <= arguments.end_byte() || whitespace_gap(source, arguments.end_byte(), offset))
}

fn active_parameter_index(arguments: Node, offset: usize) -> usize {
    let mut cursor = arguments.walk();
    arguments
        .children(&mut cursor)
        .filter(|child| child.kind() == "," && !child.is_missing() && child.end_byte() <= offset)
        .count()
}

/// Select the syntax immediately before the caret, including when the caret
/// sits in trailing whitespace or just after a closing delimiter.
fn node_before_cursor(mut node: Node, offset: usize) -> Node {
    loop {
        let mut left = 0;
        let mut right = node.child_count();
        while left < right {
            let mid = left + (right - left) / 2;
            if node
                .child(mid)
                .is_some_and(|child| child.start_byte() < offset)
            {
                left = mid + 1;
            } else {
                right = mid;
            }
        }
        let Some(child) = left.checked_sub(1).and_then(|index| node.child(index)) else {
            return node;
        };
        node = child;
    }
}

fn whitespace_gap(source: &str, start: usize, end: usize) -> bool {
    source
        .get(start..end)
        .is_some_and(|gap| gap.chars().all(char::is_whitespace))
}

#[derive(Clone, Copy)]
enum RecoveredTarget<'tree> {
    Function(Node<'tree>),
    Constructor(Node<'tree>),
    Member {
        object: Node<'tree>,
        name: Node<'tree>,
    },
    Scoped {
        scope: Node<'tree>,
        name: Node<'tree>,
    },
    // A dynamic inner call must not fall back to an outer callable's signature.
    Unknown,
}

struct ArgumentFrame<'tree> {
    delimiter: char,
    target: Option<RecoveredTarget<'tree>>,
    active: usize,
}

/// Incomplete calls can be flattened into ERROR children. Only CST delimiters
/// are inspected; strings/comments/parsed argument expressions stay opaque.
fn recover_error_call(
    error: Node,
    source: &str,
    offset: usize,
    file: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
) -> Option<SignatureHelpContext> {
    if !error_owns_cursor(error, source, offset) {
        return None;
    }
    let mut frames: Vec<ArgumentFrame> = Vec::new();
    let mut history = [None; 3];
    let mut cursor = error.walk();
    for token in error.children(&mut cursor) {
        if token.start_byte() >= offset {
            break;
        }
        if token.is_error() {
            return None;
        }
        if token.is_extra() || token.is_missing() {
            continue;
        }
        match token.kind() {
            "(" | "[" | "{" => frames.push(ArgumentFrame {
                delimiter: token.kind().chars().next()?,
                target: (token.kind() == "(")
                    .then(|| recovered_target(history))
                    .flatten(),
                active: 0,
            }),
            ")" | "]" | "}" => {
                let expected = match token.kind() {
                    ")" => '(',
                    "]" => '[',
                    _ => '{',
                };
                if frames.pop().is_none_or(|frame| frame.delimiter != expected) {
                    frames.clear();
                }
            }
            "," if token.end_byte() <= offset => {
                if let Some(frame) = frames.last_mut().filter(|frame| frame.delimiter == '(') {
                    frame.active += 1;
                }
            }
            _ => {}
        }
        history = [Some(token), history[0], history[1]];
    }
    let frame = frames.iter().rev().find(|frame| frame.target.is_some())?;
    Some(SignatureHelpContext {
        symbol: recovered_symbol(frame.target?, source, file, resolver)?,
        active_parameter: frame.active,
    })
}

fn error_owns_cursor(error: Node, source: &str, offset: usize) -> bool {
    if offset <= error.end_byte() {
        return offset >= error.start_byte();
    }
    let mut end = error.end_byte();
    let mut sibling = error.next_sibling();
    while let Some(node) = sibling.filter(|node| node.start_byte() <= offset) {
        if node.is_missing() {
            sibling = node.next_sibling();
            continue;
        }
        if node.kind() != "comment" || !whitespace_gap(source, end, node.start_byte()) {
            return false;
        }
        if offset <= node.end_byte() {
            return true;
        }
        end = node.end_byte();
        sibling = node.next_sibling();
    }
    whitespace_gap(source, end, offset)
}

fn recovered_target(history: [Option<Node>; 3]) -> Option<RecoveredTarget> {
    let target = history[0]?;
    if matches!(target.kind(), "name" | "qualified_name" | "namespace_name") {
        return match history[1].map(|node| node.kind()) {
            Some("->" | "?->") => Some(RecoveredTarget::Member {
                object: history[2]?,
                name: target,
            }),
            Some("::") => Some(RecoveredTarget::Scoped {
                scope: history[2]?,
                name: target,
            }),
            Some("new") => Some(RecoveredTarget::Constructor(target)),
            Some("function" | "fn" | "class" | "interface" | "trait") => None,
            _ => Some(RecoveredTarget::Function(target)),
        };
    }
    if matches!(
        target.kind(),
        "member_access_expression" | "nullsafe_member_access_expression"
    ) {
        return Some(RecoveredTarget::Member {
            object: target.child_by_field_name("object")?,
            name: target.child_by_field_name("name")?,
        });
    }
    if target.kind() == "class_constant_access_expression" {
        return Some(RecoveredTarget::Scoped {
            scope: target.child_by_field_name("scope")?,
            name: target.child_by_field_name("name")?,
        });
    }
    matches!(
        target.kind(),
        "variable_name"
            | "parenthesized_expression"
            | "function_call_expression"
            | "member_call_expression"
            | "nullsafe_member_call_expression"
    )
    .then_some(RecoveredTarget::Unknown)
}

fn recovered_symbol(
    target: RecoveredTarget,
    source: &str,
    file: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
) -> Option<SymbolAtPosition> {
    let name_node = match target {
        RecoveredTarget::Function(name)
        | RecoveredTarget::Constructor(name)
        | RecoveredTarget::Member { name, .. }
        | RecoveredTarget::Scoped { name, .. } => name,
        RecoveredTarget::Unknown => return None,
    };
    let start = name_node.start_position();
    let end = name_node.end_position();
    let file = file.scoped_at_byte_position(start.row as u32, start.column as u32);
    let name = &source[name_node.byte_range()];
    let (fqn, ref_kind, object_expr, allows_global_fallback) = match target {
        RecoveredTarget::Function(_) => (
            resolve_function_name_pub(name, &file),
            RefKind::FunctionCall,
            None,
            unqualified_name_allows_global_fallback(name, UseKind::Function, &file),
        ),
        RecoveredTarget::Constructor(_) => (
            format!("{}::__construct", resolve_class_name_pub(name, &file)),
            RefKind::Constructor,
            None,
            false,
        ),
        RecoveredTarget::Member { object, .. } => {
            let class = try_resolve_object_type(object, source, &file, resolver, None, None);
            (
                class
                    .map(|class| format!("{class}::{name}"))
                    .unwrap_or_else(|| name.into()),
                RefKind::MethodCall,
                Some(source[object.byte_range()].into()),
                false,
            )
        }
        RecoveredTarget::Scoped { scope, .. } => {
            let scope_text = &source[scope.byte_range()];
            (
                format!(
                    "{}::{name}",
                    resolve_scope_class_name_pub(scope_text, name_node, source, &file)
                ),
                RefKind::MethodCall,
                Some(scope_text.into()),
                false,
            )
        }
        RecoveredTarget::Unknown => return None,
    };
    Some(SymbolAtPosition {
        fqn,
        name: name.into(),
        ref_kind,
        allows_global_fallback,
        object_expr,
        range: (
            start.row as u32,
            start.column as u32,
            end.row as u32,
            end.column as u32,
        ),
    })
}

fn position_to_byte(source: &str, line: u32, byte_col: u32) -> usize {
    let mut offset = 0usize;

    for (current_line, row) in source.split_inclusive('\n').enumerate() {
        if current_line as u32 == line {
            return offset + (byte_col as usize).min(row.len());
        }
        offset += row.len();
    }

    source.len()
}

#[cfg(test)]
#[path = "signature_help_tests.rs"]
mod tests;
