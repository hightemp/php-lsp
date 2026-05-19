//! Signature help call-site detection.
//!
//! Finds the callable expression that owns the argument list at a cursor
//! position and resolves it using the existing symbol resolver.

use crate::resolve::{symbol_at_position_with_resolver, MemberTypeResolver, SymbolAtPosition};
use php_lsp_types::FileSymbols;
use tree_sitter::{Node, Point, Tree};

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
    let point = Point::new(line as usize, character as usize);
    let offset = position_to_byte(source, line, character);
    let mut node = root.descendant_for_point_range(point, point)?;

    loop {
        if is_call_node(node.kind()) {
            if let Some(arguments) = arguments_node(node) {
                if offset >= arguments.start_byte() && offset <= arguments.end_byte() {
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
                        active_parameter: active_parameter_index(arguments, source, offset),
                    });
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

fn active_parameter_index(arguments: Node, source: &str, offset: usize) -> usize {
    let end = offset.min(arguments.end_byte()).min(source.len());
    let start = arguments.start_byte().min(end);
    let text = &source[start..end];

    let mut active = 0usize;
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for ch in text.chars() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == q {
                quote = None;
            }
            continue;
        }

        match ch {
            '\'' | '"' => quote = Some(ch),
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            ',' if depth == 1 => active += 1,
            _ => {}
        }
    }

    active
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
mod tests {
    use super::*;
    use crate::parser::FileParser;
    use crate::symbols::extract_file_symbols;

    fn context_for(source: &str, line: u32, character: u32) -> SignatureHelpContext {
        let mut parser = FileParser::new();
        parser.parse_full(source);
        let tree = parser.tree().expect("tree");
        let file_symbols = extract_file_symbols(tree, source, "file:///test.php");
        signature_help_context_at_position(tree, source, line, character, &file_symbols, None)
            .expect("signature help context")
    }

    #[test]
    fn detects_function_call_active_parameter() {
        let source = "<?php\nfunction foo($a, $b) {}\nfoo(1, 2);\n";
        let ctx = context_for(source, 2, 7);
        assert_eq!(ctx.symbol.fqn, "foo");
        assert_eq!(ctx.active_parameter, 1);
    }

    #[test]
    fn detects_constructor_call() {
        let source = "<?php\nclass Foo { public function __construct($a) {} }\nnew Foo(1);\n";
        let ctx = context_for(source, 2, 9);
        assert_eq!(ctx.symbol.fqn, "Foo::__construct");
        assert_eq!(ctx.active_parameter, 0);
    }

    #[test]
    fn keeps_nested_call_context() {
        let source =
            "<?php\nfunction outer($a) {}\nfunction inner($a, $b) {}\nouter(inner(1, 2));\n";
        let ctx = context_for(source, 3, 15);
        assert_eq!(ctx.symbol.fqn, "inner");
        assert_eq!(ctx.active_parameter, 1);
    }
}
