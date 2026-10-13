use super::*;
use crate::{parser::FileParser, symbols::extract_file_symbols};

fn inferred(source: &str) -> Option<String> {
    // Give the builtin a known array input independently of literal-expression
    // inference, which is not the behavior under test here.
    let source = source.replace("['x'=>42]", "$items").replace(
        "$result=",
        "/** @var array<string,int> $items */$items=[];$result=",
    );
    let source = source.as_str();
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let tree = parser.tree().unwrap();
    assert!(!tree.root_node().has_error(), "{source}");
    let symbols = extract_file_symbols(tree, source, "file:///builtin-identity.php");
    let functions = |fqn: &str| {
        matches!(
            fqn,
            "array_keys" | "array_values" | "preg_match" | "preg_match_all"
        ) || symbols
            .symbols
            .iter()
            .any(|s| s.kind == PhpSymbolKind::Function && s.fqn.eq_ignore_ascii_case(fqn))
    };
    let _names = crate::function_names::FunctionNameScope::enter(
        tree.root_node(),
        source,
        Some(&symbols),
        Some(&functions),
    );
    let offset = source.find("/*USE*/").unwrap() + 7;
    let before = &source[..offset];
    let resolver = |owner: &str, fqn: &str| {
        if !owner.is_empty() {
            return None;
        }
        let symbol = symbols.symbols.iter().find(|s| {
            s.kind == PhpSymbolKind::Function && php_lsp_types::symbol_fqn_eq(&s.fqn, fqn, s.kind)
        })?;
        let ty = symbol.signature.as_ref()?.return_type.as_ref()?;
        Some(resolve_type_info_relative_to_symbol(ty, symbol, &symbols).to_string())
    };
    infer_variable_type_info_at_position_with_resolver(
        tree,
        source,
        &symbols,
        before.bytes().filter(|b| *b == b'\n').count() as u32,
        before.rsplit('\n').next().unwrap().len() as u32,
        "$result",
        &resolver,
    )
    .map(|ty| ty.to_string())
}

#[test]
fn p232_namespaced_array_helpers_keep_their_own_return_type() {
    for function in ["array_keys", "array_values"] {
        let source=format!("<?php namespace App; class Own {{}} function {function}($items):Own{{return new Own;}} function run(){{$result={function}(['x'=>42]);/*USE*/$result;}}");
        assert_eq!(
            inferred(&source).as_deref(),
            Some("\\App\\Own"),
            "{function}"
        );
    }
}

#[test]
fn p232_qualified_foreign_array_helpers_do_not_get_builtin_types() {
    for call in [
        "\\Vendor\\array_keys",
        "Vendor\\array_values",
        "namespace\\array_keys",
    ] {
        let source = format!(
            "<?php namespace App; function run(){{$result={call}(['x'=>42]);/*USE*/$result;}}"
        );
        assert_eq!(inferred(&source), None, "foreign function {call}");
    }
}

#[test]
fn p232_global_array_aliases_and_case_insensitive_names_keep_builtin_types() {
    for (imports, call) in [
        ("", "\\ArRaY_VaLuEs"),
        ("use function array_values as vals;", "VaLs"),
        ("use function array_values;", "array_values"),
        ("", "array_values"),
    ] {
        let source=format!("<?php namespace App; {imports} function run(){{$result={call}(['x'=>42]);/*USE*/$result;}}");
        assert_eq!(inferred(&source).as_deref(), Some("list<int>"), "{call}");
    }
}

#[test]
fn p232_function_imports_do_not_confuse_class_imports_or_repeated_sections() {
    let source="<?php namespace App {use function array_values as vals; function first(){$result=vals(['x'=>42]);}} namespace App {use function Vendor\\array_values as vals; function second(){$result=vals(['x'=>42]);/*USE*/$result;}}";
    assert_eq!(inferred(source), None);
    let source="<?php namespace App; use Vendor\\array_values; function run(){$result=array_values(['x'=>42]);/*USE*/$result;}";
    assert_eq!(inferred(source).as_deref(), Some("list<int>"));
}

#[test]
fn p232_named_array_argument_is_resolved_by_parameter_name() {
    let source = "<?php $result=\\array_keys(filter_value:42, array:['x'=>42]);/*USE*/$result;";
    assert_eq!(inferred(source).as_deref(), Some("list<string>"));
}

#[test]
fn p232_unknown_local_function_does_not_inherit_a_builtin_return() {
    let source="<?php namespace App; function array_values($items) {} function run(){$result=array_values(['x'=>42]);/*USE*/$result;}";
    assert_eq!(inferred(source), None);
}

#[test]
fn p232_nullable_method_result_is_not_projected_to_a_non_null_object_on_assignment() {
    for body in [
        "$result=$factory->first();/*USE*/$result;",
        "if($flag){$result=$factory->first();/*USE*/$result;}",
    ] {
        let source=format!("<?php namespace App; class User{{}} class Factory{{function first():?User{{return null;}}}} function run(Factory $factory,$flag){{{body}}}");
        assert_eq!(inferred(&source).as_deref(), Some("?\\App\\User"));
    }
}

#[test]
fn p232_member_only_callback_does_not_prove_a_global_function_fallback() {
    let source="<?php namespace App; function run(){/** @var array<string,int> $items */$items=[];$result=array_values($items);/*USE*/$result;}";
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let tree = parser.tree().unwrap();
    let symbols = extract_file_symbols(tree, source, "file:///member-only.php");
    let offset = source.find("/*USE*/").unwrap() + 7;
    let member_only = |_: &str, _: &str| None;
    assert_eq!(
        infer_variable_type_info_at_position_with_resolver(
            tree,
            source,
            &symbols,
            0,
            offset as u32,
            "$result",
            &member_only
        ),
        None,
        "member lookup cannot certify function identity"
    );
}

#[test]
fn p232_nullsafe_non_null_terminal_keeps_receiver_nullability() {
    let source="<?php namespace App; class User{} class Factory{function firstOrFail():User{return new User;}} function run(?Factory $factory){$result=$factory?->firstOrFail();/*USE*/$result;}";
    assert_eq!(inferred(source).as_deref(), Some("?\\App\\User"));
}
