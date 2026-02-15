//! Completion context detection.
//!
//! Determines what kind of completion is appropriate based on
//! the cursor position in the CST and surrounding text.

use php_lsp_types::FileSymbols;
use tree_sitter::{Node, Point, Tree};

/// The context in which completion was triggered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionContext {
    /// After `->`: instance member access (methods, properties).
    MemberAccess {
        /// The object expression text (e.g. "$this", "$foo").
        object_expr: String,
        /// Optional inferred FQN of object class (filled later by server).
        class_fqn: Option<String>,
    },

    /// After `::`: static member access (static methods, properties, constants).
    StaticAccess {
        /// The class name or expression (e.g. "self", "Foo").
        class_expr: String,
        /// Resolved FQN of the class.
        class_fqn: String,
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

/// Determine the completion context at a position.
pub fn detect_context(
    tree: &Tree,
    source: &str,
    line: u32,
    character: u32,
    file_symbols: &FileSymbols,
) -> CompletionContext {
    let point = Point::new(line as usize, character as usize);
    let root = tree.root_node();

    // Find the node at position
    let node = match root.descendant_for_point_range(point, point) {
        Some(n) => n,
        None => return CompletionContext::None,
    };

    // Get the text before cursor on the current line
    let line_start = source
        .lines()
        .take(line as usize)
        .map(|l| l.len() + 1)
        .sum::<usize>();
    let cursor_offset = line_start + character as usize;
    let line_text = source.lines().nth(line as usize).unwrap_or("");
    let text_before = &line_text[..std::cmp::min(character as usize, line_text.len())];

    // Check for `->` member access
    if let Some(ctx) = check_member_access(text_before, &node, source) {
        return ctx;
    }

    // Check for `::` static access
    if let Some(ctx) = check_static_access(text_before, &node, source, file_symbols) {
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

/// Check for `->` member access pattern.
fn check_member_access(text_before: &str, node: &Node, source: &str) -> Option<CompletionContext> {
    let trimmed = text_before.trim_end();

    // Check if text ends with `->`  or `->partial`
    if let Some(arrow_pos) = trimmed.rfind("->") {
        let after_arrow = &trimmed[arrow_pos + 2..];
        // Ensure after arrow is a valid identifier prefix or empty
        if after_arrow.chars().all(|c| c.is_alphanumeric() || c == '_') {
            let before_arrow = trimmed[..arrow_pos].trim_end();

            // Walk up to find the object
            let object_expr = if !before_arrow.is_empty() {
                extract_object_expr(before_arrow)
            } else {
                // Try from CST
                find_object_in_cst(node, source).unwrap_or_else(|| "$this".to_string())
            };

            return Some(CompletionContext::MemberAccess {
                object_expr,
                class_fqn: None,
            });
        }
    }

    None
}

/// Check for `::` static access pattern.
fn check_static_access(
    text_before: &str,
    _node: &Node,
    _source: &str,
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
            let class_fqn = resolve_class_for_completion(&class_expr, file_symbols);

            return Some(CompletionContext::StaticAccess {
                class_expr,
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
fn check_use_context(node: &Node, _text_before: &str, source: &str) -> Option<CompletionContext> {
    let mut current = Some(*node);
    while let Some(n) = current {
        if n.kind() == "namespace_use_declaration" || n.kind() == "namespace_use_clause" {
            // Get the text of the current node as prefix
            let text = &source[n.byte_range()];
            let prefix = text.trim_start_matches("use").trim();
            return Some(CompletionContext::UseStatement {
                prefix: prefix.to_string(),
            });
        }
        current = n.parent();
    }
    None
}

/// Extract the object expression from text before `->`.
fn extract_object_expr(text: &str) -> String {
    // Walk backwards to find the start of the expression
    let trimmed = text.trim_end();
    let end = trimmed.len();
    let chars: Vec<char> = trimmed.chars().collect();
    let mut i = chars.len();

    // Simple: take last identifier/variable
    while i > 0 {
        let c = chars[i - 1];
        if c.is_alphanumeric() || c == '_' || c == '$' {
            i -= 1;
        } else {
            break;
        }
    }

    trimmed[i..end].to_string()
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

/// Resolve a class name for completion context.
fn resolve_class_for_completion(name: &str, file_symbols: &FileSymbols) -> String {
    php_lsp_parser::resolve::resolve_class_name_pub(name, file_symbols)
}

/// Extract the word (identifier) before cursor.
fn extract_word_before_cursor(text_before: &str) -> String {
    let chars: Vec<char> = text_before.chars().collect();
    let mut i = chars.len();

    while i > 0 {
        let c = chars[i - 1];
        if c.is_alphanumeric() || c == '_' {
            i -= 1;
        } else {
            break;
        }
    }

    text_before[i..].to_string()
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
mod tests {
    use super::*;
    use php_lsp_parser::parser::FileParser;
    use php_lsp_parser::symbols::extract_file_symbols;

    fn detect(code: &str, line: u32, col: u32) -> CompletionContext {
        let mut parser = FileParser::new();
        parser.parse_full(code);
        let tree = parser.tree().unwrap();
        let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
        detect_context(tree, code, line, col, &file_symbols)
    }

    #[test]
    fn test_member_access_context() {
        let code = "<?php\n$obj->meth";
        let ctx = detect(code, 1, 11);
        match ctx {
            CompletionContext::MemberAccess { object_expr, .. } => {
                assert_eq!(object_expr, "$obj");
            }
            other => panic!("Expected MemberAccess, got {:?}", other),
        }
    }

    #[test]
    fn test_static_access_context() {
        let code = "<?php\nFoo::bar";
        let ctx = detect(code, 1, 8);
        match ctx {
            CompletionContext::StaticAccess { class_expr, .. } => {
                assert_eq!(class_expr, "Foo");
            }
            other => panic!("Expected StaticAccess, got {:?}", other),
        }
    }

    #[test]
    fn test_variable_context() {
        let code = "<?php\n$use";
        let ctx = detect(code, 1, 4);
        match ctx {
            CompletionContext::Variable { prefix } => {
                assert_eq!(prefix, "use");
            }
            other => panic!("Expected Variable, got {:?}", other),
        }
    }

    #[test]
    fn test_free_context() {
        let code = "<?php\narray_m";
        let ctx = detect(code, 1, 7);
        match ctx {
            CompletionContext::Free { prefix } => {
                assert_eq!(prefix, "array_m");
            }
            other => panic!("Expected Free, got {:?}", other),
        }
    }
}
