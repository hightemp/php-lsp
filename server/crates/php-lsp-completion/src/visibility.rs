//! Class-scope access for native completion members, without rewriting declarations.

use php_lsp_index::workspace::WorkspaceIndex;
use php_lsp_types::{FileSymbols, PhpSymbolKind, SymbolInfo, Visibility};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// PHP chooses private method binding from call syntax, even for static methods.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MemberLookup {
    /// An object call/access using `->` or `?->`.
    Object,
    /// A class call/access using `::`.
    Class,
}

/// Per-receiver visibility context. Open-file type declarations override disk metadata.
/// Callers hold an index read snapshot while collecting and filtering candidates.
pub struct MemberVisibility<'a> {
    index: &'a WorkspaceIndex,
    file: &'a FileSymbols,
    receiver: String,
    current: Option<String>,
    lookup: MemberLookup,
    types: HashMap<String, Option<Arc<SymbolInfo>>>,
    direct_members: HashMap<String, HashMap<(PhpSymbolKind, String), Visibility>>,
}

impl<'a> MemberVisibility<'a> {
    pub fn new(
        index: &'a WorkspaceIndex,
        file: &'a FileSymbols,
        receiver: &str,
        current: Option<&str>,
        lookup: MemberLookup,
    ) -> Self {
        Self {
            index,
            file,
            receiver: receiver.into(),
            current: current.map(str::to_string),
            lookup,
            types: HashMap::new(),
            direct_members: HashMap::new(),
        }
    }

    /// Select native lookup candidates before completion deduplication. An inaccessible
    /// redeclaration must not reveal a hidden property/constant through its ancestor.
    pub fn filter_members(&mut self, mut members: Vec<Arc<SymbolInfo>>) -> Vec<Arc<SymbolInfo>> {
        members.sort_by_cached_key(|member| {
            !(member.visibility == Visibility::Private
                && self.scope_bound_object_member(member)
                && self.is_visible(member))
        });
        let mut occupied = HashSet::new();
        let mut visible = Vec::new();
        for member in members {
            let key = member_key(&member);
            if occupied.contains(&key) {
                continue;
            }
            let accessible = self.is_visible(&member);
            let private_instance_binding = accessible
                && member.visibility == Visibility::Private
                && self.scope_bound_object_member(&member);
            if let Some(declaring) = member.parent_fqn.as_deref() {
                let receiver = self.receiver.clone();
                if self
                    .type_symbol(declaring)
                    .is_some_and(|symbol| symbol.kind == PhpSymbolKind::Trait)
                    && !private_instance_binding
                    && self
                        .trait_owner(&receiver, declaring, &member, &mut HashSet::new())
                        .is_none()
                {
                    continue;
                }
            }
            let occupies_lookup = member.visibility != Visibility::Private
                || !self.scope_bound_object_member(&member);
            if accessible || (occupies_lookup && self.belongs_to_receiver(&member)) {
                occupied.insert(key);
            }
            if accessible {
                visible.push(member);
            }
        }
        visible
    }

    fn scope_bound_object_member(&self, member: &SymbolInfo) -> bool {
        self.lookup == MemberLookup::Object
            && (member.kind == PhpSymbolKind::Method
                || (member.kind == PhpSymbolKind::Property && !member.modifiers.is_static))
    }

    fn belongs_to_receiver(&mut self, member: &SymbolInfo) -> bool {
        let Some(declaring) = member.parent_fqn.as_deref() else {
            return false;
        };
        let receiver = self.receiver.clone();
        match self.type_symbol(declaring) {
            Some(symbol) if symbol.kind == PhpSymbolKind::Trait => self
                .trait_owner(&receiver, declaring, member, &mut HashSet::new())
                .is_some(),
            Some(_) => self.descends_from(&receiver, declaring),
            None => false,
        }
    }

    pub fn is_visible(&mut self, member: &SymbolInfo) -> bool {
        if member.visibility == Visibility::Public {
            return true;
        }
        let Some(current) = self.current.clone() else {
            return false;
        };
        let Some(declaring) = member.parent_fqn.as_deref() else {
            return false;
        };
        let receiver = self.receiver.clone();
        let Some(declaration) = self.type_symbol(declaring) else {
            return false;
        };
        if declaration.kind == PhpSymbolKind::Trait {
            if member.visibility == Visibility::Private {
                if !self.scope_bound_object_member(member) {
                    return self
                        .trait_owner(&receiver, declaring, member, &mut HashSet::new())
                        .is_some_and(|owner| {
                            same_type(&current, &owner)
                                && (member.kind != PhpSymbolKind::ClassConstant
                                    || same_type(&receiver, &owner))
                        });
                }
                // A trait is copied into each direct consuming class independently.
                // Reusing the same trait in unrelated classes does not share private scope.
                return self.descends_from(&receiver, &current)
                    && self.provides_trait_member(
                        &current,
                        declaring,
                        member,
                        &mut HashSet::new(),
                    );
            }
            let Some(owner) = self.trait_owner(&receiver, declaring, member, &mut HashSet::new())
            else {
                return false;
            };
            return self.protected_access(&current, &owner, member);
        }
        if !self.descends_from(&receiver, declaring) {
            return false;
        }
        match member.visibility {
            Visibility::Private => {
                same_type(&current, declaring)
                    && (member.kind != PhpSymbolKind::ClassConstant
                        || same_type(&receiver, declaring))
            }
            Visibility::Protected => self.protected_access(&current, declaring, member),
            Visibility::Public => true,
        }
    }

    fn type_symbol(&mut self, fqn: &str) -> Option<Arc<SymbolInfo>> {
        let key = type_key(fqn);
        if let Some(cached) = self.types.get(&key) {
            return cached.clone();
        }
        let symbol = self
            .file
            .symbols
            .iter()
            .find(|symbol| {
                matches!(
                    symbol.kind,
                    PhpSymbolKind::Class
                        | PhpSymbolKind::Trait
                        | PhpSymbolKind::Interface
                        | PhpSymbolKind::Enum
                ) && same_type(&symbol.fqn, fqn)
            })
            .cloned()
            .map(Arc::new)
            .or_else(|| self.index.get_type(fqn));
        self.types.insert(key, symbol.clone());
        symbol
    }

    fn descends_from(&mut self, from: &str, target: &str) -> bool {
        self.extends(from, target, &mut HashSet::new())
    }

    fn extends(&mut self, from: &str, target: &str, visited: &mut HashSet<String>) -> bool {
        if same_type(from, target) {
            return true;
        }
        if !visited.insert(type_key(from)) {
            return false;
        }
        self.type_symbol(from).is_some_and(|symbol| {
            symbol
                .extends
                .iter()
                .any(|parent| self.extends(parent, target, visited))
        })
    }

    fn provides_trait_member(
        &mut self,
        owner: &str,
        target: &str,
        member: &SymbolInfo,
        visited: &mut HashSet<String>,
    ) -> bool {
        if !visited.insert(type_key(owner)) {
            return false;
        }
        if same_type(owner, target) {
            return true;
        }
        if self.direct_member_visibility(owner, member).is_some() {
            return false;
        }
        self.type_symbol(owner).is_some_and(|symbol| {
            symbol
                .traits
                .iter()
                .any(|used| self.provides_trait_member(used, target, member, visited))
        })
    }

    fn trait_owner(
        &mut self,
        receiver: &str,
        target: &str,
        member: &SymbolInfo,
        visited: &mut HashSet<String>,
    ) -> Option<String> {
        if !visited.insert(type_key(receiver)) {
            return None;
        }
        if same_type(receiver, target) {
            return Some(receiver.into());
        }
        if self.direct_member_visibility(receiver, member).is_some() {
            return None;
        }
        if self.provides_trait_member(receiver, target, member, &mut HashSet::new()) {
            return Some(receiver.into());
        }
        let symbol = self.type_symbol(receiver)?;
        symbol
            .extends
            .iter()
            .find_map(|parent| self.trait_owner(parent, target, member, visited))
    }

    fn protected_access(&mut self, current: &str, owner: &str, member: &SymbolInfo) -> bool {
        if self.descends_from(current, owner) || self.descends_from(owner, current) {
            return true;
        }
        // An override retains access through an accessible protected ancestor contract.
        // Sibling classes may call an override, but not a newly declared protected member.
        member.kind == PhpSymbolKind::Method
            && self.protected_ancestor(current, owner, member, &mut HashSet::new())
    }

    fn protected_ancestor(
        &mut self,
        current: &str,
        owner: &str,
        member: &SymbolInfo,
        visited: &mut HashSet<String>,
    ) -> bool {
        if !visited.insert(type_key(owner)) {
            return false;
        }
        let Some(symbol) = self.type_symbol(owner) else {
            return false;
        };
        symbol.extends.iter().any(|parent| {
            ((self.descends_from(current, parent) || self.descends_from(parent, current))
                && self.has_protected_member(parent, member, &mut HashSet::new()))
                || self.protected_ancestor(current, parent, member, visited)
        })
    }

    fn has_protected_member(
        &mut self,
        owner: &str,
        member: &SymbolInfo,
        visited: &mut HashSet<String>,
    ) -> bool {
        if !visited.insert(type_key(owner)) {
            return false;
        }
        if let Some(visibility) = self.direct_member_visibility(owner, member) {
            return visibility == Visibility::Protected;
        }
        self.type_symbol(owner).is_some_and(|symbol| {
            symbol
                .traits
                .iter()
                .any(|used| self.has_protected_member(used, member, visited))
        })
    }

    fn direct_member_visibility(&mut self, owner: &str, member: &SymbolInfo) -> Option<Visibility> {
        let key = type_key(owner);
        if !self.direct_members.contains_key(&key) {
            let local = self.file.symbols.iter().any(|symbol| {
                matches!(
                    symbol.kind,
                    PhpSymbolKind::Class
                        | PhpSymbolKind::Trait
                        | PhpSymbolKind::Interface
                        | PhpSymbolKind::Enum
                ) && same_type(&symbol.fqn, owner)
            });
            let mut direct = HashMap::new();
            if local {
                for candidate in &self.file.symbols {
                    if candidate
                        .parent_fqn
                        .as_deref()
                        .is_some_and(|parent| same_type(parent, owner))
                    {
                        direct.insert(member_key(candidate), candidate.visibility);
                    }
                }
            } else {
                for candidate in self.index.get_direct_members(owner) {
                    direct.insert(member_key(&candidate), candidate.visibility);
                }
            }
            self.direct_members.insert(key.clone(), direct);
        }
        self.direct_members
            .get(&key)?
            .get(&member_key(member))
            .copied()
    }
}

fn type_key(fqn: &str) -> String {
    fqn.trim_start_matches('\\').to_ascii_lowercase()
}
fn same_type(left: &str, right: &str) -> bool {
    php_lsp_types::symbol_fqn_eq(left, right, PhpSymbolKind::Class)
}
fn member_key(member: &SymbolInfo) -> (PhpSymbolKind, String) {
    (
        member.kind,
        if member.kind == PhpSymbolKind::Method {
            member.name.to_ascii_lowercase()
        } else {
            member.name.trim_start_matches('$').to_string()
        },
    )
}
