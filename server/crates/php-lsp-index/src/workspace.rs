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
    pub fn resolve_fqn(&self, fqn: &str) -> Option<Arc<SymbolInfo>> {
        self.types
            .get(fqn)
            .map(|r| r.value().clone())
            .or_else(|| self.functions.get(fqn).map(|r| r.value().clone()))
            .or_else(|| self.constants.get(fqn).map(|r| r.value().clone()))
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
    pub fn get_members(&self, type_fqn: &str) -> Vec<Arc<SymbolInfo>> {
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
}
