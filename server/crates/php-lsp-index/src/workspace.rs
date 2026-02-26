//! Global workspace symbol index.

use dashmap::DashMap;
use php_lsp_types::{FileSymbols, PhpSymbolKind, SymbolInfo};
use std::sync::Arc;

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
}

impl WorkspaceIndex {
    /// Create a new empty index.
    pub fn new() -> Self {
        WorkspaceIndex {
            types: DashMap::new(),
            functions: DashMap::new(),
            constants: DashMap::new(),
            file_symbols: DashMap::new(),
        }
    }

    /// Update symbols from a single file. Removes old symbols, adds new ones.
    pub fn update_file(&self, uri: &str, file_symbols: FileSymbols) {
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
    }

    /// Remove all symbols from a file.
    pub fn remove_file(&self, uri: &str) {
        if let Some((_, old_symbols)) = self.file_symbols.remove(uri) {
            for sym in &old_symbols.symbols {
                match sym.kind {
                    PhpSymbolKind::Class
                    | PhpSymbolKind::Interface
                    | PhpSymbolKind::Trait
                    | PhpSymbolKind::Enum => {
                        self.types.remove(&sym.fqn);
                    }
                    PhpSymbolKind::Function => {
                        self.functions.remove(&sym.fqn);
                    }
                    PhpSymbolKind::GlobalConstant => {
                        self.constants.remove(&sym.fqn);
                    }
                    _ => {}
                }
            }
        }
    }

    /// Resolve a fully qualified name to a symbol.
    ///
    /// Handles both top-level symbols (`App\Foo`) and member symbols
    /// (`App\Foo::method`, `App\Foo::CONST`, `App\Foo::$prop`).
    pub fn resolve_fqn(&self, fqn: &str) -> Option<Arc<SymbolInfo>> {
        // Try top-level lookup first
        if let Some(sym) = self.types.get(fqn).map(|r| r.value().clone()) {
            return Some(sym);
        }
        if let Some(sym) = self.functions.get(fqn).map(|r| r.value().clone()) {
            return Some(sym);
        }
        if let Some(sym) = self.constants.get(fqn).map(|r| r.value().clone()) {
            return Some(sym);
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
        self.resolve_member_in_hierarchy(class_fqn, member_name, fqn, &mut Vec::new())
    }

    /// Internal helper: resolve member walking the inheritance chain.
    /// `visited` prevents infinite loops when there are circular references.
    fn resolve_member_in_hierarchy(
        &self,
        class_fqn: &str,
        member_name: &str,
        original_fqn: &str,
        visited: &mut Vec<String>,
    ) -> Option<Arc<SymbolInfo>> {
        if visited.contains(&class_fqn.to_string()) {
            return None;
        }
        visited.push(class_fqn.to_string());

        let members = self.get_direct_members(class_fqn);
        // Prefer exact FQN match first
        if let Some(sym) = members.iter().find(|m| m.fqn == original_fqn) {
            return Some(sym.clone());
        }
        // Fallback: match by name (for cases where caller doesn't know exact FQN form)
        if let Some(sym) = members.iter().find(|m| m.name == member_name) {
            return Some(sym.clone());
        }

        // Walk the class hierarchy: look up extends and implements
        if let Some(class_sym) = self.types.get(class_fqn).map(|r| r.value().clone()) {
            // Try parent classes (extends)
            for parent_fqn in &class_sym.extends {
                if let Some(sym) =
                    self.resolve_member_in_hierarchy(parent_fqn, member_name, original_fqn, visited)
                {
                    return Some(sym);
                }
            }
            // Try implemented interfaces
            for iface_fqn in &class_sym.implements {
                if let Some(sym) =
                    self.resolve_member_in_hierarchy(iface_fqn, member_name, original_fqn, visited)
                {
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
        self.collect_members_recursive(type_fqn, &mut members, &mut Vec::new());
        members
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
        visited: &mut Vec<String>,
    ) {
        if visited.contains(&type_fqn.to_string()) {
            return;
        }
        visited.push(type_fqn.to_string());

        // Collect direct members
        let direct = self.get_direct_members(type_fqn);
        members.extend(direct);

        // Recurse into parent classes and interfaces
        if let Some(class_sym) = self.types.get(type_fqn).map(|r| r.value().clone()) {
            for parent_fqn in &class_sym.extends {
                self.collect_members_recursive(parent_fqn, members, visited);
            }
            for iface_fqn in &class_sym.implements {
                self.collect_members_recursive(iface_fqn, members, visited);
            }
        }
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
            doc_comment: None,
            signature: None,
            parent_fqn: None,
            extends: vec![],
            implements: vec![],
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
            doc_comment: None,
            signature: None,
            parent_fqn: None,
            extends: vec![],
            implements: vec![],
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
        };

        index.update_file("file:///test.php", file_symbols);
        assert!(index.resolve_fqn("App\\Foo").is_some());

        index.remove_file("file:///test.php");
        assert!(index.resolve_fqn("App\\Foo").is_none());
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
        };
        index.update_file("file:///test.php", sym_v1);
        assert!(index.resolve_fqn("Foo").is_some());

        let sym_v2 = FileSymbols {
            namespace: None,
            use_statements: vec![],
            symbols: vec![make_class("Bar", "Bar", "file:///test.php")],
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
            doc_comment: None,
            signature: None,
            parent_fqn: Some("App\\Foo".to_string()),
            extends: vec![],
            implements: vec![],
        };
        let file_symbols = FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![class_sym, method_sym],
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
            doc_comment: None,
            signature: None,
            parent_fqn: None,
            extends: vec![],
            implements: vec![],
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
            doc_comment: None,
            signature: None,
            parent_fqn: Some("App\\SoapHandler".to_string()),
            extends: vec![],
            implements: vec![],
        };
        let parent_file = FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![parent_class, parent_method],
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
            doc_comment: None,
            signature: None,
            parent_fqn: None,
            extends: vec!["App\\SoapHandler".to_string()],
            implements: vec![],
        };
        let child_file = FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![child_class],
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
            doc_comment: None,
            signature: None,
            parent_fqn: None,
            extends: vec!["B".to_string()],
            implements: vec![],
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
            doc_comment: None,
            signature: None,
            parent_fqn: None,
            extends: vec!["A".to_string()],
            implements: vec![],
        };
        let file_a = FileSymbols {
            namespace: None,
            use_statements: vec![],
            symbols: vec![class_a],
        };
        let file_b = FileSymbols {
            namespace: None,
            use_statements: vec![],
            symbols: vec![class_b],
        };
        index.update_file("file:///a.php", file_a);
        index.update_file("file:///b.php", file_b);

        // Should not hang — just return None
        assert!(index.resolve_fqn("A::nonexistent").is_none());
    }
}
