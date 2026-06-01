//! Semantic token extraction from the PHP CST.
//!
//! Produces LSP-compatible relative token data while keeping the parser crate
//! independent from the concrete server transport crate.

use crate::utf16::Utf16LineIndex;
use tree_sitter::{Node, Tree};

pub const SEMANTIC_TOKEN_TYPES: &[&str] = &[
    "namespace",
    "type",
    "class",
    "enum",
    "interface",
    "parameter",
    "variable",
    "property",
    "enumMember",
    "function",
    "method",
    "keyword",
    "modifier",
    "comment",
    "string",
    "number",
    "operator",
];

pub const SEMANTIC_TOKEN_MODIFIERS: &[&str] = &[
    "declaration",
    "definition",
    "readonly",
    "static",
    "deprecated",
    "abstract",
    "documentation",
    "defaultLibrary",
];

const TOKEN_NAMESPACE: u32 = 0;
const TOKEN_TYPE: u32 = 1;
const TOKEN_CLASS: u32 = 2;
const TOKEN_ENUM: u32 = 3;
const TOKEN_INTERFACE: u32 = 4;
const TOKEN_PARAMETER: u32 = 5;
const TOKEN_VARIABLE: u32 = 6;
const TOKEN_PROPERTY: u32 = 7;
const TOKEN_ENUM_MEMBER: u32 = 8;
const TOKEN_FUNCTION: u32 = 9;
const TOKEN_METHOD: u32 = 10;
const TOKEN_KEYWORD: u32 = 11;
const TOKEN_MODIFIER: u32 = 12;
const TOKEN_COMMENT: u32 = 13;
const TOKEN_STRING: u32 = 14;
const TOKEN_NUMBER: u32 = 15;
const TOKEN_OPERATOR: u32 = 16;

const MOD_DECLARATION: u32 = 1 << 0;
const MOD_DEFINITION: u32 = 1 << 1;
const MOD_READONLY: u32 = 1 << 2;
const MOD_STATIC: u32 = 1 << 3;
const MOD_DEPRECATED: u32 = 1 << 4;
const MOD_ABSTRACT: u32 = 1 << 5;
const MOD_DOCUMENTATION: u32 = 1 << 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemanticTokenData {
    pub delta_line: u32,
    pub delta_start: u32,
    pub length: u32,
    pub token_type: u32,
    pub token_modifiers_bitset: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AbsoluteSemanticToken {
    line: u32,
    start: u32,
    length: u32,
    token_type: u32,
    token_modifiers_bitset: u32,
}

pub fn extract_semantic_tokens(tree: &Tree, source: &str) -> Vec<SemanticTokenData> {
    let utf16_index = Utf16LineIndex::new(source);
    let mut absolute_tokens = Vec::new();
    collect_node_tokens(tree.root_node(), source, &utf16_index, &mut absolute_tokens);

    let absolute_tokens = normalize_tokens(absolute_tokens);
    encode_relative_tokens(&absolute_tokens)
}

fn collect_node_tokens(
    node: Node,
    source: &str,
    utf16_index: &Utf16LineIndex,
    tokens: &mut Vec<AbsoluteSemanticToken>,
) {
    if let Some((token_type, token_modifiers_bitset)) = classify_node(node, source) {
        push_node_tokens(
            node,
            source,
            utf16_index,
            token_type,
            token_modifiers_bitset,
            tokens,
        );
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_node_tokens(child, source, utf16_index, tokens);
    }
}

fn push_node_tokens(
    node: Node,
    source: &str,
    utf16_index: &Utf16LineIndex,
    token_type: u32,
    token_modifiers_bitset: u32,
    tokens: &mut Vec<AbsoluteSemanticToken>,
) {
    let start = node.start_position();
    let end = node.end_position();
    let start_line = start.row as u32;
    let end_line = end.row as u32;

    if start_line == end_line {
        push_line_token(
            start_line,
            start.column as u32,
            end.column as u32,
            utf16_index,
            token_type,
            token_modifiers_bitset,
            tokens,
        );
        return;
    }

    for line in start_line..=end_line {
        let start_col = if line == start_line {
            start.column as u32
        } else {
            0
        };
        let end_col = if line == end_line {
            end.column as u32
        } else {
            line_byte_len(source, line)
        };
        push_line_token(
            line,
            start_col,
            end_col,
            utf16_index,
            token_type,
            token_modifiers_bitset,
            tokens,
        );
    }
}

fn push_line_token(
    line: u32,
    start_byte_col: u32,
    end_byte_col: u32,
    utf16_index: &Utf16LineIndex,
    token_type: u32,
    token_modifiers_bitset: u32,
    tokens: &mut Vec<AbsoluteSemanticToken>,
) {
    if end_byte_col <= start_byte_col {
        return;
    }

    let start = utf16_index.byte_col_to_utf16(line, start_byte_col);
    let end = utf16_index.byte_col_to_utf16(line, end_byte_col);
    let length = end.saturating_sub(start);
    if length == 0 {
        return;
    }

    tokens.push(AbsoluteSemanticToken {
        line,
        start,
        length,
        token_type,
        token_modifiers_bitset,
    });
}

fn line_byte_len(source: &str, line: u32) -> u32 {
    source
        .split('\n')
        .nth(line as usize)
        .map(str::len)
        .unwrap_or_default() as u32
}

fn normalize_tokens(mut tokens: Vec<AbsoluteSemanticToken>) -> Vec<AbsoluteSemanticToken> {
    tokens.sort_by_key(|token| {
        (
            token.line,
            token.start,
            token.length,
            token.token_type,
            token.token_modifiers_bitset,
        )
    });
    tokens.dedup();

    let mut normalized = Vec::with_capacity(tokens.len());
    let mut last_line: Option<u32> = None;
    let mut last_end = 0u32;

    for token in tokens {
        if last_line == Some(token.line) && token.start < last_end {
            continue;
        }
        last_line = Some(token.line);
        last_end = token.start.saturating_add(token.length);
        normalized.push(token);
    }

    normalized
}

fn encode_relative_tokens(tokens: &[AbsoluteSemanticToken]) -> Vec<SemanticTokenData> {
    let mut result = Vec::with_capacity(tokens.len());
    let mut previous_line = 0u32;
    let mut previous_start = 0u32;

    for token in tokens {
        let delta_line = token.line - previous_line;
        let delta_start = if delta_line == 0 {
            token.start - previous_start
        } else {
            token.start
        };

        result.push(SemanticTokenData {
            delta_line,
            delta_start,
            length: token.length,
            token_type: token.token_type,
            token_modifiers_bitset: token.token_modifiers_bitset,
        });

        previous_line = token.line;
        previous_start = token.start;
    }

    result
}

fn classify_node(node: Node, source: &str) -> Option<(u32, u32)> {
    let kind = node.kind();

    if kind == "comment" {
        let modifiers = if is_documentation_comment(node, source) {
            MOD_DOCUMENTATION
        } else {
            0
        };
        return Some((TOKEN_COMMENT, modifiers));
    }

    if is_string_kind(kind) {
        return Some((TOKEN_STRING, 0));
    }
    if is_number_kind(kind) {
        return Some((TOKEN_NUMBER, 0));
    }
    if kind == "primitive_type" {
        return Some((TOKEN_TYPE, 0));
    }
    if kind == "variable_name" {
        return Some(classify_variable_name(node, source));
    }
    if matches!(kind, "qualified_name" | "namespace_name") {
        return Some(classify_qualified_name(node, source));
    }
    if kind == "name" {
        return classify_name(node, source);
    }
    if is_modifier_keyword(kind) {
        return Some((TOKEN_MODIFIER, modifier_keyword_bits(kind)));
    }
    if is_keyword(kind) {
        return Some((TOKEN_KEYWORD, 0));
    }
    if is_operator(kind) {
        return Some((TOKEN_OPERATOR, 0));
    }

    None
}

fn is_documentation_comment(node: Node, source: &str) -> bool {
    source
        .get(node.byte_range())
        .map(|text| text.trim_start().starts_with("/**"))
        .unwrap_or(false)
}

fn is_string_kind(kind: &str) -> bool {
    matches!(
        kind,
        "string" | "encapsed_string" | "string_value" | "heredoc" | "nowdoc"
    )
}

fn is_number_kind(kind: &str) -> bool {
    matches!(
        kind,
        "integer" | "float" | "integer_literal" | "float_literal" | "number"
    )
}

fn is_modifier_keyword(kind: &str) -> bool {
    matches!(
        kind,
        "public" | "protected" | "private" | "static" | "abstract" | "final" | "readonly"
    )
}

fn modifier_keyword_bits(kind: &str) -> u32 {
    match kind {
        "static" => MOD_STATIC,
        "abstract" => MOD_ABSTRACT,
        "readonly" => MOD_READONLY,
        _ => 0,
    }
}

fn is_keyword(kind: &str) -> bool {
    matches!(
        kind,
        "namespace"
            | "use"
            | "as"
            | "class"
            | "interface"
            | "trait"
            | "enum"
            | "extends"
            | "implements"
            | "function"
            | "fn"
            | "new"
            | "return"
            | "if"
            | "elseif"
            | "else"
            | "foreach"
            | "for"
            | "while"
            | "do"
            | "switch"
            | "case"
            | "default"
            | "break"
            | "continue"
            | "try"
            | "catch"
            | "finally"
            | "throw"
            | "yield"
            | "from"
            | "match"
            | "self"
            | "parent"
            | "echo"
            | "print"
            | "const"
            | "var"
            | "global"
            | "isset"
            | "empty"
            | "unset"
            | "declare"
            | "include"
            | "include_once"
            | "require"
            | "require_once"
            | "clone"
            | "instanceof"
            | "insteadof"
            | "and"
            | "or"
            | "xor"
    )
}

fn is_operator(kind: &str) -> bool {
    matches!(
        kind,
        "->" | "::"
            | "=>"
            | "="
            | "+"
            | "-"
            | "*"
            | "/"
            | "%"
            | "."
            | "=="
            | "==="
            | "!="
            | "!=="
            | "<"
            | "<="
            | ">"
            | ">="
            | "&&"
            | "||"
            | "!"
            | "?"
            | "??"
            | "??="
            | "+="
            | "-="
            | "*="
            | "/="
            | "%="
            | ".="
            | "|"
            | "&"
            | "^"
            | "~"
    )
}

fn classify_variable_name(node: Node, source: &str) -> (u32, u32) {
    let Some(parent) = node.parent() else {
        return (TOKEN_VARIABLE, 0);
    };

    match parent.kind() {
        "simple_parameter" => (TOKEN_PARAMETER, MOD_DECLARATION),
        "property_promotion_parameter" => (
            TOKEN_PROPERTY,
            MOD_DECLARATION | symbol_modifier_bits(parent, source),
        ),
        "property_element" => {
            let owner = parent.parent().unwrap_or(parent);
            (
                TOKEN_PROPERTY,
                MOD_DECLARATION | symbol_modifier_bits(owner, source),
            )
        }
        "scoped_property_access_expression" => (TOKEN_PROPERTY, 0),
        _ => (TOKEN_VARIABLE, 0),
    }
}

fn classify_qualified_name(node: Node, source: &str) -> (u32, u32) {
    let context = semantic_context(node);
    let Some(context) = context else {
        return (TOKEN_TYPE, 0);
    };

    match context.kind() {
        "namespace_definition" => (TOKEN_NAMESPACE, MOD_DECLARATION),
        "namespace_use_declaration" | "namespace_use_clause" | "namespace_use_group" => {
            (TOKEN_TYPE, 0)
        }
        "function_call_expression" => (TOKEN_FUNCTION, 0),
        "object_creation_expression" => (TOKEN_CLASS, 0),
        "scoped_call_expression" | "scoped_property_access_expression" => {
            if is_scope_operand(node, context, source) {
                (TOKEN_TYPE, 0)
            } else {
                (TOKEN_PROPERTY, 0)
            }
        }
        "named_type" | "optional_type" | "base_clause" | "class_interface_clause" => {
            (TOKEN_TYPE, 0)
        }
        _ => (TOKEN_TYPE, 0),
    }
}

fn classify_name(node: Node, source: &str) -> Option<(u32, u32)> {
    let parent = node.parent()?;
    let parent_kind = parent.kind();

    Some(match parent_kind {
        "class_declaration" => (
            TOKEN_CLASS,
            MOD_DECLARATION | MOD_DEFINITION | symbol_modifier_bits(parent, source),
        ),
        "interface_declaration" => (TOKEN_INTERFACE, MOD_DECLARATION | MOD_DEFINITION),
        "trait_declaration" => (TOKEN_TYPE, MOD_DECLARATION | MOD_DEFINITION),
        "enum_declaration" => (
            TOKEN_ENUM,
            MOD_DECLARATION | MOD_DEFINITION | symbol_modifier_bits(parent, source),
        ),
        "function_definition" => (TOKEN_FUNCTION, MOD_DECLARATION | MOD_DEFINITION),
        "method_declaration" => (
            TOKEN_METHOD,
            MOD_DECLARATION | MOD_DEFINITION | symbol_modifier_bits(parent, source),
        ),
        "const_element" => classify_const_name(parent),
        "enum_case" => (TOKEN_ENUM_MEMBER, MOD_DECLARATION | MOD_DEFINITION),
        "namespace_definition" => (TOKEN_NAMESPACE, MOD_DECLARATION),
        "namespace_use_clause" | "namespace_use_group" => (TOKEN_TYPE, 0),
        "object_creation_expression" => (TOKEN_CLASS, 0),
        "function_call_expression" => (TOKEN_FUNCTION, 0),
        "member_call_expression" => (TOKEN_METHOD, 0),
        "member_access_expression" => (TOKEN_PROPERTY, 0),
        "scoped_call_expression" => {
            if is_scope_operand(node, parent, source) {
                (TOKEN_TYPE, 0)
            } else {
                (TOKEN_METHOD, 0)
            }
        }
        "scoped_property_access_expression" | "class_constant_access_expression" => {
            if is_scope_operand(node, parent, source) {
                (TOKEN_TYPE, 0)
            } else {
                (TOKEN_PROPERTY, 0)
            }
        }
        "named_type" | "optional_type" | "base_clause" | "class_interface_clause" => {
            (TOKEN_TYPE, 0)
        }
        _ => return None,
    })
}

fn classify_const_name(const_element: Node) -> (u32, u32) {
    let Some(owner) = const_element.parent() else {
        return (TOKEN_VARIABLE, MOD_DECLARATION | MOD_DEFINITION);
    };

    match owner.kind() {
        "class_const_declaration" => (TOKEN_PROPERTY, MOD_DECLARATION | MOD_DEFINITION),
        "const_declaration" => {
            if owner
                .parent()
                .map(|parent| matches!(parent.kind(), "declaration_list" | "class_body"))
                .unwrap_or(false)
            {
                (TOKEN_PROPERTY, MOD_DECLARATION | MOD_DEFINITION)
            } else {
                (TOKEN_VARIABLE, MOD_DECLARATION | MOD_DEFINITION)
            }
        }
        _ => (TOKEN_VARIABLE, MOD_DECLARATION | MOD_DEFINITION),
    }
}

fn semantic_context(node: Node) -> Option<Node> {
    let mut current = node.parent();
    while let Some(parent) = current {
        if !matches!(
            parent.kind(),
            "qualified_name" | "namespace_name" | "namespace_name_as_prefix"
        ) {
            return Some(parent);
        }
        current = parent.parent();
    }
    None
}

fn is_scope_operand(node: Node, parent: Node, source: &str) -> bool {
    source
        .get(node.end_byte()..parent.end_byte())
        .map(|after| after.trim_start().starts_with("::"))
        .unwrap_or(false)
}

fn symbol_modifier_bits(node: Node, source: &str) -> u32 {
    let mut bits = 0u32;

    if node_text_contains_keyword(node, source, "readonly") {
        bits |= MOD_READONLY;
    }
    if node_text_contains_keyword(node, source, "static") {
        bits |= MOD_STATIC;
    }
    if node_text_contains_keyword(node, source, "abstract") {
        bits |= MOD_ABSTRACT;
    }
    if preceding_doc_comment_text(node, source)
        .map(|comment| comment.contains("@deprecated"))
        .unwrap_or(false)
    {
        bits |= MOD_DEPRECATED;
    }

    bits
}

fn node_text_contains_keyword(node: Node, source: &str, keyword: &str) -> bool {
    source
        .get(node.byte_range())
        .map(|text| {
            text.split(|ch: char| !is_identifier_char(ch))
                .any(|part| part == keyword)
        })
        .unwrap_or(false)
}

fn is_identifier_char(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

fn preceding_doc_comment_text<'a>(node: Node, source: &'a str) -> Option<&'a str> {
    let mut prev = node.prev_sibling();
    while let Some(candidate) = prev {
        if candidate.kind() == "comment" {
            let text = source.get(candidate.byte_range())?;
            return text.trim_start().starts_with("/**").then_some(text);
        }
        let text = source.get(candidate.byte_range()).unwrap_or("");
        if !text.trim().is_empty() {
            return None;
        }
        prev = candidate.prev_sibling();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::FileParser;

    fn parse_absolute_tokens(source: &str) -> Vec<AbsoluteSemanticToken> {
        let mut parser = FileParser::new();
        parser.parse_full(source);
        let tree = parser.tree().expect("tree");
        let relative = extract_semantic_tokens(tree, source);

        let mut line = 0u32;
        let mut start = 0u32;
        relative
            .into_iter()
            .map(|token| {
                line += token.delta_line;
                if token.delta_line == 0 {
                    start += token.delta_start;
                } else {
                    start = token.delta_start;
                }
                AbsoluteSemanticToken {
                    line,
                    start,
                    length: token.length,
                    token_type: token.token_type,
                    token_modifiers_bitset: token.token_modifiers_bitset,
                }
            })
            .collect()
    }

    fn has_token(
        tokens: &[AbsoluteSemanticToken],
        line: u32,
        start: u32,
        length: u32,
        token_type: u32,
    ) -> bool {
        tokens.iter().any(|token| {
            token.line == line
                && token.start == start
                && token.length == length
                && token.token_type == token_type
        })
    }

    #[test]
    fn extracts_declarations_references_and_literals() {
        let source = "<?php\nnamespace App\\Demo;\n\n/** @deprecated */\nclass UserService {\n    private readonly string $name = \"Ada\";\n\n    public function greet(int $count): string {\n        $message = \"Hi\";\n        return $message;\n    }\n}\n";
        let tokens = parse_absolute_tokens(source);

        assert!(has_token(&tokens, 1, 10, 8, TOKEN_NAMESPACE));
        assert!(has_token(&tokens, 4, 6, 11, TOKEN_CLASS));
        assert!(has_token(&tokens, 5, 28, 5, TOKEN_PROPERTY));
        assert!(has_token(&tokens, 7, 20, 5, TOKEN_METHOD));
        assert!(has_token(&tokens, 7, 30, 6, TOKEN_PARAMETER));
        assert!(has_token(&tokens, 8, 8, 8, TOKEN_VARIABLE));
        assert!(has_token(&tokens, 8, 19, 4, TOKEN_STRING));

        let class_token = tokens
            .iter()
            .find(|token| token.line == 4 && token.start == 6 && token.token_type == TOKEN_CLASS)
            .expect("class token");
        assert_ne!(class_token.token_modifiers_bitset & MOD_DECLARATION, 0);
        assert_ne!(class_token.token_modifiers_bitset & MOD_DEPRECATED, 0);
    }

    #[test]
    fn uses_utf16_lengths_for_non_ascii_tokens() {
        let source = "<?php\n$message = \"Привет\";\n";
        let tokens = parse_absolute_tokens(source);

        assert!(has_token(&tokens, 1, 11, 8, TOKEN_STRING));
    }

    #[test]
    fn uses_utf16_positions_after_emoji_in_php_code() {
        let emoji = "🇺🇸 👨\u{200d}👩\u{200d}👧\u{200d}👦 👍🏽 ❤️ e\u{0301}";
        let source = format!("<?php\n$emoji = \"{emoji}\"; $after = 1;\n");
        let tokens = parse_absolute_tokens(&source);
        let string_start = "$emoji = ".encode_utf16().count() as u32;
        let string_len = emoji.encode_utf16().count() as u32 + 2;
        let after_start = "$emoji = \"".encode_utf16().count() as u32
            + emoji.encode_utf16().count() as u32
            + "\"; ".encode_utf16().count() as u32;

        assert!(
            has_token(&tokens, 1, string_start, string_len, TOKEN_STRING),
            "complex emoji string token should use UTF-16 length, got {tokens:?}"
        );
        assert!(
            has_token(&tokens, 1, after_start, 6, TOKEN_VARIABLE),
            "variable after complex emoji should start at UTF-16 column {after_start}, got {tokens:?}"
        );
    }
}
