use super::*;
use crate::parser::FileParser;

fn signature(source: &str) -> Signature {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let tree = parser.tree().unwrap();
    assert!(!tree.root_node().has_error(), "{source}");
    extract_file_symbols(tree, source, "file:///phpdoc-native.php")
        .symbols
        .into_iter()
        .find(|symbol| symbol.name == "subject")
        .unwrap()
        .signature
        .unwrap()
}

#[test]
fn incompatible_phpdoc_parameter_cannot_replace_a_native_scalar() {
    for (native, doc) in [
        ("int", "Wrong"),
        ("string", "array<int, string>"),
        ("bool", "int"),
        ("array", "Wrong"),
    ] {
        let source = format!("<?php class Wrong {{}} /** @param {doc} $value */ function subject({native} $value) {{}}");
        let extracted = signature(&source);
        assert_eq!(
            extracted.params[0].type_info.as_ref().unwrap().to_string(),
            native,
            "{doc} replaced native {native}"
        );
    }
}

#[test]
fn broader_phpdoc_parameter_cannot_remove_the_native_contract() {
    for (native, doc) in [
        ("int", "mixed"),
        ("int", "int|string"),
        ("int", "?int"),
        ("?int", "int|string|null"),
    ] {
        let source =
            format!("<?php /** @param {doc} $value */ function subject({native} $value) {{}}");
        let extracted = signature(&source);
        assert_eq!(
            extracted.params[0].type_info.as_ref().unwrap().to_string(),
            native,
            "{doc} broadened native {native}"
        );
    }
}

#[test]
fn compatible_phpdoc_refinements_and_untyped_fallbacks_remain_available() {
    for (native, doc) in [
        ("array", "array<int, string>"),
        ("array", "array{name: string}"),
        ("callable", "callable(int): string"),
        ("string", "class-string<Wrong>"),
        ("mixed", "Wrong"),
        ("", "Wrong"),
    ] {
        let source = format!("<?php class Wrong {{}} /** @param {doc} $value */ function subject({native} $value) {{}}");
        let extracted = signature(&source);
        assert_eq!(
            extracted.params[0].type_info.as_ref().unwrap().to_string(),
            doc,
            "lost compatible {doc} on {native}"
        );
    }
}

#[test]
fn native_and_phpdoc_provenance_survive_a_rejected_refinement() {
    let signature=signature("<?php class Wrong {} /**\n * @param Wrong $value\n * @return Wrong\n */ function subject(int $value): int {return $value;}");
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
    assert_eq!(
        signature.native_return_type.as_ref().unwrap().to_string(),
        "int"
    );
    assert_eq!(
        signature.phpdoc_return_type.as_ref().unwrap().to_string(),
        "Wrong"
    );
    assert_eq!(signature.return_type.as_ref().unwrap().to_string(), "int");
}

#[test]
fn known_subclasses_and_bounded_templates_refine_native_declarations() {
    for doc in ["Child", "T"] {
        let source=format!("<?php class Base {{}} class Child extends Base {{}} /**\n * @template T of Child\n * @param {doc} $value\n * @return {doc}\n */ function subject(Base $value): Base {{return $value;}}");
        let signature = signature(&source);
        assert_eq!(
            signature.params[0].type_info.as_ref().unwrap().to_string(),
            doc
        );
        assert_eq!(signature.return_type.as_ref().unwrap().to_string(), doc);
    }
}

#[test]
fn unknown_class_relation_preserves_native_until_more_metadata_is_available() {
    let signature=signature("<?php /**\n * @param ExternalChild $value\n * @return ExternalChild\n */ function subject(ExternalBase $value): ExternalBase {return $value;}");
    assert_eq!(
        signature.params[0].type_info.as_ref().unwrap().to_string(),
        "ExternalBase"
    );
    assert_eq!(
        signature.return_type.as_ref().unwrap().to_string(),
        "ExternalBase"
    );
}

#[test]
fn aliases_and_namespace_imports_use_the_declaration_scope() {
    let source="<?php namespace Models {class Base {} class Child extends Base {}} namespace App {use Models\\Base as ParentType; use Models\\Child as DocType; /**\n * @param DocType $value\n * @return DocType\n */ function subject(ParentType $value): ParentType {return $value;}}";
    let signature = signature(source);
    assert_eq!(
        signature.params[0].type_info.as_ref().unwrap().to_string(),
        "DocType"
    );
    assert_eq!(
        signature.return_type.as_ref().unwrap().to_string(),
        "DocType"
    );
}

#[test]
fn phpdoc_self_or_owner_name_cannot_broaden_native_static() {
    for doc in ["self", "Base"] {
        let source=format!("<?php class Base {{ /** @return {doc} */ function subject(): static {{return new static;}} }}");
        let signature = signature(&source);
        assert!(
            signature.phpdoc_return_type.is_some(),
            "missing doc metadata: {signature:?}"
        );
        assert_eq!(
            signature.return_type,
            Some(TypeInfo::Static_),
            "{doc} weakened native static"
        );
    }
}

#[test]
fn global_namespace_imports_participate_in_native_doc_compatibility() {
    let source="<?php namespace Models {class Base {} class Child extends Base {}} namespace { use Models\\Base as ParentType; use Models\\Child as DocType; /**\n * @param DocType $value\n * @return DocType\n */ function subject(ParentType $value): ParentType {return $value;} }";
    let signature = signature(source);
    assert_eq!(
        signature.params[0].type_info.as_ref().unwrap().to_string(),
        "DocType"
    );
    assert_eq!(
        signature.return_type.as_ref().unwrap().to_string(),
        "DocType"
    );
}

#[test]
fn a_phpdoc_key_type_does_not_replace_native_object() {
    let signature=signature("<?php class Foo {const KEYS = ['name'=>1];} /**\n * @param key-of<Foo::KEYS> $value\n * @return key-of<Foo::KEYS>\n */ function subject(object $value): object {return $value;}");
    assert_eq!(
        signature.params[0].type_info,
        Some(TypeInfo::Simple("object".into()))
    );
    assert_eq!(
        signature.return_type,
        Some(TypeInfo::Simple("object".into()))
    );
}
