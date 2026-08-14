use super::*;
use crate::parser::FileParser;

fn parse_candidates(source: &str, range: (u32, u32, u32, u32)) -> Vec<MissingReturnTypeCandidate> {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    find_missing_return_type_candidates(parser.tree().unwrap(), source, range)
}

#[test]
fn finds_function_and_method_return_type_insertions() {
    let source = r#"<?php
/**
 * @return string|null
 */
function label($value) { return $value; }

class Demo {
    /**
     * @return static
     */
    public function fluent() { return $this; }
}
"#;

    let candidates = parse_candidates(source, (0, 0, 12, 0));
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].name, "label");
    assert_eq!(candidates[0].insert_position, (4, 22));
    assert_eq!(candidates[0].return_type.to_string(), "string|null");
    assert_eq!(candidates[1].name, "fluent");
    assert_eq!(candidates[1].return_type.to_string(), "static");
}

#[test]
fn skips_native_return_types_and_constructors() {
    let source = r#"<?php
class Demo {
    /** @return int */
    public function already(): int { return 1; }

    /** @return string */
    public function __construct() {}
}
"#;

    let candidates = parse_candidates(source, (0, 0, 8, 0));
    assert!(candidates.is_empty());
}
