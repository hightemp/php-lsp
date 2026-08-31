use super::*;
use php_lsp_parser::parser::FileParser;
use php_lsp_parser::symbols::extract_file_symbols;
use php_lsp_types::{PhpSymbolKind, SymbolInfo};
use std::cell::Cell;
use std::fs;

fn class_symbol(fqn: &str, extends: Vec<&str>) -> SymbolInfo {
    SymbolInfo {
        name: fqn.rsplit('\\').next().unwrap_or(fqn).to_string(),
        fqn: fqn.to_string(),
        kind: PhpSymbolKind::Class,
        uri: "file:///test.php".to_string(),
        range: (0, 0, 0, 0),
        selection_range: (0, 0, 0, 0),
        visibility: php_lsp_types::Visibility::Public,
        modifiers: php_lsp_types::SymbolModifiers::default(),
        attributes: Vec::new(),
        doc_comment: None,
        signature: None,
        parent_fqn: None,
        extends: extends.into_iter().map(str::to_string).collect(),
        implements: Vec::new(),
        traits: Vec::new(),
        templates: Vec::new(),
        template_bindings: Vec::new(),
    }
}

struct StaticMemberProvider {
    id: &'static str,
    priority: u16,
    detail: &'static str,
    calls: Cell<usize>,
}

impl StaticMemberProvider {
    fn new(id: &'static str, priority: u16, detail: &'static str) -> Self {
        Self {
            id,
            priority,
            detail,
            calls: Cell::new(0),
        }
    }
}

impl VirtualMemberProvider for StaticMemberProvider {
    fn id(&self) -> &'static str {
        self.id
    }

    fn priority(&self) -> u16 {
        self.priority
    }

    fn virtual_members(
        &self,
        _ctx: &FrameworkProviderContext<'_>,
        query: &VirtualMemberQuery,
    ) -> Vec<VirtualMember> {
        self.calls.set(self.calls.get() + 1);
        vec![VirtualMember::synthetic(
            self.id(),
            &query.owner_fqn,
            &query.member_name,
            query.kind,
            self.detail,
        )]
    }
}

#[test]
fn provider_registry_orders_and_merges_duplicate_members() {
    let index = WorkspaceIndex::new();
    let ctx = FrameworkProviderContext::new(&index);
    let high = StaticMemberProvider::new("high", 10, "first");
    let low = StaticMemberProvider::new("low", 90, "second");
    let registry = FrameworkProviderRegistry::new(vec![&low, &high]);
    let query = VirtualMemberQuery {
        owner_fqn: "App\\User".to_string(),
        member_name: "whereEmail".to_string(),
        kind: VirtualMemberKind::Method,
    };

    let members = registry.virtual_members(&ctx, &query);

    assert_eq!(members.len(), 1);
    assert_eq!(members[0].detail.as_deref(), Some("first"));
    assert_eq!(members[0].provider_ids, vec!["high", "low"]);
}

#[test]
fn provider_cache_reuses_results_until_context_fingerprint_changes() {
    let tmp = std::env::temp_dir().join(format!("php-lsp-framework-cache-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();
    let watched = tmp.join("routes.php");
    fs::write(&watched, "one").unwrap();

    let index = WorkspaceIndex::new();
    let provider = StaticMemberProvider::new("cached", 10, "cached");
    let registry = FrameworkProviderRegistry::new(vec![&provider]);
    let cache = FrameworkProviderCache::default();
    let query = VirtualMemberQuery {
        owner_fqn: "App\\User".to_string(),
        member_name: "whereEmail".to_string(),
        kind: VirtualMemberKind::Method,
    };
    let relevant_files = vec![watched.clone()];
    let ctx = FrameworkProviderContext::new(&index)
        .with_workspace(Some(tmp.as_path()), None)
        .with_relevant_files(&relevant_files);

    assert!(cache.has_virtual_member(&registry, &ctx, &query));
    assert!(cache.has_virtual_member(&registry, &ctx, &query));
    assert_eq!(provider.calls.get(), 1);
    assert_eq!(cache.virtual_member_cache_len(), 1);

    fs::write(&watched, "two changed").unwrap();
    let changed_ctx = FrameworkProviderContext::new(&index)
        .with_workspace(Some(tmp.as_path()), None)
        .with_relevant_files(&relevant_files);

    assert!(cache.has_virtual_member(&registry, &changed_ctx, &query));
    assert_eq!(provider.calls.get(), 2);
    assert_eq!(cache.virtual_member_cache_len(), 1);

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn default_framework_providers_cover_existing_dynamic_member_patterns() {
    let index = WorkspaceIndex::new();
    let uri = "file:///test.php";
    index.update_file(
        uri,
        FileSymbols {
            symbols: vec![
                class_symbol("Doctrine\\ORM\\EntityRepository", Vec::new()),
                class_symbol(
                    "Symfony\\Bundle\\FrameworkBundle\\Controller\\AbstractController",
                    Vec::new(),
                ),
                class_symbol("Illuminate\\Database\\Eloquent\\Model", Vec::new()),
                class_symbol(
                    "App\\Repository\\UserRepository",
                    vec!["Doctrine\\ORM\\EntityRepository"],
                ),
                class_symbol(
                    "App\\Controller\\DashboardController",
                    vec!["Symfony\\Bundle\\FrameworkBundle\\Controller\\AbstractController"],
                ),
                class_symbol(
                    "App\\Models\\User",
                    vec!["Illuminate\\Database\\Eloquent\\Model"],
                ),
            ],
            ..Default::default()
        },
    );

    let ctx = FrameworkProviderContext::new(&index);
    let registry = default_framework_provider_registry();
    let cache = FrameworkProviderCache::default();

    for query in [
        VirtualMemberQuery {
            owner_fqn: "App\\Repository\\UserRepository".to_string(),
            member_name: "findByEmail".to_string(),
            kind: VirtualMemberKind::Method,
        },
        VirtualMemberQuery {
            owner_fqn: "App\\Controller\\DashboardController".to_string(),
            member_name: "render".to_string(),
            kind: VirtualMemberKind::Method,
        },
        VirtualMemberQuery {
            owner_fqn: "App\\Models\\User".to_string(),
            member_name: "$email".to_string(),
            kind: VirtualMemberKind::Property,
        },
        VirtualMemberQuery {
            owner_fqn: "App\\Models\\User".to_string(),
            member_name: "whereEmail".to_string(),
            kind: VirtualMemberKind::Method,
        },
        VirtualMemberQuery {
            owner_fqn: "App\\Models\\User".to_string(),
            member_name: "firstWhere".to_string(),
            kind: VirtualMemberKind::Method,
        },
    ] {
        assert!(
            cache.has_virtual_member(&registry, &ctx, &query),
            "expected default providers to resolve {:?}",
            query
        );
    }
}

#[test]
fn laravel_model_virtual_properties_cover_static_sources() {
    let uri = "file:///laravel-model.php";
    let source = r#"<?php
namespace Illuminate\Database\Eloquent;
class Model {}

namespace Illuminate\Database\Eloquent\Casts;
class Attribute {}

namespace App\Models;

use Illuminate\Database\Eloquent\Model;
use Illuminate\Database\Eloquent\Casts\Attribute;

/**
 * @property-read string $slug
 */
class User extends Model
{
    protected $fillable = ['name'];
    protected $hidden = ['secret_token'];
    protected $casts = [
        'is_admin' => 'boolean',
        'meta' => 'array',
        'joined_at' => 'datetime',
    ];

    protected function casts(): array
    {
        return ['score' => 'integer'];
    }

    public function getFullNameAttribute(): string
    {
        return '';
    }

    /**
     * @return Attribute<int, int>
     */
    protected function age()
    {
        return Attribute::make(get: fn () => 1);
    }
}
"#;

    let mut parser = FileParser::new();
    parser.parse_full(source);
    let file_symbols = extract_file_symbols(parser.tree().unwrap(), source, uri);
    let index = WorkspaceIndex::new();
    index.update_file(uri, file_symbols.clone());

    let registry = default_framework_provider_registry();
    let ctx = FrameworkProviderContext::new(&index)
        .with_source_uri(Some(uri))
        .with_file(Some(&file_symbols), Some(source));
    let candidates = registry.virtual_member_candidates(
        &ctx,
        "App\\Models\\User",
        Some(VirtualMemberKind::Property),
    );
    let by_name: HashMap<_, _> = candidates
        .iter()
        .map(|property| (property.name.as_str(), property))
        .collect();

    assert_eq!(
        by_name
            .get("slug")
            .and_then(|property| property.type_info.as_ref())
            .map(ToString::to_string)
            .as_deref(),
        Some("string")
    );
    assert_eq!(
        by_name
            .get("is_admin")
            .and_then(|property| property.type_info.as_ref())
            .map(ToString::to_string)
            .as_deref(),
        Some("bool")
    );
    assert_eq!(
        by_name
            .get("meta")
            .and_then(|property| property.type_info.as_ref())
            .map(ToString::to_string)
            .as_deref(),
        Some("array")
    );
    assert_eq!(
        by_name
            .get("score")
            .and_then(|property| property.type_info.as_ref())
            .map(ToString::to_string)
            .as_deref(),
        Some("int")
    );
    assert_eq!(
        by_name
            .get("full_name")
            .and_then(|property| property.type_info.as_ref())
            .map(ToString::to_string)
            .as_deref(),
        Some("string")
    );
    assert_eq!(
        by_name
            .get("age")
            .and_then(|property| property.type_info.as_ref())
            .map(ToString::to_string)
            .as_deref(),
        Some("int")
    );
    assert!(matches!(
        by_name
            .get("name")
            .and_then(|property| property.type_info.as_ref()),
        Some(TypeInfo::Mixed)
    ));
    assert!(matches!(
        by_name
            .get("secret_token")
            .and_then(|property| property.type_info.as_ref()),
        Some(TypeInfo::Mixed)
    ));
    assert!(
        by_name.get("is_admin").is_some_and(|property| property
            .sources
            .iter()
            .any(|source| matches!(source, VirtualMemberSource::SourceRange { .. }))),
        "$casts property should retain a source range"
    );
}

#[test]
fn laravel_model_unknown_property_uses_magic_fallback_for_diagnostics() {
    let index = WorkspaceIndex::new();
    let uri = "file:///magic-model.php";
    index.update_file(
        uri,
        FileSymbols {
            symbols: vec![
                class_symbol("Illuminate\\Database\\Eloquent\\Model", Vec::new()),
                class_symbol(
                    "App\\Models\\User",
                    vec!["Illuminate\\Database\\Eloquent\\Model"],
                ),
            ],
            ..Default::default()
        },
    );

    let ctx = FrameworkProviderContext::new(&index);
    let registry = default_framework_provider_registry();
    let query = VirtualMemberQuery {
        owner_fqn: "App\\Models\\User".to_string(),
        member_name: "$not_declared".to_string(),
        kind: VirtualMemberKind::Property,
    };

    let members = registry.virtual_members(&ctx, &query);

    assert_eq!(members.len(), 1);
    assert_eq!(
        members[0].detail.as_deref(),
        Some("Laravel Eloquent dynamic member")
    );
}

#[test]
fn laravel_relations_expose_count_properties_and_scopes() {
    let uri = "file:///laravel-relations.php";
    let source = r#"<?php
namespace Illuminate\Database\Eloquent;
class Model {}
class Collection {}
/**
 * @template TModel
 */
class Builder {
    public function orderBy(string $column): self {}

    /**
     * @return Collection<int, TModel>
     */
    public function get() {}

    /**
     * @return TModel
     */
    public function findOrFail($id) {}
}

namespace Illuminate\Database\Eloquent\Relations;
class Relation {}
class HasMany extends Relation {}
class BelongsTo extends Relation {}

namespace App\Models;

use Illuminate\Database\Eloquent\Model;
use Illuminate\Database\Eloquent\Relations\BelongsTo;
use Illuminate\Database\Eloquent\Relations\HasMany;

class User extends Model
{
    public function posts(): HasMany
    {
        return $this->hasMany(Post::class);
    }

    public function team(): BelongsTo
    {
        return $this->belongsTo(Team::class);
    }

    public function scopeActive($query): void
    {
    }
}

class Post extends Model
{
    protected $casts = ['title' => 'string'];
}

class Team extends Model {}
"#;

    let mut parser = FileParser::new();
    parser.parse_full(source);
    let file_symbols = extract_file_symbols(parser.tree().unwrap(), source, uri);
    let index = WorkspaceIndex::new();
    index.update_file(uri, file_symbols.clone());

    let registry = default_framework_provider_registry();
    let ctx = FrameworkProviderContext::new(&index)
        .with_source_uri(Some(uri))
        .with_file(Some(&file_symbols), Some(source));
    let candidates = registry.virtual_member_candidates(&ctx, "App\\Models\\User", None);
    let by_name: HashMap<_, _> = candidates
        .iter()
        .map(|member| (member.name.as_str(), member))
        .collect();

    assert_eq!(
        by_name
            .get("posts")
            .and_then(|property| property.type_info.as_ref())
            .map(ToString::to_string)
            .as_deref(),
        Some("Illuminate\\Database\\Eloquent\\Collection<int, App\\Models\\Post>")
    );
    assert_eq!(
        by_name
            .get("team")
            .and_then(|property| property.type_info.as_ref())
            .map(ToString::to_string)
            .as_deref(),
        Some("App\\Models\\Team")
    );
    assert_eq!(
        by_name
            .get("posts_count")
            .and_then(|property| property.type_info.as_ref())
            .map(ToString::to_string)
            .as_deref(),
        Some("int")
    );
    assert_eq!(
        by_name
            .get("team_count")
            .and_then(|property| property.type_info.as_ref())
            .map(ToString::to_string)
            .as_deref(),
        Some("int")
    );
    assert!(
        by_name
            .get("active")
            .is_some_and(|member| member.kind == VirtualMemberKind::Method),
        "local scope should be exposed as active()"
    );

    let active = registry.virtual_members(
        &ctx,
        &VirtualMemberQuery {
            owner_fqn: "App\\Models\\User".to_string(),
            member_name: "active".to_string(),
            kind: VirtualMemberKind::Method,
        },
    );
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].name, "active");

    let relation_owner =
        "Illuminate\\Database\\Eloquent\\Relations\\HasMany<App\\Models\\Post, App\\Models\\User>";

    let order_by = registry.virtual_members(
        &ctx,
        &VirtualMemberQuery {
            owner_fqn: relation_owner.to_string(),
            member_name: "orderBy".to_string(),
            kind: VirtualMemberKind::Method,
        },
    );
    assert_eq!(order_by.len(), 1);
    assert_eq!(
        order_by[0]
            .type_info
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some(relation_owner)
    );

    let find_or_fail = registry.virtual_members(
        &ctx,
        &VirtualMemberQuery {
            owner_fqn: relation_owner.to_string(),
            member_name: "findOrFail".to_string(),
            kind: VirtualMemberKind::Method,
        },
    );
    assert_eq!(find_or_fail.len(), 1);
    assert_eq!(
        find_or_fail[0]
            .type_info
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("App\\Models\\Post")
    );

    let get = registry.virtual_members(
        &ctx,
        &VirtualMemberQuery {
            owner_fqn: relation_owner.to_string(),
            member_name: "get".to_string(),
            kind: VirtualMemberKind::Method,
        },
    );
    assert_eq!(get.len(), 1);
    assert_eq!(
        get[0]
            .type_info
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("Illuminate\\Database\\Eloquent\\Collection<int, App\\Models\\Post>")
    );

    let unknown = registry.virtual_members(
        &ctx,
        &VirtualMemberQuery {
            owner_fqn: relation_owner.to_string(),
            member_name: "notARealMethod".to_string(),
            kind: VirtualMemberKind::Method,
        },
    );
    assert!(
        unknown.is_empty(),
        "indexed builder symbols should not mask unknown relation methods"
    );
}

#[test]
fn laravel_relation_dynamic_forwarding_works_when_builder_symbols_are_lazy() {
    let uri = "file:///laravel-lazy-relation-builder.php";
    let source = r#"<?php
namespace Illuminate\Database\Eloquent\Relations;
class Relation {}
class HasMany extends Relation {}
"#;

    let mut parser = FileParser::new();
    parser.parse_full(source);
    let file_symbols = extract_file_symbols(parser.tree().unwrap(), source, uri);
    let index = WorkspaceIndex::new();
    index.update_file(uri, file_symbols.clone());

    let registry = default_framework_provider_registry();
    let ctx = FrameworkProviderContext::new(&index)
        .with_source_uri(Some(uri))
        .with_file(Some(&file_symbols), Some(source));

    let members = registry.virtual_members(
        &ctx,
        &VirtualMemberQuery {
            owner_fqn: "Illuminate\\Database\\Eloquent\\Relations\\HasMany".to_string(),
            member_name: "orderBy".to_string(),
            kind: VirtualMemberKind::Method,
        },
    );

    assert_eq!(members.len(), 1);
    assert_eq!(
        members[0]
            .type_info
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("Illuminate\\Database\\Eloquent\\Relations\\HasMany")
    );

    let collection_method = registry.virtual_members(
        &ctx,
        &VirtualMemberQuery {
            owner_fqn: "Illuminate\\Database\\Eloquent\\Relations\\HasMany".to_string(),
            member_name: "sortByCollator".to_string(),
            kind: VirtualMemberKind::Method,
        },
    );
    assert!(
        collection_method.is_empty(),
        "collection macros should not be accepted directly on relations"
    );
}

#[test]
fn laravel_relation_get_returns_related_collection_when_indexed_as_fluent() {
    let uri = "file:///laravel-relation-get.php";
    let source = r#"<?php
namespace Illuminate\Database\Eloquent;
class Collection {}
class Builder {
    public function get(): self {}
}

namespace Illuminate\Database\Eloquent\Relations;
class Relation {}
class HasMany extends Relation {}
"#;

    let mut parser = FileParser::new();
    parser.parse_full(source);
    let file_symbols = extract_file_symbols(parser.tree().unwrap(), source, uri);
    let index = WorkspaceIndex::new();
    index.update_file(uri, file_symbols.clone());

    let registry = default_framework_provider_registry();
    let ctx = FrameworkProviderContext::new(&index)
        .with_source_uri(Some(uri))
        .with_file(Some(&file_symbols), Some(source));
    let members = registry.virtual_members(
            &ctx,
            &VirtualMemberQuery {
                owner_fqn:
                    "Illuminate\\Database\\Eloquent\\Relations\\HasMany<App\\Models\\User, App\\Models\\Vault>"
                        .to_string(),
                member_name: "get".to_string(),
                kind: VirtualMemberKind::Method,
            },
        );

    assert_eq!(members.len(), 1);
    assert_eq!(
        members[0]
            .type_info
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("Illuminate\\Database\\Eloquent\\Collection<int, App\\Models\\User>")
    );
}

#[test]
fn laravel_collection_macros_are_virtual_methods_on_eloquent_collections() {
    let uri = "file:///laravel-collection-macro.php";
    let source = r#"<?php
namespace Illuminate\Support;
class Collection {}

namespace Illuminate\Database\Eloquent;
class Collection extends \Illuminate\Support\Collection {}

namespace App\Providers;

use Illuminate\Support\Collection;

class AppServiceProvider
{
    public function boot(): void
    {
        Collection::macro('sortByCollator', function (callable|string $callback) {
            return $this;
        });
    }
}
"#;

    let mut parser = FileParser::new();
    parser.parse_full(source);
    let file_symbols = extract_file_symbols(parser.tree().unwrap(), source, uri);
    let index = WorkspaceIndex::new();
    index.update_file(uri, file_symbols.clone());

    let registry = default_framework_provider_registry();
    let ctx = FrameworkProviderContext::new(&index)
        .with_source_uri(Some(uri))
        .with_file(Some(&file_symbols), Some(source));
    let members = registry.virtual_members(
        &ctx,
        &VirtualMemberQuery {
            owner_fqn: "Illuminate\\Database\\Eloquent\\Collection<int, App\\Models\\User>"
                .to_string(),
            member_name: "sortByCollator".to_string(),
            kind: VirtualMemberKind::Method,
        },
    );

    assert_eq!(members.len(), 1);
    assert_eq!(
        members[0]
            .type_info
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("Illuminate\\Database\\Eloquent\\Collection<int, App\\Models\\User>")
    );
    assert!(members[0]
        .sources
        .iter()
        .any(|source| matches!(source, VirtualMemberSource::SourceRange { .. })));
}

#[test]
fn laravel_collection_macro_scanner_requires_laravel_collection_import() {
    let uri = "file:///non-laravel-collection-macro.php";
    let source = r#"<?php
namespace App;
class Collection {
    public static function macro(string $name, callable $callback): void {}
}

namespace App\Providers;

use App\Collection;

class AppServiceProvider
{
    public function boot(): void
    {
        Collection::macro('notLaravelMacro', function () {
            return $this;
        });
    }
}
"#;

    let mut parser = FileParser::new();
    parser.parse_full(source);
    let file_symbols = extract_file_symbols(parser.tree().unwrap(), source, uri);
    let index = WorkspaceIndex::new();
    index.update_file(uri, file_symbols.clone());

    let registry = default_framework_provider_registry();
    let ctx = FrameworkProviderContext::new(&index)
        .with_source_uri(Some(uri))
        .with_file(Some(&file_symbols), Some(source));
    let members = registry.virtual_members(
        &ctx,
        &VirtualMemberQuery {
            owner_fqn: "Illuminate\\Database\\Eloquent\\Collection<int, App\\Models\\User>"
                .to_string(),
            member_name: "notLaravelMacro".to_string(),
            kind: VirtualMemberKind::Method,
        },
    );

    assert!(
        members.is_empty(),
        "non-Laravel Collection::macro calls must not become Eloquent collection members"
    );
}

#[test]
fn laravel_collection_macro_scanner_ignores_comments_and_strings() {
    let uri = "file:///laravel-collection-comment-macro.php";
    let source = r#"<?php
namespace Illuminate\Support;
class Collection {}

namespace Illuminate\Database\Eloquent;
class Collection extends \Illuminate\Support\Collection {}

namespace App\Providers;

use Illuminate\Support\Collection;

class AppServiceProvider
{
    public function boot(): void
    {
        // Collection::macro('commentMacro', function () {});
        $text = "Collection::macro('stringMacro', function () {})";
    }
}
"#;

    let mut parser = FileParser::new();
    parser.parse_full(source);
    let file_symbols = extract_file_symbols(parser.tree().unwrap(), source, uri);
    let index = WorkspaceIndex::new();
    index.update_file(uri, file_symbols.clone());

    let registry = default_framework_provider_registry();
    let ctx = FrameworkProviderContext::new(&index)
        .with_source_uri(Some(uri))
        .with_file(Some(&file_symbols), Some(source));

    for member_name in ["commentMacro", "stringMacro"] {
        let members = registry.virtual_members(
            &ctx,
            &VirtualMemberQuery {
                owner_fqn: "Illuminate\\Database\\Eloquent\\Collection<int, App\\Models\\User>"
                    .to_string(),
                member_name: member_name.to_string(),
                kind: VirtualMemberKind::Method,
            },
        );

        assert!(
            members.is_empty(),
            "{member_name} from comment/string must not become a collection macro"
        );
    }
}

#[test]
fn laravel_collection_macro_scanner_rejects_relative_qualified_collection_name() {
    let uri = "file:///laravel-collection-relative-qualified-macro.php";
    let source = r#"<?php
namespace Illuminate\Support;
class Collection {}

namespace Illuminate\Database\Eloquent;
class Collection extends \Illuminate\Support\Collection {}

namespace App\Providers;

Illuminate\Support\Collection::macro('relativeGhost', function () {
    return $this;
});
"#;

    let mut parser = FileParser::new();
    parser.parse_full(source);
    let file_symbols = extract_file_symbols(parser.tree().unwrap(), source, uri);
    let index = WorkspaceIndex::new();
    index.update_file(uri, file_symbols.clone());

    let registry = default_framework_provider_registry();
    let ctx = FrameworkProviderContext::new(&index)
        .with_source_uri(Some(uri))
        .with_file(Some(&file_symbols), Some(source));
    let members = registry.virtual_members(
        &ctx,
        &VirtualMemberQuery {
            owner_fqn: "Illuminate\\Database\\Eloquent\\Collection<int, App\\Models\\User>"
                .to_string(),
            member_name: "relativeGhost".to_string(),
            kind: VirtualMemberKind::Method,
        },
    );

    assert!(
        members.is_empty(),
        "relative qualified Collection names in a namespace must not be treated as Laravel FQNs"
    );
}

#[test]
fn laravel_collection_macro_scanner_accepts_bracketed_global_namespace() {
    let uri = "file:///laravel-collection-bracketed-global-macro.php";
    let source = r#"<?php
namespace Illuminate\Support {
    class Collection {}
}

namespace Illuminate\Database\Eloquent {
    class Collection extends \Illuminate\Support\Collection {}
}

namespace App {
    use Some\Other\Type;

    class Boot {}
}

namespace {
    Illuminate\Support\Collection::macro('globalMacro', function () {
        return $this;
    });
}
"#;

    let mut parser = FileParser::new();
    parser.parse_full(source);
    let file_symbols = extract_file_symbols(parser.tree().unwrap(), source, uri);
    let index = WorkspaceIndex::new();
    index.update_file(uri, file_symbols.clone());

    let registry = default_framework_provider_registry();
    let ctx = FrameworkProviderContext::new(&index)
        .with_source_uri(Some(uri))
        .with_file(Some(&file_symbols), Some(source));
    let members = registry.virtual_members(
        &ctx,
        &VirtualMemberQuery {
            owner_fqn: "Illuminate\\Database\\Eloquent\\Collection<int, App\\Models\\User>"
                .to_string(),
            member_name: "globalMacro".to_string(),
            kind: VirtualMemberKind::Method,
        },
    );

    assert_eq!(members.len(), 1);
}

#[test]
fn laravel_collection_macros_accept_eloquent_collection_import() {
    let uri = "file:///laravel-eloquent-collection-macro.php";
    let source = r#"<?php
namespace Illuminate\Support;
class Collection {}

namespace Illuminate\Database\Eloquent;
class Collection extends \Illuminate\Support\Collection {}

namespace App\Providers;

use Illuminate\Database\Eloquent\Collection;

class AppServiceProvider
{
    public function boot(): void
    {
        Collection::macro('toVaultOptions', function () {
            return $this;
        });
    }
}
"#;

    let mut parser = FileParser::new();
    parser.parse_full(source);
    let file_symbols = extract_file_symbols(parser.tree().unwrap(), source, uri);
    let index = WorkspaceIndex::new();
    index.update_file(uri, file_symbols.clone());

    let registry = default_framework_provider_registry();
    let ctx = FrameworkProviderContext::new(&index)
        .with_source_uri(Some(uri))
        .with_file(Some(&file_symbols), Some(source));
    let members = registry.virtual_members(
        &ctx,
        &VirtualMemberQuery {
            owner_fqn: "Illuminate\\Database\\Eloquent\\Collection<int, App\\Models\\Vault>"
                .to_string(),
            member_name: "toVaultOptions".to_string(),
            kind: VirtualMemberKind::Method,
        },
    );

    assert_eq!(members.len(), 1);
    assert_eq!(
        members[0]
            .type_info
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("Illuminate\\Database\\Eloquent\\Collection<int, App\\Models\\Vault>")
    );
    assert!(members[0]
        .sources
        .iter()
        .any(|source| matches!(source, VirtualMemberSource::SourceRange { .. })));
}

#[test]
fn laravel_registered_macros_are_virtual_methods_on_exact_target() {
    let uri = "file:///laravel-str-macro.php";
    let source = r#"<?php
namespace Illuminate\Support;
class Str {
    public static function macro(string $name, callable $callback): void {}
}

namespace App\Providers;

use Illuminate\Support\Str;

class AppServiceProvider
{
    public function boot(): void
    {
        Str::macro('markdownExternalLink', function (string $text): string {
            return $text;
        });
    }
}
"#;

    let mut parser = FileParser::new();
    parser.parse_full(source);
    let file_symbols = extract_file_symbols(parser.tree().unwrap(), source, uri);
    let index = WorkspaceIndex::new();
    index.update_file(uri, file_symbols.clone());

    let registry = default_framework_provider_registry();
    let ctx = FrameworkProviderContext::new(&index)
        .with_source_uri(Some(uri))
        .with_file(Some(&file_symbols), Some(source));

    let members = registry.virtual_members(
        &ctx,
        &VirtualMemberQuery {
            owner_fqn: "Illuminate\\Support\\Str".to_string(),
            member_name: "markdownExternalLink".to_string(),
            kind: VirtualMemberKind::Method,
        },
    );
    assert_eq!(members.len(), 1);
    assert!(members[0]
        .sources
        .iter()
        .any(|source| matches!(source, VirtualMemberSource::SourceRange { .. })));

    let unrelated = registry.virtual_members(
        &ctx,
        &VirtualMemberQuery {
            owner_fqn: "Illuminate\\Support\\Collection".to_string(),
            member_name: "markdownExternalLink".to_string(),
            kind: VirtualMemberKind::Method,
        },
    );
    assert!(unrelated.is_empty());
}

#[test]
fn laravel_facades_optional_and_faker_expose_dynamic_members() {
    let uri = "file:///laravel-dynamic-members.php";
    let source = r#"<?php
namespace Illuminate\Support\Facades;
class Facade {}
class URL extends Facade {}

namespace Illuminate\Support;
class Optional {}

namespace Faker;
class Generator {}
"#;

    let mut parser = FileParser::new();
    parser.parse_full(source);
    let file_symbols = extract_file_symbols(parser.tree().unwrap(), source, uri);
    let index = WorkspaceIndex::new();
    index.update_file(uri, file_symbols.clone());

    let registry = default_framework_provider_registry();
    let ctx = FrameworkProviderContext::new(&index)
        .with_source_uri(Some(uri))
        .with_file(Some(&file_symbols), Some(source));

    for query in [
        VirtualMemberQuery {
            owner_fqn: "Illuminate\\Support\\Facades\\URL".to_string(),
            member_name: "forceRootUrl".to_string(),
            kind: VirtualMemberKind::Method,
        },
        VirtualMemberQuery {
            owner_fqn: "Illuminate\\Support\\Optional".to_string(),
            member_name: "$id".to_string(),
            kind: VirtualMemberKind::Property,
        },
        VirtualMemberQuery {
            owner_fqn: "Faker\\Generator".to_string(),
            member_name: "$firstName".to_string(),
            kind: VirtualMemberKind::Property,
        },
        VirtualMemberQuery {
            owner_fqn: "Faker\\Generator".to_string(),
            member_name: "sentence".to_string(),
            kind: VirtualMemberKind::Method,
        },
    ] {
        assert!(
            !registry.virtual_members(&ctx, &query).is_empty(),
            "expected dynamic member for {:?}",
            query
        );
    }
}

#[test]
fn laravel_custom_builder_exposes_scopes_and_query_return_type() {
    let uri = "file:///laravel-builder.php";
    let source = r#"<?php
namespace Illuminate\Database\Eloquent;
class Model {}
/**
 * @template TModel
 */
class Builder
{
    /**
     * @return TModel
     */
    public function first() {}
}

namespace App\Database;

use Illuminate\Database\Eloquent\Builder;

/**
 * @extends Builder<\App\Models\User>
 */
class UserBuilder extends Builder {}

namespace App\Models;

use App\Database\UserBuilder;
use Illuminate\Database\Eloquent\Model;

class User extends Model
{
    public function newEloquentBuilder($query): UserBuilder
    {
        return new UserBuilder();
    }

    public function scopeActive($query): void
    {
    }
}
"#;

    let mut parser = FileParser::new();
    parser.parse_full(source);
    let file_symbols = extract_file_symbols(parser.tree().unwrap(), source, uri);
    let index = WorkspaceIndex::new();
    index.update_file(uri, file_symbols.clone());

    let registry = default_framework_provider_registry();
    let ctx = FrameworkProviderContext::new(&index)
        .with_source_uri(Some(uri))
        .with_file(Some(&file_symbols), Some(source));

    let query = registry.virtual_members(
        &ctx,
        &VirtualMemberQuery {
            owner_fqn: "App\\Models\\User".to_string(),
            member_name: "query".to_string(),
            kind: VirtualMemberKind::Method,
        },
    );
    assert_eq!(
        query
            .first()
            .and_then(|member| member.type_info.as_ref())
            .map(ToString::to_string)
            .as_deref(),
        Some("App\\Database\\UserBuilder")
    );

    let builder_scope = registry.virtual_members(
        &ctx,
        &VirtualMemberQuery {
            owner_fqn: "App\\Database\\UserBuilder".to_string(),
            member_name: "active".to_string(),
            kind: VirtualMemberKind::Method,
        },
    );
    assert_eq!(
        builder_scope
            .first()
            .and_then(|member| member.type_info.as_ref())
            .map(ToString::to_string)
            .as_deref(),
        Some("App\\Database\\UserBuilder")
    );

    let first = index
        .resolve_fqn("App\\Database\\UserBuilder::first")
        .expect("generic inherited builder method should resolve");
    assert_eq!(
        first
            .signature
            .as_ref()
            .and_then(|signature| signature.return_type.clone()),
        Some(TypeInfo::Simple("App\\Models\\User".to_string()))
    );
}

#[test]
fn laravel_string_key_provider_scans_static_project_files() {
    let tmp = std::env::temp_dir().join(format!("php-lsp-string-keys-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(tmp.join("config")).unwrap();
    fs::create_dir_all(tmp.join("routes")).unwrap();
    fs::create_dir_all(tmp.join("resources/lang/en")).unwrap();
    fs::create_dir_all(tmp.join("resources/views/users")).unwrap();

    fs::write(
        tmp.join("config/app.php"),
        "<?php\nreturn ['name' => 'Demo', 'mail' => ['from' => ['address' => 'x']]];\n",
    )
    .unwrap();
    fs::write(
        tmp.join("routes/web.php"),
        "<?php\nRoute::get('/dashboard', DashboardController::class)->name('dashboard.home');\n",
    )
    .unwrap();
    fs::write(
        tmp.join("resources/lang/en/messages.php"),
        "<?php\nreturn ['welcome' => ['title' => 'Welcome']];\n",
    )
    .unwrap();
    fs::write(
        tmp.join("resources/views/users/show.blade.php"),
        "<h1>{{ $user->name }}</h1>\n",
    )
    .unwrap();

    let index = WorkspaceIndex::new();
    let ctx = FrameworkProviderContext::new(&index).with_workspace(Some(tmp.as_path()), None);
    let registry = default_framework_provider_registry();

    let config = registry.string_keys(
        &ctx,
        &FrameworkStringKeyQuery {
            domain: "config".to_string(),
            prefix: "app.mail.".to_string(),
        },
    );
    assert!(
        config.iter().any(|key| key.key == "app.mail.from.address"),
        "config tree should expose nested keys: {:?}",
        config
    );
    assert!(
        config.iter().any(|key| key
            .sources
            .iter()
            .any(|source| matches!(source, VirtualMemberSource::SourceRange { .. }))),
        "config keys should retain source ranges"
    );

    let routes = registry.string_keys(
        &ctx,
        &FrameworkStringKeyQuery {
            domain: "route".to_string(),
            prefix: "dashboard.".to_string(),
        },
    );
    assert!(routes.iter().any(|key| key.key == "dashboard.home"));

    let translations = registry.string_keys(
        &ctx,
        &FrameworkStringKeyQuery {
            domain: "translation".to_string(),
            prefix: "messages.welcome.".to_string(),
        },
    );
    assert!(
        translations
            .iter()
            .any(|key| key.key == "messages.welcome.title"),
        "nested translations should be exposed: {:?}",
        translations
    );

    let views = registry.string_keys(
        &ctx,
        &FrameworkStringKeyQuery {
            domain: "view".to_string(),
            prefix: "users.".to_string(),
        },
    );
    assert!(views.iter().any(|key| key.key == "users.show"));

    let unknown = tmp.join("unknown");
    fs::create_dir_all(&unknown).unwrap();
    let unknown_ctx =
        FrameworkProviderContext::new(&index).with_workspace(Some(unknown.as_path()), None);
    assert!(registry
        .string_keys(
            &unknown_ctx,
            &FrameworkStringKeyQuery {
                domain: "view".to_string(),
                prefix: String::new(),
            },
        )
        .is_empty());

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn symfony_string_key_provider_scans_route_attributes() {
    let tmp = std::env::temp_dir().join(format!(
        "php-lsp-symfony-string-keys-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(tmp.join("src/Controller")).unwrap();
    fs::create_dir_all(tmp.join("templates")).unwrap();
    fs::write(
        tmp.join("src/Controller/DebugController.php"),
        r#"<?php
namespace App\Controller;

use Symfony\Component\Routing\Attribute\Route;

class DebugController
{
    #[Route('/debug/email', name: 'app_debug_email', methods: ['GET', 'POST'])]
    public function email(): void {}

    #[\Symfony\Component\Routing\Attribute\Route(
        path: '/debug/logs',
        name: 'app_debug_logs',
        methods: ['GET']
    )]
    public function logs(): void {}
}
"#,
    )
    .unwrap();

    let index = WorkspaceIndex::new();
    let ctx = FrameworkProviderContext::new(&index).with_workspace(Some(tmp.as_path()), None);
    let registry = default_framework_provider_registry();

    let routes = registry.string_keys(
        &ctx,
        &FrameworkStringKeyQuery {
            domain: "route".to_string(),
            prefix: "app_debug_".to_string(),
        },
    );

    assert!(
        routes.iter().any(|key| key.key == "app_debug_email"),
        "Symfony route attributes should expose route names: {:?}",
        routes
    );
    let logs = routes
        .iter()
        .find(|key| key.key == "app_debug_logs")
        .expect("multiline route attribute should be exposed");
    assert!(
        logs.sources
            .iter()
            .any(|source| matches!(source, VirtualMemberSource::SourceRange { .. })),
        "route keys should retain source ranges: {:?}",
        logs
    );

    let _ = fs::remove_dir_all(&tmp);
}

struct StaticStringKeyProvider;

impl VirtualMemberProvider for StaticStringKeyProvider {
    fn id(&self) -> &'static str {
        "string.keys"
    }

    fn virtual_members(
        &self,
        _ctx: &FrameworkProviderContext<'_>,
        _query: &VirtualMemberQuery,
    ) -> Vec<VirtualMember> {
        Vec::new()
    }

    fn string_keys(
        &self,
        _ctx: &FrameworkProviderContext<'_>,
        query: &FrameworkStringKeyQuery,
    ) -> Vec<FrameworkStringKey> {
        vec![FrameworkStringKey {
            key: format!("{}{}", query.prefix, "home"),
            detail: Some(query.domain.clone()),
            provider_ids: vec![self.id()],
            sources: vec![VirtualMemberSource::Synthetic {
                provider_id: self.id(),
                description: "test string key",
            }],
        }]
    }
}

#[test]
fn registry_supports_string_key_provider_contract() {
    let index = WorkspaceIndex::new();
    let ctx = FrameworkProviderContext::new(&index);
    let provider = StaticStringKeyProvider;
    let registry = FrameworkProviderRegistry::new(vec![&provider]);
    let query = FrameworkStringKeyQuery {
        domain: "route".to_string(),
        prefix: "dashboard.".to_string(),
    };

    let keys = registry.string_keys(&ctx, &query);

    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].key, "dashboard.home");
    assert_eq!(keys[0].detail.as_deref(), Some("route"));
}

#[test]
fn query_maps_supported_ref_kinds() {
    assert_eq!(
        VirtualMemberQuery::from_ref_kind("App\\User", "whereEmail", RefKind::MethodCall)
            .unwrap()
            .kind,
        VirtualMemberKind::Method
    );
    assert!(VirtualMemberQuery::from_ref_kind("App\\User", "User", RefKind::ClassName).is_none());
}

#[cfg(unix)]
#[test]
fn framework_string_scan_follows_external_template_symlink_without_cycles() {
    use std::os::unix::fs::symlink;

    let root =
        std::env::temp_dir().join(format!("php-lsp-framework-symlink-{}", std::process::id()));
    let external = std::env::temp_dir().join(format!(
        "php-lsp-framework-symlink-external-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&external);
    fs::create_dir_all(external.join("nested")).expect("create external templates");
    fs::create_dir_all(&root).expect("create workspace");
    fs::write(external.join("nested/page.html.twig"), "{{ value }}")
        .expect("write external Twig template");
    symlink(&external, root.join("templates")).expect("link external templates");
    symlink(&root, external.join("nested/back")).expect("create template cycle");

    let keys = framework_string_keys_for_workspace_with_limits(
        &root,
        "twig",
        TraversalLimits {
            max_files: Some(32),
            max_entries: Some(256),
        },
        &[],
    );
    assert!(keys.iter().any(|key| key.key == "nested/page.html.twig"));
    let excluded = framework_string_keys_for_workspace_with_limits(
        &root,
        "twig",
        TraversalLimits {
            max_files: Some(32),
            max_entries: Some(256),
        },
        &[PathBuf::from("templates")],
    );
    assert!(excluded.is_empty());

    fs::remove_file(external.join("nested/back")).expect("remove cycle link");
    fs::remove_dir_all(root).expect("remove workspace");
    fs::remove_dir_all(external).expect("remove external templates");
}
