use super::*;
use php_lsp_parser::symbols::extract_file_symbols;

fn index_source(source: &str) -> WorkspaceIndex {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    assert!(!parser.tree().unwrap().root_node().has_error());
    let index = WorkspaceIndex::new();
    index.update_file(
        "file:///framework-types.php",
        extract_file_symbols(
            parser.tree().unwrap(),
            source,
            "file:///framework-types.php",
        ),
    );
    index
}

fn allows_null(ty: &TypeInfo) -> bool {
    match ty {
        TypeInfo::Nullable(_) | TypeInfo::LiteralNull => true,
        TypeInfo::Simple(name) => name.eq_ignore_ascii_case("null"),
        TypeInfo::Union(types) => types.iter().any(allows_null),
        _ => false,
    }
}

#[test]
fn p232_model_terminals_preserve_nullability_and_non_null_controls() {
    let index = index_source(
        "<?php namespace App; class User extends \\Illuminate\\Database\\Eloquent\\Model {}",
    );
    let ctx = FrameworkProviderContext::new(&index);
    for method in ["first", "FIRST", "find", "FiNd", "firstWhere"] {
        let ty = laravel_model_dynamic_method_return_type(&ctx, "App\\User", method).unwrap();
        assert!(allows_null(&ty), "{method} falsely non-null: {ty}");
    }
    for method in [
        "firstOrFail",
        "FIRSTORFAIL",
        "findOrFail",
        "firstOrNew",
        "firstOrCreate",
        "create",
    ] {
        let ty = laravel_model_dynamic_method_return_type(&ctx, "App\\User", method).unwrap();
        assert!(!allows_null(&ty), "{method} falsely nullable: {ty}");
    }
}

#[test]
fn p232_relation_terminals_preserve_nullability() {
    let index = WorkspaceIndex::new();
    let ctx = FrameworkProviderContext::new(&index);
    let owner = "Illuminate\\Database\\Eloquent\\Relations\\HasMany<App\\User,App\\Team>";
    for method in ["first", "FIRST", "find", "firstWhere"] {
        let ty = laravel_relation_dynamic_method_return_type(&ctx, owner, method).unwrap();
        assert!(allows_null(&ty), "{method} falsely non-null: {ty}");
    }
    for method in ["firstOrFail", "findOrFail", "create"] {
        assert!(!allows_null(
            &laravel_relation_dynamic_method_return_type(&ctx, owner, method).unwrap()
        ));
    }
}

#[test]
fn p232_foreign_relation_leaf_names_never_gain_framework_members() {
    let index =
        index_source("<?php namespace App; class Relation {} class HasMany {} class BelongsTo {}");
    let ctx = FrameworkProviderContext::new(&index);
    let registry = default_framework_provider_registry();
    for owner in [
        "App\\Relation",
        "App\\HasMany",
        "App\\BelongsTo",
        "App\\HasMany<App\\User,App\\Team>",
    ] {
        assert!(
            !is_laravel_relation_fqn(&ctx, owner),
            "foreign relation {owner}"
        );
        assert!(laravel_relation_base_fqn(&ctx, owner).is_none());
        assert!(
            registry
                .virtual_members(
                    &ctx,
                    &VirtualMemberQuery {
                        owner_fqn: owner.into(),
                        member_name: "first".into(),
                        kind: VirtualMemberKind::Method
                    }
                )
                .is_empty(),
            "foreign virtual type {owner}"
        );
    }
}

#[test]
fn p232_canonical_relation_fqns_and_proven_subclasses_keep_support_without_stubs() {
    let index=index_source("<?php namespace App; class Custom extends \\Illuminate\\Database\\Eloquent\\Relations\\HasMany {}");
    let ctx = FrameworkProviderContext::new(&index);
    for owner in [
        "Illuminate\\Database\\Eloquent\\Relations\\HasMany",
        "illuminate\\database\\eloquent\\relations\\hasmany",
        "App\\Custom",
    ] {
        assert!(
            is_laravel_relation_fqn(&ctx, owner),
            "lost relation {owner}"
        );
        assert!(
            is_laravel_collection_relation_fqn(&ctx, owner),
            "lost collection {owner}"
        );
    }
}

#[test]
fn p232_builder_terminals_do_not_claim_to_return_the_builder() {
    let index=index_source("<?php namespace App; class User extends \\Illuminate\\Database\\Eloquent\\Model {} /** @extends \\Illuminate\\Database\\Eloquent\\Builder<User> */ class UserBuilder extends \\Illuminate\\Database\\Eloquent\\Builder {}");
    let ctx = FrameworkProviderContext::new(&index);
    let registry = default_framework_provider_registry();
    for (method, expected) in [
        ("count", "int"),
        ("first", "?\\App\\User"),
        ("firstOrFail", "\\App\\User"),
    ] {
        let members = registry.virtual_members(
            &ctx,
            &VirtualMemberQuery {
                owner_fqn: "App\\UserBuilder".into(),
                member_name: method.into(),
                kind: VirtualMemberKind::Method,
            },
        );
        assert_eq!(
            members
                .first()
                .and_then(|member| member.type_info.as_ref())
                .map(ToString::to_string)
                .as_deref(),
            Some(expected),
            "wrong builder terminal {method}: {members:?}"
        );
    }
}

#[test]
fn p232_unrecognized_builder_terminals_stay_unknown() {
    let index = WorkspaceIndex::new();
    let ctx = FrameworkProviderContext::new(&index);
    let registry = default_framework_provider_registry();
    let members = registry.virtual_members(
        &ctx,
        &VirtualMemberQuery {
            owner_fqn: "Illuminate\\Database\\Query\\Builder".into(),
            member_name: "paginate".into(),
            kind: VirtualMemberKind::Method,
        },
    );
    assert!(
        members
            .first()
            .is_none_or(|member| member.type_info.is_none()),
        "fabricated builder paginator: {members:?}"
    );
}

#[test]
fn p232_argument_free_finders_include_every_possible_result_branch() {
    let index = index_source(
        "<?php namespace App; class User extends \\Illuminate\\Database\\Eloquent\\Model {}",
    );
    let ctx = FrameworkProviderContext::new(&index);
    for method in ["find", "findOrFail"] {
        let ty = laravel_model_dynamic_method_return_type(&ctx, "App\\User", method).unwrap();
        let TypeInfo::Union(branches) = ty else {
            panic!("finder lost collection branch: {ty}");
        };
        assert_eq!(branches.len(), 2);
        assert_eq!(branches[1], laravel_eloquent_collection_type("App\\User"));
        assert_eq!(
            branches[0],
            if method == "find" {
                TypeInfo::Nullable(Box::new(TypeInfo::Simple("\\App\\User".into())))
            } else {
                TypeInfo::Simple("\\App\\User".into())
            }
        );
    }
}

#[test]
fn p232_declared_unknown_methods_are_not_replaced_by_framework_rules() {
    let index=index_source("<?php namespace App; class User extends \\Illuminate\\Database\\Eloquent\\Model {function first(){}} class OwnRelation extends \\Illuminate\\Database\\Eloquent\\Relations\\HasMany {function first(){}}");
    let ctx = FrameworkProviderContext::new(&index);
    let registry = default_framework_provider_registry();
    for owner in ["App\\User", "App\\OwnRelation<App\\User>"] {
        let members = registry.virtual_members(
            &ctx,
            &VirtualMemberQuery {
                owner_fqn: owner.into(),
                member_name: "first".into(),
                kind: VirtualMemberKind::Method,
            },
        );
        assert!(
            members.is_empty(),
            "declared method was replaced: {members:?}"
        );
    }
}

#[test]
fn p232_unbound_generic_models_do_not_become_class_fqns_or_use_the_parent_slot() {
    let index=index_source("<?php namespace Illuminate\\Database\\Eloquent {/** @template TModel */ class Builder{}} namespace App {class Team extends \\Illuminate\\Database\\Eloquent\\Model{} /**\n * @template TEntity\n * @extends \\Illuminate\\Database\\Eloquent\\Builder<TEntity>\n */ class OwnBuilder extends \\Illuminate\\Database\\Eloquent\\Builder{} /**\n * @template TEntity\n * @extends \\Illuminate\\Database\\Eloquent\\Relations\\HasMany<TEntity, Team>\n */ class OwnRelation extends \\Illuminate\\Database\\Eloquent\\Relations\\HasMany{}}");
    assert_eq!(
        index
            .get_type("App\\OwnBuilder")
            .unwrap()
            .template_bindings
            .len(),
        1
    );
    assert_eq!(
        index.get_type("App\\OwnRelation").unwrap().templates.len(),
        1
    );
    let ctx = FrameworkProviderContext::new(&index);
    let registry = default_framework_provider_registry();
    for owner in [
        "Illuminate\\Database\\Eloquent\\Builder<TModel>",
        "Illuminate\\Database\\Eloquent\\Builder<TUnknown>",
        "Illuminate\\Database\\Eloquent\\Relations\\HasMany<TRelatedModel,App\\Team>",
        "Illuminate\\Database\\Eloquent\\Relations\\HasMany<TUnknown,App\\Team>",
        "Illuminate\\Database\\Eloquent\\Relations\\HasMany<mixed,App\\Team>",
        "Illuminate\\Database\\Eloquent\\Relations\\HasMany<App\\Team|App\\Other,App\\Team>",
        "App\\OwnBuilder<TEntity>",
        "App\\OwnBuilder",
        "App\\OwnRelation",
    ] {
        assert_eq!(
            framework_terminal_return_type_from_index(&index, owner, "first"),
            None,
            "invented concrete model: {owner}"
        );
        let members = registry.virtual_members(
            &ctx,
            &VirtualMemberQuery {
                owner_fqn: owner.into(),
                member_name: "first".into(),
                kind: VirtualMemberKind::Method,
            },
        );
        assert!(
            members.iter().all(|member| member.type_info.is_none()),
            "virtual invented model: {owner}: {members:?}"
        );
    }
}

#[test]
fn p232_relation_properties_do_not_invent_models_from_templates_or_second_arguments() {
    for argument in ["TEntity", "mixed", "\\App\\Team|\\App\\Other"] {
        let source = format!("<?php namespace App; class Team extends \\Illuminate\\Database\\Eloquent\\Model{{}} /** @template TEntity */ class Owner extends \\Illuminate\\Database\\Eloquent\\Model{{ /** @return \\Illuminate\\Database\\Eloquent\\Relations\\HasMany<{argument}, Owner> */ function children(){{}} }}");
        let index = index_source(&source);
        let ctx = FrameworkProviderContext::new(&index);
        let members = default_framework_provider_registry().virtual_members(
            &ctx,
            &VirtualMemberQuery {
                owner_fqn: "App\\Owner".into(),
                member_name: "children".into(),
                kind: VirtualMemberKind::Property,
            },
        );
        assert!(
            members.iter().all(|member| member.type_info.is_none()),
            "invented relation property from {argument}: {members:?}"
        );
    }
}

#[test]
fn p232_custom_generic_parameter_order_is_not_assumed_to_match_laravel() {
    let index = index_source(
        r"<?php namespace App;
class Team extends \Illuminate\Database\Eloquent\Model {}
class User extends \Illuminate\Database\Eloquent\Model {}
/**
 * @template TUnused
 * @template TEntity
 * @extends \Illuminate\Database\Eloquent\Builder<TEntity>
 */
class SwappedBuilder extends \Illuminate\Database\Eloquent\Builder {}
/** @extends SwappedBuilder<Team, User> */
class ConcreteBuilder extends SwappedBuilder {}
/**
 * @template TUnused
 * @template TEntity
 * @extends \Illuminate\Database\Eloquent\Relations\HasMany<TEntity, Team>
 */
class SwappedRelation extends \Illuminate\Database\Eloquent\Relations\HasMany {}
/** @extends SwappedRelation<Team, User> */
class ConcreteRelation extends SwappedRelation {}
",
    );
    let ctx = FrameworkProviderContext::new(&index);
    for owner in [
        "App\\SwappedBuilder<App\\Team,App\\User>",
        "App\\SwappedRelation<App\\Team,App\\User>",
        "App\\ConcreteBuilder",
        "App\\ConcreteRelation",
    ] {
        assert_eq!(
            framework_terminal_return_type_from_index(&index, owner, "first"),
            None,
            "unproven generic slot: {owner}"
        );
        let members = default_framework_provider_registry().virtual_members(
            &ctx,
            &VirtualMemberQuery {
                owner_fqn: owner.into(),
                member_name: "first".into(),
                kind: VirtualMemberKind::Method,
            },
        );
        assert!(
            members.iter().all(|member| member.type_info.is_none()),
            "virtual generic slot: {owner}: {members:?}"
        );
    }
}

#[test]
fn p232_reusable_builder_does_not_pick_the_first_model_in_the_index() {
    let index = index_source(
        r"<?php namespace App;
class SharedBuilder extends \Illuminate\Database\Eloquent\Builder {}
class User extends \Illuminate\Database\Eloquent\Model {function newEloquentBuilder(): SharedBuilder {}}
class Team extends \Illuminate\Database\Eloquent\Model {function newEloquentBuilder(): SharedBuilder {}}
",
    );
    let ctx = FrameworkProviderContext::new(&index);
    assert_eq!(
        framework_terminal_return_type_from_index(&index, "App\\SharedBuilder", "first"),
        None
    );
    let members = default_framework_provider_registry().virtual_members(
        &ctx,
        &VirtualMemberQuery {
            owner_fqn: "App\\SharedBuilder".into(),
            member_name: "first".into(),
            kind: VirtualMemberKind::Method,
        },
    );
    assert!(
        members.iter().all(|member| member.type_info.is_none()),
        "unbound shared builder selected a model: {members:?}"
    );
}

#[test]
fn p232_concrete_generic_models_preserve_the_related_slot() {
    let index=index_source("<?php namespace {class GlobalModel extends \\Illuminate\\Database\\Eloquent\\Model{}} namespace App {class User extends \\Illuminate\\Database\\Eloquent\\Model{} class Team extends \\Illuminate\\Database\\Eloquent\\Model{}}");
    for (owner, expected) in [
        (
            "Illuminate\\Database\\Eloquent\\Builder<App\\User>",
            "?\\App\\User",
        ),
        (
            "Illuminate\\Database\\Eloquent\\Relations\\HasMany<App\\User,App\\Team>",
            "?\\App\\User",
        ),
        (
            "Illuminate\\Database\\Eloquent\\Builder<GlobalModel>",
            "?\\GlobalModel",
        ),
    ] {
        assert_eq!(
            framework_terminal_return_type_from_index(&index, owner, "first")
                .map(|ty| ty.to_string())
                .as_deref(),
            Some(expected)
        );
    }
}
