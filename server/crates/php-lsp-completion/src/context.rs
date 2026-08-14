//! Completion context detection.
//!
//! Determines what kind of completion is appropriate based on
//! the cursor position in the CST and surrounding text.

use php_lsp_types::FileSymbols;
use tree_sitter::{Node, Point, Tree};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberAccessMode {
    Read,
    Write,
}

/// The context in which completion was triggered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionContext {
    /// After `->`: instance member access (methods, properties).
    MemberAccess {
        /// The object expression text (e.g. "$this", "$foo").
        object_expr: String,
        /// The member prefix already typed after `->`.
        member_prefix: String,
        /// Optional inferred FQN of object class (filled later by server).
        class_fqn: Option<String>,
        /// Whether the member access is used for reading or writing.
        access_mode: MemberAccessMode,
    },

    /// After `::`: static member access (static methods, properties, constants).
    StaticAccess {
        /// The class name or expression (e.g. "self", "Foo").
        class_expr: String,
        /// The member prefix already typed after `::`.
        member_prefix: String,
        /// Resolved FQN of the class.
        class_fqn: String,
    },

    /// Inside an array key string: `$row['...']`.
    ArrayKey {
        /// The array expression before the current `[...]`.
        array_expr: String,
        /// The key prefix already typed inside the quote.
        key_prefix: String,
        /// The current quote if completion is inside quotes, or None after `[`.
        quote: Option<char>,
    },

    /// After `$`: variable name completion.
    Variable {
        /// Partial variable name typed so far (without $).
        prefix: String,
    },

    /// After `\` or in namespace context: namespace/class completion.
    Namespace {
        /// The partial namespace path.
        prefix: String,
    },

    /// Free context: class names, function names, keywords.
    Free {
        /// The partial word typed.
        prefix: String,
    },

    /// Inside a use statement.
    UseStatement {
        /// Partial FQN typed.
        prefix: String,
    },

    /// No completion available.
    None,
}

/// Determine the completion context at a tree-sitter byte-column position.
///
/// LSP callers must convert `Position.character` from UTF-16 to a byte column
/// before calling this function. The byte column is clamped to a valid UTF-8
/// boundary on the requested source line before any string slicing.
pub fn detect_context_at_byte_col(
    tree: &Tree,
    source: &str,
    line: u32,
    byte_col: u32,
    file_symbols: &FileSymbols,
) -> CompletionContext {
    let (line_start, line_end) = match line_byte_bounds_without_newline(source, line) {
        Some(bounds) => bounds,
        None => return CompletionContext::None,
    };
    let cursor_offset =
        clamp_line_byte_col_to_char_boundary(source, line_start, line_end, byte_col);
    let cursor_col = cursor_offset - line_start;

    let point = Point::new(line as usize, cursor_col);
    let root = tree.root_node();

    // Find the node at position
    let node = match root.descendant_for_point_range(point, point) {
        Some(n) => n,
        None => return CompletionContext::None,
    };

    // Get the text before cursor on the current line
    let line_text = &source[line_start..line_end];
    let text_before = &line_text[..cursor_col];
    let text_after = &line_text[cursor_col..];

    // Check for `->` member access
    if let Some(ctx) = check_member_access(text_before, text_after, &node, source) {
        return ctx;
    }

    // Check for `::` static access
    if let Some(ctx) = check_static_access(text_before, &node, source, file_symbols) {
        return ctx;
    }

    // Check for array-shape key access inside `$row['...']`.
    if let Some(ctx) = check_array_key_access(text_before) {
        return ctx;
    }

    // Check for `$` variable access
    if let Some(ctx) = check_variable_access(text_before) {
        return ctx;
    }

    // Check for `use` statement context
    if let Some(ctx) = check_use_context(&node, text_before, source) {
        return ctx;
    }

    // Check for `\` namespace access
    if let Some(ctx) = check_namespace_access(text_before) {
        return ctx;
    }

    // Default: free context with the current word as prefix
    let prefix = extract_word_before_cursor(text_before);

    // Don't complete on empty prefix unless triggered by a character
    if prefix.is_empty() {
        // Check if we're in a type hint position
        if is_type_hint_position(&node, source, cursor_offset) {
            return CompletionContext::Free {
                prefix: String::new(),
            };
        }
        return CompletionContext::None;
    }

    CompletionContext::Free { prefix }
}

fn line_byte_bounds_without_newline(source: &str, line: u32) -> Option<(usize, usize)> {
    let bytes = source.as_bytes();
    let mut start = 0usize;

    for _ in 0..line {
        let next_newline = bytes[start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|idx| start + idx)?;
        start = next_newline + 1;
    }

    if start > bytes.len() {
        return None;
    }

    let mut end = bytes[start..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|idx| start + idx)
        .unwrap_or(bytes.len());

    if end > start && bytes[end - 1] == b'\r' {
        end -= 1;
    }

    Some((start, end))
}

fn clamp_line_byte_col_to_char_boundary(
    source: &str,
    line_start: usize,
    line_end: usize,
    byte_col: u32,
) -> usize {
    let mut cursor = line_start.saturating_add(byte_col as usize).min(line_end);
    while cursor > line_start && !source.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

/// Check for `->` member access pattern.
fn check_member_access(
    text_before: &str,
    text_after: &str,
    node: &Node,
    source: &str,
) -> Option<CompletionContext> {
    let trimmed = text_before.trim_end();

    // Check if text ends with `->`  or `->partial`
    if let Some(arrow_pos) = trimmed.rfind("->") {
        let after_arrow = &trimmed[arrow_pos + 2..];
        // Ensure after arrow is a valid identifier prefix or empty
        if after_arrow.chars().all(|c| c.is_alphanumeric() || c == '_') {
            let before_arrow = receiver_text_before_member_arrow(&trimmed[..arrow_pos]);

            // Walk up to find the object
            let object_expr = if !before_arrow.is_empty() {
                extract_object_expr(before_arrow)
            } else {
                // Try from CST
                find_object_in_cst(node, source).unwrap_or_else(|| "$this".to_string())
            };

            return Some(CompletionContext::MemberAccess {
                object_expr,
                member_prefix: after_arrow.to_string(),
                class_fqn: None,
                access_mode: member_access_mode_after_cursor(text_after),
            });
        }
    }

    None
}

fn receiver_text_before_member_arrow(text: &str) -> &str {
    let before_arrow = text.trim_end();
    before_arrow
        .strip_suffix('?')
        .unwrap_or(before_arrow)
        .trim_end()
}

fn member_access_mode_after_cursor(text_after: &str) -> MemberAccessMode {
    let mut rest = text_after;
    loop {
        let Some(ch) = rest.chars().next() else {
            break;
        };
        if ch.is_alphanumeric() || ch == '_' {
            rest = &rest[ch.len_utf8()..];
        } else {
            break;
        }
    }

    if starts_assignment_operator(rest.trim_start()) {
        MemberAccessMode::Write
    } else {
        MemberAccessMode::Read
    }
}

fn starts_assignment_operator(text: &str) -> bool {
    if text.starts_with("===") || text.starts_with("==") || text.starts_with("=>") {
        return false;
    }
    text.starts_with('=')
        || [
            "+=", "-=", "*=", "/=", "%=", ".=", "??=", "&=", "|=", "^=", "<<=", ">>=",
        ]
        .iter()
        .any(|operator| text.starts_with(operator))
}

/// Check for `$row['...` array key completion.
fn check_array_key_access(text_before: &str) -> Option<CompletionContext> {
    let trimmed = text_before.trim_end();
    // `rfind` returns byte offsets. Slicing below is valid because the offsets
    // come from ASCII tokens (`[`, `'`, `"`), which are always UTF-8 boundaries.
    let bracket_byte = trimmed.rfind('[')?;

    if let Some((quote_byte, quote)) = trimmed
        .char_indices()
        .rev()
        .find(|(idx, ch)| *idx > bracket_byte && matches!(ch, '\'' | '"'))
    {
        let before_quote = &trimmed[..quote_byte];
        if !before_quote[bracket_byte + '['.len_utf8()..]
            .trim()
            .is_empty()
        {
            return None;
        }
        let key_prefix = &trimmed[quote_byte + quote.len_utf8()..];
        if key_prefix.contains(quote) || key_prefix.contains(']') {
            return None;
        }
        if !is_quoted_array_key_prefix(key_prefix) {
            return None;
        }

        let array_expr = extract_object_expr(trimmed[..bracket_byte].trim_end());
        if array_expr.is_empty() {
            return None;
        }

        return Some(CompletionContext::ArrayKey {
            array_expr,
            key_prefix: key_prefix.to_string(),
            quote: Some(quote),
        });
    }

    let key_prefix = trimmed[bracket_byte + '['.len_utf8()..].trim_start();
    if key_prefix.contains(']') || !is_array_key_prefix(key_prefix) {
        return None;
    }
    let array_expr = extract_object_expr(trimmed[..bracket_byte].trim_end());
    if array_expr.is_empty() {
        return None;
    }
    Some(CompletionContext::ArrayKey {
        array_expr,
        key_prefix: key_prefix.to_string(),
        quote: None,
    })
}

fn is_quoted_array_key_prefix(prefix: &str) -> bool {
    !prefix.chars().any(char::is_control)
}

fn is_array_key_prefix(prefix: &str) -> bool {
    prefix
        .chars()
        .all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '-' || ch == '\\')
}

/// Check for `::` static access pattern.
fn check_static_access(
    text_before: &str,
    node: &Node,
    source: &str,
    file_symbols: &FileSymbols,
) -> Option<CompletionContext> {
    let trimmed = text_before.trim_end();

    if let Some(colon_pos) = trimmed.rfind("::") {
        let after_colons = &trimmed[colon_pos + 2..];
        if after_colons
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
        {
            let before_colons = trimmed[..colon_pos].trim_end();
            let class_expr = extract_object_expr(before_colons);
            let class_fqn =
                resolve_scope_class_for_completion(&class_expr, *node, source, file_symbols);

            return Some(CompletionContext::StaticAccess {
                class_expr,
                member_prefix: after_colons.to_string(),
                class_fqn,
            });
        }
    }

    None
}

/// Check for `$` variable access.
fn check_variable_access(text_before: &str) -> Option<CompletionContext> {
    let trimmed = text_before.trim_end();

    // Check if we're typing a variable: $par...
    if let Some(dollar_pos) = trimmed.rfind('$') {
        let after_dollar = &trimmed[dollar_pos + 1..];
        if after_dollar
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_')
        {
            // Make sure $ is not part of a string or something else
            let before_dollar = &trimmed[..dollar_pos];
            let before_char = before_dollar.chars().last();

            // Valid if preceded by whitespace, operator, paren, etc.
            if before_char.is_none()
                || before_char
                    .map(|c| !c.is_alphanumeric() && c != '_')
                    .unwrap_or(true)
            {
                return Some(CompletionContext::Variable {
                    prefix: after_dollar.to_string(),
                });
            }
        }
    }

    None
}

/// Check for `\` namespace access.
fn check_namespace_access(text_before: &str) -> Option<CompletionContext> {
    let trimmed = text_before.trim_end();

    // Check if typing a qualified name like `App\` or `\DateTime`
    if let Some(backslash_pos) = trimmed.rfind('\\') {
        let after_bs = &trimmed[backslash_pos + 1..];
        if after_bs.chars().all(|c| c.is_alphanumeric() || c == '_') {
            // Get the full qualified name prefix
            let word_start = trimmed[..backslash_pos]
                .rfind(|c: char| !c.is_alphanumeric() && c != '_' && c != '\\')
                .map(|p| p + 1)
                .unwrap_or(0);
            let prefix = &trimmed[word_start..];

            return Some(CompletionContext::Namespace {
                prefix: prefix.to_string(),
            });
        }
    }

    None
}

/// Check if cursor is inside a use statement.
fn check_use_context(node: &Node, text_before: &str, source: &str) -> Option<CompletionContext> {
    let mut current = Some(*node);
    while let Some(n) = current {
        if n.kind() == "namespace_use_declaration" || n.kind() == "namespace_use_clause" {
            let node_text = &source[n.byte_range()];
            let prefix_source = if text_before.trim_start().starts_with("use") {
                text_before
            } else {
                node_text
            };
            let prefix = use_statement_prefix(prefix_source);
            return Some(CompletionContext::UseStatement { prefix });
        }
        current = n.parent();
    }
    None
}

fn use_statement_prefix(text: &str) -> String {
    let mut prefix = text.trim_start();
    prefix = prefix.strip_prefix("use").unwrap_or(prefix).trim_start();
    for keyword in ["function", "const"] {
        if let Some(rest) = prefix.strip_prefix(keyword) {
            prefix = rest.trim_start();
            break;
        }
    }
    prefix.trim_end_matches(';').trim().to_string()
}

/// Extract the object expression from text before `->`.
fn extract_object_expr(text: &str) -> String {
    // Walk backwards to find the start of the expression
    let trimmed = text.trim_end();
    let mut start = trimmed.len();
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;

    // Take the last object expression segment. This must keep simple member
    // chains such as `$this->client`, because completion after
    // `$this->client->` needs the property type, not just the bare `client`.
    for (idx, c) in trimmed.char_indices().rev() {
        match c {
            ')' => {
                paren_depth += 1;
                start = idx;
                continue;
            }
            '(' if paren_depth > 0 => {
                paren_depth -= 1;
                start = idx;
                continue;
            }
            '(' => break,
            ']' => {
                bracket_depth += 1;
                start = idx;
                continue;
            }
            '[' if bracket_depth > 0 => {
                bracket_depth -= 1;
                start = idx;
                continue;
            }
            '[' => break,
            _ if paren_depth > 0 || bracket_depth > 0 => {
                start = idx;
                continue;
            }
            _ => {}
        }

        if c.is_alphanumeric() || matches!(c, '_' | '$' | '\\' | ':' | '-' | '>' | '?') {
            start = idx;
        } else {
            break;
        }
    }

    let mut expr_start = start;
    let before_expr = &trimmed[..start];
    let before_expr = before_expr.trim_end();
    if let Some(new_start) = before_expr.rfind("new") {
        let has_keyword_boundary = before_expr[..new_start]
            .chars()
            .next_back()
            .is_none_or(|ch| !ch.is_alphanumeric() && ch != '_' && ch != '\\');
        if has_keyword_boundary && before_expr[new_start + 3..].trim().is_empty() {
            expr_start = new_start;
        }
    }

    trimmed[expr_start..].to_string()
}

/// Try to find the object expression from CST node context.
fn find_object_in_cst(node: &Node, source: &str) -> Option<String> {
    let mut current = Some(*node);
    while let Some(n) = current {
        if n.kind() == "member_access_expression" || n.kind() == "member_call_expression" {
            if let Some(obj) = n.child_by_field_name("object") {
                return Some(source[obj.byte_range()].to_string());
            }
        }
        current = n.parent();
    }
    None
}

/// Resolve a static access scope for completion context.
fn resolve_scope_class_for_completion(
    name: &str,
    node: Node,
    source: &str,
    file_symbols: &FileSymbols,
) -> String {
    php_lsp_parser::resolve::resolve_scope_class_name_pub(name, node, source, file_symbols)
}

/// Extract the word (identifier) before cursor.
fn extract_word_before_cursor(text_before: &str) -> String {
    let mut start = text_before.len();

    for (idx, c) in text_before.char_indices().rev() {
        if c.is_alphanumeric() || c == '_' {
            start = idx;
        } else {
            break;
        }
    }

    text_before[start..].to_string()
}

/// Check if the position is a type hint context.
fn is_type_hint_position(node: &Node, _source: &str, _cursor_offset: usize) -> bool {
    let mut current = Some(*node);
    while let Some(n) = current {
        match n.kind() {
            "named_type"
            | "optional_type"
            | "union_type"
            | "intersection_type"
            | "simple_parameter"
            | "property_declaration" => return true,
            _ => {}
        }
        current = n.parent();
    }
    false
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
