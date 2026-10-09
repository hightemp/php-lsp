use super::*;
use php_lsp_parser::parser::FileParser;

fn index_source(index: &WorkspaceIndex, uri: &str, source: &str) {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let symbols =
        php_lsp_parser::symbols::extract_file_symbols(parser.tree().unwrap(), source, uri);
    index.update_file(uri, symbols);
}

#[test]
fn materialization_preserves_native_and_rejected_doc_types() {
    let index = WorkspaceIndex::new();
    index_source(&index,"file:///native-contract.php","<?php class Wrong {} /**\n * @param Wrong $id\n * @return Wrong\n */ function subject(int $id): int {return $id;}");
    let symbol = index.resolve_fqn("subject").unwrap();
    let signature = symbol.signature.as_ref().unwrap();
    assert_eq!(
        signature.params[0].type_info.as_ref().unwrap().to_string(),
        "int"
    );
    assert_eq!(
        signature.params[0]
            .native_type_info
            .as_ref()
            .unwrap()
            .to_string(),
        "int"
    );
    assert_eq!(
        signature.params[0]
            .phpdoc_type_info
            .as_ref()
            .unwrap()
            .to_string(),
        "Wrong"
    );
    assert_eq!(signature.return_type.as_ref().unwrap().to_string(), "int");
    assert_eq!(
        signature.native_return_type.as_ref().unwrap().to_string(),
        "int"
    );
    assert_eq!(
        signature.phpdoc_return_type.as_ref().unwrap().to_string(),
        "Wrong"
    );
}

#[test]
fn class_refinement_is_reconsidered_after_dependency_updates_and_removal() {
    let index = WorkspaceIndex::new();
    index_source(&index,"file:///contract.php","<?php namespace App; class Base {} /**\n * @param Child $value\n * @return Child\n */ function subject(Base $value): Base {return $value;}");
    let check = |expected: &str| {
        let symbol = index.resolve_fqn("App\\subject").unwrap();
        let signature = symbol.signature.as_ref().unwrap();
        assert_eq!(
            signature.return_type.as_ref().unwrap().to_string(),
            expected
        );
        assert_eq!(
            signature.params[0].type_info.as_ref().unwrap().to_string(),
            expected
        );
        assert_eq!(
            signature.native_return_type.as_ref().unwrap().to_string(),
            "Base"
        );
    };
    check("Base");
    index_source(
        &index,
        "file:///child.php",
        "<?php namespace App; class Child extends Base {}",
    );
    check("Child");
    index_source(
        &index,
        "file:///child.php",
        "<?php namespace App; class Child {}",
    );
    check("Base");
    index.remove_file("file:///child.php");
    check("Base");
}

#[test]
fn phpdoc_aliases_do_not_rewrite_native_class_names() {
    let index = WorkspaceIndex::new();
    index_source(&index,"file:///alias-contract.php","<?php\n/** @phpstan-type Item int */\ndeclare(strict_types=1);\nclass Item {}\n/**\n * @param Item $value\n * @return Item\n */\nfunction subject(Item $value): Item {return $value;}");
    let symbol = index.resolve_fqn("subject").unwrap();
    let signature = symbol.signature.as_ref().unwrap();
    assert_eq!(
        signature.native_return_type.as_ref().unwrap().to_string(),
        "Item"
    );
    assert_eq!(signature.return_type.as_ref().unwrap().to_string(), "Item");
}

#[test]
fn concurrent_replacement_publishes_matching_native_doc_and_effective_types() {
    let index = Arc::new(WorkspaceIndex::new());
    let uri = "file:///concurrent-native-contract.php";
    let parse = |doc: &str| {
        let source=format!("<?php class Wrong {{}} /**\n * @param {doc} $value\n * @return {doc}\n */ function subject(int $value): int {{return $value;}}");
        let mut parser = FileParser::new();
        parser.parse_full(&source);
        php_lsp_parser::symbols::extract_file_symbols(parser.tree().unwrap(), &source, uri)
    };
    let valid = parse("positive-int");
    let invalid = parse("Wrong");
    index.update_file(uri, valid.clone());
    let gate = Arc::new(std::sync::Barrier::new(2));
    let writer_index = index.clone();
    let writer_gate = gate.clone();
    let writer = std::thread::spawn(move || {
        writer_gate.wait();
        for i in 0..256 {
            writer_index.update_file(
                uri,
                if i % 2 == 0 {
                    invalid.clone()
                } else {
                    valid.clone()
                },
            );
        }
    });
    gate.wait();
    for _ in 0..256 {
        let symbol = index.resolve_fqn("subject").unwrap();
        let signature = symbol.signature.as_ref().unwrap();
        let doc = signature.phpdoc_return_type.as_ref().unwrap().to_string();
        let expected = if doc == "Wrong" {
            "int"
        } else {
            "positive-int"
        };
        assert_eq!(
            signature.native_return_type.as_ref().unwrap().to_string(),
            "int"
        );
        assert_eq!(
            signature.return_type.as_ref().unwrap().to_string(),
            expected
        );
        assert_eq!(
            signature.params[0]
                .native_type_info
                .as_ref()
                .unwrap()
                .to_string(),
            "int"
        );
        assert_eq!(
            signature.params[0]
                .phpdoc_type_info
                .as_ref()
                .unwrap()
                .to_string(),
            doc
        );
        assert_eq!(
            signature.params[0].type_info.as_ref().unwrap().to_string(),
            expected
        );
    }
    writer.join().unwrap();
}
