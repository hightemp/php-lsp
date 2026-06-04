//! Global workspace symbol index.

use dashmap::DashMap;
use php_lsp_types::{
    ArrayShapeItem, FileSymbols, PhpSymbolKind, Signature, SymbolInfo, SymbolReference,
    TemplateBindingKind, TypeInfo,
};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

type TemplateSubstitutions = HashMap<String, TypeInfo>;
const MAX_TYPE_ALIAS_EXPANSION_DEPTH: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
enum TypeAliasScope {
    Class(String),
    File(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TypeAliasVisit {
    scope: TypeAliasScope,
    name: String,
}

/// Global index of all symbols in the workspace.
pub struct WorkspaceIndex {
    /// FQN → SymbolInfo for types (classes, interfaces, traits, enums)
    pub types: DashMap<String, Arc<SymbolInfo>>,

    /// FQN → SymbolInfo for functions
    pub functions: DashMap<String, Arc<SymbolInfo>>,

    /// FQN → SymbolInfo for constants
    pub constants: DashMap<String, Arc<SymbolInfo>>,

    /// File URI → extracted symbols for that file
    pub file_symbols: DashMap<String, FileSymbols>,

    /// File URI → precomputed non-local symbol references for that file
    pub file_references: DashMap<String, Vec<SymbolReference>>,
}

impl WorkspaceIndex {
    /// Create a new empty index.
    pub fn new() -> Self {
        WorkspaceIndex {
            types: DashMap::new(),
            functions: DashMap::new(),
            constants: DashMap::new(),
            file_symbols: DashMap::new(),
            file_references: DashMap::new(),
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
        // Remove old symbols for this file
        self.remove_file(uri);

        // Add new symbols to global indices
        for sym in &file_symbols.symbols {
            let sym_arc = Arc::new(sym.clone());
            match sym.kind {
                PhpSymbolKind::Class
                | PhpSymbolKind::Interface
                | PhpSymbolKind::Trait
                | PhpSymbolKind::Enum => {
                    self.types.insert(sym.fqn.clone(), sym_arc);
                }
                PhpSymbolKind::Function => {
                    self.functions.insert(sym.fqn.clone(), sym_arc);
                }
                PhpSymbolKind::GlobalConstant => {
                    self.constants.insert(sym.fqn.clone(), sym_arc);
                }
                // Methods, properties, class constants belong to their parent type
                // and are stored in file_symbols, queried via parent_fqn
                _ => {}
            }
        }

        // Store file symbols
        self.file_symbols.insert(uri.to_string(), file_symbols);
        self.file_references
            .insert(uri.to_string(), file_references);
    }

    /// Remove all symbols from a file.
    pub fn remove_file(&self, uri: &str) {
        self.file_references.remove(uri);
        if let Some((_, old_symbols)) = self.file_symbols.remove(uri) {
            for sym in &old_symbols.symbols {
                match sym.kind {
                    PhpSymbolKind::Class
                    | PhpSymbolKind::Interface
                    | PhpSymbolKind::Trait
                    | PhpSymbolKind::Enum => {
                        self.remove_top_level_symbol(uri, sym, &self.types);
                    }
                    PhpSymbolKind::Function => {
                        self.remove_top_level_symbol(uri, sym, &self.functions);
                    }
                    PhpSymbolKind::GlobalConstant => {
                        self.remove_top_level_symbol(uri, sym, &self.constants);
                    }
                    _ => {}
                }
            }
        }
    }

    fn remove_top_level_symbol(
        &self,
        removed_uri: &str,
        removed_symbol: &SymbolInfo,
        symbols: &DashMap<String, Arc<SymbolInfo>>,
    ) {
        let should_remove = symbols
            .get(&removed_symbol.fqn)
            .is_some_and(|entry| entry.uri == removed_uri);
        if !should_remove {
            return;
        }

        symbols.remove(&removed_symbol.fqn);

        if let Some(replacement) = self.find_top_level_symbol_replacement(removed_symbol) {
            symbols.insert(removed_symbol.fqn.clone(), replacement);
        }
    }

    fn find_top_level_symbol_replacement(
        &self,
        removed_symbol: &SymbolInfo,
    ) -> Option<Arc<SymbolInfo>> {
        self.file_symbols.iter().find_map(|entry| {
            entry
                .symbols
                .iter()
                .find(|candidate| {
                    candidate.fqn == removed_symbol.fqn && candidate.kind == removed_symbol.kind
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
        // Try top-level lookup first
        if let Some(sym) = self.types.get(fqn).map(|r| r.value().clone()) {
            return Some(self.materialize_symbol(sym, &TemplateSubstitutions::new()));
        }
        if let Some(sym) = self.functions.get(fqn).map(|r| r.value().clone()) {
            return Some(self.materialize_symbol(sym, &TemplateSubstitutions::new()));
        }
        if let Some(sym) = self.constants.get(fqn).map(|r| r.value().clone()) {
            return Some(self.materialize_symbol(sym, &TemplateSubstitutions::new()));
        }

        // Try Class::member resolution
        self.resolve_member(fqn)
    }

    /// Resolve a `Class::member` FQN to the member symbol.
    ///
    /// First tries exact FQN match (e.g. `App\Foo::test`), then falls back
    /// to matching by name for cases like property access where the FQN has `$`
    /// prefix in the symbol but not in the query.
    /// Walks the class hierarchy (extends/implements) when the member is not
    /// found directly on the given class.
    pub fn resolve_member(&self, fqn: &str) -> Option<Arc<SymbolInfo>> {
        let (class_fqn, member_name) = fqn.rsplit_once("::")?;
        self.resolve_member_in_hierarchy(
            class_fqn,
            member_name,
            fqn,
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
        visited: &mut HashSet<String>,
        substitutions: &TemplateSubstitutions,
    ) -> Option<Arc<SymbolInfo>> {
        if !visited.insert(class_fqn.to_string()) {
            return None;
        }

        let members = self.get_direct_members(class_fqn);
        // Prefer exact FQN match first
        if let Some(sym) = members.iter().find(|m| m.fqn == original_fqn) {
            return Some(self.materialize_symbol(sym.clone(), substitutions));
        }
        // Fallback: match by name (for cases where caller doesn't know exact FQN form)
        // Property names in SymbolInfo are stored without '$' prefix, so strip it for comparison
        let bare_name = member_name.strip_prefix('$').unwrap_or(member_name);
        if let Some(sym) = members
            .iter()
            .find(|m| m.name == member_name || m.name == bare_name)
        {
            return Some(self.materialize_symbol(sym.clone(), substitutions));
        }

        // Walk the class hierarchy: look up extends and implements
        if let Some(class_sym) = self.types.get(class_fqn).map(|r| r.value().clone()) {
            // Try traits first: their members are mixed into the class/trait body.
            for trait_fqn in &class_sym.traits {
                let edge_substitutions =
                    self.template_substitutions_for_edge(&class_sym, trait_fqn, substitutions);
                if let Some(sym) = self.resolve_member_in_hierarchy(
                    trait_fqn,
                    member_name,
                    original_fqn,
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
        let query_lower = query.to_lowercase();
        let mut results = Vec::new();

        for entry in self.types.iter() {
            if entry.value().name.to_lowercase().contains(&query_lower) {
                results.push(entry.value().clone());
            }
        }
        for entry in self.functions.iter() {
            if entry.value().name.to_lowercase().contains(&query_lower) {
                results.push(entry.value().clone());
            }
        }
        for entry in self.constants.iter() {
            if entry.value().name.to_lowercase().contains(&query_lower) {
                results.push(entry.value().clone());
            }
        }

        results
    }

    /// Get members (methods, properties, constants) of a type by its FQN.
    /// Includes inherited members from parent classes and interfaces.
    pub fn get_members(&self, type_fqn: &str) -> Vec<Arc<SymbolInfo>> {
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
        let mut types = Vec::new();
        self.collect_type_hierarchy_symbols(type_fqn, &mut types, &mut HashSet::new());
        types
    }

    /// Get only the direct members of a type (no inheritance traversal).
    fn get_direct_members(&self, type_fqn: &str) -> Vec<Arc<SymbolInfo>> {
        let mut members = Vec::new();
        for entry in self.file_symbols.iter() {
            for sym in &entry.value().symbols {
                if sym.parent_fqn.as_deref() == Some(type_fqn) {
                    members.push(Arc::new(sym.clone()));
                }
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
        if !visited.insert(type_fqn.to_string()) {
            return;
        }

        // Collect direct members
        let direct = self.get_direct_members(type_fqn);
        members.extend(
            direct
                .into_iter()
                .map(|sym| self.materialize_symbol(sym, substitutions)),
        );

        // Recurse into parent classes and interfaces
        if let Some(class_sym) = self.types.get(type_fqn).map(|r| r.value().clone()) {
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
            .types
            .get(target_fqn)
            .map(|entry| entry.value().clone())
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
            TypeAliasScope::File(uri) => self.file_type_alias_for_name(uri, name, visited),
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
        let class_symbol = self
            .types
            .get(class_fqn)
            .map(|entry| entry.value().clone())?;
        let file_symbols = self
            .file_symbols
            .get(&class_symbol.uri)
            .map(|entry| entry.value().clone());
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

        self.file_type_alias_for_name(&class_symbol.uri, name, visited)
    }

    fn file_type_alias_for_name(
        &self,
        uri: &str,
        name: &str,
        visited: &mut Vec<TypeAliasVisit>,
    ) -> Option<TypeInfo> {
        let file_symbols = self
            .file_symbols
            .get(uri)
            .map(|entry| entry.value().clone())?;

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
                &TypeAliasScope::File(uri.to_string()),
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
        if !visited.insert(type_fqn.to_string()) {
            return;
        }

        let Some(class_sym) = self.types.get(type_fqn).map(|r| r.value().clone()) else {
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
    left.trim_start_matches('\\') == right.trim_start_matches('\\')
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
        TypeAliasScope::File(symbol.uri.clone())
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
mod tests {
    use super::*;
    use php_lsp_types::*;

    fn make_class(name: &str, fqn: &str, uri: &str) -> SymbolInfo {
        SymbolInfo {
            name: name.to_string(),
            fqn: fqn.to_string(),
            kind: PhpSymbolKind::Class,
            uri: uri.to_string(),
            range: (0, 0, 10, 0),
            selection_range: (0, 6, 0, 6 + name.len() as u32),
            visibility: Visibility::Public,
            modifiers: SymbolModifiers::default(),
            attributes: vec![],
            doc_comment: None,
            signature: None,
            parent_fqn: None,
            extends: vec![],
            implements: vec![],
            traits: vec![],
            templates: vec![],
            template_bindings: vec![],
        }
    }

    fn make_function(name: &str, fqn: &str, uri: &str) -> SymbolInfo {
        SymbolInfo {
            name: name.to_string(),
            fqn: fqn.to_string(),
            kind: PhpSymbolKind::Function,
            uri: uri.to_string(),
            range: (0, 0, 5, 0),
            selection_range: (0, 9, 0, 9 + name.len() as u32),
            visibility: Visibility::Public,
            modifiers: SymbolModifiers::default(),
            attributes: vec![],
            doc_comment: None,
            signature: None,
            parent_fqn: None,
            extends: vec![],
            implements: vec![],
            traits: vec![],
            templates: vec![],
            template_bindings: vec![],
        }
    }

    fn make_method(name: &str, parent_fqn: &str, uri: &str) -> SymbolInfo {
        SymbolInfo {
            name: name.to_string(),
            fqn: format!("{parent_fqn}::{name}"),
            kind: PhpSymbolKind::Method,
            uri: uri.to_string(),
            range: (1, 4, 3, 5),
            selection_range: (1, 20, 1, 20 + name.len() as u32),
            visibility: Visibility::Public,
            modifiers: SymbolModifiers::default(),
            attributes: vec![],
            doc_comment: None,
            signature: None,
            parent_fqn: Some(parent_fqn.to_string()),
            extends: vec![],
            implements: vec![],
            traits: vec![],
            templates: vec![],
            template_bindings: vec![],
        }
    }

    #[test]
    fn test_update_and_resolve() {
        let index = WorkspaceIndex::new();
        let sym = make_class("Foo", "App\\Foo", "file:///test.php");
        let file_symbols = FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![sym],
            ..Default::default()
        };

        index.update_file("file:///test.php", file_symbols);

        let found = index.resolve_fqn("App\\Foo");
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "Foo");
    }

    #[test]
    fn test_remove_file() {
        let index = WorkspaceIndex::new();
        let sym = make_class("Foo", "App\\Foo", "file:///test.php");
        let file_symbols = FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![sym],
            ..Default::default()
        };

        index.update_file("file:///test.php", file_symbols);
        assert!(index.resolve_fqn("App\\Foo").is_some());

        index.remove_file("file:///test.php");
        assert!(index.resolve_fqn("App\\Foo").is_none());
    }

    #[test]
    fn test_remove_file_preserves_duplicate_fqn_from_other_file() {
        let index = WorkspaceIndex::new();
        let file_a = FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![make_class("Foo", "App\\Foo", "file:///a.php")],
            ..Default::default()
        };
        let file_b = FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![make_class("Foo", "App\\Foo", "file:///b.php")],
            ..Default::default()
        };

        index.update_file("file:///a.php", file_a);
        index.update_file("file:///b.php", file_b);

        index.remove_file("file:///a.php");

        let found = index
            .resolve_fqn("App\\Foo")
            .expect("duplicate FQN remains");
        assert_eq!(found.uri, "file:///b.php");
    }

    #[test]
    fn test_search() {
        let index = WorkspaceIndex::new();
        let file_symbols = FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![
                make_class("FooController", "App\\FooController", "file:///a.php"),
                make_class("BarService", "App\\BarService", "file:///a.php"),
                make_function("helper_foo", "App\\helper_foo", "file:///a.php"),
            ],
            ..Default::default()
        };

        index.update_file("file:///a.php", file_symbols);

        let results = index.search("foo");
        assert_eq!(results.len(), 2); // FooController + helper_foo
    }

    #[test]
    fn test_update_replaces_old() {
        let index = WorkspaceIndex::new();

        let sym_v1 = FileSymbols {
            namespace: None,
            use_statements: vec![],
            symbols: vec![make_class("Foo", "Foo", "file:///test.php")],
            ..Default::default()
        };
        index.update_file("file:///test.php", sym_v1);
        assert!(index.resolve_fqn("Foo").is_some());

        let sym_v2 = FileSymbols {
            namespace: None,
            use_statements: vec![],
            symbols: vec![make_class("Bar", "Bar", "file:///test.php")],
            ..Default::default()
        };
        index.update_file("file:///test.php", sym_v2);
        assert!(index.resolve_fqn("Foo").is_none());
        assert!(index.resolve_fqn("Bar").is_some());
    }

    #[test]
    fn test_resolve_member() {
        let index = WorkspaceIndex::new();
        let class_sym = make_class("Foo", "App\\Foo", "file:///test.php");
        let method_sym = SymbolInfo {
            name: "increment".to_string(),
            fqn: "App\\Foo::increment".to_string(),
            kind: PhpSymbolKind::Method,
            uri: "file:///test.php".to_string(),
            range: (10, 0, 15, 0),
            selection_range: (10, 20, 10, 29),
            visibility: Visibility::Public,
            modifiers: SymbolModifiers::default(),
            attributes: vec![],
            doc_comment: None,
            signature: None,
            parent_fqn: Some("App\\Foo".to_string()),
            extends: vec![],
            implements: vec![],
            traits: vec![],
            templates: vec![],
            template_bindings: vec![],
        };
        let file_symbols = FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![class_sym, method_sym],
            ..Default::default()
        };
        index.update_file("file:///test.php", file_symbols);

        // resolve_fqn should find the class
        let found = index.resolve_fqn("App\\Foo");
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "Foo");

        // resolve_fqn should also find the method via Class::member
        let found = index.resolve_fqn("App\\Foo::increment");
        assert!(found.is_some());
        let method = found.unwrap();
        assert_eq!(method.name, "increment");
        assert_eq!(method.kind, PhpSymbolKind::Method);

        // Non-existent member should return None
        assert!(index.resolve_fqn("App\\Foo::nonexistent").is_none());
    }

    #[test]
    fn test_resolve_inherited_member() {
        let index = WorkspaceIndex::new();

        // Parent class with a method
        let parent_class = SymbolInfo {
            name: "SoapHandler".to_string(),
            fqn: "App\\SoapHandler".to_string(),
            kind: PhpSymbolKind::Class,
            uri: "file:///parent.php".to_string(),
            range: (0, 0, 20, 0),
            selection_range: (0, 6, 0, 17),
            visibility: Visibility::Public,
            modifiers: SymbolModifiers::default(),
            attributes: vec![],
            doc_comment: None,
            signature: None,
            parent_fqn: None,
            extends: vec![],
            implements: vec![],
            traits: vec![],
            templates: vec![],
            template_bindings: vec![],
        };
        let parent_method = SymbolInfo {
            name: "okResponse".to_string(),
            fqn: "App\\SoapHandler::okResponse".to_string(),
            kind: PhpSymbolKind::Method,
            uri: "file:///parent.php".to_string(),
            range: (5, 4, 8, 5),
            selection_range: (5, 20, 5, 30),
            visibility: Visibility::Protected,
            modifiers: SymbolModifiers::default(),
            attributes: vec![],
            doc_comment: None,
            signature: None,
            parent_fqn: Some("App\\SoapHandler".to_string()),
            extends: vec![],
            implements: vec![],
            traits: vec![],
            templates: vec![],
            template_bindings: vec![],
        };
        let parent_file = FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![parent_class, parent_method],
            ..Default::default()
        };
        index.update_file("file:///parent.php", parent_file);

        // Child class that extends the parent
        let child_class = SymbolInfo {
            name: "TestHandler".to_string(),
            fqn: "App\\TestHandler".to_string(),
            kind: PhpSymbolKind::Class,
            uri: "file:///child.php".to_string(),
            range: (0, 0, 5, 0),
            selection_range: (0, 6, 0, 17),
            visibility: Visibility::Public,
            modifiers: SymbolModifiers::default(),
            attributes: vec![],
            doc_comment: None,
            signature: None,
            parent_fqn: None,
            extends: vec!["App\\SoapHandler".to_string()],
            implements: vec![],
            traits: vec![],
            templates: vec![],
            template_bindings: vec![],
        };
        let child_file = FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![child_class],
            ..Default::default()
        };
        index.update_file("file:///child.php", child_file);

        // Resolving TestHandler::okResponse should find the parent's method
        let found = index.resolve_fqn("App\\TestHandler::okResponse");
        assert!(found.is_some(), "should resolve inherited member");
        let method = found.unwrap();
        assert_eq!(method.name, "okResponse");
        assert_eq!(method.fqn, "App\\SoapHandler::okResponse");

        // get_members should include inherited members
        let members = index.get_members("App\\TestHandler");
        assert!(
            members.iter().any(|m| m.name == "okResponse"),
            "inherited method should be in get_members"
        );
    }

    #[test]
    fn test_resolve_member_inherited_through_interface_extends_chain() {
        let index = WorkspaceIndex::new();

        let mut base_interface = make_class("BaseForm", "Vendor\\BaseForm", "file:///base.php");
        base_interface.kind = PhpSymbolKind::Interface;
        let base_method = make_method("handleRequest", "Vendor\\BaseForm", "file:///base.php");
        index.update_file(
            "file:///base.php",
            FileSymbols {
                namespace: Some("Vendor".to_string()),
                use_statements: vec![],
                symbols: vec![base_interface, base_method],
                ..Default::default()
            },
        );

        let mut flow_interface = make_class("FlowForm", "Vendor\\FlowForm", "file:///flow.php");
        flow_interface.kind = PhpSymbolKind::Interface;
        flow_interface.extends = vec!["Vendor\\BaseForm".to_string()];
        index.update_file(
            "file:///flow.php",
            FileSymbols {
                namespace: Some("Vendor".to_string()),
                use_statements: vec![],
                symbols: vec![flow_interface],
                ..Default::default()
            },
        );

        let found = index
            .resolve_fqn("Vendor\\FlowForm::handleRequest")
            .expect("interface should inherit members through extends");
        assert_eq!(found.fqn, "Vendor\\BaseForm::handleRequest");

        let members = index.get_members("Vendor\\FlowForm");
        assert!(
            members.iter().any(|member| member.name == "handleRequest"),
            "interface-extended method should be included in get_members"
        );
    }

    #[test]
    fn test_resolve_trait_member() {
        let index = WorkspaceIndex::new();

        let trait_sym = SymbolInfo {
            name: "Assertions".to_string(),
            fqn: "App\\Assertions".to_string(),
            kind: PhpSymbolKind::Trait,
            uri: "file:///trait.php".to_string(),
            range: (0, 0, 10, 0),
            selection_range: (0, 6, 0, 16),
            visibility: Visibility::Public,
            modifiers: SymbolModifiers::default(),
            attributes: vec![],
            doc_comment: None,
            signature: None,
            parent_fqn: None,
            extends: vec![],
            implements: vec![],
            traits: vec![],
            templates: vec![],
            template_bindings: vec![],
        };
        let trait_method = SymbolInfo {
            name: "assertOk".to_string(),
            fqn: "App\\Assertions::assertOk".to_string(),
            kind: PhpSymbolKind::Method,
            uri: "file:///trait.php".to_string(),
            range: (2, 4, 4, 5),
            selection_range: (2, 20, 2, 28),
            visibility: Visibility::Public,
            modifiers: SymbolModifiers::default(),
            attributes: vec![],
            doc_comment: None,
            signature: None,
            parent_fqn: Some("App\\Assertions".to_string()),
            extends: vec![],
            implements: vec![],
            traits: vec![],
            templates: vec![],
            template_bindings: vec![],
        };
        index.update_file(
            "file:///trait.php",
            FileSymbols {
                namespace: Some("App".to_string()),
                use_statements: vec![],
                symbols: vec![trait_sym, trait_method],
                ..Default::default()
            },
        );

        let class_sym = SymbolInfo {
            name: "TestCase".to_string(),
            fqn: "App\\TestCase".to_string(),
            kind: PhpSymbolKind::Class,
            uri: "file:///class.php".to_string(),
            range: (0, 0, 5, 0),
            selection_range: (0, 6, 0, 14),
            visibility: Visibility::Public,
            modifiers: SymbolModifiers::default(),
            attributes: vec![],
            doc_comment: None,
            signature: None,
            parent_fqn: None,
            extends: vec![],
            implements: vec![],
            traits: vec!["App\\Assertions".to_string()],
            templates: vec![],
            template_bindings: vec![],
        };
        index.update_file(
            "file:///class.php",
            FileSymbols {
                namespace: Some("App".to_string()),
                use_statements: vec![],
                symbols: vec![class_sym],
                ..Default::default()
            },
        );

        let found = index.resolve_fqn("App\\TestCase::assertOk");
        assert!(found.is_some(), "should resolve methods mixed in by traits");
        assert_eq!(found.unwrap().fqn, "App\\Assertions::assertOk");
    }

    #[test]
    fn test_resolve_member_no_infinite_loop() {
        let index = WorkspaceIndex::new();

        // Two classes that extend each other (pathological case)
        let class_a = SymbolInfo {
            name: "A".to_string(),
            fqn: "A".to_string(),
            kind: PhpSymbolKind::Class,
            uri: "file:///a.php".to_string(),
            range: (0, 0, 5, 0),
            selection_range: (0, 6, 0, 7),
            visibility: Visibility::Public,
            modifiers: SymbolModifiers::default(),
            attributes: vec![],
            doc_comment: None,
            signature: None,
            parent_fqn: None,
            extends: vec!["B".to_string()],
            implements: vec![],
            traits: vec![],
            templates: vec![],
            template_bindings: vec![],
        };
        let class_b = SymbolInfo {
            name: "B".to_string(),
            fqn: "B".to_string(),
            kind: PhpSymbolKind::Class,
            uri: "file:///b.php".to_string(),
            range: (0, 0, 5, 0),
            selection_range: (0, 6, 0, 7),
            visibility: Visibility::Public,
            modifiers: SymbolModifiers::default(),
            attributes: vec![],
            doc_comment: None,
            signature: None,
            parent_fqn: None,
            extends: vec!["A".to_string()],
            implements: vec![],
            traits: vec![],
            templates: vec![],
            template_bindings: vec![],
        };
        let file_a = FileSymbols {
            namespace: None,
            use_statements: vec![],
            symbols: vec![class_a],
            ..Default::default()
        };
        let file_b = FileSymbols {
            namespace: None,
            use_statements: vec![],
            symbols: vec![class_b],
            ..Default::default()
        };
        index.update_file("file:///a.php", file_a);
        index.update_file("file:///b.php", file_b);

        // Should not hang — just return None
        assert!(index.resolve_fqn("A::nonexistent").is_none());
    }

    #[test]
    fn test_hierarchy_visited_sets_handle_trait_mixin_and_parent_cycles() {
        let index = WorkspaceIndex::new();

        let mut root = make_class("Root", "App\\Root", "file:///hierarchy.php");
        root.extends = vec!["App\\Parent".to_string()];
        root.traits = vec!["App\\SharedTrait".to_string()];
        root.template_bindings = vec![TemplateBinding {
            kind: TemplateBindingKind::Mixin,
            target: "App\\Mixin".to_string(),
            args: vec![],
        }];

        let mut parent = make_class("Parent", "App\\Parent", "file:///hierarchy.php");
        parent.extends = vec!["App\\Root".to_string()];

        let mut trait_sym = make_class("SharedTrait", "App\\SharedTrait", "file:///hierarchy.php");
        trait_sym.kind = PhpSymbolKind::Trait;
        trait_sym.traits = vec!["App\\Root".to_string()];

        let mut mixin = make_class("Mixin", "App\\Mixin", "file:///hierarchy.php");
        mixin.extends = vec!["App\\Root".to_string()];

        index.update_file(
            "file:///hierarchy.php",
            FileSymbols {
                namespace: Some("App".to_string()),
                use_statements: vec![],
                symbols: vec![
                    root,
                    parent,
                    make_method("parentMethod", "App\\Parent", "file:///hierarchy.php"),
                    trait_sym,
                    make_method("traitMethod", "App\\SharedTrait", "file:///hierarchy.php"),
                    mixin,
                    make_method("mixinMethod", "App\\Mixin", "file:///hierarchy.php"),
                ],
                ..Default::default()
            },
        );

        assert_eq!(
            index
                .resolve_fqn("App\\Root::traitMethod")
                .map(|sym| sym.fqn.clone())
                .as_deref(),
            Some("App\\SharedTrait::traitMethod")
        );
        assert_eq!(
            index
                .resolve_fqn("App\\Root::mixinMethod")
                .map(|sym| sym.fqn.clone())
                .as_deref(),
            Some("App\\Mixin::mixinMethod")
        );
        assert_eq!(
            index
                .resolve_fqn("App\\Root::parentMethod")
                .map(|sym| sym.fqn.clone())
                .as_deref(),
            Some("App\\Parent::parentMethod")
        );
        assert!(index.resolve_fqn("App\\Root::missing").is_none());

        let member_names = index
            .get_members("App\\Root")
            .into_iter()
            .map(|member| member.name.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            member_names,
            vec!["traitMethod", "mixinMethod", "parentMethod"]
        );

        let hierarchy_fqns = index
            .get_type_hierarchy_symbols("App\\Root")
            .into_iter()
            .map(|symbol| symbol.fqn.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            hierarchy_fqns,
            vec!["App\\Root", "App\\SharedTrait", "App\\Parent"]
        );
    }

    #[test]
    fn test_resolve_inherited_member_after_incremental_load() {
        // Simulates vendor lazy-loading: child class is indexed first,
        // parent is added later. After parent is indexed, inherited
        // member resolution should work.
        let index = WorkspaceIndex::new();

        // Step 1: Index child class (extends a parent not yet indexed)
        let child_class = SymbolInfo {
            name: "MyTest".to_string(),
            fqn: "App\\MyTest".to_string(),
            kind: PhpSymbolKind::Class,
            uri: "file:///tests/MyTest.php".to_string(),
            range: (0, 0, 10, 0),
            selection_range: (0, 6, 0, 12),
            visibility: Visibility::Public,
            modifiers: SymbolModifiers::default(),
            attributes: vec![],
            doc_comment: None,
            signature: None,
            parent_fqn: None,
            extends: vec!["Vendor\\TestCase".to_string()],
            implements: vec![],
            traits: vec![],
            templates: vec![],
            template_bindings: vec![],
        };
        let child_file = FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![child_class],
            ..Default::default()
        };
        index.update_file("file:///tests/MyTest.php", child_file);

        // Before parent is loaded, inherited member should NOT resolve
        assert!(
            index.resolve_fqn("App\\MyTest::doSetUp").is_none(),
            "member should not resolve before parent is indexed"
        );

        // Step 2: Index parent class (vendor lazy-load simulation)
        let parent_class = SymbolInfo {
            name: "TestCase".to_string(),
            fqn: "Vendor\\TestCase".to_string(),
            kind: PhpSymbolKind::Class,
            uri: "file:///vendor/TestCase.php".to_string(),
            range: (0, 0, 20, 0),
            selection_range: (0, 6, 0, 14),
            visibility: Visibility::Public,
            modifiers: SymbolModifiers::default(),
            attributes: vec![],
            doc_comment: None,
            signature: None,
            parent_fqn: None,
            extends: vec!["Vendor\\BaseAssert".to_string()],
            implements: vec![],
            traits: vec![],
            templates: vec![],
            template_bindings: vec![],
        };
        let parent_method = SymbolInfo {
            name: "doSetUp".to_string(),
            fqn: "Vendor\\TestCase::doSetUp".to_string(),
            kind: PhpSymbolKind::Method,
            uri: "file:///vendor/TestCase.php".to_string(),
            range: (5, 4, 8, 5),
            selection_range: (5, 20, 5, 27),
            visibility: Visibility::Protected,
            modifiers: SymbolModifiers::default(),
            attributes: vec![],
            doc_comment: None,
            signature: None,
            parent_fqn: Some("Vendor\\TestCase".to_string()),
            extends: vec![],
            implements: vec![],
            traits: vec![],
            templates: vec![],
            template_bindings: vec![],
        };
        let parent_file = FileSymbols {
            namespace: Some("Vendor".to_string()),
            use_statements: vec![],
            symbols: vec![parent_class, parent_method],
            ..Default::default()
        };
        index.update_file("file:///vendor/TestCase.php", parent_file);

        // After parent is indexed, inherited member SHOULD resolve
        let found = index.resolve_fqn("App\\MyTest::doSetUp");
        assert!(
            found.is_some(),
            "member should resolve after parent is indexed"
        );
        assert_eq!(found.unwrap().name, "doSetUp");

        // Step 3: Index grandparent class (deeper vendor lazy-load)
        let gp_class = SymbolInfo {
            name: "BaseAssert".to_string(),
            fqn: "Vendor\\BaseAssert".to_string(),
            kind: PhpSymbolKind::Class,
            uri: "file:///vendor/BaseAssert.php".to_string(),
            range: (0, 0, 30, 0),
            selection_range: (0, 6, 0, 16),
            visibility: Visibility::Public,
            modifiers: SymbolModifiers::default(),
            attributes: vec![],
            doc_comment: None,
            signature: None,
            parent_fqn: None,
            extends: vec![],
            implements: vec![],
            traits: vec![],
            templates: vec![],
            template_bindings: vec![],
        };
        let gp_method = SymbolInfo {
            name: "createStub".to_string(),
            fqn: "Vendor\\BaseAssert::createStub".to_string(),
            kind: PhpSymbolKind::Method,
            uri: "file:///vendor/BaseAssert.php".to_string(),
            range: (10, 4, 13, 5),
            selection_range: (10, 20, 10, 30),
            visibility: Visibility::Public,
            modifiers: SymbolModifiers::default(),
            attributes: vec![],
            doc_comment: None,
            signature: None,
            parent_fqn: Some("Vendor\\BaseAssert".to_string()),
            extends: vec![],
            implements: vec![],
            traits: vec![],
            templates: vec![],
            template_bindings: vec![],
        };
        let gp_file = FileSymbols {
            namespace: Some("Vendor".to_string()),
            use_statements: vec![],
            symbols: vec![gp_class, gp_method],
            ..Default::default()
        };
        index.update_file("file:///vendor/BaseAssert.php", gp_file);

        // Grandparent method should now resolve through the full chain
        let found = index.resolve_fqn("App\\MyTest::createStub");
        assert!(
            found.is_some(),
            "grandparent method should resolve through inheritance chain"
        );
        assert_eq!(found.unwrap().name, "createStub");
    }

    #[test]
    fn test_template_substitution_for_generic_repository_method() {
        let index = WorkspaceIndex::new();

        let mut repository = make_class("Repository", "App\\Repository", "file:///repo.php");
        repository.kind = PhpSymbolKind::Interface;
        repository.templates = vec![TemplateParam {
            name: "TEntity".to_string(),
            bound: Some(TypeInfo::Simple("object".to_string())),
            variance: TemplateVariance::Covariant,
        }];
        let repository_method = SymbolInfo {
            name: "find".to_string(),
            fqn: "App\\Repository::find".to_string(),
            kind: PhpSymbolKind::Method,
            uri: "file:///repo.php".to_string(),
            range: (3, 4, 3, 40),
            selection_range: (3, 20, 3, 24),
            visibility: Visibility::Public,
            modifiers: SymbolModifiers::default(),
            attributes: vec![],
            doc_comment: None,
            signature: Some(Signature {
                params: vec![],
                return_type: Some(TypeInfo::Simple("TEntity".to_string())),
            }),
            parent_fqn: Some("App\\Repository".to_string()),
            extends: vec![],
            implements: vec![],
            traits: vec![],
            templates: vec![],
            template_bindings: vec![],
        };
        index.update_file(
            "file:///repo.php",
            FileSymbols {
                namespace: Some("App".to_string()),
                use_statements: vec![],
                symbols: vec![repository, repository_method],
                ..Default::default()
            },
        );

        let mut user_repository = make_class(
            "UserRepository",
            "App\\UserRepository",
            "file:///user_repo.php",
        );
        user_repository.implements = vec!["App\\Repository".to_string()];
        user_repository.template_bindings = vec![TemplateBinding {
            kind: TemplateBindingKind::Implements,
            target: "App\\Repository".to_string(),
            args: vec![TypeInfo::Simple("App\\User".to_string())],
        }];
        index.update_file(
            "file:///user_repo.php",
            FileSymbols {
                namespace: Some("App".to_string()),
                use_statements: vec![],
                symbols: vec![user_repository],
                ..Default::default()
            },
        );

        let found = index
            .resolve_fqn("App\\UserRepository::find")
            .expect("generic inherited method should resolve");
        assert_eq!(
            found
                .signature
                .as_ref()
                .and_then(|sig| sig.return_type.clone()),
            Some(TypeInfo::Simple("App\\User".to_string()))
        );
    }

    #[test]
    fn test_template_substitution_for_collection_item_type() {
        let index = WorkspaceIndex::new();

        let mut collection = make_class("Collection", "App\\Collection", "file:///collection.php");
        collection.templates = vec![TemplateParam {
            name: "TItem".to_string(),
            bound: None,
            variance: TemplateVariance::Covariant,
        }];
        let first_method = SymbolInfo {
            name: "first".to_string(),
            fqn: "App\\Collection::first".to_string(),
            kind: PhpSymbolKind::Method,
            uri: "file:///collection.php".to_string(),
            range: (3, 4, 3, 40),
            selection_range: (3, 20, 3, 25),
            visibility: Visibility::Public,
            modifiers: SymbolModifiers::default(),
            attributes: vec![],
            doc_comment: None,
            signature: Some(Signature {
                params: vec![],
                return_type: Some(TypeInfo::Simple("TItem".to_string())),
            }),
            parent_fqn: Some("App\\Collection".to_string()),
            extends: vec![],
            implements: vec![],
            traits: vec![],
            templates: vec![],
            template_bindings: vec![],
        };
        index.update_file(
            "file:///collection.php",
            FileSymbols {
                namespace: Some("App".to_string()),
                use_statements: vec![],
                symbols: vec![collection, first_method],
                ..Default::default()
            },
        );

        let mut user_collection =
            make_class("UserCollection", "App\\UserCollection", "file:///users.php");
        user_collection.extends = vec!["App\\Collection".to_string()];
        user_collection.template_bindings = vec![TemplateBinding {
            kind: TemplateBindingKind::Extends,
            target: "App\\Collection".to_string(),
            args: vec![TypeInfo::Simple("App\\User".to_string())],
        }];
        index.update_file(
            "file:///users.php",
            FileSymbols {
                namespace: Some("App".to_string()),
                use_statements: vec![],
                symbols: vec![user_collection],
                ..Default::default()
            },
        );

        let members = index.get_members("App\\UserCollection");
        let first = members
            .iter()
            .find(|member| member.name == "first")
            .expect("inherited collection method should be returned");
        assert_eq!(
            first
                .signature
                .as_ref()
                .and_then(|signature| signature.return_type.clone()),
            Some(TypeInfo::Simple("App\\User".to_string()))
        );
    }

    #[test]
    fn test_type_alias_expands_class_scoped_array_shape() {
        let index = WorkspaceIndex::new();

        let mut service = make_class("UserService", "App\\UserService", "file:///service.php");
        service.doc_comment =
            Some("/**\n * @phpstan-type UserShape array{id: int, name?: string}\n */".to_string());
        let method = SymbolInfo {
            name: "getShape".to_string(),
            fqn: "App\\UserService::getShape".to_string(),
            kind: PhpSymbolKind::Method,
            uri: "file:///service.php".to_string(),
            range: (5, 4, 7, 5),
            selection_range: (5, 20, 5, 28),
            visibility: Visibility::Public,
            modifiers: SymbolModifiers::default(),
            attributes: vec![],
            doc_comment: Some("/** @return UserShape */".to_string()),
            signature: Some(Signature {
                params: vec![],
                return_type: Some(TypeInfo::Simple("UserShape".to_string())),
            }),
            parent_fqn: Some("App\\UserService".to_string()),
            extends: vec![],
            implements: vec![],
            traits: vec![],
            templates: vec![],
            template_bindings: vec![],
        };
        index.update_file(
            "file:///service.php",
            FileSymbols {
                namespace: Some("App".to_string()),
                use_statements: vec![],
                symbols: vec![service, method],
                ..Default::default()
            },
        );

        let found = index
            .resolve_fqn("App\\UserService::getShape")
            .expect("method with type alias should resolve");
        let return_type = found
            .signature
            .as_ref()
            .and_then(|signature| signature.return_type.as_ref())
            .expect("return type should be available");
        let TypeInfo::ArrayShape(items) = return_type else {
            panic!("expected alias to expand to array shape, got {return_type:?}");
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].key.as_deref(), Some("id"));
        assert_eq!(items[1].key.as_deref(), Some("name"));
        assert!(items[1].optional);
    }

    #[test]
    fn test_imported_type_alias_expands_from_source_class() {
        let index = WorkspaceIndex::new();

        let mut types = make_class("Types", "App\\Types", "file:///types.php");
        types.doc_comment = Some("/**\n * @phpstan-type UserShape array{id: int}\n */".to_string());
        index.update_file(
            "file:///types.php",
            FileSymbols {
                namespace: Some("App".to_string()),
                use_statements: vec![],
                symbols: vec![types],
                ..Default::default()
            },
        );

        let mut service = make_class("UserService", "App\\UserService", "file:///service.php");
        service.doc_comment = Some(
            "/**\n * @phpstan-import-type UserShape from Types as LocalShape\n */".to_string(),
        );
        let method = SymbolInfo {
            name: "getShape".to_string(),
            fqn: "App\\UserService::getShape".to_string(),
            kind: PhpSymbolKind::Method,
            uri: "file:///service.php".to_string(),
            range: (5, 4, 7, 5),
            selection_range: (5, 20, 5, 28),
            visibility: Visibility::Public,
            modifiers: SymbolModifiers::default(),
            attributes: vec![],
            doc_comment: Some("/** @return LocalShape */".to_string()),
            signature: Some(Signature {
                params: vec![],
                return_type: Some(TypeInfo::Simple("LocalShape".to_string())),
            }),
            parent_fqn: Some("App\\UserService".to_string()),
            extends: vec![],
            implements: vec![],
            traits: vec![],
            templates: vec![],
            template_bindings: vec![],
        };
        index.update_file(
            "file:///service.php",
            FileSymbols {
                namespace: Some("App".to_string()),
                use_statements: vec![],
                symbols: vec![service, method],
                ..Default::default()
            },
        );

        let found = index
            .resolve_fqn("App\\UserService::getShape")
            .expect("method with imported type alias should resolve");
        assert!(matches!(
            found
                .signature
                .as_ref()
                .and_then(|signature| signature.return_type.as_ref()),
            Some(TypeInfo::ArrayShape(_))
        ));
    }

    #[test]
    fn test_file_level_type_alias_expands_function_return() {
        let index = WorkspaceIndex::new();

        let function = SymbolInfo {
            name: "getShape".to_string(),
            fqn: "App\\getShape".to_string(),
            kind: PhpSymbolKind::Function,
            uri: "file:///functions.php".to_string(),
            range: (6, 0, 8, 1),
            selection_range: (6, 9, 6, 17),
            visibility: Visibility::Public,
            modifiers: SymbolModifiers::default(),
            attributes: vec![],
            doc_comment: Some("/** @return UserShape */".to_string()),
            signature: Some(Signature {
                params: vec![],
                return_type: Some(TypeInfo::Simple("UserShape".to_string())),
            }),
            parent_fqn: None,
            extends: vec![],
            implements: vec![],
            traits: vec![],
            templates: vec![],
            template_bindings: vec![],
        };
        index.update_file(
            "file:///functions.php",
            FileSymbols {
                namespace: Some("App".to_string()),
                use_statements: vec![],
                symbols: vec![function],
                type_aliases: vec![PhpDocTypeAlias {
                    name: "UserShape".to_string(),
                    type_info: TypeInfo::ArrayShape(vec![ArrayShapeItem {
                        key: Some("id".to_string()),
                        optional: false,
                        value: TypeInfo::Simple("int".to_string()),
                    }]),
                }],
                ..Default::default()
            },
        );

        let found = index
            .resolve_fqn("App\\getShape")
            .expect("function with file-level type alias should resolve");
        assert!(matches!(
            found
                .signature
                .as_ref()
                .and_then(|signature| signature.return_type.as_ref()),
            Some(TypeInfo::ArrayShape(_))
        ));
    }

    #[test]
    fn test_recursive_type_alias_falls_back_to_raw_alias() {
        let index = WorkspaceIndex::new();

        let mut service = make_class("LoopService", "App\\LoopService", "file:///loop.php");
        service.doc_comment =
            Some("/**\n * @phpstan-type A B\n * @phpstan-type B A\n */".to_string());
        let method = SymbolInfo {
            name: "loop".to_string(),
            fqn: "App\\LoopService::loop".to_string(),
            kind: PhpSymbolKind::Method,
            uri: "file:///loop.php".to_string(),
            range: (5, 4, 7, 5),
            selection_range: (5, 20, 5, 24),
            visibility: Visibility::Public,
            modifiers: SymbolModifiers::default(),
            attributes: vec![],
            doc_comment: Some("/** @return A */".to_string()),
            signature: Some(Signature {
                params: vec![],
                return_type: Some(TypeInfo::Simple("A".to_string())),
            }),
            parent_fqn: Some("App\\LoopService".to_string()),
            extends: vec![],
            implements: vec![],
            traits: vec![],
            templates: vec![],
            template_bindings: vec![],
        };
        index.update_file(
            "file:///loop.php",
            FileSymbols {
                namespace: Some("App".to_string()),
                use_statements: vec![],
                symbols: vec![service, method],
                ..Default::default()
            },
        );

        let found = index
            .resolve_fqn("App\\LoopService::loop")
            .expect("recursive alias method should still resolve");
        assert_eq!(
            found
                .signature
                .as_ref()
                .and_then(|signature| signature.return_type.clone()),
            Some(TypeInfo::Simple("A".to_string()))
        );
    }
}
