use super::*;
use crate::{parser::FileParser, symbols::extract_file_symbols};

fn parse(source: &str) -> (FileParser, FileSymbols) {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let tree = parser.tree().unwrap();
    assert!(
        !tree.root_node().has_error(),
        "{source}\n{}",
        tree.root_node().to_sexp()
    );
    let symbols = extract_file_symbols(tree, source, "file:///constants.php");
    (parser, symbols)
}

fn marked_range(source: &str, marker: &str) -> (u32, u32, u32, u32) {
    let start = source.find(marker).unwrap() + marker.len();
    let offset = start + usize::from(source[start..].starts_with('$'));
    let prefix = &source[..offset];
    let line = prefix.bytes().filter(|b| *b == b'\n').count() as u32;
    let col = prefix.rsplit('\n').next().unwrap().len() as u32;
    (
        line,
        col,
        line,
        col + source[offset..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .count() as u32,
    )
}

fn assert_not_constant(body: &str) {
    let source = if body.starts_with("namespace ") {
        format!("<?php {body} const FLAG=1;")
    } else {
        format!("<?php const FLAG=1; {body}")
    };
    let (parser, symbols) = parse(&source);
    let tree = parser.tree().unwrap();
    let range = marked_range(&source, "/*NAME*/");
    let refs = collect_symbol_references_in_file(tree, &source, &symbols);
    assert!(
        !refs
            .iter()
            .any(|r| (r.range.0, r.range.1) == (range.0, range.1)
                && r.target_kind == PhpSymbolKind::GlobalConstant),
        "false indexed constant in {body}: {refs:?}"
    );
    let refs = find_references_in_file(
        tree,
        &source,
        &symbols,
        "FLAG",
        PhpSymbolKind::GlobalConstant,
        false,
    );
    assert!(
        !refs
            .iter()
            .any(|r| (r.range.0, r.range.1) == (range.0, range.1)),
        "false scanned constant in {body}: {refs:?}"
    );
    let symbol = crate::resolve::symbol_at_position(tree, &source, range.0, range.1 + 1, &symbols);
    assert!(
        !symbol.is_some_and(|s| s.ref_kind == RefKind::GlobalConstant),
        "false cursor constant in {body}"
    );
}

#[test]
fn constant_names_exclude_method_and_function_declarations() {
    for body in [
        "class C {function /*NAME*/FLAG(){}}",
        "function /*NAME*/FLAG(){}",
    ] {
        assert_not_constant(body);
    }
}
#[test]
fn constant_names_exclude_class_constants_and_enum_case_names() {
    for body in [
        "class C {const /*NAME*/FLAG=1;}",
        "enum E {case /*NAME*/FLAG;}",
        "enum E:int {case /*NAME*/FLAG=1;}",
    ] {
        assert_not_constant(body);
    }
}
#[test]
fn constant_names_exclude_member_names_in_all_access_syntaxes() {
    for body in [
        "$obj->/*NAME*/FLAG;",
        "$obj?->/*NAME*/FLAG;",
        "$obj->/*NAME*/FLAG();",
        "$obj?->/*NAME*/FLAG();",
        "C::/*NAME*/FLAG;",
        "C::/*NAME*/FLAG();",
        "C::/*NAME*/$FLAG;",
    ] {
        assert_not_constant(body);
    }
}
#[test]
fn constant_names_exclude_named_argument_labels() {
    for body in [
        "f(/*NAME*/FLAG: 1);",
        "$obj?->run(/*NAME*/FLAG: 1);",
        "#[A(/*NAME*/FLAG: 1)] function f(){}",
    ] {
        assert_not_constant(body);
    }
}
#[test]
fn constant_names_exclude_attribute_and_type_names() {
    for body in [
        "#[/*NAME*/FLAG] function f(){}",
        "#[/*NAME*/Lib\\FLAG] class C{}",
        "function f(/*NAME*/FLAG $x){}",
        "$x instanceof /*NAME*/FLAG;",
        "new /*NAME*/FLAG;",
        "class C extends /*NAME*/FLAG {}",
        "class C implements /*NAME*/FLAG {}",
        "class C {use /*NAME*/FLAG;}",
    ] {
        assert_not_constant(body);
    }
}
#[test]
fn constant_names_exclude_labels_variables_and_namespace_segments() {
    for body in [
        "/*NAME*/FLAG: goto FLAG;",
        "goto /*NAME*/FLAG;",
        "/*NAME*/$FLAG=1;",
        "use Lib\\Thing as /*NAME*/FLAG;",
        "namespace /*NAME*/FLAG;",
    ] {
        assert_not_constant(body);
    }
}
#[test]
fn constant_names_exclude_property_hook_names() {
    assert_not_constant("class C {public string $x { /*NAME*/get => 'x'; }}");
}

#[test]
fn real_constant_expression_roles_are_collected_and_resolved() {
    for body in [
        "/*READ*/FLAG;",
        "echo /*READ*/FLAG;",
        "return /*READ*/FLAG;",
        "$x=/*READ*/FLAG;",
        "$x+=/*READ*/FLAG;",
        "$x=(/*READ*/FLAG);",
        "$x=/*READ*/FLAG+1;",
        "$x=1+/*READ*/FLAG;",
        "$x=!/*READ*/FLAG;",
        "$x=/*READ*/FLAG?1:2;",
        "$x=true?/*READ*/FLAG:2;",
        "$x=false?1:/*READ*/FLAG;",
        "f(/*READ*/FLAG);",
        "f(value: /*READ*/FLAG);",
        "new C(/*READ*/FLAG);",
        "#[A(value: /*READ*/FLAG)] function f(){}",
        "$x=[/*READ*/FLAG=>1];",
        "$x=[/*READ*/FLAG];",
        "const OTHER=/*READ*/FLAG;",
        "class C {const OTHER=/*READ*/FLAG;}",
        "class C {public $x=/*READ*/FLAG;}",
        "function f($x=/*READ*/FLAG){}",
        "class C {function __construct(public $x=/*READ*/FLAG){}}",
        "function f(){static $x=/*READ*/FLAG;}",
        "enum E:int {case Item=/*READ*/FLAG;}",
        "$x=fn()=>/*READ*/FLAG;",
        "if(/*READ*/FLAG){}",
        "while(/*READ*/FLAG){}",
        "do{}while(/*READ*/FLAG);",
        "for(;/*READ*/FLAG;){}",
        "foreach(/*READ*/FLAG as $x){}",
        "switch(1){case /*READ*/FLAG:break;}",
        "$x=match(1){/*READ*/FLAG=>1,default=>0};",
        "$x=match(1){1=>/*READ*/FLAG,default=>0};",
        "$x=match(1){default=>/*READ*/FLAG};",
        "function f(){yield /*READ*/FLAG;}",
        "function f(){yield from /*READ*/FLAG;}",
        "$x=$items[/*READ*/FLAG];",
        "$x=/*READ*/FLAG[0];",
        "[$x[/*READ*/FLAG]]=$items;",
        "[/*READ*/FLAG=>$x]=$items;",
        "include /*READ*/FLAG;",
        "include_once /*READ*/FLAG;",
        "require /*READ*/FLAG;",
        "require_once /*READ*/FLAG;",
        "print /*READ*/FLAG;",
        "exit(/*READ*/FLAG);",
        "throw /*READ*/FLAG;",
        "$x=clone /*READ*/FLAG;",
        "$x=(string)/*READ*/FLAG;",
        "$x=@/*READ*/FLAG;",
    ] {
        let source = format!("<?php const FLAG=1; {body}");
        let (parser, symbols) = parse(&source);
        let tree = parser.tree().unwrap();
        let range = marked_range(&source, "/*READ*/");
        let refs = collect_symbol_references_in_file(tree, &source, &symbols);
        assert!(
            refs.iter().any(|r| !r.is_declaration
                && r.target_fqn == "FLAG"
                && r.target_kind == PhpSymbolKind::GlobalConstant
                && r.range == range),
            "lost indexed constant in {body}: {refs:?}"
        );
        let refs = find_references_in_file(
            tree,
            &source,
            &symbols,
            "FLAG",
            PhpSymbolKind::GlobalConstant,
            false,
        );
        assert!(
            refs.iter().any(|r| r.range == range),
            "lost scanned constant in {body}"
        );
        let symbol =
            crate::resolve::symbol_at_position(tree, &source, range.0, range.1 + 1, &symbols)
                .unwrap();
        assert_eq!(
            (symbol.ref_kind, symbol.fqn.as_str()),
            (RefKind::GlobalConstant, "FLAG"),
            "{body}"
        );
    }
}

#[test]
fn dynamic_member_expressions_keep_constants_without_static_member_names() {
    for body in [
        "$obj->{/*READ*/FLAG};",
        "$obj?->{/*READ*/FLAG};",
        "$obj->{/*READ*/FLAG}();",
        "$obj?->{/*READ*/FLAG}();",
        "C::{/*READ*/FLAG}();",
        "C::{/*READ*/FLAG};",
    ] {
        let source = format!("<?php const FLAG='member'; {body}");
        let (parser, symbols) = parse(&source);
        let tree = parser.tree().unwrap();
        let range = marked_range(&source, "/*READ*/");
        let refs = collect_symbol_references_in_file(tree, &source, &symbols);
        assert!(
            refs.iter()
                .any(|r| r.range == range && r.target_kind == PhpSymbolKind::GlobalConstant),
            "{body}: {refs:?}"
        );
        assert!(
            !refs.iter().any(|r| r.range == range
                && matches!(
                    r.target_kind,
                    PhpSymbolKind::Property | PhpSymbolKind::Method | PhpSymbolKind::ClassConstant
                )),
            "dynamic expression is not a static member: {body}: {refs:?}"
        );
        let symbol =
            crate::resolve::symbol_at_position(tree, &source, range.0, range.1 + 1, &symbols)
                .unwrap();
        assert_eq!(
            symbol.ref_kind,
            RefKind::GlobalConstant,
            "{body}: {symbol:?}"
        );
    }
}

#[test]
fn constant_declarations_have_one_occurrence_and_keep_their_kind() {
    for (body, target, kind, ref_kind) in [
        (
            "const /*DECL*/FLAG=1;",
            "FLAG",
            PhpSymbolKind::GlobalConstant,
            RefKind::GlobalConstant,
        ),
        (
            "class C {const /*DECL*/FLAG=1;}",
            "C::FLAG",
            PhpSymbolKind::ClassConstant,
            RefKind::ClassConstant,
        ),
        (
            "enum E {case /*DECL*/FLAG;}",
            "E::FLAG",
            PhpSymbolKind::EnumCase,
            RefKind::ClassConstant,
        ),
    ] {
        let source = format!("<?php {body}");
        let (parser, symbols) = parse(&source);
        let tree = parser.tree().unwrap();
        let range = marked_range(&source, "/*DECL*/");
        let refs = collect_symbol_references_in_file(tree, &source, &symbols);
        let at = refs.iter().filter(|r| r.range == range).collect::<Vec<_>>();
        assert_eq!(at.len(), 1, "duplicate declaration: {body}: {refs:?}");
        assert!(at[0].is_declaration);
        assert_eq!(
            (at[0].target_fqn.as_str(), at[0].target_kind),
            (target, kind)
        );
        let refs = find_references_in_file(
            tree,
            &source,
            &symbols,
            "FLAG",
            PhpSymbolKind::GlobalConstant,
            false,
        );
        assert!(
            refs.is_empty(),
            "declaration treated as a read: {body}: {refs:?}"
        );
        let symbol =
            crate::resolve::symbol_at_position(tree, &source, range.0, range.1 + 1, &symbols)
                .unwrap();
        assert_eq!(
            (symbol.fqn.as_str(), symbol.ref_kind),
            (target, ref_kind),
            "{body}"
        );
    }
}

#[test]
fn qualified_constant_names_are_atomic_and_resolve_from_every_segment() {
    let source = r"<?php namespace App; echo \Vendor\FLAG; echo namespace\FLAG; echo Sub\FLAG;";
    let (parser, symbols) = parse(source);
    let tree = parser.tree().unwrap();
    let refs = collect_symbol_references_in_file(tree, source, &symbols);
    let constants = refs
        .iter()
        .filter(|r| r.target_kind == PhpSymbolKind::GlobalConstant)
        .collect::<Vec<_>>();
    assert_eq!(
        constants.len(),
        3,
        "qualified names must be atomic: {refs:?}"
    );
    for (text, target) in [
        (r"\Vendor\FLAG", r"Vendor\FLAG"),
        (r"namespace\FLAG", r"App\FLAG"),
        (r"Sub\FLAG", r"App\Sub\FLAG"),
    ] {
        let offset = source.find(text).unwrap();
        for column in [offset + 1, offset + text.len() - 2] {
            let symbol =
                crate::resolve::symbol_at_position(tree, source, 0, column as u32, &symbols)
                    .unwrap();
            assert_eq!(
                (symbol.fqn.as_str(), symbol.ref_kind),
                (target, RefKind::GlobalConstant),
                "{text}: {column}"
            );
            assert_eq!(
                symbol.range,
                (0, offset as u32, 0, (offset + text.len()) as u32),
                "promoted name range: {text}"
            );
        }
        assert!(constants
            .iter()
            .any(|r| r.target_fqn == target && !r.allows_global_fallback));
    }
}

#[test]
fn scalar_type_words_can_be_user_constant_names_in_expression_roles() {
    for name in [
        "string", "int", "float", "bool", "object", "mixed", "iterable", "callable", "void",
        "never",
    ] {
        let source = format!("<?php define('{name}',1); echo {name};");
        let (parser, symbols) = parse(&source);
        let refs = collect_symbol_references_in_file(parser.tree().unwrap(), &source, &symbols);
        assert_eq!(
            refs.iter()
                .filter(|r| r.target_kind == PhpSymbolKind::GlobalConstant
                    && !r.is_declaration
                    && r.target_fqn == name)
                .count(),
            1,
            "{source}: {refs:?}"
        );
    }
}

#[test]
fn unused_constant_imports_ignore_unrelated_names_but_keep_expression_uses() {
    for body in [
        "class C {function FLAG(){}}",
        "f(FLAG: 1);",
        "#[FLAG] function f(){}",
        "$obj?->FLAG();",
    ] {
        let source = format!("<?php use const Lib\\FLAG; {body}");
        let (parser, symbols) = parse(&source);
        let diagnostics = crate::semantic::extract_semantic_diagnostics(
            parser.tree().unwrap(),
            &source,
            &symbols,
            |_, _| None,
        );
        assert!(
            diagnostics
                .iter()
                .any(|d| d.kind == crate::semantic::SemanticDiagnosticKind::UnusedImport),
            "{body}: {diagnostics:?}"
        );
    }
    let source = "<?php use const Lib\\FLAG; f(value: FLAG);";
    let (parser, symbols) = parse(source);
    let diagnostics = crate::semantic::extract_semantic_diagnostics(
        parser.tree().unwrap(),
        source,
        &symbols,
        |_, _| None,
    );
    assert!(
        !diagnostics
            .iter()
            .any(|d| d.kind == crate::semantic::SemanticDiagnosticKind::UnusedImport),
        "{diagnostics:?}"
    );
}

#[test]
fn unbraced_string_subscript_keys_are_literal_names_not_constants() {
    let source = "<?php const FLAG=1; echo \"$items[FLAG]\";";
    let (parser, symbols) = parse(source);
    let tree = parser.tree().unwrap();
    let range = marked_range(source, "echo \"$items[");
    let refs = collect_symbol_references_in_file(tree, source, &symbols);
    assert!(
        !refs
            .iter()
            .any(|r| r.range == range && r.target_kind == PhpSymbolKind::GlobalConstant),
        "{refs:?}"
    );
}

#[test]
fn braced_string_subscript_keys_evaluate_constants() {
    let source = "<?php const FLAG=1; echo \"{$items[/*READ*/FLAG]}\";";
    let (parser, symbols) = parse(source);
    let tree = parser.tree().unwrap();
    let range = marked_range(source, "/*READ*/");
    let refs = collect_symbol_references_in_file(tree, source, &symbols);
    assert!(
        refs.iter()
            .any(|r| r.range == range && r.target_kind == PhpSymbolKind::GlobalConstant),
        "{refs:?}"
    );
}

#[test]
fn dynamic_class_constant_keeps_class_scope_reference() {
    let source = "<?php const FLAG='KEY'; C::{FLAG};";
    let (parser, symbols) = parse(source);
    let refs = collect_symbol_references_in_file(parser.tree().unwrap(), source, &symbols);
    assert!(
        refs.iter()
            .any(|r| r.target_fqn == "C" && r.target_kind == PhpSymbolKind::Class),
        "lost class scope: {refs:?}"
    );
    assert!(
        refs.iter().any(|r| !r.is_declaration
            && r.target_fqn == "FLAG"
            && r.target_kind == PhpSymbolKind::GlobalConstant),
        "{refs:?}"
    );
    assert!(
        !refs
            .iter()
            .any(|r| r.target_kind == PhpSymbolKind::ClassConstant),
        "{refs:?}"
    );
}

#[test]
fn dynamic_class_constant_expression_wrappers_do_not_hide_constant_reads() {
    for expression in [
        "FLAG . 'suffix'",
        "(FLAG)",
        "FLAG ? 'a' : 'b'",
        r"\Lib\FLAG",
        "strtolower(FLAG)",
    ] {
        let source = format!("<?php const FLAG='KEY'; C::{{{expression}}};");
        let (parser, symbols) = parse(&source);
        let refs = collect_symbol_references_in_file(parser.tree().unwrap(), &source, &symbols);
        let reads = refs
            .iter()
            .filter(|r| !r.is_declaration && r.target_kind == PhpSymbolKind::GlobalConstant)
            .collect::<Vec<_>>();
        assert_eq!(reads.len(), 1, "{expression}: {refs:?}");
        assert_eq!(
            reads[0].target_fqn,
            if expression.starts_with('\\') {
                r"Lib\FLAG"
            } else {
                "FLAG"
            },
            "{expression}: {refs:?}"
        );
    }
}

#[test]
fn attribute_names_keep_class_identity_and_argument_values_keep_constant_identity() {
    let source = "<?php use Lib\\Tag; use const Lib\\FLAG; #[Tag(FLAG)] function f(){}";
    let (parser, symbols) = parse(source);
    let refs = collect_symbol_references_in_file(parser.tree().unwrap(), source, &symbols);
    assert!(
        refs.iter().any(|r| !r.is_import_target
            && !r.is_declaration
            && r.target_fqn == r"Lib\Tag"
            && r.target_kind == PhpSymbolKind::Class),
        "{refs:?}"
    );
    assert_eq!(
        refs.iter()
            .filter(|r| !r.is_import_target
                && !r.is_declaration
                && r.target_kind == PhpSymbolKind::GlobalConstant)
            .map(|r| r.target_fqn.as_str())
            .collect::<Vec<_>>(),
        vec![r"Lib\FLAG"]
    );
}
