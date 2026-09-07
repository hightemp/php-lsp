use super::*;

fn byte_range_for(source: &str, needle: &str, last: bool) -> (u32, u32, u32, u32) {
    let start = if last {
        source.rfind(needle)
    } else {
        source.find(needle)
    }
    .unwrap_or_else(|| panic!("missing test needle `{needle}`"));
    let end = start + needle.len();
    let (start_line, start_col) = line_col_for_byte_offset(source, start);
    let (end_line, end_col) = line_col_for_byte_offset(source, end);
    (start_line, start_col, end_line, end_col)
}

fn extract_available(source: &str, needle: &str) -> bool {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let tree = parser.tree().expect("parsed PHP tree");
    let file_symbols = extract_file_symbols(tree, source, "file:///test.php");
    extract_variable_plan(
        tree,
        source,
        &file_symbols,
        byte_range_for(source, needle, false),
        None,
    )
    .is_some()
}

fn inline_available_at(source: &str, variable: &str, last: bool) -> bool {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let tree = parser.tree().expect("parsed PHP tree");
    let file_symbols = extract_file_symbols(tree, source, "file:///test.php");
    inline_variable_plan(
        tree,
        source,
        &file_symbols,
        byte_range_for(source, variable, last),
        None,
    )
    .is_some()
}

fn inline_available(source: &str, variable: &str) -> bool {
    inline_available_at(source, variable, true)
}

#[test]
fn extract_variable_accepts_only_pure_unconditional_left_spine() {
    let safe = "<?php\nfunction f(int $a, int $b): int {\n    return ($a + $b) * 2;\n}\n";
    assert!(extract_available(safe, "$a + $b"));

    let unsafe_cases = [
        (
            "<?php\nfunction f(bool $ready, int $a, int $b): bool\n{\n    return $ready && ($a + $b);\n}\n",
            "$a + $b",
        ),
        (
            "<?php\nfunction f(bool $ready, int $a, int $b): bool\n{\n    return $ready || ($a + $b);\n}\n",
            "$a + $b",
        ),
        (
            "<?php\nfunction f(bool $ready, int $a, int $b): int\n{\n    return $ready ? ($a + $b) : 0;\n}\n",
            "$a + $b",
        ),
        (
            "<?php\nfunction f(bool $ready, int $a, int $b): int\n{\n    return match ($ready) { true => $a + $b, default => 0 };\n}\n",
            "$a + $b",
        ),
        (
            "<?php\nfunction f(int $a, int $b): void\n{\n    while ($a + $b) {}\n}\n",
            "$a + $b",
        ),
        (
            "<?php\nfunction f(int $a, int $b): void\n{\n    for (; $a + $b; ) {}\n}\n",
            "$a + $b",
        ),
        (
            "<?php\nfunction f(int $a, int $b): void\n{\n    do {} while ($a + $b);\n}\n",
            "$a + $b",
        ),
        (
            "<?php\nfunction f(): int\n{\n    return expensive();\n}\n",
            "expensive()",
        ),
        (
            "<?php\nfunction f(object $object): mixed\n{\n    return $object->value;\n}\n",
            "$object->value",
        ),
        (
            "<?php\nfunction f(int $a, int $b): int\n{\n    return $a + ($b * 2);\n}\n",
            "$b * 2",
        ),
        (
            "<?php\nfunction f(int $a, int $b): int { $a = 1; return $a + $b; }\n",
            "$a + $b",
        ),
    ];

    for (source, needle) in unsafe_cases {
        assert!(
            !extract_available(source, needle),
            "unsafe extract should be suppressed for `{needle}` in `{source}`"
        );
    }
}

#[test]
fn refactor_purity_whitelist_excludes_magic_and_conditional_operations() {
    let safe_cases = [
        (
            "<?php\nfunction f(): string\n{\n    return 'stable';\n}\n",
            "'stable'",
        ),
        (
            "<?php\nfunction f(int $value): int\n{\n    return -$value;\n}\n",
            "-$value",
        ),
        (
            "<?php\nfunction f(int $value): int\n{\n    return ~$value;\n}\n",
            "~$value",
        ),
        (
            "<?php\nfunction f(int $left, int $right): int\n{\n    return $left & ($right | 1);\n}\n",
            "$left & ($right | 1)",
        ),
    ];
    for (source, needle) in safe_cases {
        assert!(
            extract_available(source, needle),
            "pure extract should remain available for `{needle}`"
        );
    }

    let unsafe_cases = [
        (
            "<?php\nfunction f($left, $right): string\n{\n    return $left . $right;\n}\n",
            "$left . $right",
        ),
        (
            "<?php\nfunction f($value): string\n{\n    return (string) $value;\n}\n",
            "(string) $value",
        ),
        (
            "<?php\nfunction f($value): bool\n{\n    return $value instanceof Thing;\n}\n",
            "$value instanceof Thing",
        ),
        (
            "<?php\nfunction f($value): mixed\n{\n    return $value ?? 0;\n}\n",
            "$value ?? 0",
        ),
        (
            "<?php\nfunction f(float $value): int\n{\n    return ~$value;\n}\n",
            "~$value",
        ),
        (
            "<?php\nfunction f(int $left, int $right): int\n{\n    return $left << $right;\n}\n",
            "$left << $right",
        ),
        (
            "<?php\nfunction f(int $value): int\n{\n    return 4294967296 & $value;\n}\n",
            "4294967296 & $value",
        ),
        (
            "<?php\nfunction f(string $value): string\n{\n    return \"prefix $value\";\n}\n",
            "\"prefix $value\"",
        ),
        (
            "<?php\nfunction f(bool $left, bool $right): bool\n{\n    return $left xor $right;\n}\n",
            "$left xor $right",
        ),
        (
            "<?php\nfunction f(int $left, int $right): float\n{\n    return $left / $right;\n}\n",
            "$left / $right",
        ),
        (
            "<?php\nfunction f(string $left, string $right): mixed\n{\n    return $left + $right;\n}\n",
            "$left + $right",
        ),
    ];
    for (source, needle) in unsafe_cases {
        assert!(
            !extract_available(source, needle),
            "effectful or conditional extract should be suppressed for `{needle}`"
        );
    }
}

#[test]
fn variable_refactors_require_local_value_return_scopes_and_fresh_names() {
    let top_level_extract = "<?php\n$left = 1;\n$right = 2;\nreturn $left + $right;\n";
    assert!(!extract_available(top_level_extract, "$left + $right"));

    let top_level_inline = "<?php\n$value = 1;\nreturn $value;\n";
    assert!(!inline_available(top_level_inline, "$value"));

    let refcounted_extract = "<?php\nfunction f(object $object): void\n{\n    if ($object) {}\n    unset($object);\n    echo 'A';\n}\n";
    assert!(!extract_available(refcounted_extract, "($object)"));

    let overwritten_scalar_extract = "<?php\nfunction f(int $value): void\n{\n    $value = new Thing();\n    if (($value)) {}\n    unset($value);\n    echo 'A';\n}\n";
    assert!(!extract_available(overwritten_scalar_extract, "($value)"));

    let loop_mutated_scalar_extract = "<?php\nfunction f(int $value): void\n{\n    while (true) {\n        if (($value)) {}\n        $value = new Thing();\n    }\n}\n";
    assert!(!extract_available(loop_mutated_scalar_extract, "($value)"));

    let array_union_extract =
        "<?php\nfunction f(array $left, array $right): array\n{\n    return $left + $right;\n}\n";
    assert!(!extract_available(array_union_extract, "$left + $right"));

    let reference_return_inline =
        "<?php\nfunction &f(int &$source): int\n{\n    $value = $source;\n    return $value;\n}\n";
    assert!(!inline_available(reference_return_inline, "$value"));

    let reference_return_extract =
        "<?php\nfunction &f(int &$source): int\n{\n    return $source;\n}\n";
    let mut reference_parser = FileParser::new();
    reference_parser.parse_full(reference_return_extract);
    let reference_tree = reference_parser.tree().expect("parsed PHP tree");
    let reference_symbols = extract_file_symbols(
        reference_tree,
        reference_return_extract,
        "file:///reference-return.php",
    );
    assert!(extract_variable_plan(
        reference_tree,
        reference_return_extract,
        &reference_symbols,
        byte_range_for(reference_return_extract, "$source", true),
        None,
    )
    .is_none());

    let captured_name = "<?php\nfunction f(int $left, int $right): int\n{\n    $arrow = fn () => $extracted;\n    return $left + $right;\n}\n";
    let mut captured_parser = FileParser::new();
    captured_parser.parse_full(captured_name);
    let captured_tree = captured_parser.tree().expect("parsed PHP tree");
    let captured_symbols =
        extract_file_symbols(captured_tree, captured_name, "file:///captured-name.php");
    let captured_range = byte_range_for(captured_name, "$left + $right", false);
    let plan = extract_variable_plan(
        captured_tree,
        captured_name,
        &captured_symbols,
        captured_range,
        None,
    )
    .expect("safe extraction with collision fallback");
    assert_eq!(plan.variable_name, "extracted2");
    assert!(extract_variable_plan(
        captured_tree,
        captured_name,
        &captured_symbols,
        captured_range,
        Some("$extracted"),
    )
    .is_none());
    assert!(extract_variable_plan(
        captured_tree,
        captured_name,
        &captured_symbols,
        captured_range,
        Some("$extracted2"),
    )
    .is_some());
}

#[test]
fn dynamic_symbol_table_aliases_and_indirect_calls_block_refactors() {
    let inline_cases = [
        "<?php\nuse function get_defined_vars as vars;\nfunction f(int $a): int\n{\n    $value = $a + 1;\n    return $value;\n    vars();\n}\n",
        "<?php\nfunction f(int $a): int\n{\n    $value = $a + 1;\n    return $value;\n    call_user_func('get_defined_vars');\n}\n",
        "<?php\nfunction f(int $a, callable $observer): int\n{\n    $value = $a + 1;\n    return $value;\n    $observer();\n}\n",
        "<?php\nfunction f(int $a): int\n{\n    $value = $a + 1;\n    return $value;\n    assert('$value = 0;');\n}\n",
        "<?php\nfunction f(int $a): int\n{\n    mb_parse_str('a=not_numeric');\n    $value = $a + 1;\n    return $value;\n}\n",
        "<?php\nuse function mb_parse_str as importVariables;\nfunction f(int $a): int\n{\n    importVariables('a=not_numeric');\n    $value = $a + 1;\n    return $value;\n}\n",
    ];
    for source in inline_cases {
        assert!(
            !inline_available(source, "$value"),
            "dynamic call must suppress inline for `{source}`"
        );
    }

    let aliased_extract = "<?php\nuse function get_defined_vars as vars;\nfunction f(int $a): int\n{\n    vars();\n    return $a + 1;\n}\n";
    assert!(!extract_available(aliased_extract, "$a + 1"));
    let mb_parse_extract = "<?php\nfunction f(int $a): int\n{\n    \\mb_parse_str('a=not_numeric');\n    return $a + 1;\n}\n";
    assert!(!extract_available(mb_parse_extract, "$a + 1"));
}

#[test]
fn refactor_edits_preserve_line_count_and_ticks_disable_actions() {
    let ticks_extract = "<?php\ndeclare(ticks=1);\nfunction f(int $left, int $right): int\n{\n    return $left + $right;\n}\n";
    assert!(!extract_available(ticks_extract, "$left + $right"));
    let commented_ticks_extract = "<?php\ndeclare(ticks /* keep */ = 1);\nfunction f(int $left, int $right): int\n{\n    return $left + $right;\n}\n";
    assert!(!extract_available(
        commented_ticks_extract,
        "$left + $right"
    ));
    let ticks_inline = "<?php\ndeclare(ticks=1);\nfunction f(int $source): int\n{\n    $value = $source + 1;\n    return $value;\n}\n";
    assert!(!inline_available(ticks_inline, "$value"));

    let extract_source =
        "<?php\nfunction f(int $left, int $right): int\n{\n    return $left + $right;\n}\n";
    let mut extract_parser = FileParser::new();
    extract_parser.parse_full(extract_source);
    let extract_tree = extract_parser.tree().expect("parsed PHP tree");
    let extract_symbols = extract_file_symbols(extract_tree, extract_source, "file:///lines.php");
    let extract_plan = extract_variable_plan(
        extract_tree,
        extract_source,
        &extract_symbols,
        byte_range_for(extract_source, "$left + $right", false),
        None,
    )
    .expect("line-preserving extract plan");
    assert!(!extract_plan.assignment_text.contains(['\n', '\r']));
    assert_eq!(
        extract_source.as_bytes()[extract_plan.assignment_insert],
        b'r',
        "assignment is inserted after indentation at the statement start"
    );

    let expression_statement_source =
        "<?php\nfunction f(int $left, int $right): void\n{\n    $left + $right;\n}\n";
    let mut expression_statement_parser = FileParser::new();
    expression_statement_parser.parse_full(expression_statement_source);
    let expression_statement_tree = expression_statement_parser.tree().expect("parsed PHP tree");
    let expression_statement_symbols = extract_file_symbols(
        expression_statement_tree,
        expression_statement_source,
        "file:///expression-statement.php",
    );
    let expression_statement_uri: Uri = "file:///expression-statement.php"
        .parse()
        .expect("test URI");
    let expression_statement_edit = extract_variable_edit(
        expression_statement_uri.clone(),
        expression_statement_tree,
        expression_statement_source,
        &expression_statement_symbols,
        byte_range_for(expression_statement_source, "$left + $right", false),
        "$extracted",
    )
    .expect("combined non-overlapping extract edit");
    let expression_statement_edits = expression_statement_edit
        .changes
        .expect("changes")
        .remove(&expression_statement_uri)
        .expect("URI edits");
    assert_eq!(expression_statement_edits.len(), 1);
    assert_eq!(
        expression_statement_edits[0].new_text,
        "$extracted = $left + $right; $extracted"
    );

    let inline_source = "<?php\r\nfunction f(int $source): int\r\n{\r\n    $value = $source + 1;\r\n    return $value;\r\n}\r\n";
    let mut inline_parser = FileParser::new();
    inline_parser.parse_full(inline_source);
    let inline_tree = inline_parser.tree().expect("parsed PHP tree");
    let inline_symbols = extract_file_symbols(inline_tree, inline_source, "file:///lines.php");
    let inline_plan = inline_variable_plan(
        inline_tree,
        inline_source,
        &inline_symbols,
        byte_range_for(inline_source, "$value", true),
        None,
    )
    .expect("line-preserving inline plan");
    assert!(
        !inline_source[inline_plan.assignment_delete.0..inline_plan.assignment_delete.1]
            .contains(['\n', '\r'])
    );
    assert!(inline_source[inline_plan.assignment_delete.1..].starts_with("\r\n"));
}

#[test]
fn inline_variable_requires_one_adjacent_unaliased_safe_read() {
    let safe = "<?php\nfunction f(int $a, int $b): int {\n    $total = $a + $b;\n    // keep this comment\n    return $total;\n}\n";
    assert!(inline_available(safe, "$total"));
    let safe_scalar_copy = "<?php\nfunction f(string $source): string {\n    $value = $source;\n    return $value;\n}\n";
    assert!(inline_available(safe_scalar_copy, "$value"));

    let unsafe_cases = [
        "<?php\nfunction f(): int {\n    $value = nextId();\n    return $value;\n}\n",
        "<?php\nfunction f(object $object): mixed {\n    $value = $object->value;\n    return $value;\n}\n",
        "<?php\nfunction f(int $a): int {\n    $value = $a + 1;\n    logValue();\n    return $value;\n}\n",
        "<?php\nfunction f(int $a): int {\n    $value = $a + 1;\n    $a++;\n    return $value;\n}\n",
        "<?php\nfunction f(int $a): int {\n    $value = $a + 1;\n    return $value + $value;\n}\n",
        "<?php\nfunction f(int $a): void {\n    $value = $a + 1;\n    while ($value) {}\n}\n",
        "<?php\nfunction f(bool $flag, int $a): int {\n    $value = $a + 1;\n    return $flag ? $value : 0;\n}\n",
        "<?php\nfunction f(int $value, int $a): int {\n    $value = $a + 1;\n    return $value;\n}\n",
        "<?php\nfunction f(int &$value, int $a): int {\n    $value = $a + 1;\n    return $value;\n}\n",
        "<?php\nfunction f(int $a): int {\n    static $value;\n    $value = $a + 1;\n    return $value;\n}\n",
        "<?php\nfunction f(int $a): int {\n    global $value;\n    $value = $a + 1;\n    return $value;\n}\n",
        "<?php\nfunction f(int $a): int {\n    $value =& $a;\n    return $value;\n}\n",
        "<?php\nfunction f(int $a): int {\n    $value = $a + 1;\n    $result = $value;\n    $alias =& $value;\n    return $result;\n}\n",
        "<?php\nfunction f(int $a, array $items): int {\n    $value = $a + 1;\n    $result = $value;\n    foreach ($items as &$value) {}\n    return $result;\n}\n",
        "<?php\nfunction f(int $a): int {\n    $value = $a + 1;\n    $result = $value;\n    compact('value');\n    return $result;\n}\n",
        "<?php\nfunction f(int $a): int {\n    $value = $a + 1;\n    return $value;\n    get_defined_vars();\n}\n",
        "<?php\nfunction f(int $a): int {\n    $value = $a + 1;\n    return $value;\n    extract([]);\n}\n",
        "<?php\nfunction f(int $a): int {\n    $value = $a + 1;\n    return $value;\n    parse_str('value=1');\n}\n",
        "<?php\nfunction f(int $a): int {\n    mb_parse_str('a=not_numeric');\n    $value = $a + 1;\n    return $value;\n}\n",
        "<?php\nfunction f(int $a): int {\n    $value = $a + 1;\n    return $value;\n    eval('$value = 0;');\n}\n",
        "<?php\nfunction f(int $a): int {\n    $value = $a + 1;\n    return $value;\n    include 'dynamic.php';\n}\n",
        "<?php\nfunction f(int $a): int {\n    $value = $a + 1;\n    return $value;\n    $name = 'value';\n    $$name;\n}\n",
        "<?php\nfunction f(int $a): int {\n    extract([]);\n    $value = $a + 1;\n    return $value;\n}\n",
        "<?php\nfunction f(object $object): void {\n    $value = $object;\n    $result = $value;\n    unset($object, $result);\n    echo 'A';\n}\n",
        "<?php\nfunction f(array $left, array $right): void {\n    $value = $left + $right;\n    $result = $value;\n    unset($left, $right, $result);\n}\n",
        "<?php\nfunction f(int $a): void {\n    $a = new Thing();\n    $value = $a;\n    $result = $value;\n    unset($a, $result);\n    echo 'A';\n}\n",
        "<?php\nfunction f(int $a): void {\n    while (true) {\n        $value = $a;\n        $result = $value;\n        $a = new Thing();\n    }\n}\n",
        "<?php\nfunction f(int $a): int {\n    $value = $a + 1; return $value;\n}\n",
    ];

    for source in unsafe_cases {
        assert!(
            !inline_available(source, "$value"),
            "unsafe inline should be suppressed for `{source}`"
        );
    }

    let reachable_capture = "<?php\nfunction f(int $a): int {\n    $value = $a + 1;\n    $result = $value;\n    $closure = function () use ($value) { return $value; };\n    return $result;\n}\n";
    assert!(!inline_available_at(reachable_capture, "$value", false));
}

#[test]
fn unsafe_variable_refactors_fail_closed_when_building_edits() {
    let extract_source =
        "<?php\nfunction f(bool $ready, int $a): bool { return $ready && expensive($a); }\n";
    let mut extract_parser = FileParser::new();
    extract_parser.parse_full(extract_source);
    let extract_tree = extract_parser.tree().expect("parsed PHP tree");
    let extract_symbols = extract_file_symbols(
        extract_tree,
        extract_source,
        "file:///test/UnsafeExtract.php",
    );
    assert!(extract_variable_edit(
        "file:///test/UnsafeExtract.php".parse().expect("test URI"),
        extract_tree,
        extract_source,
        &extract_symbols,
        byte_range_for(extract_source, "expensive($a)", false),
        "extracted",
    )
    .is_none());

    let inline_source =
        "<?php\nfunction f(): int {\n    $value = nextId();\n    return $value;\n}\n";
    let mut inline_parser = FileParser::new();
    inline_parser.parse_full(inline_source);
    let inline_tree = inline_parser.tree().expect("parsed PHP tree");
    let inline_symbols =
        extract_file_symbols(inline_tree, inline_source, "file:///test/UnsafeInline.php");
    assert!(inline_variable_edit(
        "file:///test/UnsafeInline.php".parse().expect("test URI"),
        inline_tree,
        inline_source,
        &inline_symbols,
        byte_range_for(inline_source, "$value", true),
        "$value",
    )
    .is_none());
}
