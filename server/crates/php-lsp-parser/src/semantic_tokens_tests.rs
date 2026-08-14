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
