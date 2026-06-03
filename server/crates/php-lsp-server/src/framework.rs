//! Framework-aware static providers.
//!
//! Providers in this module are intentionally static: they receive readonly
//! workspace/index context and must not bootstrap applications, open databases,
//! or execute user code.

use crate::util::uri::path_to_uri;
use php_lsp_index::composer::NamespaceMap;
use php_lsp_index::workspace::WorkspaceIndex;
use php_lsp_parser::phpdoc::parse_phpdoc;
use php_lsp_parser::resolve::{resolve_class_name, RefKind};
use php_lsp_types::{
    FileSymbols, PhpDocPropertyAccess, PhpSymbolKind, SymbolInfo, TemplateBindingKind, TypeInfo,
};
use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum VirtualMemberKind {
    Method,
    Property,
    StaticProperty,
    ClassConstant,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct VirtualMemberQuery {
    pub(crate) owner_fqn: String,
    pub(crate) member_name: String,
    pub(crate) kind: VirtualMemberKind,
}

impl VirtualMemberQuery {
    pub(crate) fn from_ref_kind(
        owner_fqn: impl Into<String>,
        member_name: impl Into<String>,
        ref_kind: RefKind,
    ) -> Option<Self> {
        let kind = match ref_kind {
            RefKind::MethodCall => VirtualMemberKind::Method,
            RefKind::PropertyAccess => VirtualMemberKind::Property,
            RefKind::StaticPropertyAccess => VirtualMemberKind::StaticProperty,
            RefKind::ClassConstant => VirtualMemberKind::ClassConstant,
            _ => return None,
        };

        Some(Self {
            owner_fqn: owner_fqn.into(),
            member_name: member_name.into(),
            kind,
        })
    }

    fn cache_key(&self) -> VirtualMemberCacheKey {
        VirtualMemberCacheKey {
            owner_fqn: normalize_fqn(&self.owner_fqn),
            member_name: normalize_member_name(self.kind, &self.member_name),
            kind: self.kind,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum VirtualMemberSource {
    Synthetic {
        provider_id: &'static str,
        description: &'static str,
    },
    SourceRange {
        uri: String,
        range: (u32, u32, u32, u32),
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VirtualMember {
    pub(crate) name: String,
    pub(crate) owner_fqn: String,
    pub(crate) fqn: String,
    pub(crate) kind: VirtualMemberKind,
    pub(crate) type_info: Option<TypeInfo>,
    pub(crate) access: Option<PhpDocPropertyAccess>,
    pub(crate) detail: Option<String>,
    pub(crate) provider_ids: Vec<&'static str>,
    pub(crate) sources: Vec<VirtualMemberSource>,
}

impl VirtualMember {
    pub(crate) fn synthetic(
        provider_id: &'static str,
        owner_fqn: impl Into<String>,
        member_name: impl Into<String>,
        kind: VirtualMemberKind,
        detail: impl Into<String>,
    ) -> Self {
        let owner_fqn = owner_fqn.into();
        let name = member_name.into();
        let fqn = match kind {
            VirtualMemberKind::Property | VirtualMemberKind::StaticProperty => {
                format!("{}::${}", owner_fqn, name.trim_start_matches('$'))
            }
            VirtualMemberKind::Method | VirtualMemberKind::ClassConstant => {
                format!("{}::{}", owner_fqn, name)
            }
        };
        Self {
            fqn,
            name,
            owner_fqn,
            kind,
            type_info: None,
            access: None,
            detail: Some(detail.into()),
            provider_ids: vec![provider_id],
            sources: vec![VirtualMemberSource::Synthetic {
                provider_id,
                description: "static framework provider",
            }],
        }
    }

    fn identity(&self) -> VirtualMemberIdentity {
        VirtualMemberIdentity {
            owner_fqn: normalize_fqn(&self.owner_fqn),
            member_name: normalize_member_name(self.kind, &self.name),
            kind: self.kind,
        }
    }

    fn merge_from(&mut self, other: Self) {
        if self.type_info.is_none() {
            self.type_info = other.type_info;
        }
        if self.access.is_none() {
            self.access = other.access;
        }
        if self.detail.is_none() {
            self.detail = other.detail;
        }
        for provider_id in other.provider_ids {
            if !self.provider_ids.contains(&provider_id) {
                self.provider_ids.push(provider_id);
            }
        }
        for source in other.sources {
            if !self.sources.contains(&source) {
                self.sources.push(source);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct VirtualMemberIdentity {
    owner_fqn: String,
    member_name: String,
    kind: VirtualMemberKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct VirtualMemberCacheKey {
    owner_fqn: String,
    member_name: String,
    kind: VirtualMemberKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct FrameworkStringKeyQuery {
    pub(crate) domain: String,
    pub(crate) prefix: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FrameworkStringKey {
    pub(crate) key: String,
    pub(crate) detail: Option<String>,
    pub(crate) provider_ids: Vec<&'static str>,
    pub(crate) sources: Vec<VirtualMemberSource>,
}

impl FrameworkStringKey {
    fn identity(&self) -> String {
        self.key.clone()
    }

    fn merge_from(&mut self, other: Self) {
        if self.detail.is_none() {
            self.detail = other.detail;
        }
        for provider_id in other.provider_ids {
            if !self.provider_ids.contains(&provider_id) {
                self.provider_ids.push(provider_id);
            }
        }
        for source in other.sources {
            if !self.sources.contains(&source) {
                self.sources.push(source);
            }
        }
    }
}

pub(crate) struct FrameworkProviderContext<'a> {
    pub(crate) workspace_root: Option<&'a Path>,
    pub(crate) namespace_map: Option<&'a NamespaceMap>,
    pub(crate) index: &'a WorkspaceIndex,
    pub(crate) source_uri: Option<&'a str>,
    pub(crate) file_symbols: Option<&'a FileSymbols>,
    pub(crate) source: Option<&'a str>,
    pub(crate) relevant_files: &'a [PathBuf],
}

impl<'a> FrameworkProviderContext<'a> {
    pub(crate) fn new(index: &'a WorkspaceIndex) -> Self {
        Self {
            workspace_root: None,
            namespace_map: None,
            index,
            source_uri: None,
            file_symbols: None,
            source: None,
            relevant_files: &[],
        }
    }

    pub(crate) fn with_workspace(
        mut self,
        workspace_root: Option<&'a Path>,
        namespace_map: Option<&'a NamespaceMap>,
    ) -> Self {
        self.workspace_root = workspace_root;
        self.namespace_map = namespace_map;
        self
    }

    pub(crate) fn with_file(
        mut self,
        file_symbols: Option<&'a FileSymbols>,
        source: Option<&'a str>,
    ) -> Self {
        self.file_symbols = file_symbols;
        self.source = source;
        self
    }

    pub(crate) fn with_source_uri(mut self, source_uri: Option<&'a str>) -> Self {
        self.source_uri = source_uri;
        self
    }

    pub(crate) fn with_relevant_files(mut self, relevant_files: &'a [PathBuf]) -> Self {
        self.relevant_files = relevant_files;
        self
    }

    fn fingerprint(&self) -> FrameworkProviderFingerprint {
        FrameworkProviderFingerprint {
            workspace_hash: hash_workspace_root(self.workspace_root),
            composer_hash: self
                .namespace_map
                .map(hash_namespace_map)
                .unwrap_or_default(),
            source_hash: self.source.map(hash_source).unwrap_or_default(),
            relevant_files_hash: hash_relevant_files(self.relevant_files),
        }
    }

    fn class_is_or_extends(&self, class_fqn: &str, target_class: &str) -> bool {
        fqn_matches(class_fqn, target_class)
            || self.class_extends_or_implements(class_fqn, target_class, &mut Vec::new())
    }

    fn class_extends_or_implements(
        &self,
        current_class: &str,
        target_class: &str,
        visited: &mut Vec<String>,
    ) -> bool {
        let current_class = current_class.trim_start_matches('\\');
        let target_class = target_class.trim_start_matches('\\');
        if visited
            .iter()
            .any(|visited| fqn_matches(visited, current_class))
        {
            return false;
        }
        visited.push(current_class.to_string());

        let Some(class_sym) = self
            .index
            .types
            .get(current_class)
            .map(|entry| entry.value().clone())
        else {
            return false;
        };

        class_sym
            .extends
            .iter()
            .chain(class_sym.implements.iter())
            .any(|parent| {
                fqn_matches(parent, target_class)
                    || self.class_extends_or_implements(parent, target_class, visited)
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FrameworkProviderFingerprint {
    workspace_hash: u64,
    composer_hash: u64,
    source_hash: u64,
    relevant_files_hash: u64,
}

pub(crate) trait VirtualMemberProvider {
    fn id(&self) -> &'static str;

    fn priority(&self) -> u16 {
        100
    }

    fn virtual_members(
        &self,
        ctx: &FrameworkProviderContext<'_>,
        query: &VirtualMemberQuery,
    ) -> Vec<VirtualMember>;

    fn virtual_member_candidates(
        &self,
        _ctx: &FrameworkProviderContext<'_>,
        _class_fqn: &str,
        _kind: Option<VirtualMemberKind>,
    ) -> Vec<VirtualMember> {
        Vec::new()
    }

    #[allow(dead_code)]
    fn string_keys(
        &self,
        _ctx: &FrameworkProviderContext<'_>,
        _query: &FrameworkStringKeyQuery,
    ) -> Vec<FrameworkStringKey> {
        Vec::new()
    }
}

pub(crate) struct FrameworkProviderRegistry<'a> {
    providers: Vec<&'a dyn VirtualMemberProvider>,
}

impl<'a> FrameworkProviderRegistry<'a> {
    pub(crate) fn new(mut providers: Vec<&'a dyn VirtualMemberProvider>) -> Self {
        providers.sort_by_key(|provider| (provider.priority(), provider.id()));
        Self { providers }
    }

    pub(crate) fn virtual_members(
        &self,
        ctx: &FrameworkProviderContext<'_>,
        query: &VirtualMemberQuery,
    ) -> Vec<VirtualMember> {
        let mut merged: Vec<VirtualMember> = Vec::new();
        let mut seen = HashMap::<VirtualMemberIdentity, usize>::new();

        for provider in &self.providers {
            for member in provider.virtual_members(ctx, query) {
                let identity = member.identity();
                if let Some(index) = seen.get(&identity).copied() {
                    merged[index].merge_from(member);
                } else {
                    seen.insert(identity, merged.len());
                    merged.push(member);
                }
            }
        }

        merged
    }

    pub(crate) fn virtual_member_candidates(
        &self,
        ctx: &FrameworkProviderContext<'_>,
        class_fqn: &str,
        kind: Option<VirtualMemberKind>,
    ) -> Vec<VirtualMember> {
        let mut merged: Vec<VirtualMember> = Vec::new();
        let mut seen = HashMap::<VirtualMemberIdentity, usize>::new();

        for provider in &self.providers {
            for member in provider.virtual_member_candidates(ctx, class_fqn, kind) {
                let identity = member.identity();
                if let Some(index) = seen.get(&identity).copied() {
                    merged[index].merge_from(member);
                } else {
                    seen.insert(identity, merged.len());
                    merged.push(member);
                }
            }
        }

        merged
    }

    #[allow(dead_code)]
    pub(crate) fn string_keys(
        &self,
        ctx: &FrameworkProviderContext<'_>,
        query: &FrameworkStringKeyQuery,
    ) -> Vec<FrameworkStringKey> {
        let mut merged: Vec<FrameworkStringKey> = Vec::new();
        let mut seen = HashMap::<String, usize>::new();

        for provider in &self.providers {
            for key in provider.string_keys(ctx, query) {
                let identity = key.identity();
                if let Some(index) = seen.get(&identity).copied() {
                    merged[index].merge_from(key);
                } else {
                    seen.insert(identity, merged.len());
                    merged.push(key);
                }
            }
        }

        merged
    }
}

#[derive(Default)]
pub(crate) struct FrameworkProviderCache {
    fingerprint: RefCell<Option<FrameworkProviderFingerprint>>,
    virtual_members: RefCell<HashMap<VirtualMemberCacheKey, Vec<VirtualMember>>>,
    string_keys: RefCell<HashMap<FrameworkStringKeyQuery, Vec<FrameworkStringKey>>>,
}

impl FrameworkProviderCache {
    pub(crate) fn virtual_members(
        &self,
        registry: &FrameworkProviderRegistry<'_>,
        ctx: &FrameworkProviderContext<'_>,
        query: &VirtualMemberQuery,
    ) -> Vec<VirtualMember> {
        self.invalidate_if_needed(ctx.fingerprint());
        let key = query.cache_key();
        if let Some(value) = self.virtual_members.borrow().get(&key).cloned() {
            return value;
        }

        let value = registry.virtual_members(ctx, query);
        self.virtual_members.borrow_mut().insert(key, value.clone());
        value
    }

    pub(crate) fn has_virtual_member(
        &self,
        registry: &FrameworkProviderRegistry<'_>,
        ctx: &FrameworkProviderContext<'_>,
        query: &VirtualMemberQuery,
    ) -> bool {
        !self.virtual_members(registry, ctx, query).is_empty()
    }

    #[allow(dead_code)]
    pub(crate) fn string_keys(
        &self,
        registry: &FrameworkProviderRegistry<'_>,
        ctx: &FrameworkProviderContext<'_>,
        query: &FrameworkStringKeyQuery,
    ) -> Vec<FrameworkStringKey> {
        self.invalidate_if_needed(ctx.fingerprint());
        if let Some(value) = self.string_keys.borrow().get(query).cloned() {
            return value;
        }

        let value = registry.string_keys(ctx, query);
        self.string_keys
            .borrow_mut()
            .insert(query.clone(), value.clone());
        value
    }

    fn invalidate_if_needed(&self, fingerprint: FrameworkProviderFingerprint) {
        let mut current = self.fingerprint.borrow_mut();
        if current.as_ref() == Some(&fingerprint) {
            return;
        }

        *current = Some(fingerprint);
        self.virtual_members.borrow_mut().clear();
        self.string_keys.borrow_mut().clear();
    }

    #[cfg(test)]
    fn virtual_member_cache_len(&self) -> usize {
        self.virtual_members.borrow().len()
    }
}

static DOCTRINE_REPOSITORY_PROVIDER: DoctrineRepositoryProvider = DoctrineRepositoryProvider;
static SYMFONY_CONTROLLER_PROVIDER: SymfonyControllerProvider = SymfonyControllerProvider;
static SYMFONY_STRING_KEY_PROVIDER: SymfonyStringKeyProvider = SymfonyStringKeyProvider;
static LARAVEL_ELOQUENT_PROVIDER: LaravelEloquentProvider = LaravelEloquentProvider;
static LARAVEL_STRING_KEY_PROVIDER: LaravelStringKeyProvider = LaravelStringKeyProvider;

pub(crate) fn default_framework_provider_registry() -> FrameworkProviderRegistry<'static> {
    FrameworkProviderRegistry::new(vec![
        &DOCTRINE_REPOSITORY_PROVIDER,
        &SYMFONY_CONTROLLER_PROVIDER,
        &SYMFONY_STRING_KEY_PROVIDER,
        &LARAVEL_ELOQUENT_PROVIDER,
        &LARAVEL_STRING_KEY_PROVIDER,
    ])
}

pub(crate) fn framework_string_keys_for_workspace(
    root: &Path,
    domain: &str,
) -> Vec<FrameworkStringKey> {
    let index = WorkspaceIndex::new();
    let ctx = FrameworkProviderContext::new(&index)
        .with_workspace(Some(root), None)
        .with_relevant_files(&[]);
    let registry = default_framework_provider_registry();
    let query = FrameworkStringKeyQuery {
        domain: domain.to_string(),
        prefix: String::new(),
    };

    registry.string_keys(&ctx, &query)
}

struct DoctrineRepositoryProvider;

impl VirtualMemberProvider for DoctrineRepositoryProvider {
    fn id(&self) -> &'static str {
        "doctrine.repository"
    }

    fn priority(&self) -> u16 {
        20
    }

    fn virtual_members(
        &self,
        ctx: &FrameworkProviderContext<'_>,
        query: &VirtualMemberQuery,
    ) -> Vec<VirtualMember> {
        if query.kind != VirtualMemberKind::Method
            || !ctx.class_is_or_extends(&query.owner_fqn, "Doctrine\\ORM\\EntityRepository")
            || !(query.member_name.starts_with("findBy")
                || query.member_name.starts_with("findOneBy")
                || query.member_name.starts_with("countBy"))
        {
            return Vec::new();
        }

        vec![VirtualMember::synthetic(
            self.id(),
            &query.owner_fqn,
            &query.member_name,
            query.kind,
            "Doctrine repository dynamic finder",
        )]
    }
}

struct SymfonyControllerProvider;

impl VirtualMemberProvider for SymfonyControllerProvider {
    fn id(&self) -> &'static str {
        "symfony.controller"
    }

    fn priority(&self) -> u16 {
        30
    }

    fn virtual_members(
        &self,
        ctx: &FrameworkProviderContext<'_>,
        query: &VirtualMemberQuery,
    ) -> Vec<VirtualMember> {
        if query.kind != VirtualMemberKind::Method
            || !ctx.class_is_or_extends(
                &query.owner_fqn,
                "Symfony\\Bundle\\FrameworkBundle\\Controller\\AbstractController",
            )
            || !is_symfony_controller_helper(&query.member_name)
        {
            return Vec::new();
        }

        vec![VirtualMember::synthetic(
            self.id(),
            &query.owner_fqn,
            &query.member_name,
            query.kind,
            "Symfony controller helper",
        )]
    }
}

struct SymfonyStringKeyProvider;

impl VirtualMemberProvider for SymfonyStringKeyProvider {
    fn id(&self) -> &'static str {
        "symfony.string-keys"
    }

    fn priority(&self) -> u16 {
        35
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
        ctx: &FrameworkProviderContext<'_>,
        query: &FrameworkStringKeyQuery,
    ) -> Vec<FrameworkStringKey> {
        let Some(root) = ctx.workspace_root else {
            return Vec::new();
        };
        if !is_symfony_twig_layout(root) {
            return Vec::new();
        }

        let mut keys = match query.domain.as_str() {
            "twig" => collect_symfony_twig_template_keys(self.id(), root, &query.prefix),
            "route" => collect_symfony_route_keys(self.id(), root, &query.prefix),
            _ => Vec::new(),
        };
        keys.sort_by(|left, right| left.key.cmp(&right.key));
        keys
    }
}

struct LaravelEloquentProvider;

impl VirtualMemberProvider for LaravelEloquentProvider {
    fn id(&self) -> &'static str {
        "laravel.eloquent"
    }

    fn priority(&self) -> u16 {
        40
    }

    fn virtual_members(
        &self,
        ctx: &FrameworkProviderContext<'_>,
        query: &VirtualMemberQuery,
    ) -> Vec<VirtualMember> {
        let is_model = is_laravel_model(ctx, &query.owner_fqn);
        let is_builder = is_laravel_builder(ctx, &query.owner_fqn);

        let accepted = match query.kind {
            VirtualMemberKind::Method if is_model => {
                if let Some(scope) =
                    laravel_scope_virtual_method(ctx, &query.owner_fqn, &query.member_name)
                {
                    return vec![scope];
                }
                if is_laravel_eloquent_dynamic_method(&query.member_name) {
                    let mut member = VirtualMember::synthetic(
                        self.id(),
                        &query.owner_fqn,
                        &query.member_name,
                        query.kind,
                        "Laravel Eloquent dynamic method",
                    );
                    member.type_info = laravel_model_dynamic_method_return_type(
                        ctx,
                        &query.owner_fqn,
                        &query.member_name,
                    );
                    return vec![member];
                }
                false
            }
            VirtualMemberKind::Method if is_builder => {
                if let Some(scope) =
                    laravel_builder_scope_virtual_method(ctx, &query.owner_fqn, &query.member_name)
                {
                    return vec![scope];
                }
                if is_laravel_eloquent_dynamic_method(&query.member_name) {
                    let mut member = VirtualMember::synthetic(
                        self.id(),
                        &query.owner_fqn,
                        &query.member_name,
                        query.kind,
                        "Laravel Eloquent builder dynamic method",
                    );
                    member.type_info = Some(TypeInfo::Simple(query.owner_fqn.clone()));
                    return vec![member];
                }
                false
            }
            VirtualMemberKind::Property if is_model => {
                let member_name = query.member_name.trim_start_matches('$');
                let properties = laravel_model_virtual_properties(ctx, &query.owner_fqn);
                if let Some(property) = properties
                    .into_iter()
                    .find(|property| property.name.trim_start_matches('$') == member_name)
                {
                    return vec![property];
                }
                // Eloquent models expose attributes through Model::__get/__set at runtime.
                // If vendor symbols are absent, keep the conservative pre-IE-041 fallback
                // for diagnostics while completion still lists only statically discovered
                // properties.
                class_has_magic_property_method(ctx, &query.owner_fqn, "__get")
                    || class_has_magic_property_method(ctx, &query.owner_fqn, "__set")
                    || is_model
            }
            VirtualMemberKind::StaticProperty | VirtualMemberKind::ClassConstant => false,
            VirtualMemberKind::Method => false,
            VirtualMemberKind::Property => false,
        };

        if !accepted {
            return Vec::new();
        }

        vec![VirtualMember::synthetic(
            self.id(),
            &query.owner_fqn,
            &query.member_name,
            query.kind,
            "Laravel Eloquent dynamic member",
        )]
    }

    fn virtual_member_candidates(
        &self,
        ctx: &FrameworkProviderContext<'_>,
        class_fqn: &str,
        kind: Option<VirtualMemberKind>,
    ) -> Vec<VirtualMember> {
        if !is_laravel_model(ctx, class_fqn) {
            if !is_laravel_builder(ctx, class_fqn) {
                return Vec::new();
            }
            if kind.is_some_and(|kind| kind != VirtualMemberKind::Method) {
                return Vec::new();
            }
            return laravel_builder_scope_virtual_methods(ctx, class_fqn);
        }
        if kind.is_some_and(|kind| {
            !matches!(
                kind,
                VirtualMemberKind::Property | VirtualMemberKind::Method
            )
        }) {
            return Vec::new();
        }

        let mut members = Vec::new();
        if kind.is_none() || kind == Some(VirtualMemberKind::Property) {
            members.extend(laravel_model_virtual_properties(ctx, class_fqn));
        }
        if kind.is_none() || kind == Some(VirtualMemberKind::Method) {
            members.extend(laravel_scope_virtual_methods(ctx, class_fqn));
        }
        members
    }
}

struct LaravelStringKeyProvider;

impl VirtualMemberProvider for LaravelStringKeyProvider {
    fn id(&self) -> &'static str {
        "laravel.string-keys"
    }

    fn priority(&self) -> u16 {
        50
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
        ctx: &FrameworkProviderContext<'_>,
        query: &FrameworkStringKeyQuery,
    ) -> Vec<FrameworkStringKey> {
        let Some(root) = ctx.workspace_root else {
            return Vec::new();
        };
        if !is_laravel_string_key_layout(root) {
            return Vec::new();
        }

        let mut keys = match query.domain.as_str() {
            "config" => collect_laravel_config_keys(self.id(), root, &query.prefix),
            "route" => collect_laravel_route_keys(self.id(), root, &query.prefix),
            "translation" => collect_laravel_translation_keys(self.id(), root, &query.prefix),
            "view" => collect_laravel_view_keys(self.id(), root, &query.prefix),
            _ => Vec::new(),
        };
        keys.sort_by(|left, right| left.key.cmp(&right.key));
        keys
    }
}

fn is_laravel_model(ctx: &FrameworkProviderContext<'_>, class_fqn: &str) -> bool {
    ctx.class_is_or_extends(class_fqn, "Illuminate\\Database\\Eloquent\\Model")
}

fn is_laravel_builder(ctx: &FrameworkProviderContext<'_>, class_fqn: &str) -> bool {
    ctx.class_is_or_extends(class_fqn, "Illuminate\\Database\\Eloquent\\Builder")
        || ctx.class_is_or_extends(class_fqn, "Illuminate\\Database\\Query\\Builder")
        || ctx.class_is_or_extends(
            class_fqn,
            "Illuminate\\Database\\Eloquent\\Relations\\Relation",
        )
}

fn laravel_model_virtual_properties(
    ctx: &FrameworkProviderContext<'_>,
    class_fqn: &str,
) -> Vec<VirtualMember> {
    let mut properties = Vec::new();
    let mut seen = HashMap::<VirtualMemberIdentity, usize>::new();

    for owner in ctx.index.get_type_hierarchy_symbols(class_fqn) {
        collect_phpdoc_properties(ctx, &owner, &mut properties, &mut seen);
        collect_laravel_accessor_properties(ctx, &owner, &mut properties, &mut seen);
        collect_laravel_source_properties(ctx, &owner, &mut properties, &mut seen);
        collect_laravel_relation_count_properties(ctx, &owner, &mut properties, &mut seen);
    }

    properties
}

fn push_laravel_property(
    properties: &mut Vec<VirtualMember>,
    seen: &mut HashMap<VirtualMemberIdentity, usize>,
    property: VirtualMember,
) {
    let identity = property.identity();
    if let Some(index) = seen.get(&identity).copied() {
        properties[index].merge_from(property);
    } else {
        seen.insert(identity, properties.len());
        properties.push(property);
    }
}

fn collect_phpdoc_properties(
    _ctx: &FrameworkProviderContext<'_>,
    owner: &std::sync::Arc<SymbolInfo>,
    properties: &mut Vec<VirtualMember>,
    seen: &mut HashMap<VirtualMemberIdentity, usize>,
) {
    let Some(doc_comment) = owner.doc_comment.as_deref() else {
        return;
    };
    let phpdoc = parse_phpdoc(doc_comment);
    for property in phpdoc.properties {
        let mut member = VirtualMember::synthetic(
            LARAVEL_ELOQUENT_PROVIDER.id(),
            &owner.fqn,
            &property.name,
            VirtualMemberKind::Property,
            "Laravel model PHPDoc property",
        );
        member.type_info = property.type_info;
        member.access = Some(property.access);
        member.detail = Some(match member.type_info.as_ref() {
            Some(type_info) => format!("{} {}", phpdoc_property_tag(property.access), type_info),
            None => phpdoc_property_tag(property.access).to_string(),
        });
        if let Some(description) = property.description {
            member.detail = Some(match member.detail {
                Some(detail) => format!("{detail} - {description}"),
                None => description,
            });
        }
        push_laravel_property(properties, seen, member);
    }
}

fn collect_laravel_accessor_properties(
    ctx: &FrameworkProviderContext<'_>,
    owner: &std::sync::Arc<SymbolInfo>,
    properties: &mut Vec<VirtualMember>,
    seen: &mut HashMap<VirtualMemberIdentity, usize>,
) {
    let members = ctx.index.get_members(&owner.fqn);
    for method in members.iter().filter(|member| {
        member.parent_fqn.as_deref() == Some(owner.fqn.as_str())
            && member.kind == PhpSymbolKind::Method
    }) {
        if let Some(property_name) = legacy_accessor_property_name(&method.name) {
            let mut member = laravel_property_from_symbol(
                owner,
                &property_name,
                method
                    .signature
                    .as_ref()
                    .and_then(|signature| signature.return_type.clone())
                    .or(Some(TypeInfo::Mixed)),
                "Laravel legacy accessor property",
                Some(method),
            );
            member.access = Some(PhpDocPropertyAccess::ReadOnly);
            push_laravel_property(properties, seen, member);
            continue;
        }

        if let Some(type_info) = modern_attribute_get_type(method) {
            let property_name = method.name.clone();
            let mut member = laravel_property_from_symbol(
                owner,
                &property_name,
                Some(type_info),
                "Laravel Attribute accessor property",
                Some(method),
            );
            member.access = Some(PhpDocPropertyAccess::ReadOnly);
            push_laravel_property(properties, seen, member);
        }
    }
}

fn collect_laravel_source_properties(
    ctx: &FrameworkProviderContext<'_>,
    owner: &std::sync::Arc<SymbolInfo>,
    properties: &mut Vec<VirtualMember>,
    seen: &mut HashMap<VirtualMemberIdentity, usize>,
) {
    let Some(source) = source_for_symbol(ctx, owner) else {
        return;
    };

    let members = ctx.index.get_members(&owner.fqn);
    for property in members.iter().filter(|member| {
        member.parent_fqn.as_deref() == Some(owner.fqn.as_str())
            && member.kind == PhpSymbolKind::Property
    }) {
        match property.name.as_str() {
            "casts" => {
                let Some(text) = source_text_for_range(source, property.range) else {
                    continue;
                };
                for (name, cast_value) in parse_array_string_pairs(text) {
                    let source_range = property_source_range(property);
                    let member = laravel_property_from_source(
                        owner,
                        &name,
                        cast_value_to_type(&cast_value).or(Some(TypeInfo::Mixed)),
                        "Laravel $casts property",
                        source_range,
                    );
                    push_laravel_property(properties, seen, member);
                }
            }
            "fillable" | "guarded" | "hidden" | "visible" => {
                let Some(text) = source_text_for_range(source, property.range) else {
                    continue;
                };
                for name in parse_array_string_values(text) {
                    if name == "*" {
                        continue;
                    }
                    let source_range = property_source_range(property);
                    let member = laravel_property_from_source(
                        owner,
                        &name,
                        Some(TypeInfo::Mixed),
                        format!("Laravel ${} weak property fallback", property.name),
                        source_range,
                    );
                    push_laravel_property(properties, seen, member);
                }
            }
            _ => {}
        }
    }

    for method in members.iter().filter(|member| {
        member.parent_fqn.as_deref() == Some(owner.fqn.as_str())
            && member.kind == PhpSymbolKind::Method
            && member.name == "casts"
    }) {
        let Some(text) = source_text_for_range(source, method.range) else {
            continue;
        };
        for (name, cast_value) in parse_array_string_pairs(text) {
            let member = laravel_property_from_source(
                owner,
                &name,
                cast_value_to_type(&cast_value).or(Some(TypeInfo::Mixed)),
                "Laravel casts() method",
                property_source_range(method),
            );
            push_laravel_property(properties, seen, member);
        }
    }
}

#[derive(Debug, Clone)]
struct LaravelRelation {
    name: String,
    related_model: Option<String>,
    source: Option<VirtualMemberSource>,
}

fn collect_laravel_relation_count_properties(
    ctx: &FrameworkProviderContext<'_>,
    owner: &std::sync::Arc<SymbolInfo>,
    properties: &mut Vec<VirtualMember>,
    seen: &mut HashMap<VirtualMemberIdentity, usize>,
) {
    for relation in laravel_model_relations_for_owner(ctx, owner) {
        let property_name = format!("{}_count", studly_to_snake(&relation.name));
        let detail = relation
            .related_model
            .as_ref()
            .map(|model| format!("Laravel relation count for {} ({})", relation.name, model))
            .unwrap_or_else(|| format!("Laravel relation count for {}", relation.name));
        let member = laravel_property_from_source(
            owner,
            &property_name,
            Some(TypeInfo::Simple("int".to_string())),
            detail,
            relation.source,
        );
        push_laravel_property(properties, seen, member);
    }
}

fn laravel_model_relations_for_owner(
    ctx: &FrameworkProviderContext<'_>,
    owner: &std::sync::Arc<SymbolInfo>,
) -> Vec<LaravelRelation> {
    ctx.index
        .get_members(&owner.fqn)
        .into_iter()
        .filter(|member| {
            member.parent_fqn.as_deref() == Some(owner.fqn.as_str())
                && member.kind == PhpSymbolKind::Method
                && !member.modifiers.is_static
        })
        .filter_map(|method| laravel_relation_from_method(ctx, owner, &method))
        .collect()
}

fn laravel_relation_from_method(
    ctx: &FrameworkProviderContext<'_>,
    owner: &SymbolInfo,
    method: &SymbolInfo,
) -> Option<LaravelRelation> {
    if method.name == "casts"
        || method.name.starts_with("__")
        || scope_method_name(&method.name).is_some()
        || legacy_accessor_property_name(&method.name).is_some()
        || modern_attribute_get_type(method).is_some()
    {
        return None;
    }

    let return_type = method
        .signature
        .as_ref()
        .and_then(|signature| signature.return_type.as_ref());
    let related_from_return = return_type
        .and_then(|type_info| laravel_relation_related_model_from_type_info(ctx, owner, type_info));
    let returns_relation =
        return_type.is_some_and(|type_info| is_laravel_relation_type_info(ctx, owner, type_info));
    let related_from_source = laravel_relation_related_model_from_source(ctx, owner, method);

    if !returns_relation && related_from_source.is_none() {
        return None;
    }

    Some(LaravelRelation {
        name: method.name.clone(),
        related_model: related_from_return.or(related_from_source),
        source: property_source_range(method),
    })
}

fn laravel_relation_related_model_from_type_info(
    ctx: &FrameworkProviderContext<'_>,
    owner: &SymbolInfo,
    type_info: &TypeInfo,
) -> Option<String> {
    match type_info {
        TypeInfo::Generic { base, args }
            if resolve_type_name_to_fqn(ctx, owner, base)
                .as_deref()
                .is_some_and(|fqn| is_laravel_relation_fqn(ctx, fqn)) =>
        {
            args.iter()
                .find_map(|arg| type_info_to_fqn(ctx, owner, arg))
        }
        TypeInfo::Nullable(inner) => {
            laravel_relation_related_model_from_type_info(ctx, owner, inner)
        }
        TypeInfo::Union(types) | TypeInfo::Intersection(types) => {
            types.iter().find_map(|type_info| {
                laravel_relation_related_model_from_type_info(ctx, owner, type_info)
            })
        }
        _ => None,
    }
}

fn is_laravel_relation_type_info(
    ctx: &FrameworkProviderContext<'_>,
    owner: &SymbolInfo,
    type_info: &TypeInfo,
) -> bool {
    match type_info {
        TypeInfo::Simple(name) => resolve_type_name_to_fqn(ctx, owner, name)
            .as_deref()
            .is_some_and(|fqn| is_laravel_relation_fqn(ctx, fqn)),
        TypeInfo::Generic { base, .. } => resolve_type_name_to_fqn(ctx, owner, base)
            .as_deref()
            .is_some_and(|fqn| is_laravel_relation_fqn(ctx, fqn)),
        TypeInfo::Nullable(inner) => is_laravel_relation_type_info(ctx, owner, inner),
        TypeInfo::Union(types) | TypeInfo::Intersection(types) => types
            .iter()
            .any(|type_info| is_laravel_relation_type_info(ctx, owner, type_info)),
        _ => false,
    }
}

fn is_laravel_relation_fqn(ctx: &FrameworkProviderContext<'_>, fqn: &str) -> bool {
    ctx.class_is_or_extends(fqn, "Illuminate\\Database\\Eloquent\\Relations\\Relation")
        || matches!(
            fqn.trim_start_matches('\\')
                .rsplit('\\')
                .next()
                .unwrap_or(fqn),
            "Relation"
                | "BelongsTo"
                | "BelongsToMany"
                | "HasMany"
                | "HasManyThrough"
                | "HasOne"
                | "HasOneThrough"
                | "MorphMany"
                | "MorphOne"
                | "MorphTo"
                | "MorphToMany"
                | "MorphedByMany"
        )
}

fn laravel_relation_related_model_from_source(
    ctx: &FrameworkProviderContext<'_>,
    owner: &SymbolInfo,
    method: &SymbolInfo,
) -> Option<String> {
    let text = source_for_symbol(ctx, method)?;
    let text = source_text_for_range(text, method.range)?;
    for factory in LARAVEL_RELATION_FACTORIES {
        let mut search_start = 0usize;
        let needle = format!("{factory}(");
        while let Some(relative) = text[search_start..].find(&needle) {
            let args_start = search_start + relative + needle.len();
            let first_arg = first_call_argument_text(&text[args_start..])?;
            if let Some(model) = class_reference_text_to_fqn(ctx, owner, first_arg) {
                return Some(model);
            }
            search_start = args_start;
        }
    }
    None
}

const LARAVEL_RELATION_FACTORIES: &[&str] = &[
    "belongsTo",
    "belongsToMany",
    "hasMany",
    "hasManyThrough",
    "hasOne",
    "hasOneThrough",
    "morphMany",
    "morphOne",
    "morphTo",
    "morphToMany",
    "morphedByMany",
];

fn first_call_argument_text(text_after_open_paren: &str) -> Option<&str> {
    let mut quote: Option<char> = None;
    let mut depth = 0usize;
    for (idx, ch) in text_after_open_paren.char_indices() {
        if let Some(active_quote) = quote {
            if ch == '\\' {
                continue;
            }
            if ch == active_quote {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '(' | '[' => depth += 1,
            ')' if depth == 0 => return Some(text_after_open_paren[..idx].trim()),
            ')' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => return Some(text_after_open_paren[..idx].trim()),
            _ => {}
        }
    }
    None
}

fn class_reference_text_to_fqn(
    ctx: &FrameworkProviderContext<'_>,
    owner: &SymbolInfo,
    text: &str,
) -> Option<String> {
    let text = text.trim();
    if let Some(class_pos) = text.find("::class") {
        let before = text[..class_pos].trim();
        let class_name = before
            .rsplit(|ch: char| !(ch.is_alphanumeric() || ch == '_' || ch == '\\'))
            .next()
            .unwrap_or(before)
            .trim();
        return resolve_type_name_to_fqn(ctx, owner, class_name);
    }

    if (text.starts_with('\'') && text.ends_with('\''))
        || (text.starts_with('"') && text.ends_with('"'))
    {
        return resolve_type_name_to_fqn(ctx, owner, &text[1..text.len().saturating_sub(1)]);
    }

    None
}

fn laravel_scope_virtual_method(
    ctx: &FrameworkProviderContext<'_>,
    model_fqn: &str,
    member_name: &str,
) -> Option<VirtualMember> {
    laravel_scope_virtual_methods(ctx, model_fqn)
        .into_iter()
        .find(|member| member.name.eq_ignore_ascii_case(member_name))
}

fn laravel_scope_virtual_methods(
    ctx: &FrameworkProviderContext<'_>,
    model_fqn: &str,
) -> Vec<VirtualMember> {
    let mut methods = Vec::new();
    let mut seen = HashMap::<VirtualMemberIdentity, usize>::new();
    for owner in ctx.index.get_type_hierarchy_symbols(model_fqn) {
        collect_laravel_scope_methods(
            ctx,
            &owner,
            model_fqn,
            laravel_builder_type_for_model(ctx, model_fqn),
            &mut methods,
            &mut seen,
        );
    }
    methods
}

fn laravel_builder_scope_virtual_method(
    ctx: &FrameworkProviderContext<'_>,
    builder_fqn: &str,
    member_name: &str,
) -> Option<VirtualMember> {
    laravel_builder_scope_virtual_methods(ctx, builder_fqn)
        .into_iter()
        .find(|member| member.name.eq_ignore_ascii_case(member_name))
}

fn laravel_builder_scope_virtual_methods(
    ctx: &FrameworkProviderContext<'_>,
    builder_fqn: &str,
) -> Vec<VirtualMember> {
    let Some(model_fqn) = laravel_model_for_builder(ctx, builder_fqn) else {
        return Vec::new();
    };

    let mut methods = Vec::new();
    let mut seen = HashMap::<VirtualMemberIdentity, usize>::new();
    for owner in ctx.index.get_type_hierarchy_symbols(&model_fqn) {
        collect_laravel_scope_methods(
            ctx,
            &owner,
            builder_fqn,
            Some(TypeInfo::Simple(builder_fqn.to_string())),
            &mut methods,
            &mut seen,
        );
    }
    methods
}

fn collect_laravel_scope_methods(
    ctx: &FrameworkProviderContext<'_>,
    owner: &std::sync::Arc<SymbolInfo>,
    exposed_owner_fqn: &str,
    return_type: Option<TypeInfo>,
    methods: &mut Vec<VirtualMember>,
    seen: &mut HashMap<VirtualMemberIdentity, usize>,
) {
    for method in ctx
        .index
        .get_members(&owner.fqn)
        .into_iter()
        .filter(|member| {
            member.parent_fqn.as_deref() == Some(owner.fqn.as_str())
                && member.kind == PhpSymbolKind::Method
                && !member.modifiers.is_static
        })
    {
        let Some(scope_name) = scope_method_name(&method.name) else {
            continue;
        };
        let mut member = VirtualMember::synthetic(
            LARAVEL_ELOQUENT_PROVIDER.id(),
            exposed_owner_fqn,
            &scope_name,
            VirtualMemberKind::Method,
            format!("Laravel local scope from {}::{}", owner.fqn, method.name),
        );
        member.type_info = return_type.clone();
        member.sources.push(VirtualMemberSource::SourceRange {
            uri: method.uri.clone(),
            range: method.selection_range,
        });
        push_virtual_member(methods, seen, member);
    }
}

fn scope_method_name(method_name: &str) -> Option<String> {
    let suffix = method_name.strip_prefix("scope")?;
    if suffix.is_empty() {
        return None;
    }
    Some(lowercase_first(suffix))
}

fn lowercase_first(value: &str) -> String {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut out = String::new();
    out.extend(first.to_lowercase());
    out.push_str(chars.as_str());
    out
}

fn push_virtual_member(
    members: &mut Vec<VirtualMember>,
    seen: &mut HashMap<VirtualMemberIdentity, usize>,
    member: VirtualMember,
) {
    let identity = member.identity();
    if let Some(index) = seen.get(&identity).copied() {
        members[index].merge_from(member);
    } else {
        seen.insert(identity, members.len());
        members.push(member);
    }
}

fn laravel_model_dynamic_method_return_type(
    ctx: &FrameworkProviderContext<'_>,
    model_fqn: &str,
    method_name: &str,
) -> Option<TypeInfo> {
    let lower = method_name.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "query" | "newquery" | "newmodelquery" | "newquerywithoutrelationships"
    ) || lower.starts_with("where")
        || lower.starts_with("orwhere")
        || lower.starts_with("wherehas")
        || lower.starts_with("orwherehas")
        || lower.starts_with("withwherehas")
        || lower.starts_with("doesnthave")
        || lower.starts_with("ordoesnthave")
    {
        return laravel_builder_type_for_model(ctx, model_fqn);
    }

    if matches!(
        lower.as_str(),
        "find" | "findorfail" | "first" | "firstorfail" | "firstornew" | "firstorcreate" | "create"
    ) {
        return Some(TypeInfo::Simple(model_fqn.to_string()));
    }

    if matches!(lower.as_str(), "count") {
        return Some(TypeInfo::Simple("int".to_string()));
    }

    None
}

fn laravel_builder_type_for_model(
    ctx: &FrameworkProviderContext<'_>,
    model_fqn: &str,
) -> Option<TypeInfo> {
    laravel_custom_builder_for_model(ctx, model_fqn)
        .map(TypeInfo::Simple)
        .or_else(|| {
            Some(TypeInfo::Generic {
                base: "Illuminate\\Database\\Eloquent\\Builder".to_string(),
                args: vec![TypeInfo::Simple(model_fqn.to_string())],
            })
        })
}

fn laravel_custom_builder_for_model(
    ctx: &FrameworkProviderContext<'_>,
    model_fqn: &str,
) -> Option<String> {
    for owner in ctx.index.get_type_hierarchy_symbols(model_fqn) {
        if let Some(builder) = laravel_custom_builder_from_attribute(ctx, &owner) {
            return Some(builder);
        }

        for method in ctx
            .index
            .get_members(&owner.fqn)
            .into_iter()
            .filter(|member| {
                member.parent_fqn.as_deref() == Some(owner.fqn.as_str())
                    && member.kind == PhpSymbolKind::Method
                    && matches!(
                        member.name.as_str(),
                        "newEloquentBuilder" | "newModelQuery" | "newQuery" | "query"
                    )
            })
        {
            let Some(return_type) = method
                .signature
                .as_ref()
                .and_then(|signature| signature.return_type.as_ref())
            else {
                continue;
            };
            let Some(builder_fqn) = type_info_to_fqn(ctx, &owner, return_type) else {
                continue;
            };
            if is_laravel_builder(ctx, &builder_fqn)
                && !is_default_laravel_builder_fqn(&builder_fqn)
            {
                return Some(builder_fqn);
            }
        }
    }

    None
}

fn laravel_custom_builder_from_attribute(
    ctx: &FrameworkProviderContext<'_>,
    owner: &SymbolInfo,
) -> Option<String> {
    let source = source_for_symbol(ctx, owner)?;
    let class_text = source_text_for_range(source, owner.range)?;
    let attr_pos = class_text.find("UseEloquentBuilder")?;
    let after_attr = &class_text[attr_pos..];
    let open = after_attr.find('(')?;
    let first_arg = first_call_argument_text(&after_attr[open + 1..])?;
    class_reference_text_to_fqn(ctx, owner, first_arg)
}

fn is_default_laravel_builder_fqn(fqn: &str) -> bool {
    matches!(
        fqn.trim_start_matches('\\'),
        "Illuminate\\Database\\Eloquent\\Builder" | "Illuminate\\Database\\Query\\Builder"
    )
}

fn laravel_model_for_builder(
    ctx: &FrameworkProviderContext<'_>,
    builder_fqn: &str,
) -> Option<String> {
    let builder = ctx
        .index
        .types
        .get(builder_fqn.trim_start_matches('\\'))
        .map(|entry| entry.value().clone())?;

    for binding in &builder.template_bindings {
        if !matches!(
            binding.kind,
            TemplateBindingKind::Extends
                | TemplateBindingKind::Implements
                | TemplateBindingKind::Mixin
        ) {
            continue;
        }
        if is_laravel_builder(ctx, &binding.target) {
            if let Some(model) = binding
                .args
                .iter()
                .find_map(|arg| type_info_to_fqn(ctx, &builder, arg))
            {
                return Some(model);
            }
        }
    }

    for entry in ctx.index.types.iter() {
        let symbol = entry.value();
        if symbol.kind != PhpSymbolKind::Class || !is_laravel_model(ctx, &symbol.fqn) {
            continue;
        }
        if laravel_custom_builder_for_model(ctx, &symbol.fqn)
            .as_deref()
            .is_some_and(|custom_builder| fqn_matches(custom_builder, builder_fqn))
        {
            return Some(symbol.fqn.clone());
        }
    }

    None
}

fn type_info_to_fqn(
    ctx: &FrameworkProviderContext<'_>,
    owner: &SymbolInfo,
    type_info: &TypeInfo,
) -> Option<String> {
    match type_info {
        TypeInfo::Simple(name) => resolve_type_name_to_fqn(ctx, owner, name),
        TypeInfo::Generic { base, .. } => resolve_type_name_to_fqn(ctx, owner, base),
        TypeInfo::Nullable(inner) => type_info_to_fqn(ctx, owner, inner),
        TypeInfo::Union(types) | TypeInfo::Intersection(types) => types
            .iter()
            .find_map(|type_info| type_info_to_fqn(ctx, owner, type_info)),
        TypeInfo::ClassString(Some(inner)) => type_info_to_fqn(ctx, owner, inner),
        TypeInfo::Self_ | TypeInfo::Static_ => Some(owner.fqn.clone()),
        TypeInfo::Parent_ => owner.extends.first().cloned(),
        _ => None,
    }
}

fn resolve_type_name_to_fqn(
    ctx: &FrameworkProviderContext<'_>,
    owner: &SymbolInfo,
    type_name: &str,
) -> Option<String> {
    let type_name = type_name.trim().trim_start_matches('\\');
    if type_name.is_empty() || is_builtin_type_name(type_name) {
        return None;
    }
    if type_name.contains(['|', '&', '<', '>', '{', '}', '(', ')', ',', ' ']) {
        return None;
    }
    if type_name.contains('\\') {
        return Some(type_name.to_string());
    }

    if let Some(file_symbols) = ctx.index.file_symbols.get(owner.uri.as_str()) {
        return Some(resolve_class_name(type_name, file_symbols.value()));
    }
    if ctx
        .source_uri
        .is_some_and(|source_uri| source_uri == owner.uri.as_str())
    {
        if let Some(file_symbols) = ctx.file_symbols {
            return Some(resolve_class_name(type_name, file_symbols));
        }
    }

    Some(type_name.to_string())
}

fn is_builtin_type_name(type_name: &str) -> bool {
    matches!(
        type_name
            .trim_start_matches('\\')
            .to_ascii_lowercase()
            .as_str(),
        "array"
            | "bool"
            | "boolean"
            | "callable"
            | "double"
            | "false"
            | "float"
            | "int"
            | "integer"
            | "iterable"
            | "mixed"
            | "never"
            | "null"
            | "object"
            | "real"
            | "resource"
            | "self"
            | "static"
            | "string"
            | "true"
            | "void"
    )
}

fn laravel_property_from_symbol(
    owner: &SymbolInfo,
    property_name: &str,
    type_info: Option<TypeInfo>,
    detail: impl Into<String>,
    source_symbol: Option<&SymbolInfo>,
) -> VirtualMember {
    let mut member = VirtualMember::synthetic(
        LARAVEL_ELOQUENT_PROVIDER.id(),
        &owner.fqn,
        property_name,
        VirtualMemberKind::Property,
        detail,
    );
    member.type_info = type_info;
    if let Some(source_symbol) = source_symbol {
        member.sources.push(VirtualMemberSource::SourceRange {
            uri: source_symbol.uri.clone(),
            range: source_symbol.selection_range,
        });
    }
    member
}

fn laravel_property_from_source(
    owner: &SymbolInfo,
    property_name: &str,
    type_info: Option<TypeInfo>,
    detail: impl Into<String>,
    source: Option<VirtualMemberSource>,
) -> VirtualMember {
    let mut member = VirtualMember::synthetic(
        LARAVEL_ELOQUENT_PROVIDER.id(),
        &owner.fqn,
        property_name,
        VirtualMemberKind::Property,
        detail,
    );
    member.type_info = type_info;
    if let Some(source) = source {
        member.sources.push(source);
    }
    member
}

fn property_source_range(symbol: &SymbolInfo) -> Option<VirtualMemberSource> {
    Some(VirtualMemberSource::SourceRange {
        uri: symbol.uri.clone(),
        range: symbol.selection_range,
    })
}

fn source_for_symbol<'a>(
    ctx: &'a FrameworkProviderContext<'a>,
    symbol: &SymbolInfo,
) -> Option<&'a str> {
    let source = ctx.source?;
    if ctx
        .source_uri
        .is_some_and(|source_uri| source_uri == symbol.uri.as_str())
    {
        return Some(source);
    }

    if ctx.source_uri.is_none() {
        return Some(source);
    }

    None
}

fn source_text_for_range(source: &str, range: (u32, u32, u32, u32)) -> Option<&str> {
    let start = byte_offset_for_line_col(source, range.0, range.1)?;
    let end = byte_offset_for_line_col(source, range.2, range.3)?;
    source.get(start..end)
}

fn byte_offset_for_line_col(source: &str, line: u32, byte_col: u32) -> Option<usize> {
    let mut offset = 0usize;
    for (idx, segment) in source.split_inclusive('\n').enumerate() {
        if idx == line as usize {
            let candidate = offset + byte_col as usize;
            return (candidate <= source.len()).then_some(candidate);
        }
        offset += segment.len();
    }
    (line == source.lines().count() as u32 && byte_col == 0).then_some(source.len())
}

fn parse_array_string_values(text: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut index = 0usize;
    while let Some((value, _start, end)) = next_quoted_string(text, index) {
        values.push(value);
        index = end;
    }
    values
}

fn parse_array_string_pairs(text: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut index = 0usize;

    while let Some((key, _key_start, key_end)) = next_quoted_string(text, index) {
        let Some(arrow_relative) = text[key_end..].find("=>") else {
            index = key_end;
            continue;
        };
        let value_start = key_end + arrow_relative + 2;
        if let Some((value, _start, end)) = next_cast_value(text, value_start) {
            pairs.push((key, value));
            index = end;
        } else {
            index = value_start;
        }
    }

    pairs
}

fn next_cast_value(text: &str, start: usize) -> Option<(String, usize, usize)> {
    let rest = text.get(start..)?;
    let leading_ws = rest.len() - rest.trim_start().len();
    let token_start = start + leading_ws;
    let first = text[token_start..].chars().next()?;
    if first == '\'' || first == '"' {
        return next_quoted_string(text, token_start);
    }

    let token_end = text[token_start..]
        .find([',', ']', ')', '\n'])
        .map(|offset| token_start + offset)
        .unwrap_or(text.len());
    let value = text[token_start..token_end].trim();
    (!value.is_empty()).then(|| (value.to_string(), token_start, token_end))
}

fn next_quoted_string(text: &str, start: usize) -> Option<(String, usize, usize)> {
    let bytes = text.as_bytes();
    let mut index = start;
    while index < bytes.len() {
        let quote = bytes[index];
        if quote == b'\'' || quote == b'"' {
            let mut value = String::new();
            let mut cursor = index + 1;
            while cursor < bytes.len() {
                let ch = bytes[cursor] as char;
                if bytes[cursor] == b'\\' {
                    if cursor + 1 < bytes.len() {
                        value.push(bytes[cursor + 1] as char);
                        cursor += 2;
                        continue;
                    }
                    return None;
                }
                if bytes[cursor] == quote {
                    return Some((value, index, cursor + 1));
                }
                value.push(ch);
                cursor += 1;
            }
            return None;
        }
        index += 1;
    }
    None
}

fn cast_value_to_type(value: &str) -> Option<TypeInfo> {
    let mut normalized = value.trim().trim_matches(['\'', '"']).to_string();
    if let Some(class_name) = normalized.strip_suffix("::class") {
        return Some(TypeInfo::Simple(
            class_name.trim_start_matches('\\').to_string(),
        ));
    }
    if let Some(rest) = normalized.strip_prefix("encrypted:") {
        normalized = rest.to_string();
    }
    let base = normalized
        .split(':')
        .next()
        .unwrap_or(normalized.as_str())
        .to_ascii_lowercase();

    match base.as_str() {
        "int" | "integer" => Some(TypeInfo::Simple("int".to_string())),
        "real" | "float" | "double" => Some(TypeInfo::Simple("float".to_string())),
        "decimal" | "string" => Some(TypeInfo::Simple("string".to_string())),
        "bool" | "boolean" => Some(TypeInfo::Simple("bool".to_string())),
        "array" | "json" => Some(TypeInfo::Simple("array".to_string())),
        "object" => Some(TypeInfo::Simple("object".to_string())),
        "collection" => Some(TypeInfo::Simple(
            "Illuminate\\Support\\Collection".to_string(),
        )),
        "date" | "datetime" | "immutable_date" | "immutable_datetime" | "timestamp" => {
            Some(TypeInfo::Simple("Carbon\\CarbonInterface".to_string()))
        }
        _ if normalized.contains('\\') => Some(TypeInfo::Simple(
            normalized.trim_start_matches('\\').to_string(),
        )),
        _ => None,
    }
}

fn legacy_accessor_property_name(method_name: &str) -> Option<String> {
    let stem = method_name.strip_prefix("get")?.strip_suffix("Attribute")?;
    (!stem.is_empty()).then(|| studly_to_snake(stem))
}

fn modern_attribute_get_type(method: &SymbolInfo) -> Option<TypeInfo> {
    let return_type = method.signature.as_ref()?.return_type.as_ref()?;
    match return_type {
        TypeInfo::Generic { base, args } if type_name_ends_with(base, "Attribute") => {
            args.first().cloned().or(Some(TypeInfo::Mixed))
        }
        TypeInfo::Simple(base) if type_name_ends_with(base, "Attribute") => Some(TypeInfo::Mixed),
        _ => None,
    }
}

fn type_name_ends_with(type_name: &str, suffix: &str) -> bool {
    type_name
        .trim_start_matches('\\')
        .rsplit('\\')
        .next()
        .is_some_and(|name| name == suffix)
}

fn studly_to_snake(value: &str) -> String {
    let mut out = String::new();
    for (idx, ch) in value.chars().enumerate() {
        if ch.is_uppercase() && idx > 0 {
            out.push('_');
        }
        out.extend(ch.to_lowercase());
    }
    out
}

fn class_has_magic_property_method(
    ctx: &FrameworkProviderContext<'_>,
    class_fqn: &str,
    method_name: &str,
) -> bool {
    ctx.index.get_members(class_fqn).into_iter().any(|member| {
        member.kind == PhpSymbolKind::Method
            && member.name.eq_ignore_ascii_case(method_name)
            && !member.modifiers.is_static
    })
}

fn is_symfony_controller_helper(member_name: &str) -> bool {
    matches!(
        member_name.to_ascii_lowercase().as_str(),
        "render"
            | "renderform"
            | "json"
            | "redirect"
            | "redirecttoroute"
            | "redirecttourl"
            | "forward"
            | "generateurl"
            | "addflash"
            | "getuser"
            | "isgranted"
            | "denyaccessunlessgranted"
            | "createform"
            | "createformbuilder"
            | "getparameter"
    )
}

fn phpdoc_property_tag(access: PhpDocPropertyAccess) -> &'static str {
    match access {
        PhpDocPropertyAccess::ReadWrite => "@property",
        PhpDocPropertyAccess::ReadOnly => "@property-read",
        PhpDocPropertyAccess::WriteOnly => "@property-write",
    }
}

fn is_laravel_eloquent_dynamic_method(member_name: &str) -> bool {
    let lower = member_name.to_ascii_lowercase();
    lower.starts_with("where")
        || lower.starts_with("orwhere")
        || lower.starts_with("wherehas")
        || lower.starts_with("orwherehas")
        || lower.starts_with("withwherehas")
        || lower.starts_with("doesnthave")
        || lower.starts_with("ordoesnthave")
        || matches!(
            lower.as_str(),
            "query"
                | "newquery"
                | "newmodelquery"
                | "newquerywithoutrelationships"
                | "find"
                | "findorfail"
                | "findmany"
                | "first"
                | "firstorfail"
                | "firstornew"
                | "firstorcreate"
                | "updateorcreate"
                | "create"
                | "forcecreate"
                | "save"
                | "push"
                | "update"
                | "delete"
                | "destroy"
                | "restore"
                | "with"
                | "without"
                | "load"
                | "loadmissing"
                | "pluck"
                | "count"
                | "exists"
                | "paginate"
                | "simplepaginate"
        )
}

#[derive(Debug, Clone)]
struct StaticStringKey {
    key: String,
    range: (u32, u32, u32, u32),
}

fn is_laravel_string_key_layout(root: &Path) -> bool {
    root.join("artisan").is_file()
        || root.join("config").is_dir()
        || root.join("routes").is_dir()
        || root.join("resources/views").is_dir()
        || root.join("resources/lang").is_dir()
        || root.join("lang").is_dir()
}

fn is_symfony_twig_layout(root: &Path) -> bool {
    root.join("templates").is_dir()
        || root.join("symfony.lock").is_file()
        || root.join("bin/console").is_file()
}

fn collect_laravel_config_keys(
    provider_id: &'static str,
    root: &Path,
    prefix: &str,
) -> Vec<FrameworkStringKey> {
    let config_dir = root.join("config");
    let mut keys = Vec::new();
    for path in collect_static_files(&config_dir, &["php"], 512) {
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some(uri) = path_to_file_uri(&path) else {
            continue;
        };
        for parsed in parse_php_array_key_paths(&source) {
            let key = format!("{}.{}", stem, parsed.key);
            if key.starts_with(prefix) {
                keys.push(framework_string_key(
                    provider_id,
                    key,
                    "Laravel config key",
                    uri.clone(),
                    parsed.range,
                ));
            }
        }
    }
    keys
}

fn collect_laravel_route_keys(
    provider_id: &'static str,
    root: &Path,
    prefix: &str,
) -> Vec<FrameworkStringKey> {
    let routes_dir = root.join("routes");
    let mut keys = Vec::new();
    for path in collect_static_files(&routes_dir, &["php"], 512) {
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some(uri) = path_to_file_uri(&path) else {
            continue;
        };
        for parsed in parse_named_call_string_args(&source, "name") {
            if parsed.key.starts_with(prefix) {
                keys.push(framework_string_key(
                    provider_id,
                    parsed.key,
                    "Laravel route name",
                    uri.clone(),
                    parsed.range,
                ));
            }
        }
    }
    keys
}

fn collect_laravel_translation_keys(
    provider_id: &'static str,
    root: &Path,
    prefix: &str,
) -> Vec<FrameworkStringKey> {
    let mut keys = Vec::new();
    for lang_root in [root.join("resources/lang"), root.join("lang")] {
        if !lang_root.is_dir() {
            continue;
        }
        for path in collect_static_files(&lang_root, &["php"], 2048) {
            let Ok(relative) = path.strip_prefix(&lang_root) else {
                continue;
            };
            let mut components = relative.components();
            if components.next().is_none() {
                continue;
            }
            let relative_without_locale = components.as_path();
            let Some(file_key) = php_key_from_relative_path(relative_without_locale) else {
                continue;
            };
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some(uri) = path_to_file_uri(&path) else {
                continue;
            };
            for parsed in parse_php_array_key_paths(&source) {
                let key = format!("{}.{}", file_key, parsed.key);
                if key.starts_with(prefix) {
                    keys.push(framework_string_key(
                        provider_id,
                        key,
                        "Laravel translation key",
                        uri.clone(),
                        parsed.range,
                    ));
                }
            }
        }
    }
    keys
}

fn collect_laravel_view_keys(
    provider_id: &'static str,
    root: &Path,
    prefix: &str,
) -> Vec<FrameworkStringKey> {
    let view_dir = root.join("resources/views");
    let mut keys = Vec::new();
    for path in collect_static_files(&view_dir, &["php"], 4096) {
        let Ok(relative) = path.strip_prefix(&view_dir) else {
            continue;
        };
        let Some(key) = view_key_from_relative_path(relative) else {
            continue;
        };
        let Some(uri) = path_to_file_uri(&path) else {
            continue;
        };
        if key.starts_with(prefix) {
            keys.push(framework_string_key(
                provider_id,
                key,
                "Laravel view template",
                uri,
                (0, 0, 0, 0),
            ));
        }
    }
    keys
}

fn collect_symfony_twig_template_keys(
    provider_id: &'static str,
    root: &Path,
    prefix: &str,
) -> Vec<FrameworkStringKey> {
    let template_dir = root.join("templates");
    let mut keys = Vec::new();
    for path in collect_static_files(&template_dir, &["twig"], 4096) {
        let Ok(relative) = path.strip_prefix(&template_dir) else {
            continue;
        };
        let Some(key) = twig_template_key_from_relative_path(relative) else {
            continue;
        };
        let Some(uri) = path_to_file_uri(&path) else {
            continue;
        };
        if key.starts_with(prefix) {
            keys.push(framework_string_key(
                provider_id,
                key,
                "Symfony Twig template",
                uri,
                (0, 0, 0, 0),
            ));
        }
    }
    keys
}

fn collect_symfony_route_keys(
    provider_id: &'static str,
    root: &Path,
    prefix: &str,
) -> Vec<FrameworkStringKey> {
    let src_dir = root.join("src");
    let mut keys = Vec::new();
    for path in collect_static_files(&src_dir, &["php"], 4096) {
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some(uri) = path_to_file_uri(&path) else {
            continue;
        };
        for parsed in parse_symfony_route_attribute_names(&source) {
            if parsed.key.starts_with(prefix) {
                keys.push(framework_string_key(
                    provider_id,
                    parsed.key,
                    "Symfony route name",
                    uri.clone(),
                    parsed.range,
                ));
            }
        }
    }
    keys
}

fn framework_string_key(
    provider_id: &'static str,
    key: String,
    detail: &'static str,
    uri: String,
    range: (u32, u32, u32, u32),
) -> FrameworkStringKey {
    FrameworkStringKey {
        key,
        detail: Some(detail.to_string()),
        provider_ids: vec![provider_id],
        sources: vec![VirtualMemberSource::SourceRange { uri, range }],
    }
}

fn collect_static_files(root: &Path, extensions: &[&str], limit: usize) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_static_files_recursive(root, extensions, limit, &mut files);
    files.sort();
    files
}

fn collect_static_files_recursive(
    root: &Path,
    extensions: &[&str],
    limit: usize,
    files: &mut Vec<PathBuf>,
) {
    if files.len() >= limit || !root.is_dir() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        if files.len() >= limit {
            return;
        }
        let path = entry.path();
        if path.is_dir() {
            collect_static_files_recursive(&path, extensions, limit, files);
        } else if path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| {
                extensions
                    .iter()
                    .any(|expected| ext.eq_ignore_ascii_case(expected))
            })
        {
            files.push(path);
        }
    }
}

fn parse_php_array_key_paths(source: &str) -> Vec<StaticStringKey> {
    let mut keys = Vec::new();
    parse_php_array_key_paths_in(source, 0, source.len(), &[], &mut keys);
    keys
}

fn parse_php_array_key_paths_in(
    source: &str,
    start: usize,
    end: usize,
    path: &[String],
    keys: &mut Vec<StaticStringKey>,
) {
    let mut index = start;
    while index < end {
        let Some((value, quote_start, quote_end)) = next_quoted_string(source, index) else {
            break;
        };
        if quote_start >= end {
            break;
        }

        let after_quote = skip_ascii_ws(source, quote_end);
        if !source[after_quote..].starts_with("=>") {
            index = quote_end;
            continue;
        }

        let mut full_path = path.to_vec();
        full_path.push(value);
        let key = full_path.join(".");
        keys.push(StaticStringKey {
            key,
            range: range_for_offsets(source, quote_start + 1, quote_end.saturating_sub(1)),
        });

        let value_start = skip_ascii_ws(source, after_quote + 2);
        if source[value_start..].starts_with('[') {
            if let Some(close) = find_matching_delimiter(source, value_start, '[', ']') {
                parse_php_array_key_paths_in(source, value_start + 1, close, &full_path, keys);
                index = close + 1;
                continue;
            }
        } else if source[value_start..].starts_with("array") {
            let after_array = skip_ascii_ws(source, value_start + "array".len());
            if source[after_array..].starts_with('(') {
                if let Some(close) = find_matching_delimiter(source, after_array, '(', ')') {
                    parse_php_array_key_paths_in(source, after_array + 1, close, &full_path, keys);
                    index = close + 1;
                    continue;
                }
            }
        }

        index = quote_end;
    }
}

fn parse_named_call_string_args(source: &str, call_name: &str) -> Vec<StaticStringKey> {
    let mut keys = Vec::new();
    let mut index = 0usize;
    let needle = format!("{call_name}(");
    while let Some(relative) = source[index..].find(&needle) {
        let name_start = index + relative;
        if name_start > 0 {
            let previous = source[..name_start].chars().next_back().unwrap_or_default();
            if previous.is_alphanumeric() || previous == '_' {
                index = name_start + call_name.len();
                continue;
            }
        }
        let arg_start = skip_ascii_ws(source, name_start + needle.len());
        if let Some((value, quote_start, quote_end)) = next_quoted_string(source, arg_start) {
            if quote_start == arg_start {
                keys.push(StaticStringKey {
                    key: value,
                    range: range_for_offsets(source, quote_start + 1, quote_end.saturating_sub(1)),
                });
            }
        }
        index = name_start + needle.len();
    }
    keys
}

fn parse_symfony_route_attribute_names(source: &str) -> Vec<StaticStringKey> {
    let mut keys = Vec::new();
    let mut index = 0usize;
    while let Some(relative) = source[index..].find("#[") {
        let group_start = index + relative;
        let bracket_start = group_start + 1;
        let Some(group_end) = find_matching_delimiter(source, bracket_start, '[', ']') else {
            break;
        };
        parse_symfony_route_attribute_group(source, bracket_start + 1, group_end, &mut keys);
        index = group_end + 1;
    }
    keys
}

fn parse_symfony_route_attribute_group(
    source: &str,
    start: usize,
    end: usize,
    keys: &mut Vec<StaticStringKey>,
) {
    let mut index = start;
    while index < end {
        let Some(relative) = source[index..end].find("Route") else {
            break;
        };
        let name_start = index + relative;
        if !attribute_name_boundary_before(source, name_start) {
            index = name_start + "Route".len();
            continue;
        }
        let after_name = skip_ascii_ws(source, name_start + "Route".len());
        if after_name >= end || source.as_bytes().get(after_name) != Some(&b'(') {
            index = name_start + "Route".len();
            continue;
        }
        let Some(args_end) = find_matching_delimiter(source, after_name, '(', ')') else {
            break;
        };
        if args_end <= end {
            if let Some(key) = parse_named_string_argument(source, after_name + 1, args_end, "name")
            {
                keys.push(key);
            }
        }
        index = args_end.saturating_add(1);
    }
}

fn parse_named_string_argument(
    source: &str,
    start: usize,
    end: usize,
    argument_name: &str,
) -> Option<StaticStringKey> {
    let mut index = start;
    while index < end {
        let Some(relative) = source[index..end].find(argument_name) else {
            break;
        };
        let name_start = index + relative;
        let name_end = name_start + argument_name.len();
        if !identifier_boundary(source, name_start, name_end) {
            index = name_end;
            continue;
        }
        let separator = skip_ascii_ws(source, name_end);
        if separator >= end || source.as_bytes().get(separator) != Some(&b':') {
            index = name_end;
            continue;
        }
        let value_start = skip_ascii_ws(source, separator + 1);
        let (value, quote_start, quote_end) = next_quoted_string(source, value_start)?;
        if quote_start >= end || quote_start != value_start {
            return None;
        }
        return Some(StaticStringKey {
            key: value,
            range: range_for_offsets(source, quote_start + 1, quote_end.saturating_sub(1)),
        });
    }
    None
}

fn attribute_name_boundary_before(source: &str, start: usize) -> bool {
    source
        .get(..start)
        .and_then(|prefix| prefix.chars().next_back())
        .is_none_or(|ch| !ch.is_alphanumeric() && ch != '_')
}

fn identifier_boundary(source: &str, start: usize, end: usize) -> bool {
    let before_ok = source
        .get(..start)
        .and_then(|prefix| prefix.chars().next_back())
        .is_none_or(|ch| !ch.is_alphanumeric() && ch != '_');
    let after_ok = source
        .get(end..)
        .and_then(|suffix| suffix.chars().next())
        .is_none_or(|ch| !ch.is_alphanumeric() && ch != '_');
    before_ok && after_ok
}

fn php_key_from_relative_path(path: &Path) -> Option<String> {
    let without_ext = path.with_extension("");
    path_components_to_dot_key(&without_ext)
}

fn view_key_from_relative_path(path: &Path) -> Option<String> {
    let file_name = path.file_name()?.to_str()?;
    let without_suffix = file_name
        .strip_suffix(".blade.php")
        .or_else(|| file_name.strip_suffix(".php"))?;
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let combined = if parent.as_os_str().is_empty() {
        PathBuf::from(without_suffix)
    } else {
        parent.join(without_suffix)
    };
    path_components_to_dot_key(&combined)
}

fn twig_template_key_from_relative_path(path: &Path) -> Option<String> {
    let parts: Vec<String> = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect();
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn path_components_to_dot_key(path: &Path) -> Option<String> {
    let parts: Vec<String> = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect();
    (!parts.is_empty()).then(|| parts.join("."))
}

fn skip_ascii_ws(source: &str, mut index: usize) -> usize {
    while index < source.len() && source.as_bytes()[index].is_ascii_whitespace() {
        index += 1;
    }
    index
}

fn find_matching_delimiter(
    source: &str,
    open_index: usize,
    open: char,
    close: char,
) -> Option<usize> {
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (idx, ch) in source[open_index..].char_indices() {
        let idx = open_index + idx;
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active_quote {
                quote = None;
            }
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }
        if ch == open {
            depth += 1;
        } else if ch == close {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some(idx);
            }
        }
    }
    None
}

fn range_for_offsets(source: &str, start: usize, end: usize) -> (u32, u32, u32, u32) {
    let (start_line, start_col) = line_col_for_offset(source, start);
    let (end_line, end_col) = line_col_for_offset(source, end);
    (start_line, start_col, end_line, end_col)
}

fn line_col_for_offset(source: &str, target: usize) -> (u32, u32) {
    let mut line = 0u32;
    let mut line_start = 0usize;
    for (idx, ch) in source.char_indices() {
        if idx >= target {
            break;
        }
        if ch == '\n' {
            line += 1;
            line_start = idx + ch.len_utf8();
        }
    }
    (line, target.saturating_sub(line_start) as u32)
}

fn path_to_file_uri(path: &Path) -> Option<String> {
    path_to_uri(path).ok()
}

fn normalize_fqn(fqn: &str) -> String {
    fqn.trim_start_matches('\\').to_string()
}

fn normalize_member_name(kind: VirtualMemberKind, member_name: &str) -> String {
    match kind {
        VirtualMemberKind::Method | VirtualMemberKind::ClassConstant => {
            member_name.to_ascii_lowercase()
        }
        VirtualMemberKind::Property | VirtualMemberKind::StaticProperty => member_name.to_string(),
    }
}

fn fqn_matches(left: &str, right: &str) -> bool {
    left.trim_start_matches('\\') == right.trim_start_matches('\\')
}

fn hash_workspace_root(root: Option<&Path>) -> u64 {
    let mut hasher = DefaultHasher::new();
    root.map(Path::to_path_buf).hash(&mut hasher);
    hasher.finish()
}

fn hash_source(source: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    source.len().hash(&mut hasher);
    source.hash(&mut hasher);
    hasher.finish()
}

fn hash_namespace_map(namespace_map: &NamespaceMap) -> u64 {
    let mut hasher = DefaultHasher::new();
    let mut entries = Vec::new();

    for (prefix, dirs) in &namespace_map.psr4 {
        entries.push((
            "psr4",
            prefix.clone(),
            dirs.iter().map(PathBuf::from).collect::<Vec<_>>(),
        ));
    }
    for (prefix, dirs) in &namespace_map.psr0 {
        entries.push((
            "psr0",
            prefix.clone(),
            dirs.iter().map(PathBuf::from).collect::<Vec<_>>(),
        ));
    }
    entries.sort_by(|left, right| left.0.cmp(right.0).then(left.1.cmp(&right.1)));
    for (kind, prefix, mut dirs) in entries {
        kind.hash(&mut hasher);
        prefix.hash(&mut hasher);
        dirs.sort();
        dirs.hash(&mut hasher);
    }

    let mut classmap = namespace_map.classmap.clone();
    classmap.sort();
    classmap.hash(&mut hasher);

    let mut files = namespace_map.files.clone();
    files.sort();
    files.hash(&mut hasher);

    hasher.finish()
}

fn hash_relevant_files(paths: &[PathBuf]) -> u64 {
    let mut hasher = DefaultHasher::new();
    let mut paths = paths.to_vec();
    paths.sort();
    for path in paths {
        path.hash(&mut hasher);
        match std::fs::metadata(&path) {
            Ok(metadata) => {
                metadata.len().hash(&mut hasher);
                if let Ok(modified) = metadata.modified() {
                    match modified.duration_since(UNIX_EPOCH) {
                        Ok(duration) => duration.as_nanos().hash(&mut hasher),
                        Err(err) => err.duration().as_nanos().hash(&mut hasher),
                    }
                }
            }
            Err(_) => {
                "missing".hash(&mut hasher);
            }
        }
    }
    hasher.finish()
}

#[cfg(test)]
mod tests {
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
        let tmp =
            std::env::temp_dir().join(format!("php-lsp-framework-cache-{}", std::process::id()));
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
class Builder {}

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
        assert!(
            VirtualMemberQuery::from_ref_kind("App\\User", "User", RefKind::ClassName).is_none()
        );
    }
}
