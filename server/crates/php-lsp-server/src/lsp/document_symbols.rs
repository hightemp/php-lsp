//! Document Symbols LSP handlers extracted from `server.rs`.

use super::super::*;

#[derive(Debug, Clone)]
pub(crate) struct WorkspaceSymbolCandidate {
    pub(crate) score: i64,
    pub(crate) symbol: php_lsp_types::SymbolInfo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceSymbolKindFilter {
    Type,
    Class,
    Interface,
    Trait,
    Enum,
    Function,
    Method,
    Property,
    Constant,
}

pub(crate) fn workspace_symbol_candidates(
    index: &WorkspaceIndex,
    raw_query: &str,
) -> Vec<WorkspaceSymbolCandidate> {
    let (kind_filter, query) = parse_workspace_symbol_query(raw_query);
    if query.is_empty() && kind_filter.is_none() {
        return Vec::new();
    }

    let mut candidates = Vec::new();
    for file_symbols in index.file_symbols.iter() {
        for symbol in &file_symbols.symbols {
            if symbol.modifiers.is_builtin {
                continue;
            }
            if !kind_filter.is_none_or(|filter| workspace_symbol_kind_matches(symbol.kind, filter))
            {
                continue;
            }
            let Some(score) = workspace_symbol_score(symbol, &query) else {
                continue;
            };
            candidates.push(WorkspaceSymbolCandidate {
                score,
                symbol: symbol.clone(),
            });
        }
    }

    candidates.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| {
                workspace_symbol_kind_rank(left.symbol.kind)
                    .cmp(&workspace_symbol_kind_rank(right.symbol.kind))
            })
            .then_with(|| left.symbol.fqn.cmp(&right.symbol.fqn))
    });
    candidates
}

fn parse_workspace_symbol_query(raw_query: &str) -> (Option<WorkspaceSymbolKindFilter>, String) {
    let query = raw_query.trim();
    if let Some((prefix, rest)) = query.split_once(':') {
        if let Some(filter) = parse_workspace_symbol_kind_filter(prefix) {
            return (Some(filter), rest.trim().to_string());
        }
    }

    if let Some((prefix, rest)) = query.split_once(char::is_whitespace) {
        if let Some(filter) = parse_workspace_symbol_kind_filter(prefix) {
            return (Some(filter), rest.trim().to_string());
        }
    }

    (None, query.to_string())
}

fn parse_workspace_symbol_kind_filter(raw: &str) -> Option<WorkspaceSymbolKindFilter> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "type" | "types" => Some(WorkspaceSymbolKindFilter::Type),
        "class" | "classes" => Some(WorkspaceSymbolKindFilter::Class),
        "interface" | "interfaces" => Some(WorkspaceSymbolKindFilter::Interface),
        "trait" | "traits" => Some(WorkspaceSymbolKindFilter::Trait),
        "enum" | "enums" => Some(WorkspaceSymbolKindFilter::Enum),
        "function" | "functions" | "fn" => Some(WorkspaceSymbolKindFilter::Function),
        "method" | "methods" => Some(WorkspaceSymbolKindFilter::Method),
        "property" | "properties" | "prop" | "props" => Some(WorkspaceSymbolKindFilter::Property),
        "const" | "constant" | "constants" => Some(WorkspaceSymbolKindFilter::Constant),
        _ => None,
    }
}

fn workspace_symbol_kind_matches(
    kind: php_lsp_types::PhpSymbolKind,
    filter: WorkspaceSymbolKindFilter,
) -> bool {
    match filter {
        WorkspaceSymbolKindFilter::Type => matches!(
            kind,
            php_lsp_types::PhpSymbolKind::Class
                | php_lsp_types::PhpSymbolKind::Interface
                | php_lsp_types::PhpSymbolKind::Trait
                | php_lsp_types::PhpSymbolKind::Enum
        ),
        WorkspaceSymbolKindFilter::Class => kind == php_lsp_types::PhpSymbolKind::Class,
        WorkspaceSymbolKindFilter::Interface => kind == php_lsp_types::PhpSymbolKind::Interface,
        WorkspaceSymbolKindFilter::Trait => kind == php_lsp_types::PhpSymbolKind::Trait,
        WorkspaceSymbolKindFilter::Enum => kind == php_lsp_types::PhpSymbolKind::Enum,
        WorkspaceSymbolKindFilter::Function => kind == php_lsp_types::PhpSymbolKind::Function,
        WorkspaceSymbolKindFilter::Method => kind == php_lsp_types::PhpSymbolKind::Method,
        WorkspaceSymbolKindFilter::Property => kind == php_lsp_types::PhpSymbolKind::Property,
        WorkspaceSymbolKindFilter::Constant => matches!(
            kind,
            php_lsp_types::PhpSymbolKind::ClassConstant
                | php_lsp_types::PhpSymbolKind::GlobalConstant
                | php_lsp_types::PhpSymbolKind::EnumCase
        ),
    }
}

fn workspace_symbol_score(symbol: &php_lsp_types::SymbolInfo, query: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(1_000 + workspace_symbol_kind_bonus(symbol.kind));
    }

    let mut best_score = fuzzy_text_score(&symbol.name, query);
    if let Some(fqn_score) = fuzzy_text_score(&symbol.fqn, query) {
        let qualified_bonus = if query.contains('\\') { 700 } else { 100 };
        best_score = Some(
            best_score
                .unwrap_or(i64::MIN)
                .max(fqn_score + qualified_bonus),
        );
    }
    if let Some(container) = workspace_symbol_container_name(symbol) {
        if container
            .to_ascii_lowercase()
            .contains(&query.to_ascii_lowercase())
        {
            best_score = Some(best_score.unwrap_or(i64::MIN).max(5_500));
        }
    }

    Some(best_score? + workspace_symbol_kind_bonus(symbol.kind))
}

fn fuzzy_text_score(text: &str, query: &str) -> Option<i64> {
    let text_lower = text.to_ascii_lowercase();
    let query_lower = query.to_ascii_lowercase();
    if query_lower.is_empty() {
        return Some(1_000);
    }
    if text_lower == query_lower {
        return Some(10_000);
    }
    if text_lower.starts_with(&query_lower) {
        return Some(9_000 - text_lower.len().saturating_sub(query_lower.len()) as i64);
    }
    if let Some(index) = text_lower.find(&query_lower) {
        return Some(7_000 - (index as i64 * 10));
    }

    fuzzy_abbreviation_score(&text_lower, &query_lower)
}

fn fuzzy_abbreviation_score(text: &str, query: &str) -> Option<i64> {
    let mut score = 4_000i64;
    let mut last_match_index: Option<usize> = None;
    let mut search_from = 0usize;

    for query_char in query.chars() {
        let relative_index = text[search_from..].find(query_char)?;
        let absolute_index = search_from + relative_index;
        if let Some(last_match_index) = last_match_index {
            let gap = absolute_index.saturating_sub(last_match_index + 1);
            score -= gap as i64 * 8;
        } else {
            score -= absolute_index as i64 * 4;
        }
        if absolute_index == 0
            || text[..absolute_index]
                .chars()
                .last()
                .is_some_and(|ch| ch == '\\' || ch == '_' || ch == '-' || ch.is_whitespace())
        {
            score += 80;
        }
        last_match_index = Some(absolute_index);
        search_from = absolute_index + query_char.len_utf8();
    }

    Some(score - text.len() as i64)
}

fn workspace_symbol_kind_bonus(kind: php_lsp_types::PhpSymbolKind) -> i64 {
    match kind {
        php_lsp_types::PhpSymbolKind::Class => 90,
        php_lsp_types::PhpSymbolKind::Enum => 85,
        php_lsp_types::PhpSymbolKind::Interface => 80,
        php_lsp_types::PhpSymbolKind::Trait => 70,
        php_lsp_types::PhpSymbolKind::Function => 60,
        php_lsp_types::PhpSymbolKind::Method => 40,
        php_lsp_types::PhpSymbolKind::Property => 30,
        php_lsp_types::PhpSymbolKind::ClassConstant
        | php_lsp_types::PhpSymbolKind::GlobalConstant
        | php_lsp_types::PhpSymbolKind::EnumCase => 20,
        php_lsp_types::PhpSymbolKind::Namespace => 10,
    }
}

fn workspace_symbol_kind_rank(kind: php_lsp_types::PhpSymbolKind) -> u8 {
    match kind {
        php_lsp_types::PhpSymbolKind::Class => 0,
        php_lsp_types::PhpSymbolKind::Enum => 1,
        php_lsp_types::PhpSymbolKind::Interface => 2,
        php_lsp_types::PhpSymbolKind::Trait => 3,
        php_lsp_types::PhpSymbolKind::Function => 4,
        php_lsp_types::PhpSymbolKind::Method => 5,
        php_lsp_types::PhpSymbolKind::Property => 6,
        php_lsp_types::PhpSymbolKind::ClassConstant
        | php_lsp_types::PhpSymbolKind::GlobalConstant
        | php_lsp_types::PhpSymbolKind::EnumCase => 7,
        php_lsp_types::PhpSymbolKind::Namespace => 8,
    }
}

fn workspace_symbol_container_name(symbol: &php_lsp_types::SymbolInfo) -> Option<String> {
    symbol.parent_fqn.clone().or_else(|| {
        let fqn = &symbol.fqn;
        fqn.rfind('\\').map(|index| fqn[..index].to_string())
    })
}

async fn workspace_symbol_source_for_uri(
    uri_str: &str,
    open_files: &DashMap<String, FileParser>,
    source_cache: &mut HashMap<String, Option<String>>,
) -> Option<String> {
    if let Some(cached) = source_cache.get(uri_str) {
        return cached.clone();
    }

    let source = { open_files.get(uri_str).map(|parser| parser.source()) };
    let source = if source.is_some() {
        source
    } else if let Some(path) = uri_to_path(uri_str) {
        read_file_to_string_blocking(path, "workspace/symbol source read")
            .await
            .ok()
    } else {
        None
    };

    source_cache.insert(uri_str.to_string(), source.clone());
    source
}

async fn workspace_symbol_information(
    symbol: &php_lsp_types::SymbolInfo,
    open_files: &DashMap<String, FileParser>,
    source_cache: &mut HashMap<String, Option<String>>,
) -> Option<SymbolInformation> {
    let uri: Uri = symbol.uri.parse().ok()?;
    let source = workspace_symbol_source_for_uri(&symbol.uri, open_files, source_cache).await?;
    let range = workspace_symbol_lsp_range(&source, symbol.range);

    #[allow(deprecated)]
    Some(SymbolInformation {
        name: symbol.name.clone(),
        kind: php_kind_to_lsp(symbol.kind),
        tags: if symbol.modifiers.is_deprecated {
            Some(vec![SymbolTag::DEPRECATED])
        } else {
            None
        },
        deprecated: None,
        location: Location { uri, range },
        container_name: workspace_symbol_container_name(symbol),
    })
}

pub(crate) fn workspace_symbol_lsp_range(source: &str, range: (u32, u32, u32, u32)) -> Range {
    range_from_byte_range(source, range)
}

impl PhpLspBackend {
    pub(crate) async fn lsp_selection_range(
        &self,
        params: SelectionRangeParams,
    ) -> Result<Option<Vec<SelectionRange>>> {
        let uri_str = params.text_document.uri.as_str().to_string();
        let parser = match self.open_files.get(&uri_str) {
            Some(parser) => parser,
            None => return Ok(None),
        };
        let tree = match parser.tree() {
            Some(tree) => tree,
            None => return Ok(None),
        };
        let source = parser.source();
        let root = tree.root_node();

        let mut results = Vec::with_capacity(params.positions.len());
        for position in params.positions {
            let byte_col = utf16_col_to_byte(&source, position.line, position.character);
            let point = tree_sitter::Point::new(position.line as usize, byte_col as usize);
            let mut node = match root.descendant_for_point_range(point, point) {
                Some(node) => node,
                None => continue,
            };

            while !node.is_named() {
                node = match node.parent() {
                    Some(parent) => parent,
                    None => break,
                };
            }

            let mut byte_ranges = Vec::new();
            let mut current = Some(node);
            while let Some(node) = current {
                if node.is_named() && node.kind() != "program" {
                    let start = node.start_position();
                    let end = node.end_position();
                    let range = (
                        start.row as u32,
                        start.column as u32,
                        end.row as u32,
                        end.column as u32,
                    );
                    if byte_ranges.last() != Some(&range) {
                        byte_ranges.push(range);
                    }
                }
                current = node.parent();
            }

            if let Some(selection_range) = selection_range_from_byte_ranges(&source, byte_ranges) {
                results.push(selection_range);
            }
        }

        if results.is_empty() {
            Ok(None)
        } else {
            Ok(Some(results))
        }
    }

    pub(crate) async fn lsp_linked_editing_range(
        &self,
        params: LinkedEditingRangeParams,
    ) -> Result<Option<LinkedEditingRanges>> {
        let uri_str = params
            .text_document_position_params
            .text_document
            .uri
            .as_str()
            .to_string();
        let position = params.text_document_position_params.position;

        let parser = match self.open_files.get(&uri_str) {
            Some(parser) => parser,
            None => return Ok(None),
        };
        let tree = match parser.tree() {
            Some(tree) => tree,
            None => return Ok(None),
        };
        let source = parser.source();
        let byte_col = utf16_col_to_byte(&source, position.line, position.character);
        let point = tree_sitter::Point::new(position.line as usize, byte_col as usize);
        let root = tree.root_node();
        let mut node = match root.descendant_for_point_range(point, point) {
            Some(node) => node,
            None => return Ok(None),
        };

        while !node.is_named() {
            node = match node.parent() {
                Some(parent) => parent,
                None => return Ok(None),
            };
        }

        let Some(byte_ranges) = linked_editing_ranges_for_namespace_or_use(&source, node) else {
            return Ok(None);
        };
        let ranges = byte_ranges
            .into_iter()
            .map(|range| {
                let range = range_byte_to_utf16(&source, range);
                Range {
                    start: Position::new(range.0, range.1),
                    end: Position::new(range.2, range.3),
                }
            })
            .collect();

        Ok(Some(LinkedEditingRanges {
            ranges,
            word_pattern: Some("[A-Za-z_][A-Za-z0-9_]*".to_string()),
        }))
    }

    pub(crate) async fn lsp_document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri_str = params.text_document.uri.as_str().to_string();

        // Try open files first, then fall back to index
        let (file_symbols, source) = if let Some(parser) = self.open_files.get(&uri_str) {
            if let Some(tree) = parser.tree() {
                let source = parser.source();
                (extract_file_symbols(tree, &source, &uri_str), source)
            } else {
                return Ok(None);
            }
        } else if let Some(file_symbols) = self
            .index
            .file_symbols
            .get(&uri_str)
            .map(|entry| entry.value().clone())
        {
            let Some(source) = self
                .source_for_uri(&uri_str, "documentSymbol source read")
                .await
            else {
                return Ok(None);
            };
            (file_symbols, source)
        } else {
            return Ok(None);
        };

        // Build hierarchical DocumentSymbol tree
        let mut top_level: Vec<DocumentSymbol> = Vec::new();

        // Collect type-level symbols (classes, interfaces, traits, enums, functions, constants)
        // and member symbols (methods, properties, class constants, enum cases)
        let mut type_symbols: Vec<&php_lsp_types::SymbolInfo> = Vec::new();
        let mut member_symbols: Vec<&php_lsp_types::SymbolInfo> = Vec::new();
        let mut namespace_sym: Option<&php_lsp_types::SymbolInfo> = None;

        for sym in &file_symbols.symbols {
            match sym.kind {
                php_lsp_types::PhpSymbolKind::Class
                | php_lsp_types::PhpSymbolKind::Interface
                | php_lsp_types::PhpSymbolKind::Trait
                | php_lsp_types::PhpSymbolKind::Enum
                | php_lsp_types::PhpSymbolKind::Function
                | php_lsp_types::PhpSymbolKind::GlobalConstant => {
                    type_symbols.push(sym);
                }
                php_lsp_types::PhpSymbolKind::Method
                | php_lsp_types::PhpSymbolKind::Property
                | php_lsp_types::PhpSymbolKind::ClassConstant
                | php_lsp_types::PhpSymbolKind::EnumCase => {
                    member_symbols.push(sym);
                }
                php_lsp_types::PhpSymbolKind::Namespace => {
                    namespace_sym = Some(sym);
                }
            }
        }

        // Helper to convert SymbolInfo range to LSP Range
        let to_range =
            |range: (u32, u32, u32, u32)| -> Range { range_from_byte_range(&source, range) };

        // Build DocumentSymbol for a symbol with its children
        #[allow(deprecated)] // DocumentSymbol.deprecated field
        let make_doc_symbol =
            |sym: &php_lsp_types::SymbolInfo, children: Vec<DocumentSymbol>| -> DocumentSymbol {
                DocumentSymbol {
                    name: sym.name.clone(),
                    detail: sym.signature.as_ref().map(|sig| {
                        let params_str: Vec<String> = sig
                            .params
                            .iter()
                            .map(|p| {
                                let mut s = String::new();
                                if let Some(ref t) = p.type_info {
                                    s.push_str(&t.to_string());
                                    s.push(' ');
                                }
                                s.push('$');
                                s.push_str(&p.name);
                                s
                            })
                            .collect();
                        let mut detail = format!("({})", params_str.join(", "));
                        if let Some(ref ret) = sig.return_type {
                            detail.push_str(&format!(": {}", ret));
                        }
                        detail
                    }),
                    kind: php_kind_to_lsp(sym.kind),
                    tags: if sym.modifiers.is_deprecated {
                        Some(vec![SymbolTag::DEPRECATED])
                    } else {
                        None
                    },
                    deprecated: None,
                    range: to_range(sym.range),
                    selection_range: to_range(sym.selection_range),
                    children: if children.is_empty() {
                        None
                    } else {
                        Some(children)
                    },
                }
            };

        // Build type symbols with their children
        for type_sym in &type_symbols {
            let children: Vec<DocumentSymbol> = member_symbols
                .iter()
                .filter(|m| m.parent_fqn.as_deref() == Some(&type_sym.fqn))
                .map(|m| make_doc_symbol(m, vec![]))
                .collect();

            top_level.push(make_doc_symbol(type_sym, children));
        }

        // Wrap in namespace if present
        if let Some(ns) = namespace_sym {
            #[allow(deprecated)]
            let ns_symbol = DocumentSymbol {
                name: ns.name.clone(),
                detail: None,
                kind: SymbolKind::NAMESPACE,
                tags: None,
                deprecated: None,
                range: to_range(ns.range),
                selection_range: to_range(ns.selection_range),
                children: if top_level.is_empty() {
                    None
                } else {
                    Some(top_level)
                },
            };
            return Ok(Some(DocumentSymbolResponse::Nested(vec![ns_symbol])));
        }

        if top_level.is_empty() {
            Ok(None)
        } else {
            Ok(Some(DocumentSymbolResponse::Nested(top_level)))
        }
    }

    pub(crate) async fn lsp_symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<WorkspaceSymbolResponse>> {
        let query = &params.query;

        // Empty query returns nothing (avoid overwhelming results)
        if query.is_empty() {
            return Ok(Some(WorkspaceSymbolResponse::Flat(vec![])));
        }

        let candidates = workspace_symbol_candidates(&self.index, query);

        // Limit results to avoid overwhelming the client.
        let mut source_cache = HashMap::new();
        let mut symbols = Vec::new();
        for candidate in candidates.into_iter().take(200) {
            if let Some(symbol) =
                workspace_symbol_information(&candidate.symbol, &self.open_files, &mut source_cache)
                    .await
            {
                symbols.push(symbol);
            }
        }

        Ok(Some(WorkspaceSymbolResponse::Flat(symbols)))
    }
}

pub(in crate::server) fn selection_range_from_byte_ranges(
    source: &str,
    byte_ranges: Vec<(u32, u32, u32, u32)>,
) -> Option<SelectionRange> {
    let mut parent = None;

    for byte_range in byte_ranges.into_iter().rev() {
        let range = range_byte_to_utf16(source, byte_range);
        parent = Some(Box::new(SelectionRange {
            range: Range {
                start: Position::new(range.0, range.1),
                end: Position::new(range.2, range.3),
            },
            parent,
        }));
    }

    parent.map(|selection_range| *selection_range)
}

pub(in crate::server) fn node_byte_range(node: tree_sitter::Node) -> (u32, u32, u32, u32) {
    let start = node.start_position();
    let end = node.end_position();
    (
        start.row as u32,
        start.column as u32,
        end.row as u32,
        end.column as u32,
    )
}

pub(in crate::server) fn node_text<'a>(source: &'a str, node: tree_sitter::Node) -> &'a str {
    source.get(node.byte_range()).unwrap_or("")
}

pub(in crate::server) fn enclosing_linked_edit_construct(
    mut node: tree_sitter::Node,
) -> Option<tree_sitter::Node> {
    loop {
        if matches!(
            node.kind(),
            "namespace_definition"
                | "namespace_use_declaration"
                | "namespace_use_clause"
                | "namespace_use_group"
        ) {
            return Some(node);
        }
        node = node.parent()?;
    }
}

pub(in crate::server) fn collect_matching_name_ranges(
    node: tree_sitter::Node,
    source: &str,
    target: &str,
    ranges: &mut Vec<(u32, u32, u32, u32)>,
) {
    if node.kind() == "name" && node_text(source, node) == target {
        ranges.push(node_byte_range(node));
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_matching_name_ranges(child, source, target, ranges);
    }
}

pub(in crate::server) fn linked_editing_ranges_for_namespace_or_use(
    source: &str,
    node: tree_sitter::Node,
) -> Option<Vec<(u32, u32, u32, u32)>> {
    if node.kind() != "name" {
        return None;
    }

    let target = node_text(source, node);
    if target.is_empty() {
        return None;
    }

    let construct = enclosing_linked_edit_construct(node)?;
    let mut ranges = Vec::new();
    collect_matching_name_ranges(construct, source, target, &mut ranges);
    ranges.sort_unstable();
    ranges.dedup();

    (ranges.len() >= 2).then_some(ranges)
}
