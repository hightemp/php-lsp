use super::*;

fn index(source: &str) -> WorkspaceIndex {
    let index = WorkspaceIndex::new();
    let mut parser = FileParser::new();
    parser.parse_full(source);
    assert!(!parser.tree().unwrap().root_node().has_error());
    index.update_file(
        "file:///qualified.php",
        extract_file_symbols(parser.tree().unwrap(), source, "file:///qualified.php"),
    );
    index
}

#[test]
fn indexed_absolute_decoy_cannot_capture_a_relative_qualified_type() {
    let index = index("<?php namespace App {class Foo {}} namespace App\\Sub\\App {class Foo {}} namespace App\\Sub {class Service {}}");
    assert_eq!(
        simple_type_fqn_from_owner_or_index(
            &index,
            "App\\Sub\\Service",
            "file:///qualified.php",
            "App\\Foo"
        )
        .as_deref(),
        Some("App\\Sub\\App\\Foo")
    );
}

#[test]
fn missing_relative_type_never_falls_back_to_indexed_absolute_decoy() {
    let index = index("<?php namespace App {class Foo {}} namespace App\\Sub {class Service {}}");
    assert_eq!(
        simple_type_fqn_from_owner_or_index(
            &index,
            "App\\Sub\\Service",
            "file:///qualified.php",
            "App\\Foo"
        )
        .as_deref(),
        Some("App\\Sub\\App\\Foo")
    );
}

#[test]
fn namespace_root_matching_is_relative_even_without_indexed_decoys() {
    let index = index("<?php namespace App\\Sub; class Service {}");
    assert_eq!(
        simple_type_fqn_from_owner_or_index(
            &index,
            "App\\Sub\\Service",
            "file:///qualified.php",
            "App\\Foo"
        )
        .as_deref(),
        Some("App\\Sub\\App\\Foo")
    );
}

#[test]
fn absolute_namespace_relative_and_aliased_names_remain_distinct() {
    let index = index("<?php namespace App\\Sub; use Vendor as App; class Service {}");
    for (name, expected) in [
        ("\\App\\Foo", "App\\Foo"),
        ("namespace\\App\\Foo", "App\\Sub\\App\\Foo"),
        ("aPP\\Foo", "Vendor\\Foo"),
    ] {
        assert_eq!(
            simple_type_fqn_from_owner_or_index(
                &index,
                "App\\Sub\\Service",
                "file:///qualified.php",
                name
            )
            .as_deref(),
            Some(expected),
            "{name}"
        );
    }
}

#[test]
fn owner_scope_survives_repeated_namespaces_and_import_kinds() {
    let index = index("<?php namespace App\\Sub {use Left as App; class One {}} namespace App\\Sub {use Right as App; use function Wrong\\f as Other; class Two {}}");
    for (owner, name, expected) in [
        ("One", "App\\Foo", "Left\\Foo"),
        ("Two", "App\\Foo", "Right\\Foo"),
        ("Two", "Other\\Foo", "App\\Sub\\Other\\Foo"),
    ] {
        assert_eq!(
            simple_type_fqn_from_owner_or_index(
                &index,
                &format!("App\\Sub\\{owner}"),
                "file:///qualified.php",
                name
            )
            .as_deref(),
            Some(expected)
        );
    }
}

#[test]
fn empty_owner_uses_file_namespace_for_relative_type_names() {
    let index = index("<?php namespace App\\Sub; function run() {}");
    assert_eq!(
        simple_type_fqn_from_owner_or_index(&index, "", "file:///qualified.php", "App\\Foo")
            .as_deref(),
        Some("App\\Sub\\App\\Foo")
    );
}

#[test]
fn resolved_phpdoc_type_leaves_keep_an_absolute_marker_when_reused() {
    let file = php_lsp_types::FileSymbols {
        namespace: Some("App\\Sub".into()),
        ..Default::default()
    };
    let ty = php_lsp_types::TypeInfo::Simple("App\\Foo".into());
    let resolved = resolve_call_site_type_names(&ty, &file);
    assert_eq!(resolved.to_string(), "\\App\\Sub\\App\\Foo");
    assert_eq!(resolve_call_site_type_names(&resolved, &file), resolved);
}

#[test]
fn class_string_argument_types_keep_absolute_fqns_for_template_binding() {
    let index = index("<?php namespace Lib {class Model {}} namespace App\\Sub {class Service {}}");
    let file = php_lsp_types::FileSymbols {
        namespace: Some("App\\Sub".into()),
        ..Default::default()
    };
    for raw in [r"\Lib\Model::class", r"'\Lib\Model'"] {
        let ty = call_site_argument_type_from_text(raw, &file, &index).unwrap();
        assert_eq!(ty.to_string(), "class-string<\\Lib\\Model>", "{raw}");
    }
}

#[test]
fn synthesized_doctrine_entity_types_remain_absolute_in_another_owner_scope() {
    let index = index("<?php namespace Lib {class Model {}} namespace Lib\\Lib {class Model {}} namespace App\\Sub {class Service {}}");
    for method in ["find", "findAll"] {
        let ty = doctrine_standard_repository_method_return_type(method, "Lib\\Model").unwrap();
        let resolved = type_info_resolved_text_from_index(
            &index,
            "App\\Sub\\Service",
            "file:///qualified.php",
            &ty,
        )
        .unwrap();
        assert!(
            resolved.contains("\\Lib\\Model"),
            "lost absolute type: {resolved}"
        );
        assert!(
            !resolved.contains("App\\Sub\\Lib"),
            "requalified type: {resolved}"
        );
    }
}

#[test]
fn synthesized_laravel_relation_and_model_types_keep_resolved_identity() {
    let index = index("<?php namespace Illuminate\\Database\\Eloquent {class Model {} class Builder {}} namespace Illuminate\\Database\\Eloquent\\Relations {class Relation {} class HasMany extends Relation {}} namespace Lib {class Model extends \\Illuminate\\Database\\Eloquent\\Model {}} namespace Lib\\Lib {class Model {}}");
    let relation = "Illuminate\\Database\\Eloquent\\Relations\\HasMany<Lib\\Model>";
    for owner in [relation, "Lib\\Model"] {
        for method in ["find", "create"] {
            let ty =
                framework_virtual_member_type_fqn(&index, owner, method, None, None, None).unwrap();
            assert_eq!(ty, "\\Lib\\Model", "{owner}::{method}");
        }
        let ty = framework_virtual_member_type_fqn(&index, owner, "whereName", None, None, None)
            .unwrap();
        assert!(
            ty.starts_with("\\Illuminate\\"),
            "requalified fluent type: {ty}"
        );
        assert!(
            ty.contains("<\\Lib\\Model>"),
            "requalified model argument: {ty}"
        );
    }
}

#[test]
fn owner_empty_wrappers_resolve_file_scope_before_indexed_global_names() {
    let index = index("<?php namespace App\\Sub; class Foo {} function run() {}");
    let source = "<?php namespace {class Foo {}} namespace App {class Foo {}}";
    let mut parser = FileParser::new();
    parser.parse_full(source);
    index.update_file(
        "file:///decoys.php",
        extract_file_symbols(parser.tree().unwrap(), source, "file:///decoys.php"),
    );
    for (name, expected) in [
        ("App\\Foo", "App\\Sub\\App\\Foo"),
        ("Foo", "App\\Sub\\Foo"),
        ("\\Foo", "Foo"),
    ] {
        let ty = php_lsp_types::TypeInfo::Simple(name.into());
        assert_eq!(
            type_info_fqn_from_index(&index, "", "file:///qualified.php", &ty).as_deref(),
            Some(expected),
            "{name}"
        );
    }
}

#[test]
fn unindexed_resolved_global_types_keep_their_absolute_marker() {
    let index = WorkspaceIndex::new();
    let ty = php_lsp_types::TypeInfo::Simple("MissingGlobal".into());
    assert_eq!(
        type_info_resolved_text_from_index(&index, "globalFactory", "file:///missing.php", &ty)
            .as_deref(),
        Some("\\MissingGlobal")
    );
}

#[test]
fn laravel_builtin_cast_classes_cannot_be_captured_by_namespaced_decoys() {
    let source = "<?php namespace Illuminate\\Database\\Eloquent {class Model {}} namespace Carbon {interface CarbonInterface {}} namespace Illuminate\\Support {class Collection {}} namespace App\\Models\\Carbon {interface CarbonInterface {}} namespace App\\Models\\Illuminate\\Support {class Collection {}} namespace App\\Models {class Record extends \\Illuminate\\Database\\Eloquent\\Model {protected $casts=['created_at'=>'datetime','tags'=>'collection'];}}";
    let index = index(source);
    let file = index
        .read()
        .file_symbols()
        .get("file:///qualified.php")
        .unwrap()
        .value()
        .clone();
    for (property, expected) in [
        ("$created_at", "\\Carbon\\CarbonInterface"),
        ("$tags", "\\Illuminate\\Support\\Collection"),
    ] {
        let ty = framework_virtual_member_type_fqn(
            &index,
            "App\\Models\\Record",
            property,
            Some("file:///qualified.php"),
            Some(&file),
            Some(source),
        )
        .unwrap();
        assert_eq!(ty, expected, "{property}");
    }
}

#[test]
fn laravel_accessor_declarations_distinguish_absolute_and_relative_class_names() {
    let source = "<?php namespace Illuminate\\Database\\Eloquent {class Model {}} namespace Lib {class Model {}} namespace App\\Sub\\Lib {class Model {}} namespace App\\Sub {class Record extends \\Illuminate\\Database\\Eloquent\\Model {function getAbsoluteAttribute(): \\Lib\\Model {return new \\Lib\\Model();} function getRelativeAttribute(): Lib\\Model {return new Lib\\Model();}}}";
    let index = index(source);
    for (name, expected) in [
        ("$absolute", "\\Lib\\Model"),
        ("$relative", "\\App\\Sub\\Lib\\Model"),
    ] {
        assert_eq!(
            framework_virtual_member_type_fqn(&index, "App\\Sub\\Record", name, None, None, None)
                .as_deref(),
            Some(expected),
            "{name}"
        );
    }
}

#[test]
fn laravel_relation_signature_qualified_names_use_the_declaration_namespace() {
    let source="<?php namespace Illuminate\\Database\\Eloquent {class Model {}} namespace Illuminate\\Database\\Eloquent\\Relations {class Relation {} class HasMany extends Relation {}} namespace Lib {class Model {}} namespace App\\Sub\\Relations {class HasMany extends \\Illuminate\\Database\\Eloquent\\Relations\\HasMany {}} namespace App\\Sub {class Record extends \\Illuminate\\Database\\Eloquent\\Model {/** @return Relations\\HasMany<\\Lib\\Model> */ function children(): Relations\\HasMany {return new Relations\\HasMany();}}}";
    let index = index(source);
    let ty = framework_virtual_member_type_fqn(
        &index,
        "App\\Sub\\Record",
        "$children",
        None,
        None,
        None,
    )
    .expect("qualified relation property");
    assert!(
        ty.contains("<int, \\Lib\\Model>"),
        "wrong related model: {ty}"
    );
}

#[test]
fn inherited_return_types_use_declaring_namespace_before_receiver_namespace() {
    let index = index("<?php namespace App\\Sub {class Base {function load(): App\\Foo {return new App\\Foo();}}} namespace Other {class Child extends \\App\\Sub\\Base {}}");
    let method = index.resolve_fqn("App\\Sub\\Base::load").unwrap();
    assert_eq!(
        symbol_return_type_text_from_index(&index, "Other\\Child", &method).as_deref(),
        Some("\\App\\Sub\\App\\Foo")
    );
}

#[test]
fn hover_class_links_hide_internal_absolute_markers_without_changing_targets() {
    let index = index("<?php namespace App; class Foo {}");
    let file = php_lsp_types::FileSymbols {
        namespace: Some("Other".into()),
        ..Default::default()
    };
    let ty = php_lsp_types::TypeInfo::Generic {
        base: "array".into(),
        args: vec![php_lsp_types::TypeInfo::Simple("\\App\\Foo".into())],
    };
    let markdown = resolved_type_info_raw_with_links(
        &index,
        &file,
        "Other\\Child",
        "file:///qualified.php",
        &ty,
    );
    let source_markdown =
        type_info_raw_with_links(&index, &file, "Other\\Child", "file:///qualified.php", &ty);
    assert!(
        source_markdown.contains("`array<\\App\\Foo>`"),
        "source spelling lost: {source_markdown}"
    );
    assert!(
        markdown.contains("`array<App\\Foo>`"),
        "internal marker in display: {markdown}"
    );
    assert!(
        markdown.contains("[`App\\Foo`](<file:///qualified.php#L1>)"),
        "wrong target or label: {markdown}"
    );
}

#[test]
fn computed_shape_inlay_text_hides_nested_absolute_markers() {
    let ty = php_lsp_types::TypeInfo::ArrayShape(vec![php_lsp_types::ArrayShapeItem {
        key: Some("item".into()),
        optional: false,
        value: php_lsp_types::TypeInfo::Simple("\\App\\Model".into()),
    }]);
    let index = WorkspaceIndex::new();
    assert_eq!(
        local_variable_type_info_display(
            &index,
            "Other\\Owner",
            "file:///none.php",
            &ty,
            &php_lsp_types::FileSymbols::default()
        ),
        "array{item: App\\Model}"
    );
    assert_eq!(ty.to_string(), "array{item: \\App\\Model}");
}
