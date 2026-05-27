//! Framework-aware static providers.
//!
//! Providers in this module are intentionally static: they receive readonly
//! workspace/index context and must not bootstrap applications, open databases,
//! or execute user code.

use php_lsp_index::composer::NamespaceMap;
use php_lsp_index::workspace::WorkspaceIndex;
use php_lsp_parser::resolve::RefKind;
use php_lsp_types::{FileSymbols, TypeInfo};
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
        Self {
            fqn: format!("{}::{}", owner_fqn, name),
            name,
            owner_fqn,
            kind,
            type_info: None,
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
static LARAVEL_ELOQUENT_PROVIDER: LaravelEloquentProvider = LaravelEloquentProvider;

pub(crate) fn default_framework_provider_registry() -> FrameworkProviderRegistry<'static> {
    FrameworkProviderRegistry::new(vec![
        &DOCTRINE_REPOSITORY_PROVIDER,
        &SYMFONY_CONTROLLER_PROVIDER,
        &LARAVEL_ELOQUENT_PROVIDER,
    ])
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
        let is_model =
            ctx.class_is_or_extends(&query.owner_fqn, "Illuminate\\Database\\Eloquent\\Model");
        let is_builder = ctx
            .class_is_or_extends(&query.owner_fqn, "Illuminate\\Database\\Eloquent\\Builder")
            || ctx.class_is_or_extends(&query.owner_fqn, "Illuminate\\Database\\Query\\Builder")
            || ctx.class_is_or_extends(
                &query.owner_fqn,
                "Illuminate\\Database\\Eloquent\\Relations\\Relation",
            );

        let accepted = match query.kind {
            VirtualMemberKind::Method => {
                (is_model || is_builder) && is_laravel_eloquent_dynamic_method(&query.member_name)
            }
            VirtualMemberKind::Property => is_model,
            VirtualMemberKind::StaticProperty | VirtualMemberKind::ClassConstant => false,
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
