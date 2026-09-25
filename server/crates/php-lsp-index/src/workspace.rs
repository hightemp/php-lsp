//! Global workspace symbol index.

use dashmap::{iter::Iter, mapref::one::Ref, DashMap};
use parking_lot::{RwLock as PublicationLock, RwLockReadGuard as PublicationReadGuard};
use php_lsp_types::{
    global_constant_fqn_key, symbol_fqn_eq, ArrayShapeItem, FileSymbols, PhpSymbolKind, Signature,
    SymbolInfo, SymbolReference, TemplateBindingKind, TypeInfo,
};
use std::{
    borrow::Borrow,
    collections::{HashMap, HashSet},
    hash::Hash,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, RwLock,
    },
};

type TemplateSubstitutions = HashMap<String, TypeInfo>;
const MAX_TYPE_ALIAS_EXPANSION_DEPTH: usize = 32;
const MAX_COMMITTED_SNAPSHOT_RETRIES: usize = 8;

/// Dependencies of one synchronous derived-data computation, including misses.
#[derive(Default, Clone)]
pub struct IndexReadDependencies {
    pub files: HashSet<String>,
    pub symbols: HashSet<String>,
    pub whole_index: bool,
}

std::thread_local! {
    static READ_DEPENDENCIES: std::cell::RefCell<Option<(usize, IndexReadDependencies)>> = const { std::cell::RefCell::new(None) };
}

struct RestoreReadDependencies(Option<(usize, IndexReadDependencies)>);
impl Drop for RestoreReadDependencies {
    fn drop(&mut self) {
        READ_DEPENDENCIES.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}

#[derive(Clone)]
struct DirectMemberSource {
    uri: Arc<str>,
    file_symbols: Arc<FileSymbols>,
    symbol_indices: Arc<[usize]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeIndexGeneration {
    type_fqn: String,
    uri: String,
    generation: u64,
}

impl TypeIndexGeneration {
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

#[derive(Debug, Clone)]
pub struct CommittedTypeSnapshot {
    pub symbol: Arc<SymbolInfo>,
    pub generation: TypeIndexGeneration,
}

/// Identity of the exact file bytes used to extract one disk symbol snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceFingerprint {
    pub size: u64,
    pub content_hash: u64,
}

impl SourceFingerprint {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            size: bytes.len() as u64,
            content_hash: crate::cache::stable_hash_bytes(bytes),
        }
    }
}

struct TypeResolutionSnapshot {
    type_snapshot: CommittedTypeSnapshot,
    direct_members: Vec<Arc<SymbolInfo>>,
}

fn member_kind_matches(kind: PhpSymbolKind, expected_kinds: Option<&[PhpSymbolKind]>) -> bool {
    expected_kinds.is_none_or(|kinds| kinds.contains(&kind))
}

fn case_insensitive_fqn_key(fqn: &str) -> String {
    fqn.trim_start_matches('\\').to_ascii_lowercase()
}

fn top_level_symbol_key(symbol: &SymbolInfo) -> String {
    match symbol.kind {
        PhpSymbolKind::Class
        | PhpSymbolKind::Interface
        | PhpSymbolKind::Trait
        | PhpSymbolKind::Enum
        | PhpSymbolKind::Function => case_insensitive_fqn_key(&symbol.fqn),
        PhpSymbolKind::GlobalConstant => global_constant_fqn_key(&symbol.fqn),
        _ => symbol.fqn.trim_start_matches('\\').to_string(),
    }
}

fn top_level_symbol_kinds_share_table(left: PhpSymbolKind, right: PhpSymbolKind) -> bool {
    match right {
        PhpSymbolKind::Class
        | PhpSymbolKind::Interface
        | PhpSymbolKind::Trait
        | PhpSymbolKind::Enum => matches!(
            left,
            PhpSymbolKind::Class
                | PhpSymbolKind::Interface
                | PhpSymbolKind::Trait
                | PhpSymbolKind::Enum
        ),
        PhpSymbolKind::Function => left == PhpSymbolKind::Function,
        PhpSymbolKind::GlobalConstant => left == PhpSymbolKind::GlobalConstant,
        _ => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum TopLevelSymbolTable {
    Types,
    Functions,
    Constants,
}

fn top_level_generation_key(symbol: &SymbolInfo) -> Option<(TopLevelSymbolTable, String)> {
    let table = match symbol.kind {
        PhpSymbolKind::Class
        | PhpSymbolKind::Interface
        | PhpSymbolKind::Trait
        | PhpSymbolKind::Enum => TopLevelSymbolTable::Types,
        PhpSymbolKind::Function => TopLevelSymbolTable::Functions,
        PhpSymbolKind::GlobalConstant => TopLevelSymbolTable::Constants,
        _ => return None,
    };
    Some((table, top_level_symbol_key(symbol)))
}

fn top_level_generation_keys(file_symbols: &FileSymbols) -> HashSet<(TopLevelSymbolTable, String)> {
    file_symbols
        .symbols
        .iter()
        .filter_map(top_level_generation_key)
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TypeAliasScope {
    Class(String),
    File(String, (u32, u32)),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TypeAliasVisit {
    scope: TypeAliasScope,
    name: String,
}

/// Global index of all symbols in the workspace.
pub struct WorkspaceIndex {
    tables: IndexTables,

    /// Readers hold a shared lease across a lookup; a file commit holds the
    /// exclusive lease across every map and generation it changes.
    publication_barrier: PublicationLock<()>,

    /// File URI → generation and per-URI write barrier for snapshot replacement.
    file_update_generations: DashMap<String, u64>,

    /// Monotonic source for file symbol snapshot generations.
    next_file_symbol_generation: AtomicU64,

    /// File writers share this barrier; aggregate publication briefly takes it exclusively.
    mutation_barrier: RwLock<()>,

    /// Monotonic whole-index revision for staged aggregate validation.
    mutation_revision: AtomicU64,
}

struct IndexTables {
    /// ASCII-lowercased FQN → SymbolInfo for types.
    types: DashMap<String, Arc<SymbolInfo>>,

    /// ASCII-lowercased FQN → SymbolInfo for functions.
    functions: DashMap<String, Arc<SymbolInfo>>,

    /// FQN → SymbolInfo for constants
    constants: DashMap<String, Arc<SymbolInfo>>,

    /// File URI → extracted symbols for that file
    file_symbols: DashMap<String, Arc<FileSymbols>>,

    /// File URI → precomputed non-local symbol references for that file
    file_references: DashMap<String, Vec<SymbolReference>>,

    /// File URI → bytes that produced the published disk snapshot.
    source_fingerprints: DashMap<String, SourceFingerprint>,

    /// ASCII-lowercased parent FQN → compact locations of its direct members.
    direct_members_by_parent: DashMap<String, Arc<[DirectMemberSource]>>,
}

pub struct WorkspaceIndexRead<'a> {
    index: &'a WorkspaceIndex,
    _guard: PublicationReadGuard<'a, ()>,
}

impl WorkspaceIndexRead<'_> {
    pub fn types(&self) -> ReadOnlyIndexMap<'_, String, Arc<SymbolInfo>> {
        ReadOnlyIndexMap {
            map: &self.index.tables.types,
        }
    }

    pub fn functions(&self) -> ReadOnlyIndexMap<'_, String, Arc<SymbolInfo>> {
        ReadOnlyIndexMap {
            map: &self.index.tables.functions,
        }
    }

    pub fn constants(&self) -> ReadOnlyIndexMap<'_, String, Arc<SymbolInfo>> {
        ReadOnlyIndexMap {
            map: &self.index.tables.constants,
        }
    }

    pub fn file_symbols(&self) -> ReadOnlyIndexMap<'_, String, Arc<FileSymbols>> {
        ReadOnlyIndexMap {
            map: &self.index.tables.file_symbols,
        }
    }

    pub fn file_references(&self) -> ReadOnlyIndexMap<'_, String, Vec<SymbolReference>> {
        ReadOnlyIndexMap {
            map: &self.index.tables.file_references,
        }
    }

    pub fn source_fingerprints(&self) -> ReadOnlyIndexMap<'_, String, SourceFingerprint> {
        ReadOnlyIndexMap {
            map: &self.index.tables.source_fingerprints,
        }
    }
}

/// Read-only access to one map while a WorkspaceIndex publication lease is held.
pub struct ReadOnlyIndexMap<'a, K, V> {
    map: &'a DashMap<K, V>,
}

impl<'a, K: Eq + Hash, V> ReadOnlyIndexMap<'a, K, V> {
    pub fn get<Q>(&self, key: &Q) -> Option<Ref<'a, K, V>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.get(key)
    }

    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.contains_key(key)
    }

    pub fn iter(&self) -> Iter<'a, K, V> {
        self.map.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceIndexRevision {
    identity: usize,
    revision: u64,
}

impl WorkspaceIndex {
    /// Trace synchronous lookups without changing the behavior of other readers.
    pub fn trace_read_dependencies<T>(
        &self,
        read: impl FnOnce() -> T,
    ) -> (T, IndexReadDependencies) {
        let previous = READ_DEPENDENCIES.with(|slot| {
            slot.replace(Some((
                self as *const Self as usize,
                IndexReadDependencies::default(),
            )))
        });
        let _restore = RestoreReadDependencies(previous);
        let value = read();
        let dependencies = READ_DEPENDENCIES.with(|slot| slot.borrow_mut().take().unwrap().1);
        (value, dependencies)
    }

    fn record_read(&self, symbol: Option<&str>, file: Option<&str>, whole_index: bool) {
        READ_DEPENDENCIES.with(|slot| {
            if let Some((identity, dependencies)) = slot.borrow_mut().as_mut() {
                if *identity != self as *const Self as usize {
                    return;
                }
                if let Some(symbol) = symbol {
                    dependencies
                        .symbols
                        .insert(symbol.trim_start_matches('\\').to_ascii_lowercase());
                }
                if let Some(file) = file {
                    dependencies.files.insert(file.to_string());
                }
                dependencies.whole_index |= whole_index;
            }
        });
    }

    /// Mark a direct enumeration whose dependency cannot be narrowed to a name.
    pub fn observe_type_inventory(&self) {
        self.record_read(None, None, true);
    }

    /// Create a new empty index.
    pub fn new() -> Self {
        WorkspaceIndex {
            tables: IndexTables {
                types: DashMap::new(),
                functions: DashMap::new(),
                constants: DashMap::new(),
                file_symbols: DashMap::new(),
                file_references: DashMap::new(),
                source_fingerprints: DashMap::new(),
                direct_members_by_parent: DashMap::new(),
            },
            publication_barrier: PublicationLock::new(()),
            file_update_generations: DashMap::new(),
            next_file_symbol_generation: AtomicU64::new(1),
            mutation_barrier: RwLock::new(()),
            mutation_revision: AtomicU64::new(0),
        }
    }

    /// Pin a coherent published generation while reading several index maps.
    /// Release the lease before mutating this index or awaiting external work.
    pub fn read(&self) -> WorkspaceIndexRead<'_> {
        WorkspaceIndexRead {
            index: self,
            _guard: self.publication_barrier.read_recursive(),
        }
    }

    /// Update symbols from a single file. Removes old symbols, adds new ones.
    pub fn update_file(&self, uri: &str, file_symbols: FileSymbols) {
        self.update_file_with_references(uri, file_symbols, Vec::new());
    }

    /// Update symbols and precomputed references from a single file.
    pub fn update_file_with_references(
        &self,
        uri: &str,
        file_symbols: FileSymbols,
        file_references: Vec<SymbolReference>,
    ) {
        self.update_file_with_references_with_hook(uri, file_symbols, file_references, || {});
    }

    /// Publish a disk parse together with the identity of its source bytes.
    pub fn update_file_with_references_from_source(
        &self,
        uri: &str,
        file_symbols: FileSymbols,
        file_references: Vec<SymbolReference>,
        source: SourceFingerprint,
    ) {
        let _mutation = self
            .mutation_barrier
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _publication = self.publication_barrier.write();
        self.update_file_with_references_unguarded(
            uri,
            file_symbols,
            file_references,
            Some(source),
            || {},
            || {},
        );
    }

    pub fn update_file_from_source(
        &self,
        uri: &str,
        file_symbols: FileSymbols,
        source: SourceFingerprint,
    ) {
        self.update_file_with_references_from_source(uri, file_symbols, Vec::new(), source);
    }

    fn update_file_with_references_with_hook<F>(
        &self,
        uri: &str,
        file_symbols: FileSymbols,
        file_references: Vec<SymbolReference>,
        before_direct_member_publish: F,
    ) where
        F: FnOnce(),
    {
        self.update_file_with_references_with_hooks(
            uri,
            file_symbols,
            file_references,
            || {},
            before_direct_member_publish,
        );
    }

    fn update_file_with_references_with_hooks<F, G>(
        &self,
        uri: &str,
        file_symbols: FileSymbols,
        file_references: Vec<SymbolReference>,
        before_top_level_publish: F,
        before_direct_member_publish: G,
    ) where
        F: FnOnce(),
        G: FnOnce(),
    {
        let _mutation = self
            .mutation_barrier
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _publication = self.publication_barrier.write();
        self.update_file_with_references_unguarded(
            uri,
            file_symbols,
            file_references,
            None,
            before_top_level_publish,
            before_direct_member_publish,
        );
    }

    fn update_file_with_references_unguarded<F, G>(
        &self,
        uri: &str,
        file_symbols: FileSymbols,
        file_references: Vec<SymbolReference>,
        source: Option<SourceFingerprint>,
        before_top_level_publish: F,
        before_direct_member_publish: G,
    ) where
        F: FnOnce(),
        G: FnOnce(),
    {
        let uri_key = uri.to_string();
        // The mutable generation guard serializes writers of this URI. The
        // publication lease keeps readers out until all maps are committed.
        let mut generation_guard = self
            .file_update_generations
            .entry(uri_key.clone())
            .or_insert(0);
        let (old_direct_member_parents, old_file_symbols) = self.take_file_snapshot(uri);
        before_top_level_publish();

        let generation = self
            .next_file_symbol_generation
            .fetch_add(1, Ordering::Relaxed);
        let mut direct_member_indices: HashMap<String, Vec<usize>> = HashMap::new();
        let file_symbols = Arc::new(file_symbols);

        // Add new symbols to global indices and collect compact direct-member locators.
        for (symbol_index, sym) in file_symbols.symbols.iter().enumerate() {
            if let Some(parent_fqn) = sym.parent_fqn.as_deref() {
                direct_member_indices
                    .entry(case_insensitive_fqn_key(parent_fqn))
                    .or_default()
                    .push(symbol_index);
            }
            match sym.kind {
                PhpSymbolKind::Class
                | PhpSymbolKind::Interface
                | PhpSymbolKind::Trait
                | PhpSymbolKind::Enum => {
                    self.tables
                        .types
                        .insert(case_insensitive_fqn_key(&sym.fqn), Arc::new(sym.clone()));
                }
                PhpSymbolKind::Function => {
                    self.tables
                        .functions
                        .insert(case_insensitive_fqn_key(&sym.fqn), Arc::new(sym.clone()));
                }
                PhpSymbolKind::GlobalConstant => {
                    self.tables
                        .constants
                        .insert(global_constant_fqn_key(&sym.fqn), Arc::new(sym.clone()));
                }
                // Members are stored through compact locators below.
                _ => {}
            }
        }
        if let Some(old_file_symbols) = old_file_symbols.as_ref() {
            self.remove_replaced_top_level_symbols(uri, old_file_symbols, &file_symbols);
        }

        // Replace the file snapshot and references before updating member
        // locators; the publication lease hides this intermediate state.
        let member_uri: Arc<str> = Arc::from(uri);
        self.tables
            .file_symbols
            .insert(uri_key.clone(), Arc::clone(&file_symbols));
        self.tables.file_references.insert(uri_key, file_references);
        if let Some(source) = source {
            self.tables
                .source_fingerprints
                .insert(uri.to_string(), source);
        }
        let new_direct_member_parents = direct_member_indices
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        let removed_direct_member_parents = old_direct_member_parents
            .difference(&new_direct_member_parents)
            .cloned()
            .collect::<HashSet<_>>();
        self.remove_direct_member_sources(uri, &removed_direct_member_parents);
        before_direct_member_publish();
        for (parent_key, symbol_indices) in direct_member_indices {
            self.insert_direct_member_source(
                parent_key,
                DirectMemberSource {
                    uri: Arc::clone(&member_uri),
                    file_symbols: Arc::clone(&file_symbols),
                    symbol_indices: Arc::from(symbol_indices),
                },
            );
        }
        *generation_guard = generation;
        self.mutation_revision.fetch_add(1, Ordering::SeqCst);
    }

    /// Remove all symbols from a file.
    pub fn remove_file(&self, uri: &str) {
        self.remove_file_with_hook(uri, || {});
    }

    fn remove_file_with_hook<F>(&self, uri: &str, after_file_snapshot: F)
    where
        F: FnOnce(),
    {
        let _mutation = self
            .mutation_barrier
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _publication = self.publication_barrier.write();
        self.remove_file_unguarded_with_hook(uri, after_file_snapshot);
    }

    fn remove_file_unguarded(&self, uri: &str) {
        self.remove_file_unguarded_with_hook(uri, || {});
    }

    fn remove_file_unguarded_with_hook<F>(&self, uri: &str, after_file_snapshot: F)
    where
        F: FnOnce(),
    {
        match self.file_update_generations.entry(uri.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(entry) => {
                let direct_member_parents =
                    self.remove_file_snapshot_with_hook(uri, after_file_snapshot);
                self.remove_direct_member_sources(uri, &direct_member_parents);
                entry.remove();
            }
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                let direct_member_parents =
                    self.remove_file_snapshot_with_hook(uri, after_file_snapshot);
                self.remove_direct_member_sources(uri, &direct_member_parents);
                drop(entry);
            }
        }
        self.mutation_revision.fetch_add(1, Ordering::SeqCst);
    }

    pub fn revision_snapshot(&self) -> WorkspaceIndexRevision {
        WorkspaceIndexRevision {
            identity: self as *const Self as usize,
            revision: self.mutation_revision.load(Ordering::SeqCst),
        }
    }

    /// Publish derived data only while the source revision is unchanged.
    /// The callback must not mutate this index (writers take the same barrier).
    pub fn with_revision<T>(
        &self,
        expected: WorkspaceIndexRevision,
        publish: impl FnOnce() -> T,
    ) -> Option<T> {
        let _barrier = self
            .mutation_barrier
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (self.revision_snapshot() == expected).then(publish)
    }

    pub fn replace_from_staged_if_sources_current(
        &self,
        staged: &WorkspaceIndex,
        source_indexes: &[Arc<WorkspaceIndex>],
        expected_revisions: &[WorkspaceIndexRevision],
    ) -> bool {
        let staged_read = staged.read();
        let staged_files = staged_read
            .file_symbols()
            .iter()
            .map(|entry| {
                let uri = entry.key().clone();
                let references = staged_read
                    .file_references()
                    .get(&uri)
                    .map(|references| references.value().clone())
                    .unwrap_or_default();
                let source = staged_read
                    .source_fingerprints()
                    .get(&uri)
                    .map(|source| *source.value());
                (uri, entry.value().as_ref().clone(), references, source)
            })
            .collect::<Vec<_>>();
        drop(staged_read);

        let mut indexes = source_indexes
            .iter()
            .map(Arc::as_ref)
            .chain(std::iter::once(self))
            .collect::<Vec<_>>();
        indexes.sort_by_key(|index| *index as *const Self as usize);
        indexes.dedup_by_key(|index| *index as *const Self as usize);
        let _guards = indexes
            .iter()
            .map(|index| {
                index
                    .mutation_barrier
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            })
            .collect::<Vec<_>>();

        let _publication = self.publication_barrier.write();

        if source_indexes.iter().any(|index| {
            let actual = index.revision_snapshot();
            !expected_revisions.contains(&actual)
        }) {
            return false;
        }

        let destination_uris = self
            .tables
            .file_symbols
            .iter()
            .map(|entry| entry.key().clone())
            .collect::<Vec<_>>();
        for uri in destination_uris {
            self.remove_file_unguarded(&uri);
        }
        for (uri, symbols, references, source) in staged_files {
            self.update_file_with_references_unguarded(
                &uri,
                symbols,
                references,
                source,
                || {},
                || {},
            );
        }
        true
    }

    fn remove_file_snapshot_with_hook<F>(
        &self,
        uri: &str,
        after_file_snapshot: F,
    ) -> HashSet<String>
    where
        F: FnOnce(),
    {
        let (direct_member_parents, old_file_symbols) = self.take_file_snapshot(uri);
        after_file_snapshot();
        if let Some(old_symbols) = old_file_symbols {
            for sym in &old_symbols.symbols {
                match sym.kind {
                    PhpSymbolKind::Class
                    | PhpSymbolKind::Interface
                    | PhpSymbolKind::Trait
                    | PhpSymbolKind::Enum => {
                        self.remove_top_level_symbol(uri, sym, &self.tables.types);
                    }
                    PhpSymbolKind::Function => {
                        self.remove_top_level_symbol(uri, sym, &self.tables.functions);
                    }
                    PhpSymbolKind::GlobalConstant => {
                        self.remove_top_level_symbol(uri, sym, &self.tables.constants);
                    }
                    _ => {}
                }
            }
        }
        direct_member_parents
    }

    fn take_file_snapshot(&self, uri: &str) -> (HashSet<String>, Option<Arc<FileSymbols>>) {
        self.tables.file_references.remove(uri);
        self.tables.source_fingerprints.remove(uri);
        let old_file_symbols = self
            .tables
            .file_symbols
            .remove(uri)
            .map(|(_, symbols)| symbols);
        let direct_member_parents = old_file_symbols
            .iter()
            .flat_map(|symbols| symbols.symbols.iter())
            .filter_map(|symbol| symbol.parent_fqn.as_deref())
            .map(case_insensitive_fqn_key)
            .collect();
        (direct_member_parents, old_file_symbols)
    }

    fn remove_replaced_top_level_symbols(
        &self,
        uri: &str,
        old_file_symbols: &FileSymbols,
        new_file_symbols: &FileSymbols,
    ) {
        let new_top_level_keys = top_level_generation_keys(new_file_symbols);
        for old_symbol in &old_file_symbols.symbols {
            let Some(old_key) = top_level_generation_key(old_symbol) else {
                continue;
            };
            if new_top_level_keys.contains(&old_key) {
                continue;
            }
            match old_symbol.kind {
                PhpSymbolKind::Class
                | PhpSymbolKind::Interface
                | PhpSymbolKind::Trait
                | PhpSymbolKind::Enum => {
                    self.remove_top_level_symbol(uri, old_symbol, &self.tables.types);
                }
                PhpSymbolKind::Function => {
                    self.remove_top_level_symbol(uri, old_symbol, &self.tables.functions);
                }
                PhpSymbolKind::GlobalConstant => {
                    self.remove_top_level_symbol(uri, old_symbol, &self.tables.constants);
                }
                _ => {}
            }
        }
    }

    fn insert_direct_member_source(&self, parent_key: String, source: DirectMemberSource) {
        match self.tables.direct_members_by_parent.entry(parent_key) {
            dashmap::mapref::entry::Entry::Occupied(mut entry) => {
                let mut sources = entry
                    .get()
                    .iter()
                    .filter(|existing| existing.uri.as_ref() != source.uri.as_ref())
                    .cloned()
                    .collect::<Vec<_>>();
                sources.push(source);
                entry.insert(Arc::from(sources));
            }
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                entry.insert(Arc::from(vec![source]));
            }
        }
    }

    fn remove_direct_member_sources(&self, uri: &str, parent_keys: &HashSet<String>) {
        for parent_key in parent_keys {
            let dashmap::mapref::entry::Entry::Occupied(mut entry) = self
                .tables
                .direct_members_by_parent
                .entry(parent_key.clone())
            else {
                continue;
            };
            let sources = entry
                .get()
                .iter()
                .filter(|source| source.uri.as_ref() != uri)
                .cloned()
                .collect::<Vec<_>>();
            if sources.is_empty() {
                entry.remove();
            } else if sources.len() != entry.get().len() {
                entry.insert(Arc::from(sources));
            }
        }
    }

    fn remove_top_level_symbol(
        &self,
        removed_uri: &str,
        removed_symbol: &SymbolInfo,
        symbols: &DashMap<String, Arc<SymbolInfo>>,
    ) {
        let key = top_level_symbol_key(removed_symbol);
        let should_remove = symbols
            .get(&key)
            .is_some_and(|entry| entry.uri == removed_uri);
        if !should_remove {
            return;
        }

        symbols.remove(&key);

        if let Some(replacement) = self.find_top_level_symbol_replacement(removed_symbol) {
            symbols.insert(top_level_symbol_key(&replacement), replacement);
        }
    }

    fn find_top_level_symbol_replacement(
        &self,
        removed_symbol: &SymbolInfo,
    ) -> Option<Arc<SymbolInfo>> {
        self.tables.file_symbols.iter().find_map(|entry| {
            entry
                .symbols
                .iter()
                .find(|candidate| {
                    top_level_symbol_kinds_share_table(candidate.kind, removed_symbol.kind)
                        && symbol_fqn_eq(&candidate.fqn, &removed_symbol.fqn, removed_symbol.kind)
                })
                .cloned()
                .map(Arc::new)
        })
    }

    /// Resolve a fully qualified name to a symbol.
    ///
    /// Handles both top-level symbols (`App\Foo`) and member symbols
    /// (`App\Foo::method`, `App\Foo::CONST`, `App\Foo::$prop`).
    pub fn resolve_fqn(&self, fqn: &str) -> Option<Arc<SymbolInfo>> {
        let _publication = self.publication_barrier.read_recursive();
        self.record_read(Some(fqn), None, false);
        let normalized = fqn.trim_start_matches('\\');
        let case_insensitive_key = case_insensitive_fqn_key(normalized);

        if let Some(sym) = self
            .tables
            .types
            .get(&case_insensitive_key)
            .map(|entry| entry.value().clone())
        {
            return Some(self.materialize_symbol(sym, &TemplateSubstitutions::new()));
        }
        if let Some(sym) = self
            .tables
            .functions
            .get(&case_insensitive_key)
            .map(|entry| entry.value().clone())
        {
            return Some(self.materialize_symbol(sym, &TemplateSubstitutions::new()));
        }
        if let Some(sym) = self
            .tables
            .constants
            .get(&global_constant_fqn_key(normalized))
            .map(|entry| entry.value().clone())
        {
            return Some(self.materialize_symbol(sym, &TemplateSubstitutions::new()));
        }

        self.resolve_member(normalized)
    }

    /// Resolve an FQN to a symbol of one of the expected kinds.
    ///
    /// Top-level PHP symbol tables are independent, so a class and a function
    /// may legally share the same case-insensitive FQN. Selecting the symbol
    /// before checking its kind would make the map lookup order observable.
    pub fn resolve_fqn_matching_kinds(
        &self,
        fqn: &str,
        expected_kinds: &[PhpSymbolKind],
    ) -> Option<Arc<SymbolInfo>> {
        let _publication = self.publication_barrier.read_recursive();
        self.record_read(Some(fqn), None, false);
        let normalized = fqn.trim_start_matches('\\');
        if normalized.contains("::") {
            return self.resolve_member_matching_kinds(normalized, expected_kinds);
        }

        let case_insensitive_key = case_insensitive_fqn_key(normalized);
        if let Some(sym) = self
            .tables
            .types
            .get(&case_insensitive_key)
            .map(|entry| entry.value().clone())
            .filter(|symbol| expected_kinds.contains(&symbol.kind))
        {
            return Some(self.materialize_symbol(sym, &TemplateSubstitutions::new()));
        }
        if let Some(sym) = self
            .tables
            .functions
            .get(&case_insensitive_key)
            .map(|entry| entry.value().clone())
            .filter(|symbol| expected_kinds.contains(&symbol.kind))
        {
            return Some(self.materialize_symbol(sym, &TemplateSubstitutions::new()));
        }
        if let Some(sym) = self
            .tables
            .constants
            .get(&global_constant_fqn_key(normalized))
            .map(|entry| entry.value().clone())
            .filter(|symbol| expected_kinds.contains(&symbol.kind))
        {
            return Some(self.materialize_symbol(sym, &TemplateSubstitutions::new()));
        }

        None
    }

    /// Return whether a class-like symbol exists using PHP's casing rules.
    pub fn contains_type(&self, fqn: &str) -> bool {
        let _publication = self.publication_barrier.read_recursive();
        self.record_read(Some(fqn), None, false);
        self.tables
            .types
            .contains_key(&case_insensitive_fqn_key(fqn))
    }

    /// Get a class-like symbol using PHP's casing rules.
    pub fn get_type(&self, fqn: &str) -> Option<Arc<SymbolInfo>> {
        let _publication = self.publication_barrier.read_recursive();
        self.record_read(Some(fqn), None, false);
        self.tables
            .types
            .get(&case_insensitive_fqn_key(fqn))
            .map(|entry| entry.value().clone())
    }

    /// Return a type only after the complete file generation that published it
    /// (including direct-member locators) is visible.
    pub fn get_committed_type(&self, fqn: &str) -> Option<CommittedTypeSnapshot> {
        let _publication = self.publication_barrier.read_recursive();
        self.committed_type_snapshot_with(fqn, || ())
            .map(|(snapshot, ())| snapshot)
    }

    /// Check whether a previously observed committed type generation is still current.
    pub fn type_generation_is_current(&self, expected: &TypeIndexGeneration) -> bool {
        let _publication = self.publication_barrier.read_recursive();
        let Some(generation) = self.file_update_generations.get(&expected.uri) else {
            return false;
        };
        if *generation != expected.generation {
            return false;
        }
        self.get_type(&expected.type_fqn)
            .is_some_and(|symbol| symbol.uri == expected.uri)
    }

    fn committed_type_resolution_snapshot(&self, fqn: &str) -> Option<TypeResolutionSnapshot> {
        self.record_read(Some(fqn), None, false);
        self.committed_type_snapshot_with(fqn, || self.get_direct_members(fqn))
            .map(|(type_snapshot, direct_members)| TypeResolutionSnapshot {
                type_snapshot,
                direct_members,
            })
    }

    fn committed_type_snapshot_with<T>(
        &self,
        fqn: &str,
        capture: impl Fn() -> T,
    ) -> Option<(CommittedTypeSnapshot, T)> {
        for _ in 0..MAX_COMMITTED_SNAPSHOT_RETRIES {
            let before = self.get_type(fqn)?;
            let uri = before.uri.clone();
            let generation_guard = self.file_update_generations.get(&uri)?;
            if *generation_guard == 0 {
                continue;
            }
            let Some(current) = self.get_type(fqn) else {
                continue;
            };
            if !Arc::ptr_eq(&before, &current) || current.uri != uri {
                continue;
            }

            let captured = capture();
            let generation = TypeIndexGeneration {
                type_fqn: current.fqn.clone(),
                uri,
                generation: *generation_guard,
            };
            return Some((
                CommittedTypeSnapshot {
                    symbol: current,
                    generation,
                },
                captured,
            ));
        }
        None
    }

    /// Resolve a `Class::member` FQN to the member symbol.
    ///
    /// First tries exact FQN match (e.g. `App\Foo::test`), then falls back
    /// to matching by name for cases like property access where the FQN has `$`
    /// prefix in the symbol but not in the query.
    /// Walks the class hierarchy (extends/implements) when the member is not
    /// found directly on the given class.
    pub fn resolve_member(&self, fqn: &str) -> Option<Arc<SymbolInfo>> {
        let _publication = self.publication_barrier.read_recursive();
        let (class_fqn, member_name) = fqn.rsplit_once("::")?;
        self.resolve_member_in_hierarchy(
            class_fqn,
            member_name,
            fqn,
            None,
            &mut HashSet::new(),
            &TemplateSubstitutions::new(),
        )
    }

    /// Resolve a `Class::member` FQN to a member symbol of one of the expected kinds.
    pub fn resolve_member_matching_kinds(
        &self,
        fqn: &str,
        expected_kinds: &[PhpSymbolKind],
    ) -> Option<Arc<SymbolInfo>> {
        let _publication = self.publication_barrier.read_recursive();
        let (class_fqn, member_name) = fqn.rsplit_once("::")?;
        self.resolve_member_in_hierarchy(
            class_fqn,
            member_name,
            fqn,
            Some(expected_kinds),
            &mut HashSet::new(),
            &TemplateSubstitutions::new(),
        )
    }

    /// Internal helper: resolve member walking the inheritance chain.
    /// `visited` prevents infinite loops when there are circular references.
    fn resolve_member_in_hierarchy(
        &self,
        class_fqn: &str,
        member_name: &str,
        original_fqn: &str,
        expected_kinds: Option<&[PhpSymbolKind]>,
        visited: &mut HashSet<String>,
        substitutions: &TemplateSubstitutions,
    ) -> Option<Arc<SymbolInfo>> {
        if !visited.insert(case_insensitive_fqn_key(class_fqn)) {
            return None;
        }

        let snapshot = self.committed_type_resolution_snapshot(class_fqn)?;
        let members = snapshot.direct_members;
        // Prefer exact FQN match first
        if let Some(sym) = members.iter().find(|member| {
            member_kind_matches(member.kind, expected_kinds)
                && symbol_fqn_eq(&member.fqn, original_fqn, member.kind)
        }) {
            return Some(self.materialize_symbol(sym.clone(), substitutions));
        }
        // Fallback: match by PHP member lookup semantics.
        if let Some(sym) = members.iter().find(|member| {
            member_kind_matches(member.kind, expected_kinds)
                && member.matches_member_lookup_name(member_name)
        }) {
            return Some(self.materialize_symbol(sym.clone(), substitutions));
        }

        // Walk the class hierarchy: look up extends and implements
        {
            let class_sym = snapshot.type_snapshot.symbol;
            // Try traits first: their members are mixed into the class/trait body.
            for trait_fqn in &class_sym.traits {
                let edge_substitutions =
                    self.template_substitutions_for_edge(&class_sym, trait_fqn, substitutions);
                if let Some(sym) = self.resolve_member_in_hierarchy(
                    trait_fqn,
                    member_name,
                    original_fqn,
                    expected_kinds,
                    visited,
                    &edge_substitutions,
                ) {
                    return Some(sym);
                }
            }
            // Try PHPDoc mixins as member providers.
            for mixin_fqn in class_sym
                .template_bindings
                .iter()
                .filter(|binding| binding.kind == TemplateBindingKind::Mixin)
                .map(|binding| binding.target.as_str())
            {
                let edge_substitutions =
                    self.template_substitutions_for_edge(&class_sym, mixin_fqn, substitutions);
                if let Some(sym) = self.resolve_member_in_hierarchy(
                    mixin_fqn,
                    member_name,
                    original_fqn,
                    expected_kinds,
                    visited,
                    &edge_substitutions,
                ) {
                    return Some(sym);
                }
            }
            // Try parent classes (extends)
            for parent_fqn in &class_sym.extends {
                let edge_substitutions =
                    self.template_substitutions_for_edge(&class_sym, parent_fqn, substitutions);
                if let Some(sym) = self.resolve_member_in_hierarchy(
                    parent_fqn,
                    member_name,
                    original_fqn,
                    expected_kinds,
                    visited,
                    &edge_substitutions,
                ) {
                    return Some(sym);
                }
            }
            // Try implemented interfaces
            for iface_fqn in &class_sym.implements {
                let edge_substitutions =
                    self.template_substitutions_for_edge(&class_sym, iface_fqn, substitutions);
                if let Some(sym) = self.resolve_member_in_hierarchy(
                    iface_fqn,
                    member_name,
                    original_fqn,
                    expected_kinds,
                    visited,
                    &edge_substitutions,
                ) {
                    return Some(sym);
                }
            }
        }

        None
    }

    /// Search symbols by name (simple substring match for now).
    pub fn search(&self, query: &str) -> Vec<Arc<SymbolInfo>> {
        let _publication = self.publication_barrier.read_recursive();
        let query_lower = query.to_lowercase();
        let mut results = Vec::new();

        for entry in self.tables.types.iter() {
            if entry.value().name.to_lowercase().contains(&query_lower) {
                results.push(entry.value().clone());
            }
        }
        for entry in self.tables.functions.iter() {
            if entry.value().name.to_lowercase().contains(&query_lower) {
                results.push(entry.value().clone());
            }
        }
        for entry in self.tables.constants.iter() {
            if entry.value().name.to_lowercase().contains(&query_lower) {
                results.push(entry.value().clone());
            }
        }

        results
    }

    /// Get members (methods, properties, constants) of a type by its FQN.
    /// Includes inherited members from parent classes and interfaces.
    pub fn get_members(&self, type_fqn: &str) -> Vec<Arc<SymbolInfo>> {
        let _publication = self.publication_barrier.read_recursive();
        let mut members = Vec::new();
        self.collect_members_recursive(
            type_fqn,
            &mut members,
            &mut HashSet::new(),
            &TemplateSubstitutions::new(),
        );
        members
    }

    /// Get a type symbol and all type symbols in its trait/parent/interface hierarchy.
    pub fn get_type_hierarchy_symbols(&self, type_fqn: &str) -> Vec<Arc<SymbolInfo>> {
        let _publication = self.publication_barrier.read_recursive();
        let mut types = Vec::new();
        self.collect_type_hierarchy_symbols(type_fqn, &mut types, &mut HashSet::new());
        types
    }

    /// Get only the direct members of a type (no inheritance traversal).
    fn get_direct_members(&self, type_fqn: &str) -> Vec<Arc<SymbolInfo>> {
        let _publication = self.publication_barrier.read_recursive();
        let parent_key = case_insensitive_fqn_key(type_fqn);
        let Some(sources) = self
            .tables
            .direct_members_by_parent
            .get(&parent_key)
            .map(|entry| Arc::clone(entry.value()))
        else {
            return Vec::new();
        };
        self.direct_members_from_sources(&parent_key, &sources)
    }

    fn direct_members_from_sources(
        &self,
        parent_key: &str,
        sources: &[DirectMemberSource],
    ) -> Vec<Arc<SymbolInfo>> {
        let capacity = sources
            .iter()
            .map(|source| source.symbol_indices.len())
            .sum();
        let mut members = Vec::with_capacity(capacity);
        for source in sources {
            for &symbol_index in source.symbol_indices.iter() {
                let Some(symbol) = source.file_symbols.symbols.get(symbol_index) else {
                    continue;
                };
                let parent_matches = symbol.parent_fqn.as_deref().is_some_and(|parent_fqn| {
                    parent_fqn
                        .trim_start_matches('\\')
                        .eq_ignore_ascii_case(parent_key)
                });
                if !parent_matches {
                    continue;
                }
                members.push(Arc::new(symbol.clone()));
            }
        }
        members
    }

    /// Recursively collect members including those from parent classes/interfaces.
    fn collect_members_recursive(
        &self,
        type_fqn: &str,
        members: &mut Vec<Arc<SymbolInfo>>,
        visited: &mut HashSet<String>,
        substitutions: &TemplateSubstitutions,
    ) {
        if !visited.insert(case_insensitive_fqn_key(type_fqn)) {
            return;
        }

        // Collect direct members
        let Some(snapshot) = self.committed_type_resolution_snapshot(type_fqn) else {
            return;
        };
        let direct = snapshot.direct_members;
        members.extend(
            direct
                .into_iter()
                .map(|sym| self.materialize_symbol(sym, substitutions)),
        );

        // Recurse into parent classes and interfaces
        {
            let class_sym = snapshot.type_snapshot.symbol;
            for trait_fqn in &class_sym.traits {
                let edge_substitutions =
                    self.template_substitutions_for_edge(&class_sym, trait_fqn, substitutions);
                self.collect_members_recursive(trait_fqn, members, visited, &edge_substitutions);
            }
            for mixin_fqn in class_sym
                .template_bindings
                .iter()
                .filter(|binding| binding.kind == TemplateBindingKind::Mixin)
                .map(|binding| binding.target.as_str())
            {
                let edge_substitutions =
                    self.template_substitutions_for_edge(&class_sym, mixin_fqn, substitutions);
                self.collect_members_recursive(mixin_fqn, members, visited, &edge_substitutions);
            }
            for parent_fqn in &class_sym.extends {
                let edge_substitutions =
                    self.template_substitutions_for_edge(&class_sym, parent_fqn, substitutions);
                self.collect_members_recursive(parent_fqn, members, visited, &edge_substitutions);
            }
            for iface_fqn in &class_sym.implements {
                let edge_substitutions =
                    self.template_substitutions_for_edge(&class_sym, iface_fqn, substitutions);
                self.collect_members_recursive(iface_fqn, members, visited, &edge_substitutions);
            }
        }
    }

    fn template_substitutions_for_edge(
        &self,
        from: &SymbolInfo,
        target_fqn: &str,
        inherited: &TemplateSubstitutions,
    ) -> TemplateSubstitutions {
        let Some(binding) = from
            .template_bindings
            .iter()
            .find(|binding| same_fqn(&binding.target, target_fqn))
        else {
            return TemplateSubstitutions::new();
        };

        let Some(target) = self
            .get_committed_type(target_fqn)
            .map(|snapshot| snapshot.symbol)
        else {
            return TemplateSubstitutions::new();
        };

        target
            .templates
            .iter()
            .zip(binding.args.iter())
            .map(|(template, arg)| (template.name.clone(), substitute_type_info(arg, inherited)))
            .collect()
    }

    fn materialize_symbol(
        &self,
        symbol: Arc<SymbolInfo>,
        substitutions: &TemplateSubstitutions,
    ) -> Arc<SymbolInfo> {
        self.record_read(None, Some(&symbol.uri), false);
        if symbol.signature.is_none() && substitutions.is_empty() {
            return symbol;
        }

        let mut materialized = (*symbol).clone();
        let mut changed = false;

        if let Some(signature) = materialized.signature.as_ref() {
            let scope = alias_scope_for_symbol(&materialized);
            let expanded = self.expand_signature_type_aliases(signature, &scope);
            if expanded != *signature {
                materialized.signature = Some(expanded);
                changed = true;
            }
        }

        let mut scoped_substitutions = substitutions.clone();
        for template in &materialized.templates {
            scoped_substitutions.remove(&template.name);
        }
        if !scoped_substitutions.is_empty() {
            materialized.signature = materialized
                .signature
                .as_ref()
                .map(|signature| substitute_signature(signature, &scoped_substitutions));
            changed = true;
        }

        if changed {
            Arc::new(materialized)
        } else {
            symbol
        }
    }

    fn expand_signature_type_aliases(
        &self,
        signature: &Signature,
        scope: &TypeAliasScope,
    ) -> Signature {
        Signature {
            params: signature
                .params
                .iter()
                .map(|param| {
                    let mut param = param.clone();
                    param.type_info = param.type_info.as_ref().map(|type_info| {
                        self.expand_type_aliases(type_info, scope, &mut Vec::new())
                    });
                    param
                })
                .collect(),
            return_type: signature
                .return_type
                .as_ref()
                .map(|type_info| self.expand_type_aliases(type_info, scope, &mut Vec::new())),
        }
    }

    fn expand_type_aliases(
        &self,
        type_info: &TypeInfo,
        scope: &TypeAliasScope,
        visited: &mut Vec<TypeAliasVisit>,
    ) -> TypeInfo {
        match type_info {
            TypeInfo::Simple(name) => self
                .type_alias_for_name(name, scope, visited)
                .unwrap_or_else(|| TypeInfo::Simple(name.clone())),
            TypeInfo::Generic { base, args } => {
                let base = self
                    .type_alias_for_name(base, scope, visited)
                    .unwrap_or_else(|| TypeInfo::Simple(base.clone()));
                let args = args
                    .iter()
                    .map(|arg| self.expand_type_aliases(arg, scope, visited))
                    .collect();
                match base {
                    TypeInfo::Simple(base) => TypeInfo::Generic { base, args },
                    TypeInfo::Generic {
                        base,
                        args: mut base_args,
                    } => {
                        base_args.extend(args);
                        TypeInfo::Generic {
                            base,
                            args: base_args,
                        }
                    }
                    other => other,
                }
            }
            TypeInfo::ArrayShape(items) => {
                TypeInfo::ArrayShape(self.expand_shape_items(items, scope, visited))
            }
            TypeInfo::ObjectShape(items) => {
                TypeInfo::ObjectShape(self.expand_shape_items(items, scope, visited))
            }
            TypeInfo::Callable {
                params,
                return_type,
            } => TypeInfo::Callable {
                params: params
                    .iter()
                    .map(|param| self.expand_type_aliases(param, scope, visited))
                    .collect(),
                return_type: return_type.as_ref().map(|return_type| {
                    Box::new(self.expand_type_aliases(return_type, scope, visited))
                }),
            },
            TypeInfo::ClassString(Some(inner)) => TypeInfo::ClassString(Some(Box::new(
                self.expand_type_aliases(inner, scope, visited),
            ))),
            TypeInfo::ClassString(None) => TypeInfo::ClassString(None),
            TypeInfo::Conditional {
                subject,
                target,
                if_type,
                else_type,
            } => TypeInfo::Conditional {
                subject: subject.clone(),
                target: Box::new(self.expand_type_aliases(target, scope, visited)),
                if_type: Box::new(self.expand_type_aliases(if_type, scope, visited)),
                else_type: Box::new(self.expand_type_aliases(else_type, scope, visited)),
            },
            TypeInfo::Union(types) => TypeInfo::Union(
                types
                    .iter()
                    .map(|type_info| self.expand_type_aliases(type_info, scope, visited))
                    .collect(),
            ),
            TypeInfo::Intersection(types) => TypeInfo::Intersection(
                types
                    .iter()
                    .map(|type_info| self.expand_type_aliases(type_info, scope, visited))
                    .collect(),
            ),
            TypeInfo::Nullable(inner) => {
                TypeInfo::Nullable(Box::new(self.expand_type_aliases(inner, scope, visited)))
            }
            TypeInfo::LiteralString(_)
            | TypeInfo::LiteralInt(_)
            | TypeInfo::LiteralFloat(_)
            | TypeInfo::LiteralBool(_)
            | TypeInfo::LiteralNull
            | TypeInfo::Void
            | TypeInfo::Never
            | TypeInfo::Mixed
            | TypeInfo::Self_
            | TypeInfo::Static_
            | TypeInfo::Parent_ => type_info.clone(),
        }
    }

    fn expand_shape_items(
        &self,
        items: &[ArrayShapeItem],
        scope: &TypeAliasScope,
        visited: &mut Vec<TypeAliasVisit>,
    ) -> Vec<ArrayShapeItem> {
        items
            .iter()
            .map(|item| ArrayShapeItem {
                key: item.key.clone(),
                optional: item.optional,
                value: self.expand_type_aliases(&item.value, scope, visited),
            })
            .collect()
    }

    fn type_alias_for_name(
        &self,
        name: &str,
        scope: &TypeAliasScope,
        visited: &mut Vec<TypeAliasVisit>,
    ) -> Option<TypeInfo> {
        let name = name.trim();
        if name.is_empty()
            || name.starts_with('$')
            || name.contains('\\')
            || is_phpdoc_builtin_type(name)
            || visited.len() >= MAX_TYPE_ALIAS_EXPANSION_DEPTH
        {
            return None;
        }

        let visit = TypeAliasVisit {
            scope: scope.clone(),
            name: name.to_string(),
        };
        if visited.contains(&visit) {
            return None;
        }
        visited.push(visit);

        let resolved = match scope {
            TypeAliasScope::Class(class_fqn) => {
                self.class_type_alias_for_name(class_fqn, name, visited)
            }
            TypeAliasScope::File(uri, position) => {
                self.file_type_alias_for_name(uri, *position, name, visited)
            }
        };

        visited.pop();
        resolved
    }

    fn class_type_alias_for_name(
        &self,
        class_fqn: &str,
        name: &str,
        visited: &mut Vec<TypeAliasVisit>,
    ) -> Option<TypeInfo> {
        let class_symbol = self.get_type(class_fqn)?;
        let file_symbols = self
            .tables
            .file_symbols
            .get(&class_symbol.uri)
            .map(|entry| {
                entry
                    .value()
                    .scoped_at_byte_position(class_symbol.range.0, class_symbol.range.1)
                    .into_owned()
            });
        let phpdoc = class_symbol
            .doc_comment
            .as_deref()
            .map(php_lsp_parser::phpdoc::parse_phpdoc)
            .unwrap_or_default();

        if let Some(alias) = phpdoc.type_aliases.iter().find(|alias| alias.name == name) {
            let type_info = if let Some(file_symbols) = file_symbols.as_ref() {
                let alias_names = visible_alias_names_for_class(&phpdoc, file_symbols);
                let template_names = phpdoc
                    .templates
                    .iter()
                    .map(|template| template.name.clone())
                    .collect();
                resolve_alias_type_names_in_file(
                    &alias.type_info,
                    file_symbols,
                    &alias_names,
                    &template_names,
                )
            } else {
                alias.type_info.clone()
            };
            return Some(self.expand_type_aliases(
                &type_info,
                &TypeAliasScope::Class(class_fqn.to_string()),
                visited,
            ));
        }

        if let Some(import) = phpdoc
            .type_alias_imports
            .iter()
            .find(|import| import.name == name)
        {
            let source_type = file_symbols
                .as_ref()
                .map(|file_symbols| resolve_alias_source_type(&import.source_type, file_symbols))
                .unwrap_or_else(|| import.source_type.trim_start_matches('\\').to_string());
            return self.type_alias_for_name(
                &import.source_alias,
                &TypeAliasScope::Class(source_type),
                visited,
            );
        }

        self.file_type_alias_for_name(
            &class_symbol.uri,
            (class_symbol.range.0, class_symbol.range.1),
            name,
            visited,
        )
    }

    fn file_type_alias_for_name(
        &self,
        uri: &str,
        position: (u32, u32),
        name: &str,
        visited: &mut Vec<TypeAliasVisit>,
    ) -> Option<TypeInfo> {
        let file_symbols = self.tables.file_symbols.get(uri).map(|entry| {
            entry
                .value()
                .scoped_at_byte_position(position.0, position.1)
                .into_owned()
        })?;

        if let Some(alias) = file_symbols
            .type_aliases
            .iter()
            .find(|alias| alias.name == name)
        {
            let alias_names = visible_alias_names_for_file(&file_symbols);
            let template_names = HashSet::new();
            let type_info = resolve_alias_type_names_in_file(
                &alias.type_info,
                &file_symbols,
                &alias_names,
                &template_names,
            );
            return Some(self.expand_type_aliases(
                &type_info,
                &TypeAliasScope::File(uri.to_string(), position),
                visited,
            ));
        }

        if let Some(import) = file_symbols
            .type_alias_imports
            .iter()
            .find(|import| import.name == name)
        {
            let source_type = resolve_alias_source_type(&import.source_type, &file_symbols);
            return self.type_alias_for_name(
                &import.source_alias,
                &TypeAliasScope::Class(source_type),
                visited,
            );
        }

        None
    }

    fn collect_type_hierarchy_symbols(
        &self,
        type_fqn: &str,
        types: &mut Vec<Arc<SymbolInfo>>,
        visited: &mut HashSet<String>,
    ) {
        if !visited.insert(case_insensitive_fqn_key(type_fqn)) {
            return;
        }

        let Some(class_sym) = self
            .get_committed_type(type_fqn)
            .map(|snapshot| snapshot.symbol)
        else {
            return;
        };
        types.push(class_sym.clone());

        for trait_fqn in &class_sym.traits {
            self.collect_type_hierarchy_symbols(trait_fqn, types, visited);
        }
        for parent_fqn in &class_sym.extends {
            self.collect_type_hierarchy_symbols(parent_fqn, types, visited);
        }
        for iface_fqn in &class_sym.implements {
            self.collect_type_hierarchy_symbols(iface_fqn, types, visited);
        }
    }
}

fn same_fqn(left: &str, right: &str) -> bool {
    left.trim_start_matches('\\')
        .eq_ignore_ascii_case(right.trim_start_matches('\\'))
}

fn alias_scope_for_symbol(symbol: &SymbolInfo) -> TypeAliasScope {
    if let Some(parent_fqn) = symbol.parent_fqn.as_ref() {
        TypeAliasScope::Class(parent_fqn.clone())
    } else if matches!(
        symbol.kind,
        PhpSymbolKind::Class
            | PhpSymbolKind::Interface
            | PhpSymbolKind::Trait
            | PhpSymbolKind::Enum
    ) {
        TypeAliasScope::Class(symbol.fqn.clone())
    } else {
        TypeAliasScope::File(symbol.uri.clone(), (symbol.range.0, symbol.range.1))
    }
}

fn visible_alias_names_for_class(
    phpdoc: &php_lsp_types::PhpDoc,
    file_symbols: &FileSymbols,
) -> HashSet<String> {
    let mut names = visible_alias_names_for_file(file_symbols);
    names.extend(phpdoc.type_aliases.iter().map(|alias| alias.name.clone()));
    names.extend(
        phpdoc
            .type_alias_imports
            .iter()
            .map(|import| import.name.clone()),
    );
    names
}

fn visible_alias_names_for_file(file_symbols: &FileSymbols) -> HashSet<String> {
    let mut names = HashSet::new();
    names.extend(
        file_symbols
            .type_aliases
            .iter()
            .map(|alias| alias.name.clone()),
    );
    names.extend(
        file_symbols
            .type_alias_imports
            .iter()
            .map(|import| import.name.clone()),
    );
    names
}

fn resolve_alias_source_type(source_type: &str, file_symbols: &FileSymbols) -> String {
    php_lsp_parser::resolve::resolve_class_name_pub(source_type, file_symbols)
}

fn resolve_alias_type_names_in_file(
    type_info: &TypeInfo,
    file_symbols: &FileSymbols,
    alias_names: &HashSet<String>,
    template_names: &HashSet<String>,
) -> TypeInfo {
    match type_info {
        TypeInfo::Simple(name) => {
            if should_preserve_alias_type_name(name, alias_names, template_names) {
                TypeInfo::Simple(name.clone())
            } else {
                TypeInfo::Simple(php_lsp_parser::resolve::resolve_class_name_pub(
                    name,
                    file_symbols,
                ))
            }
        }
        TypeInfo::Generic { base, args } => {
            let base = if should_preserve_alias_type_name(base, alias_names, template_names) {
                base.clone()
            } else {
                php_lsp_parser::resolve::resolve_class_name_pub(base, file_symbols)
            };
            TypeInfo::Generic {
                base,
                args: args
                    .iter()
                    .map(|arg| {
                        resolve_alias_type_names_in_file(
                            arg,
                            file_symbols,
                            alias_names,
                            template_names,
                        )
                    })
                    .collect(),
            }
        }
        TypeInfo::ArrayShape(items) => TypeInfo::ArrayShape(
            items
                .iter()
                .map(|item| ArrayShapeItem {
                    key: item.key.clone(),
                    optional: item.optional,
                    value: resolve_alias_type_names_in_file(
                        &item.value,
                        file_symbols,
                        alias_names,
                        template_names,
                    ),
                })
                .collect(),
        ),
        TypeInfo::ObjectShape(items) => TypeInfo::ObjectShape(
            items
                .iter()
                .map(|item| ArrayShapeItem {
                    key: item.key.clone(),
                    optional: item.optional,
                    value: resolve_alias_type_names_in_file(
                        &item.value,
                        file_symbols,
                        alias_names,
                        template_names,
                    ),
                })
                .collect(),
        ),
        TypeInfo::Callable {
            params,
            return_type,
        } => TypeInfo::Callable {
            params: params
                .iter()
                .map(|param| {
                    resolve_alias_type_names_in_file(
                        param,
                        file_symbols,
                        alias_names,
                        template_names,
                    )
                })
                .collect(),
            return_type: return_type.as_ref().map(|return_type| {
                Box::new(resolve_alias_type_names_in_file(
                    return_type,
                    file_symbols,
                    alias_names,
                    template_names,
                ))
            }),
        },
        TypeInfo::ClassString(Some(inner)) => TypeInfo::ClassString(Some(Box::new(
            resolve_alias_type_names_in_file(inner, file_symbols, alias_names, template_names),
        ))),
        TypeInfo::ClassString(None) => TypeInfo::ClassString(None),
        TypeInfo::Conditional {
            subject,
            target,
            if_type,
            else_type,
        } => TypeInfo::Conditional {
            subject: subject.clone(),
            target: Box::new(resolve_alias_type_names_in_file(
                target,
                file_symbols,
                alias_names,
                template_names,
            )),
            if_type: Box::new(resolve_alias_type_names_in_file(
                if_type,
                file_symbols,
                alias_names,
                template_names,
            )),
            else_type: Box::new(resolve_alias_type_names_in_file(
                else_type,
                file_symbols,
                alias_names,
                template_names,
            )),
        },
        TypeInfo::Union(types) => TypeInfo::Union(
            types
                .iter()
                .map(|type_info| {
                    resolve_alias_type_names_in_file(
                        type_info,
                        file_symbols,
                        alias_names,
                        template_names,
                    )
                })
                .collect(),
        ),
        TypeInfo::Intersection(types) => TypeInfo::Intersection(
            types
                .iter()
                .map(|type_info| {
                    resolve_alias_type_names_in_file(
                        type_info,
                        file_symbols,
                        alias_names,
                        template_names,
                    )
                })
                .collect(),
        ),
        TypeInfo::Nullable(inner) => TypeInfo::Nullable(Box::new(
            resolve_alias_type_names_in_file(inner, file_symbols, alias_names, template_names),
        )),
        TypeInfo::LiteralString(_)
        | TypeInfo::LiteralInt(_)
        | TypeInfo::LiteralFloat(_)
        | TypeInfo::LiteralBool(_)
        | TypeInfo::LiteralNull
        | TypeInfo::Void
        | TypeInfo::Never
        | TypeInfo::Mixed
        | TypeInfo::Self_
        | TypeInfo::Static_
        | TypeInfo::Parent_ => type_info.clone(),
    }
}

fn should_preserve_alias_type_name(
    name: &str,
    alias_names: &HashSet<String>,
    template_names: &HashSet<String>,
) -> bool {
    name.starts_with('$')
        || alias_names.contains(name)
        || template_names.contains(name)
        || is_phpdoc_builtin_type(name)
}

fn is_phpdoc_builtin_type(name: &str) -> bool {
    matches!(
        name.trim_start_matches('\\').to_ascii_lowercase().as_str(),
        "array"
            | "bool"
            | "boolean"
            | "callable"
            | "false"
            | "float"
            | "int"
            | "integer"
            | "iterable"
            | "list"
            | "mixed"
            | "never"
            | "null"
            | "object"
            | "resource"
            | "scalar"
            | "self"
            | "static"
            | "string"
            | "true"
            | "void"
    )
}

fn substitute_signature(signature: &Signature, substitutions: &TemplateSubstitutions) -> Signature {
    Signature {
        params: signature
            .params
            .iter()
            .map(|param| {
                let mut param = param.clone();
                param.type_info = param
                    .type_info
                    .as_ref()
                    .map(|type_info| substitute_type_info(type_info, substitutions));
                param
            })
            .collect(),
        return_type: signature
            .return_type
            .as_ref()
            .map(|type_info| substitute_type_info(type_info, substitutions)),
    }
}

fn substitute_type_info(type_info: &TypeInfo, substitutions: &TemplateSubstitutions) -> TypeInfo {
    match type_info {
        TypeInfo::Simple(name) => substitutions
            .get(name)
            .cloned()
            .unwrap_or_else(|| TypeInfo::Simple(name.clone())),
        TypeInfo::Generic { base, args } => TypeInfo::Generic {
            base: base.clone(),
            args: args
                .iter()
                .map(|arg| substitute_type_info(arg, substitutions))
                .collect(),
        },
        TypeInfo::ArrayShape(items) => TypeInfo::ArrayShape(
            items
                .iter()
                .map(|item| ArrayShapeItem {
                    key: item.key.clone(),
                    optional: item.optional,
                    value: substitute_type_info(&item.value, substitutions),
                })
                .collect(),
        ),
        TypeInfo::ObjectShape(items) => TypeInfo::ObjectShape(
            items
                .iter()
                .map(|item| ArrayShapeItem {
                    key: item.key.clone(),
                    optional: item.optional,
                    value: substitute_type_info(&item.value, substitutions),
                })
                .collect(),
        ),
        TypeInfo::Callable {
            params,
            return_type,
        } => TypeInfo::Callable {
            params: params
                .iter()
                .map(|param| substitute_type_info(param, substitutions))
                .collect(),
            return_type: return_type
                .as_ref()
                .map(|return_type| Box::new(substitute_type_info(return_type, substitutions))),
        },
        TypeInfo::ClassString(Some(inner)) => {
            TypeInfo::ClassString(Some(Box::new(substitute_type_info(inner, substitutions))))
        }
        TypeInfo::ClassString(None) => TypeInfo::ClassString(None),
        TypeInfo::Conditional {
            subject,
            target,
            if_type,
            else_type,
        } => TypeInfo::Conditional {
            subject: subject.clone(),
            target: Box::new(substitute_type_info(target, substitutions)),
            if_type: Box::new(substitute_type_info(if_type, substitutions)),
            else_type: Box::new(substitute_type_info(else_type, substitutions)),
        },
        TypeInfo::Union(types) => TypeInfo::Union(
            types
                .iter()
                .map(|type_info| substitute_type_info(type_info, substitutions))
                .collect(),
        ),
        TypeInfo::Intersection(types) => TypeInfo::Intersection(
            types
                .iter()
                .map(|type_info| substitute_type_info(type_info, substitutions))
                .collect(),
        ),
        TypeInfo::Nullable(inner) => {
            TypeInfo::Nullable(Box::new(substitute_type_info(inner, substitutions)))
        }
        TypeInfo::LiteralString(_)
        | TypeInfo::LiteralInt(_)
        | TypeInfo::LiteralFloat(_)
        | TypeInfo::LiteralBool(_)
        | TypeInfo::LiteralNull
        | TypeInfo::Void
        | TypeInfo::Never
        | TypeInfo::Mixed
        | TypeInfo::Self_
        | TypeInfo::Static_
        | TypeInfo::Parent_ => type_info.clone(),
    }
}

impl Default for WorkspaceIndex {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[path = "workspace_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "workspace_dependency_tests.rs"]
mod dependency_tests;
