use super::*;
use php_lsp_parser::parser::FileParser;
use php_lsp_parser::symbols::extract_file_symbols;

fn detect_at_byte_col(code: &str, line: u32, byte_col: u32) -> CompletionContext {
    let mut parser = FileParser::new();
    parser.parse_full(code);
    let tree = parser.tree().unwrap();
    let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
    detect_context_at_byte_col(tree, code, line, byte_col, &file_symbols)
}

fn detect_at_marker(code: &str) -> CompletionContext {
    let marker = "/*caret*/";
    let offset = code.find(marker).expect("test code should contain marker");
    let code = code.replace(marker, "");
    let prefix = &code[..offset];
    let line = prefix.bytes().filter(|b| *b == b'\n').count() as u32;
    let line_start = prefix.rfind('\n').map(|idx| idx + 1).unwrap_or(0);
    let byte_col = prefix[line_start..].len() as u32;

    detect_at_byte_col(&code, line, byte_col)
}

#[test]
fn test_member_access_context() {
    let code = "<?php\n$obj->meth";
    let ctx = detect_at_byte_col(code, 1, 11);
    match ctx {
        CompletionContext::MemberAccess {
            object_expr,
            member_prefix,
            access_mode,
            ..
        } => {
            assert_eq!(object_expr, "$obj");
            assert_eq!(member_prefix, "meth");
            assert_eq!(access_mode, MemberAccessMode::Read);
        }
        other => panic!("Expected MemberAccess, got {:?}", other),
    }
}

#[test]
fn test_member_access_context_detects_write_assignment() {
    let code = "<?php\n$subject->dirty = false;";
    let ctx = detect_at_byte_col(code, 1, 10);
    match ctx {
        CompletionContext::MemberAccess {
            object_expr,
            member_prefix,
            access_mode,
            ..
        } => {
            assert_eq!(object_expr, "$subject");
            assert_eq!(member_prefix, "");
            assert_eq!(access_mode, MemberAccessMode::Write);
        }
        other => panic!("Expected MemberAccess, got {:?}", other),
    }
}

#[test]
fn test_member_access_context_trims_single_nullsafe_marker() {
    let code = "<?php\n$session?->get";
    let ctx = detect_at_byte_col(code, 1, 14);
    match ctx {
        CompletionContext::MemberAccess {
            object_expr,
            member_prefix,
            ..
        } => {
            assert_eq!(object_expr, "$session");
            assert_eq!(member_prefix, "get");
        }
        other => panic!("Expected MemberAccess, got {:?}", other),
    }
}

#[test]
fn test_member_access_receiver_text_strips_only_one_question_mark() {
    assert_eq!(receiver_text_before_member_arrow("$session?"), "$session");
    assert_eq!(receiver_text_before_member_arrow("$session??"), "$session?");
    assert_eq!(
        receiver_text_before_member_arrow("$items[$i ?? 0]?"),
        "$items[$i ?? 0]"
    );
    assert_eq!(
        receiver_text_before_member_arrow("$flag ? $left : $right"),
        "$flag ? $left : $right"
    );
}

#[test]
fn test_member_access_context_inside_parenthesized_condition() {
    let code = "<?php\nif ($reflMethod->isSt) {}";
    let ctx = detect_at_byte_col(code, 1, 17);
    match ctx {
        CompletionContext::MemberAccess {
            object_expr,
            member_prefix,
            ..
        } => {
            assert_eq!(object_expr, "$reflMethod");
            assert_eq!(member_prefix, "");
        }
        other => panic!("Expected MemberAccess, got {:?}", other),
    }
}

#[test]
fn test_member_access_context_keeps_property_chain() {
    let code = "<?php\n$this->client->reques";
    let ctx = detect_at_byte_col(code, 1, 21);
    match ctx {
        CompletionContext::MemberAccess {
            object_expr,
            member_prefix,
            ..
        } => {
            assert_eq!(object_expr, "$this->client");
            assert_eq!(member_prefix, "reques");
        }
        other => panic!("Expected MemberAccess, got {:?}", other),
    }
}

#[test]
fn test_member_access_context_keeps_array_access_object() {
    let code = "<?php\n$users[0]->";
    let ctx = detect_at_byte_col(code, 1, 11);
    match ctx {
        CompletionContext::MemberAccess {
            object_expr,
            member_prefix,
            ..
        } => {
            assert_eq!(object_expr, "$users[0]");
            assert_eq!(member_prefix, "");
        }
        other => panic!("Expected MemberAccess, got {:?}", other),
    }
}

#[test]
fn test_array_key_context_inside_string_key() {
    let code = "<?php\n$row['fo";
    let ctx = detect_at_byte_col(code, 1, 8);
    match ctx {
        CompletionContext::ArrayKey {
            array_expr,
            key_prefix,
            quote,
        } => {
            assert_eq!(array_expr, "$row");
            assert_eq!(key_prefix, "fo");
            assert_eq!(quote, Some('\''));
        }
        other => panic!("Expected ArrayKey, got {:?}", other),
    }
}

#[test]
fn test_array_key_context_keeps_nested_array_access_base() {
    let code = "<?php\n$row['meta']['";
    let ctx = detect_at_byte_col(code, 1, 14);
    match ctx {
        CompletionContext::ArrayKey {
            array_expr,
            key_prefix,
            quote,
        } => {
            assert_eq!(array_expr, "$row['meta']");
            assert_eq!(key_prefix, "");
            assert_eq!(quote, Some('\''));
        }
        other => panic!("Expected ArrayKey, got {:?}", other),
    }
}

#[test]
fn test_array_key_context_after_open_bracket() {
    let code = "<?php\n$row[";
    let ctx = detect_at_byte_col(code, 1, 5);
    match ctx {
        CompletionContext::ArrayKey {
            array_expr,
            key_prefix,
            quote,
        } => {
            assert_eq!(array_expr, "$row");
            assert_eq!(key_prefix, "");
            assert_eq!(quote, None);
        }
        other => panic!("Expected ArrayKey, got {:?}", other),
    }
}

#[test]
fn test_array_key_context_after_non_ascii_text_before_key() {
    let code = "<?php\n$label = '中文测试 བོད'; $row['fo/*caret*/";
    let ctx = detect_at_marker(code);
    match ctx {
        CompletionContext::ArrayKey {
            array_expr,
            key_prefix,
            quote,
        } => {
            assert_eq!(array_expr, "$row");
            assert_eq!(key_prefix, "fo");
            assert_eq!(quote, Some('\''));
        }
        other => panic!("Expected ArrayKey, got {:?}", other),
    }
}

#[test]
fn test_array_key_context_accepts_quoted_non_ascii_prefixes() {
    for (code, expected_prefix, expected_quote) in [
        ("<?php\n$row['中文/*caret*/", "中文", '\''),
        ("<?php\n$row[\"བོད/*caret*/", "བོད", '"'),
    ] {
        let ctx = detect_at_marker(code);
        match ctx {
            CompletionContext::ArrayKey {
                array_expr,
                key_prefix,
                quote,
            } => {
                assert_eq!(array_expr, "$row");
                assert_eq!(key_prefix, expected_prefix);
                assert_eq!(quote, Some(expected_quote));
            }
            other => panic!("Expected ArrayKey for {code:?}, got {:?}", other),
        }
    }
}

#[test]
fn test_array_key_context_accepts_unfinished_quoted_key_before_cursor() {
    for code in ["<?php\n$row['/*caret*/", "<?php\n$row[\"/*caret*/"] {
        let ctx = detect_at_marker(code);
        match ctx {
            CompletionContext::ArrayKey {
                array_expr,
                key_prefix,
                quote,
            } => {
                assert_eq!(array_expr, "$row");
                assert_eq!(key_prefix, "");
                assert!(quote.is_some());
            }
            other => panic!("Expected ArrayKey for {code:?}, got {:?}", other),
        }
    }
}

#[test]
fn test_member_access_context_keeps_method_array_access_object() {
    let code = "<?php\n$repo->findAll()[0]->";
    let ctx = detect_at_byte_col(code, 1, 21);
    match ctx {
        CompletionContext::MemberAccess {
            object_expr,
            member_prefix,
            ..
        } => {
            assert_eq!(object_expr, "$repo->findAll()[0]");
            assert_eq!(member_prefix, "");
        }
        other => panic!("Expected MemberAccess, got {:?}", other),
    }
}

#[test]
fn test_member_access_context_keeps_parenthesized_new_expression() {
    let code = "<?php\n(new Uri('https://example.com'))->set";
    let ctx = detect_at_byte_col(code, 1, 38);
    match ctx {
        CompletionContext::MemberAccess {
            object_expr,
            member_prefix,
            ..
        } => {
            assert_eq!(object_expr, "(new Uri('https://example.com'))");
            assert_eq!(member_prefix, "set");
        }
        other => panic!("Expected MemberAccess, got {:?}", other),
    }
}

#[test]
fn test_member_access_context_keeps_bare_new_expression() {
    let code = "<?php\nnew \\ReflectionClass($service)->is";
    let ctx = detect_at_byte_col(code, 1, 35);
    match ctx {
        CompletionContext::MemberAccess {
            object_expr,
            member_prefix,
            ..
        } => {
            assert_eq!(object_expr, "new \\ReflectionClass($service)");
            assert_eq!(member_prefix, "is");
        }
        other => panic!("Expected MemberAccess, got {:?}", other),
    }
}

#[test]
fn test_member_access_context_keeps_static_call_object() {
    for (code, expected) in [
        ("<?php\nself::make()->", "self::make()"),
        ("<?php\nstatic::make()->", "static::make()"),
        ("<?php\nparent::make()->", "parent::make()"),
        ("<?php\nUser::query()->", "User::query()"),
    ] {
        let ctx = detect_at_byte_col(code, 1, code.lines().nth(1).unwrap().len() as u32);
        match ctx {
            CompletionContext::MemberAccess {
                object_expr,
                member_prefix,
                ..
            } => {
                assert_eq!(object_expr, expected);
                assert_eq!(member_prefix, "");
            }
            other => panic!("Expected MemberAccess for {code:?}, got {:?}", other),
        }
    }
}

#[test]
fn test_static_access_context() {
    let code = "<?php\nFoo::bar";
    let ctx = detect_at_byte_col(code, 1, 8);
    match ctx {
        CompletionContext::StaticAccess {
            class_expr,
            member_prefix,
            ..
        } => {
            assert_eq!(class_expr, "Foo");
            assert_eq!(member_prefix, "bar");
        }
        other => panic!("Expected StaticAccess, got {:?}", other),
    }
}

#[test]
fn test_static_access_context_after_non_ascii_text_on_same_line() {
    let code = "<?php\n$this->assertSame('ཇི་ཨེམ་ཏི་-03:00', Timezones::/*caret*/get";
    let ctx = detect_at_marker(code);
    match ctx {
        CompletionContext::StaticAccess {
            class_expr,
            member_prefix,
            ..
        } => {
            assert_eq!(class_expr, "Timezones");
            assert_eq!(member_prefix, "");
        }
        other => panic!("Expected StaticAccess, got {:?}", other),
    }
}

#[test]
fn test_context_clamps_invalid_byte_col_to_utf8_boundary() {
    for code in ["<?php\n$привет", "<?php\n$中文", "<?php\n$བོད"] {
        let ctx = detect_at_byte_col(code, 1, 2);
        match ctx {
            CompletionContext::Variable { prefix } => {
                assert_eq!(prefix, "");
            }
            other => panic!("Expected Variable for {code:?}, got {:?}", other),
        }
    }
}

#[test]
fn test_static_access_context_on_crlf_line() {
    for code in [
        "<?php\r\n// Привет\r\nTimezones::/*caret*/get",
        "<?php\r\n// 中文测试\r\nTimezones::/*caret*/get",
        "<?php\r\n// བོད་ཡིག\r\nTimezones::/*caret*/get",
    ] {
        let ctx = detect_at_marker(code);
        match ctx {
            CompletionContext::StaticAccess {
                class_expr,
                member_prefix,
                ..
            } => {
                assert_eq!(class_expr, "Timezones");
                assert_eq!(member_prefix, "");
            }
            other => panic!("Expected StaticAccess for {code:?}, got {:?}", other),
        }
    }
}

#[test]
fn test_member_access_context_after_chinese_and_tibetan_text_on_same_line() {
    for code in [
        "<?php\n$label = '中文测试'; $target->/*caret*/complete",
        "<?php\n$label = 'བོད་ཡིག'; $target->/*caret*/complete",
    ] {
        let ctx = detect_at_marker(code);
        match ctx {
            CompletionContext::MemberAccess {
                object_expr,
                member_prefix,
                ..
            } => {
                assert_eq!(object_expr, "$target");
                assert_eq!(member_prefix, "");
            }
            other => panic!("Expected MemberAccess for {code:?}, got {:?}", other),
        }
    }
}

#[test]
fn test_static_access_context_resolves_self_static_and_parent() {
    let code = "<?php\nnamespace App;\nclass Base {}\nclass Child extends Base { public function run() { self::/*caret*/foo(); static::bar(); parent::baz(); } }";
    let ctx = detect_at_marker(code);
    match ctx {
        CompletionContext::StaticAccess { class_fqn, .. } => {
            assert_eq!(class_fqn, "App\\Child");
        }
        other => panic!("Expected StaticAccess, got {:?}", other),
    }

    let code = "<?php\nnamespace App;\nclass Base {}\nclass Child extends Base { public function run() { self::foo(); static::/*caret*/bar(); parent::baz(); } }";
    let ctx = detect_at_marker(code);
    match ctx {
        CompletionContext::StaticAccess { class_fqn, .. } => {
            assert_eq!(class_fqn, "App\\Child");
        }
        other => panic!("Expected StaticAccess, got {:?}", other),
    }

    let code = "<?php\nnamespace App;\nclass Base {}\nclass Child extends Base { public function run() { self::foo(); static::bar(); parent::/*caret*/baz(); } }";
    let ctx = detect_at_marker(code);
    match ctx {
        CompletionContext::StaticAccess { class_fqn, .. } => {
            assert_eq!(class_fqn, "App\\Base");
        }
        other => panic!("Expected StaticAccess, got {:?}", other),
    }
}

#[test]
fn test_variable_context() {
    let code = "<?php\n$use";
    let ctx = detect_at_byte_col(code, 1, 4);
    match ctx {
        CompletionContext::Variable { prefix } => {
            assert_eq!(prefix, "use");
        }
        other => panic!("Expected Variable, got {:?}", other),
    }
}

#[test]
fn test_use_statement_context_uses_text_before_cursor() {
    let code = "<?php\nnamespace App;\nuse Ven;\n";
    let ctx = detect_at_byte_col(code, 2, 7);
    match ctx {
        CompletionContext::UseStatement { prefix } => {
            assert_eq!(prefix, "Ven");
        }
        other => panic!("Expected UseStatement, got {:?}", other),
    }
}

#[test]
fn test_free_context() {
    let code = "<?php\narray_m";
    let ctx = detect_at_byte_col(code, 1, 7);
    match ctx {
        CompletionContext::Free { prefix } => {
            assert_eq!(prefix, "array_m");
        }
        other => panic!("Expected Free, got {:?}", other),
    }
}

#[test]
fn test_free_context_after_non_ascii_text_on_same_line() {
    let code = "<?php\nfoo('ཇི་ཨེམ་ཏི', Timez/*caret*/);";
    let ctx = detect_at_marker(code);
    match ctx {
        CompletionContext::Free { prefix } => {
            assert_eq!(prefix, "Timez");
        }
        other => panic!("Expected Free, got {:?}", other),
    }
}
