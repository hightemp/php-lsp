use super::*;

fn parsed(source: &str) -> FileSymbols {
    let mut parser = crate::parser::FileParser::new();
    parser.parse_full(source);
    assert!(!parser.tree().unwrap().root_node().has_error(), "{source}");
    crate::symbols::extract_file_symbols(parser.tree().unwrap(), source, "file:///qualified.php")
}

fn resolved(source: &str, name: &str) -> String {
    let file = parsed(source);
    let symbol = file
        .symbols
        .iter()
        .find(|symbol| symbol.name == "load")
        .unwrap();
    resolve_type_name_relative_to_symbol(name, symbol, &file)
}

#[test]
fn qualified_type_sharing_namespace_root_is_still_relative() {
    for declaration in ["class Service { function load() {} }", "function load() {}"] {
        let source = format!("<?php namespace App\\Sub; {declaration}");
        assert_eq!(resolved(&source, "App\\Foo"), "\\App\\Sub\\App\\Foo");
    }
}

#[test]
fn different_root_and_case_do_not_change_qualified_name_rules() {
    let source = "<?php namespace App\\Sub; class Service {function load(){}}";
    for name in ["Other\\Foo", "app\\Foo", "App\\Sub\\Foo"] {
        assert_eq!(resolved(source, name), format!("\\App\\Sub\\{name}"));
    }
}

#[test]
fn explicit_absolute_type_bypasses_namespace_and_aliases() {
    let source = "<?php namespace App\\Sub; use Vendor as App; class Service {function load(){}}";
    assert_eq!(resolved(source, "\\App\\Foo"), "\\App\\Foo");
}

#[test]
fn explicit_namespace_relative_types_bypass_aliases() {
    let source = "<?php namespace App\\Sub; use Vendor as App; class Service {function load(){}}";
    for name in ["namespace\\App\\Foo", "NAMESPACE\\App\\Foo"] {
        assert_eq!(resolved(source, name), "\\App\\Sub\\App\\Foo");
    }
}

#[test]
fn class_aliases_expand_only_first_segment_and_ignore_function_imports() {
    let source = "<?php namespace App\\Sub; use Vendor as App; use function Wrong\\factory as Other; use const Wrong\\VALUE as Constant; class Service {function load(){}}";
    assert_eq!(resolved(source, "aPP\\Foo"), "\\Vendor\\Foo");
    for name in ["Other\\Foo", "Constant\\Foo"] {
        assert_eq!(resolved(source, name), format!("\\App\\Sub\\{name}"));
    }
}

#[test]
fn repeated_namespace_sections_keep_distinct_type_aliases() {
    let file = parsed("<?php namespace App\\Sub {use Left as App; class One {function load(){}}} namespace App\\Sub {use Right as App; class Two {function load(){}}}");
    for (owner, expected) in [("One", "\\Left\\Foo"), ("Two", "\\Right\\Foo")] {
        let symbol = file
            .symbols
            .iter()
            .find(|symbol| {
                symbol.name == "load"
                    && symbol.parent_fqn.as_deref() == Some(format!("App\\Sub\\{owner}").as_str())
            })
            .unwrap();
        assert_eq!(
            resolve_type_name_relative_to_symbol("App\\Foo", symbol, &file),
            expected
        );
    }
}

#[test]
fn composite_type_leaves_share_qualification_without_changing_shape_keys() {
    let file = parsed("<?php namespace App\\Sub; class Service {function load(){}}");
    let symbol = file
        .symbols
        .iter()
        .find(|symbol| symbol.name == "load")
        .unwrap();
    let doc = parse_phpdoc("/** @return array{App: App\\Foo, callback: callable(App\\Foo): ?App\\Foo, type: class-string<App\\Foo>, choices: (App\\Foo&Other\\Bar)|null} */");
    let actual =
        resolve_type_info_relative_to_symbol(doc.return_type.as_ref().unwrap(), symbol, &file)
            .to_string();
    assert!(actual.contains("App: \\App\\Sub\\App\\Foo"), "{actual}");
    assert!(
        actual.contains("callable(\\App\\Sub\\App\\Foo): ?\\App\\Sub\\App\\Foo"),
        "{actual}"
    );
    assert!(
        actual.contains("class-string<\\App\\Sub\\App\\Foo>"),
        "{actual}"
    );
    assert!(actual.contains("\\App\\Sub\\Other\\Bar"), "{actual}");
}

#[test]
fn global_namespace_relative_types_do_not_gain_namespace_keyword() {
    let source = "<?php function load() {}";
    assert_eq!(resolved(source, "namespace\\App\\Foo"), "\\App\\Foo");
    assert_eq!(resolved(source, "App\\Foo"), "\\App\\Foo");
    assert_eq!(resolved(source, "Foo"), "Foo");
}

#[test]
fn cached_instance_reference_metadata_uses_the_relative_return_type() {
    let source = "<?php namespace App {class Foo {function walk(){}}} namespace App\\Sub\\App {class Foo {function walk(){}}} namespace App\\Sub {class Service {function model(): App\\Foo {return new App\\Foo();} function run() {$model=$this->model(); $model->walk();}}}";
    let mut parser = crate::parser::FileParser::new();
    parser.parse_full(source);
    let tree = parser.tree().unwrap();
    let file = crate::symbols::extract_file_symbols(tree, source, "file:///qualified.php");
    let references = crate::references::collect_symbol_references_in_file(tree, source, &file);
    assert!(
        references.iter().any(|reference| !reference.is_declaration
            && reference.target_fqn == "App\\Sub\\App\\Foo::walk"),
        "wrong cached references: {references:?}"
    );
    assert!(
        !references
            .iter()
            .any(|reference| !reference.is_declaration && reference.target_fqn == "App\\Foo::walk"),
        "absolute decoy cached: {references:?}"
    );
}

#[test]
fn doctrine_target_entity_type_retains_its_resolved_absolute_identity() {
    let file = parsed("<?php namespace Lib {class Model {}} namespace App\\Sub {use Doctrine\\Common\\Collections\\Collection; use Doctrine\\ORM\\Mapping as ORM; class Service {#[ORM\\OneToMany(targetEntity: \\Lib\\Model::class)] public Collection $items;}}");
    let property = file
        .symbols
        .iter()
        .find(|symbol| symbol.name == "items")
        .unwrap();
    let ty = property
        .signature
        .as_ref()
        .unwrap()
        .return_type
        .as_ref()
        .unwrap();
    assert!(
        ty.to_string().contains("\\Lib\\Model"),
        "unmarked target entity: {ty}"
    );
}

#[test]
fn template_binding_class_arguments_keep_their_resolved_namespace_identity() {
    let file = parsed("<?php namespace App\\Database; use Vendor\\Builder; /** @extends Builder<\\Lib\\Model> */ class UserBuilder extends Builder {}");
    let builder = file
        .symbols
        .iter()
        .find(|symbol| symbol.name == "UserBuilder")
        .unwrap();
    assert_eq!(
        builder.template_bindings[0].args[0],
        TypeInfo::Simple("\\Lib\\Model".into())
    );
}
