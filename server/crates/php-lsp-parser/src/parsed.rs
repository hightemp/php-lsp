//! Immutable source facts shared by readers of one parsed document revision.
use crate::{function_names::FunctionNameScope, line_index::LineIndex, symbols::*};
use php_lsp_types::{FileSymbols, SymbolReference};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tree_sitter::Tree;

// Keep source facts for a small set of simultaneous extraction contexts, never
// an unbounded history of logical aliases or runtime configuration changes.
const MAX_ANALYSIS_VARIANTS: usize = 4;
type AnalysisKey = (String, Option<PhpSymbolExtractionVersion>);
#[derive(Debug)]
pub struct FileAnalysis {
    pub(crate) symbols: Arc<FileSymbols>,
    source: Arc<str>,
    tree: Tree,
    references: Mutex<Option<Arc<Vec<SymbolReference>>>>,
}
#[derive(Debug)]
pub struct ParsedSnapshot {
    pub(crate) tree: Option<Tree>,
    pub(crate) source: Arc<str>,
    pub(crate) lines: Arc<LineIndex>,
    analyses: Mutex<VecDeque<(AnalysisKey, Arc<FileAnalysis>)>>,
}
impl ParsedSnapshot {
    pub fn source(&self) -> Arc<str> {
        self.source.clone()
    }
    pub fn line_index(&self) -> Arc<LineIndex> {
        self.lines.clone()
    }
    pub(crate) fn new(tree: Option<Tree>, source: Arc<str>, lines: Arc<LineIndex>) -> Self {
        Self {
            tree,
            source,
            lines,
            analyses: Mutex::new(VecDeque::new()),
        }
    }
    pub fn tree(&self) -> Option<&Tree> {
        self.tree.as_ref()
    }
    /// Caller must drop parser-map guards before computing source facts.
    /// Cancelled work is never retained as a successful empty analysis.
    pub fn analysis(
        &self,
        uri: &str,
        version: Option<PhpSymbolExtractionVersion>,
    ) -> Option<Arc<FileAnalysis>> {
        if crate::control::is_cancelled() {
            return None;
        }
        let mut cache = self
            .analyses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if crate::control::is_cancelled() {
            return None;
        }
        if let Some(position) = cache
            .iter()
            .position(|(key, _)| key.0 == uri && key.1 == version)
        {
            let entry = cache.remove(position).unwrap();
            let result = entry.1.clone();
            cache.push_back(entry);
            return Some(result);
        }
        let tree = self.tree()?;
        let _isolation = FunctionNameScope::isolate();
        let symbols = match version {
            Some(version) => extract_file_symbols_for_php_version(tree, &self.source, uri, version),
            None => extract_file_symbols(tree, &self.source, uri),
        };
        if crate::control::is_cancelled() {
            return None;
        }
        let result = Arc::new(FileAnalysis {
            symbols: Arc::new(symbols),
            source: self.source.clone(),
            tree: tree.clone(),
            references: Mutex::new(None),
        });
        if cache.len() == MAX_ANALYSIS_VARIANTS {
            cache.pop_front();
        }
        cache.push_back(((uri.to_string(), version), result.clone()));
        Some(result)
    }
}

impl FileAnalysis {
    pub fn symbols(&self) -> Arc<FileSymbols> {
        self.symbols.clone()
    }
    /// Resolver-free references belong to the same frozen source as the symbols.
    pub fn references(&self) -> Option<Arc<Vec<SymbolReference>>> {
        if crate::control::is_cancelled() {
            return None;
        }
        let mut cache = self
            .references
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if crate::control::is_cancelled() {
            return None;
        }
        if let Some(result) = &*cache {
            return Some(result.clone());
        }
        let independent = crate::resolve::budget::independent_source_analysis();
        let _isolation = FunctionNameScope::isolate();
        let references = crate::references::collect_symbol_references_in_file_cancellable(
            &self.tree,
            &self.source,
            &self.symbols,
            None,
            None,
            &crate::control::is_cancelled,
        )?;
        if crate::control::is_cancelled() {
            return None;
        }
        let result = Arc::new(references);
        if independent {
            *cache = Some(result.clone());
        }
        Some(result)
    }
}

#[cfg(test)]
#[path = "parsed_tests.rs"]
mod tests;
