//! LSP server implementation — LanguageServer trait.

use dashmap::DashMap;
use php_lsp_completion::context::detect_context;
use php_lsp_completion::provider::provide_completions;
use php_lsp_index::composer::{parse_composer_json, NamespaceMap};
use php_lsp_index::stubs;
use php_lsp_index::workspace::WorkspaceIndex;
use php_lsp_parser::diagnostics::extract_syntax_errors;
use php_lsp_parser::parser::FileParser;
use php_lsp_parser::phpdoc::parse_phpdoc;
use php_lsp_parser::references::{find_references_in_file, find_variable_references_at_position};
use php_lsp_parser::resolve::{
    infer_variable_type_at_position, resolve_class_name_pub, symbol_at_position,
    symbol_at_position_with_resolver, variable_definition_at_position,
    variable_hover_info_at_position, RefKind, SymbolAtPosition,
};
use php_lsp_parser::return_type::{
    find_missing_return_type_candidates, MissingReturnTypeCandidate,
};
use php_lsp_parser::semantic::collect_aliased_class_fqns;
use php_lsp_parser::semantic::extract_semantic_diagnostics;
use php_lsp_parser::semantic_tokens::{
    extract_semantic_tokens, SEMANTIC_TOKEN_MODIFIERS, SEMANTIC_TOKEN_TYPES,
};
use php_lsp_parser::signature_help::signature_help_context_at_position;
use php_lsp_parser::symbols::extract_file_symbols;
use php_lsp_parser::utf16::{range_byte_to_utf16, utf16_col_to_byte, Utf16LineIndex};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, Semaphore};
use tower_lsp::jsonrpc::Result;
use tower_lsp::ls_types::*;
use tower_lsp::{Client, LanguageServer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct PhpVersion {
    major: u16,
    minor: u16,
}

impl PhpVersion {
    const DEFAULT: Self = Self { major: 8, minor: 2 };

    fn parse(raw: &str) -> Option<Self> {
        let mut parts = raw.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().unwrap_or("0").parse().ok()?;
        Some(Self { major, minor })
    }

    fn at_least(self, major: u16, minor: u16) -> bool {
        self >= Self { major, minor }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FormattingConfig {
    provider: String,
    command: Option<String>,
}

impl Default for FormattingConfig {
    fn default() -> Self {
        Self {
            provider: "none".to_string(),
            command: None,
        }
    }
}

impl FormattingConfig {
    fn from_options(provider: Option<&str>, command: Option<&str>) -> Self {
        let provider = provider.unwrap_or("none").trim().to_ascii_lowercase();
        let command = command
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        Self { provider, command }
    }

    fn command_template(&self) -> Option<String> {
        if let Some(command) = &self.command {
            return Some(command.clone());
        }

        match self.provider.as_str() {
            "php-cs-fixer" => Some("php-cs-fixer fix --using-cache=no --quiet {file}".to_string()),
            "phpcbf" => Some("phpcbf {file}".to_string()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct SemanticTokensSnapshot {
    result_id: String,
    data: Vec<SemanticToken>,
}

#[derive(Debug, Default)]
struct SemanticTokensCache {
    next_result_id: u64,
    by_uri: HashMap<String, SemanticTokensSnapshot>,
}

impl SemanticTokensCache {
    fn store(&mut self, uri: &str, data: Vec<SemanticToken>) -> SemanticTokensSnapshot {
        self.next_result_id += 1;
        let snapshot = SemanticTokensSnapshot {
            result_id: format!("semantic-tokens-{}", self.next_result_id),
            data,
        };
        self.by_uri.insert(uri.to_string(), snapshot.clone());
        snapshot
    }

    fn previous_data(&self, uri: &str, result_id: &str) -> Option<Vec<SemanticToken>> {
        let snapshot = self.by_uri.get(uri)?;
        (snapshot.result_id == result_id).then(|| snapshot.data.clone())
    }

    fn remove(&mut self, uri: &str) {
        self.by_uri.remove(uri);
    }
}

/// Main LSP backend holding all state.
pub struct PhpLspBackend {
    /// Client handle for sending notifications to VS Code.
    client: Client,
    /// Open document parsers (URI string → FileParser).
    open_files: Arc<DashMap<String, FileParser>>,
    /// Global workspace symbol index.
    index: Arc<WorkspaceIndex>,
    /// Workspace root path (set during initialize).
    workspace_root: Mutex<Option<PathBuf>>,
    /// Namespace map from composer.json.
    namespace_map: Mutex<Option<NamespaceMap>>,
    /// Trace level from InitializeParams (off/messages/verbose).
    trace_level: Mutex<TraceValue>,
    /// Path to bundled phpstorm-stubs (from client initializationOptions).
    stubs_path: Mutex<Option<PathBuf>>,
    /// Target PHP version from client initializationOptions.
    php_version: Mutex<PhpVersion>,
    /// External formatter configuration.
    formatting_config: Mutex<FormattingConfig>,
    /// Last semantic token snapshots used for full/delta requests.
    semantic_tokens_cache: Mutex<SemanticTokensCache>,
}

impl PhpLspBackend {
    pub fn new(client: Client) -> Self {
        PhpLspBackend {
            client,
            open_files: Arc::new(DashMap::new()),
            index: Arc::new(WorkspaceIndex::new()),
            workspace_root: Mutex::new(None),
            namespace_map: Mutex::new(None),
            trace_level: Mutex::new(TraceValue::Off),
            stubs_path: Mutex::new(None),
            php_version: Mutex::new(PhpVersion::DEFAULT),
            formatting_config: Mutex::new(FormattingConfig::default()),
            semantic_tokens_cache: Mutex::new(SemanticTokensCache::default()),
        }
    }

    /// Log a message to the client if trace level is verbose.
    async fn log_trace(&self, message: &str) {
        let level = *self.trace_level.lock().await;
        if level == TraceValue::Verbose {
            tracing::trace!("{}", message);
            self.client.log_message(MessageType::LOG, message).await;
        }
    }

    /// Resolve a member's type from the workspace index (for cross-file type resolution).
    ///
    /// For properties (`member_name` starts with `$`): returns the property type FQN.
    /// For methods: returns the method's return type FQN.
    ///
    /// Walks the class hierarchy to find inherited members.
    fn resolve_member_type(&self, class_fqn: &str, member_name: &str) -> Option<String> {
        // Build the member FQN to look up
        let member_fqn = format!("{}::{}", class_fqn, member_name);
        tracing::debug!("resolve_member_type: looking up {}", member_fqn);

        // Try to resolve via the workspace index (includes inheritance walk)
        let sym = match self.index.resolve_fqn(&member_fqn) {
            Some(s) => s,
            None => {
                tracing::debug!("resolve_member_type: {} not found in index", member_fqn);
                return None;
            }
        };

        // Get the type from the signature
        let sig = sym.signature.as_ref()?;
        let ret = sig.return_type.as_ref()?;
        let type_str = ret.to_string();
        tracing::debug!(
            "resolve_member_type: {} -> return type '{}'",
            member_fqn,
            type_str
        );

        if type_str.is_empty() || type_str == "mixed" {
            return None;
        }

        // Strip nullable prefix for resolution
        let base_type = type_str.strip_prefix('?').unwrap_or(&type_str);

        // Handle self/static/$this return types → return the owning class FQN
        if base_type == "self" || base_type == "static" || base_type == "$this" {
            return Some(class_fqn.to_string());
        }

        // If it looks like an FQN already (contains \), return as-is
        if base_type.contains('\\') {
            return Some(base_type.to_string());
        }

        // Resolve the type name using the member's file's use statements
        if let Some(file_syms) = self.index.file_symbols.get(&sym.uri) {
            let resolved = php_lsp_parser::resolve::resolve_class_name(base_type, &file_syms);
            Some(resolved)
        } else {
            Some(base_type.to_string())
        }
    }

    /// Resolve a FQN, falling back to lazy vendor indexing if not found.
    async fn resolve_fqn_lazy(
        &self,
        fqn: &str,
    ) -> Option<std::sync::Arc<php_lsp_types::SymbolInfo>> {
        // Try direct lookup first
        if let Some(sym) = self.index.resolve_fqn(fqn) {
            return Some(sym);
        }

        // For member FQNs like "Class::method", extract the class part
        // so PSR-4 resolution works (PSR-4 maps class names, not members).
        let class_fqn = if let Some((cls, _member)) = fqn.rsplit_once("::") {
            cls
        } else {
            fqn
        };

        // Try vendor lazy loading using the class FQN
        self.lazy_index_class(class_fqn).await;

        // After indexing the class file, also index parent classes recursively
        self.lazy_index_parents(class_fqn, 0).await;

        // Retry resolution with the full FQN
        if let Some(sym) = self.index.resolve_fqn(fqn) {
            return Some(sym);
        }

        None
    }

    /// Lazy-index a single class FQN by finding its file via PSR-4/vendor mappings.
    /// Returns true if a new file was indexed.
    async fn lazy_index_class(&self, class_fqn: &str) -> bool {
        // Skip if already in the index
        if self.index.types.contains_key(class_fqn) {
            return false;
        }

        let ns_map = self.namespace_map.lock().await;
        let root = self.workspace_root.lock().await;

        if let (Some(ref ns_map), Some(ref root)) = (&*ns_map, &*root) {
            let candidate_paths = ns_map.resolve_class_to_paths(class_fqn);

            let vendor_dir = root.join("vendor");
            let mut all_paths = candidate_paths;

            if vendor_dir.is_dir() {
                let vendor_autoload = root.join("vendor/composer/autoload_psr4.php");
                if vendor_autoload.exists() && all_paths.is_empty() {
                    if let Some(vendor_paths) = resolve_vendor_paths(class_fqn, &vendor_dir) {
                        all_paths.extend(vendor_paths);
                    }
                }
            }

            for path in &all_paths {
                let abs = if path.is_absolute() {
                    path.clone()
                } else {
                    root.join(path)
                };

                if abs.exists() {
                    if let Ok(source) = std::fs::read_to_string(&abs) {
                        let mut parser = FileParser::new();
                        parser.parse_full(&source);
                        if let Some(tree) = parser.tree() {
                            let uri = path_to_uri(&abs);
                            let file_symbols = extract_file_symbols(tree, &source, &uri);
                            self.index.update_file(&uri, file_symbols);
                            tracing::debug!("Lazy-indexed file: {}", abs.display());
                            return true;
                        }
                    }
                }
            }
        }

        false
    }

    /// Recursively lazy-index parent classes (extends + implements) up to a depth limit.
    fn lazy_index_parents<'a>(
        &'a self,
        class_fqn: &'a str,
        depth: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            const MAX_DEPTH: usize = 10;
            if depth >= MAX_DEPTH {
                return;
            }

            // Get the class from the index to read its extends/implements
            let parent_fqns: Vec<String> = if let Some(sym) = self.index.types.get(class_fqn) {
                sym.extends
                    .iter()
                    .chain(sym.implements.iter())
                    .cloned()
                    .collect()
            } else {
                return;
            };

            for parent_fqn in parent_fqns {
                // Lazy-index the parent class file
                self.lazy_index_class(&parent_fqn).await;
                // Recurse into the parent's parents
                self.lazy_index_parents(&parent_fqn, depth + 1).await;
            }
        })
    }

    /// Resolve symbol from index with fallback for global built-ins.
    fn resolve_fqn_with_fallback(
        &self,
        fqn: &str,
        ref_kind: RefKind,
    ) -> Option<std::sync::Arc<php_lsp_types::SymbolInfo>> {
        if let Some(sym) = self.index.resolve_fqn(fqn) {
            return Some(sym);
        }
        if ref_kind == RefKind::FunctionCall || ref_kind == RefKind::GlobalConstant {
            if let Some((_, short_name)) = fqn.rsplit_once('\\') {
                if let Some(sym) = self.index.resolve_fqn(short_name) {
                    return Some(sym);
                }
            }
        }
        None
    }

    /// Fallback for `$this->prop->member()` when the declared property type
    /// doesn't have `member`. Scans the file for `$this->prop = <expr>`
    /// assignments, infers the RHS type, and tries to resolve the member on that
    /// type instead.
    async fn try_property_assignment_type_fallback(
        &self,
        uri_str: &str,
        prop_name: &str,
        member_name: &str,
    ) -> Option<GotoDefinitionResponse> {
        use php_lsp_parser::resolve::infer_property_type_from_assignments;

        let inferred_types = {
            let parser = match self.open_files.get(uri_str) {
                Some(p) => p,
                None => {
                    tracing::debug!("Property fallback: file not open: {}", uri_str);
                    return None;
                }
            };
            let tree = match parser.tree() {
                Some(t) => t,
                None => {
                    tracing::debug!("Property fallback: no tree for {}", uri_str);
                    return None;
                }
            };
            let source = parser.source();

            let file_symbols = self
                .index
                .file_symbols
                .get(uri_str)
                .map(|entry| entry.value().clone())
                .unwrap_or_default();

            let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
                self.resolve_member_type(class_fqn, member_name)
            };

            let result = infer_property_type_from_assignments(
                tree,
                &source,
                prop_name,
                &file_symbols,
                Some(&resolver),
            );
            tracing::debug!(
                "Property fallback: infer_property_type_from_assignments('{}') = {:?}",
                prop_name,
                result
            );
            result
        };

        for assigned_type in &inferred_types {
            let fallback_fqn = format!("{}::{}", assigned_type, member_name);
            tracing::debug!(
                "Property assignment fallback: $this->{} assigned type '{}', trying '{}'",
                prop_name,
                assigned_type,
                fallback_fqn
            );

            if let Some(sym) = self.resolve_fqn_lazy(&fallback_fqn).await {
                if let Ok(target_uri) = sym.uri.parse::<Uri>() {
                    let range = Range {
                        start: Position::new(sym.selection_range.0, sym.selection_range.1),
                        end: Position::new(sym.selection_range.2, sym.selection_range.3),
                    };
                    return Some(GotoDefinitionResponse::Scalar(Location {
                        uri: target_uri,
                        range,
                    }));
                }
            }
        }

        None
    }

    /// Resolve symbol lazily with fallback for global built-ins.
    async fn resolve_fqn_lazy_with_fallback(
        &self,
        fqn: &str,
        ref_kind: RefKind,
    ) -> Option<std::sync::Arc<php_lsp_types::SymbolInfo>> {
        if let Some(sym) = self.resolve_fqn_lazy(fqn).await {
            return Some(sym);
        }
        if ref_kind == RefKind::FunctionCall || ref_kind == RefKind::GlobalConstant {
            if let Some((_, short_name)) = fqn.rsplit_once('\\') {
                if let Some(sym) = self.resolve_fqn_lazy(short_name).await {
                    return Some(sym);
                }
            }
        }
        None
    }

    fn import_declaration_at_position(
        &self,
        uri: &Uri,
        pos: Position,
    ) -> Option<GotoDefinitionResponse> {
        let uri_str = uri.as_str().to_string();
        let parser = self.open_files.get(&uri_str)?;
        let tree = parser.tree()?;
        let source = parser.source();
        let byte_col = utf16_col_to_byte(&source, pos.line, pos.character);
        let file_symbols = extract_file_symbols(tree, &source, &uri_str);
        let sym = symbol_at_position(tree, &source, pos.line, byte_col, &file_symbols)?;
        let use_stmt = imported_use_statement_for_symbol(&file_symbols, &sym)?;
        let range = range_byte_to_utf16(&source, use_stmt.range);

        Some(GotoDefinitionResponse::Scalar(Location {
            uri: uri.clone(),
            range: Range {
                start: Position::new(range.0, range.1),
                end: Position::new(range.2, range.3),
            },
        }))
    }

    fn file_symbols_for_uri(&self, uri_str: &str) -> Option<php_lsp_types::FileSymbols> {
        if let Some(file_symbols) = self.index.file_symbols.get(uri_str) {
            return Some(file_symbols.value().clone());
        }

        let parser = self.open_files.get(uri_str)?;
        let tree = parser.tree()?;
        let source = parser.source();
        Some(extract_file_symbols(tree, &source, uri_str))
    }

    fn type_definition_fqn_for_symbol(
        &self,
        symbol: &php_lsp_types::SymbolInfo,
        fallback_file_symbols: &php_lsp_types::FileSymbols,
    ) -> Option<String> {
        if matches!(
            symbol.kind,
            php_lsp_types::PhpSymbolKind::Class
                | php_lsp_types::PhpSymbolKind::Interface
                | php_lsp_types::PhpSymbolKind::Trait
                | php_lsp_types::PhpSymbolKind::Enum
        ) {
            return Some(symbol.fqn.clone());
        }

        let return_type = symbol.signature.as_ref()?.return_type.as_ref()?;
        let declaring_file_symbols = self
            .file_symbols_for_uri(&symbol.uri)
            .unwrap_or_else(|| fallback_file_symbols.clone());

        first_type_definition_fqn(
            return_type,
            &declaring_file_symbols,
            symbol.parent_fqn.as_deref(),
        )
    }

    async fn location_for_type_fqn(&self, fqn: &str) -> Option<Location> {
        if is_builtin_type_name(fqn) {
            return None;
        }

        let symbol = self
            .resolve_fqn_lazy_with_fallback(fqn, RefKind::ClassName)
            .await?;
        let uri = symbol.uri.parse::<Uri>().ok()?;
        Some(Location {
            uri,
            range: Range {
                start: Position::new(symbol.selection_range.0, symbol.selection_range.1),
                end: Position::new(symbol.selection_range.2, symbol.selection_range.3),
            },
        })
    }

    /// Publish diagnostics for a file.
    async fn publish_diagnostics(&self, uri: &Uri) {
        let uri_str = uri.as_str().to_string();

        // Pre-resolve use statements via lazy indexing so that vendor classes
        // are available for the synchronous `compute_diagnostics` resolver.
        if let Some(fs) = self.index.file_symbols.get(&uri_str) {
            let fqns_to_resolve: Vec<String> = fs
                .use_statements
                .iter()
                .filter(|u| u.kind == php_lsp_types::UseKind::Class)
                .filter(|u| u.fqn.contains('\\'))
                .filter(|u| !self.index.types.contains_key(u.fqn.as_str()))
                .map(|u| u.fqn.clone())
                .collect();
            drop(fs); // release DashMap ref before async calls
            for fqn in fqns_to_resolve {
                self.lazy_index_class(&fqn).await;
            }
        }

        // Also pre-resolve: class FQNs from aliased qualified names used in code.
        // e.g. `use Symfony\...\Constraints as Assert;` → `new Assert\NotBlank`
        // → need to lazily index `Symfony\...\Constraints\NotBlank`.
        if let Some(parser) = self.open_files.get(&uri_str) {
            if let Some(tree) = parser.tree() {
                let source = parser.source();
                if let Some(fs) = self.index.file_symbols.get(&uri_str) {
                    let alias_fqns = collect_aliased_class_fqns(tree, &source, &fs);
                    drop(fs);
                    for fqn in alias_fqns {
                        if !self.index.types.contains_key(fqn.as_str()) {
                            self.lazy_index_class(&fqn).await;
                        }
                    }
                }
            }
        }

        let diagnostics = {
            if let Some(parser) = self.open_files.get(&uri_str) {
                compute_diagnostics(&uri_str, &parser, &self.index)
            } else {
                vec![]
            }
        };

        self.client
            .publish_diagnostics(uri.clone(), diagnostics, None)
            .await;
    }
}

fn normalize_variable_new_name(new_name: &str) -> Option<String> {
    let trimmed = new_name.trim();
    if trimmed.is_empty() {
        return None;
    }

    let raw = trimmed.strip_prefix('$').unwrap_or(trimmed);
    if raw.is_empty() {
        return None;
    }

    let mut chars = raw.chars();
    let first = chars.next()?;
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return None;
    }
    if !chars.all(|c| c == '_' || c.is_ascii_alphanumeric()) {
        return None;
    }

    Some(format!("${}", raw))
}

fn normalize_property_new_name(new_name: &str) -> Option<String> {
    let var = normalize_variable_new_name(new_name)?;
    Some(var.trim_start_matches('$').to_string())
}

fn is_renameable_variable(var_name: &str) -> bool {
    !matches!(
        var_name,
        "$this"
            | "$GLOBALS"
            | "$_SERVER"
            | "$_GET"
            | "$_POST"
            | "$_FILES"
            | "$_COOKIE"
            | "$_SESSION"
            | "$_REQUEST"
            | "$_ENV"
            | "$http_response_header"
            | "$argc"
            | "$argv"
    )
}

fn line_byte_col_to_byte(source: &str, line: u32, byte_col: u32) -> Option<usize> {
    let mut offset = 0usize;

    for (current_line, l) in source.split_inclusive('\n').enumerate() {
        if current_line as u32 == line {
            let col = byte_col as usize;
            return (col <= l.len()).then_some(offset + col);
        }
        offset += l.len();
    }

    None
}

fn range_starts_with_dollar(source: &str, range: (u32, u32, u32, u32)) -> bool {
    let Some(start) = line_byte_col_to_byte(source, range.0, range.1) else {
        return false;
    };
    source
        .as_bytes()
        .get(start)
        .map(|b| *b == b'$')
        .unwrap_or(false)
}

fn starts_with_assignment_operator(text: &str) -> bool {
    matches!(
        text.as_bytes(),
        [b'=', rest @ ..] if !matches!(rest.first(), Some(b'=' | b'>'))
    ) || text.starts_with("+=")
        || text.starts_with("-=")
        || text.starts_with("*=")
        || text.starts_with("/=")
        || text.starts_with("%=")
        || text.starts_with(".=")
        || text.starts_with("&=")
        || text.starts_with("|=")
        || text.starts_with("^=")
        || text.starts_with("??=")
        || text.starts_with("<<=")
        || text.starts_with(">>=")
}

fn is_declaration_like_write(before_trimmed: &str, after_trimmed: &str) -> bool {
    let segment = before_trimmed
        .rsplit([';', '{', '}'])
        .next()
        .unwrap_or(before_trimmed)
        .trim_start();
    let declaration_tail = after_trimmed.starts_with([',', ')', ';', '=']);

    declaration_tail
        && (segment.contains("function ")
            || segment.starts_with("public ")
            || segment.starts_with("protected ")
            || segment.starts_with("private ")
            || segment.starts_with("readonly ")
            || segment.starts_with("static ")
            || segment.starts_with("var "))
}

fn is_write_reference(source: &str, range: (u32, u32, u32, u32)) -> bool {
    let Some(start) = line_byte_col_to_byte(source, range.0, range.1) else {
        return false;
    };
    let Some(end) = line_byte_col_to_byte(source, range.2, range.3) else {
        return false;
    };
    if start > end || end > source.len() {
        return false;
    }

    let line_start = source[..start].rfind('\n').map(|idx| idx + 1).unwrap_or(0);
    let line_end = source[end..]
        .find('\n')
        .map(|idx| end + idx)
        .unwrap_or(source.len());
    let before_trimmed = source[line_start..start].trim_end();
    let after_trimmed = source[end..line_end].trim_start();

    starts_with_assignment_operator(after_trimmed)
        || after_trimmed.starts_with("++")
        || after_trimmed.starts_with("--")
        || before_trimmed.ends_with("++")
        || before_trimmed.ends_with("--")
        || is_declaration_like_write(before_trimmed, after_trimmed)
}

fn document_highlight_kind(
    source: &str,
    range: (u32, u32, u32, u32),
    read_write_capable: bool,
) -> DocumentHighlightKind {
    if !read_write_capable {
        return DocumentHighlightKind::TEXT;
    }

    if is_write_reference(source, range) {
        DocumentHighlightKind::WRITE
    } else {
        DocumentHighlightKind::READ
    }
}

fn document_highlight_from_range(
    source: &str,
    range: (u32, u32, u32, u32),
    read_write_capable: bool,
) -> DocumentHighlight {
    let rng = range_byte_to_utf16(source, range);
    DocumentHighlight {
        range: Range {
            start: Position::new(rng.0, rng.1),
            end: Position::new(rng.2, rng.3),
        },
        kind: Some(document_highlight_kind(source, range, read_write_capable)),
    }
}

fn php_symbol_kind_for_ref_kind(ref_kind: RefKind) -> Option<php_lsp_types::PhpSymbolKind> {
    match ref_kind {
        RefKind::ClassName | RefKind::Constructor => Some(php_lsp_types::PhpSymbolKind::Class),
        RefKind::FunctionCall => Some(php_lsp_types::PhpSymbolKind::Function),
        RefKind::MethodCall => Some(php_lsp_types::PhpSymbolKind::Method),
        RefKind::PropertyAccess | RefKind::StaticPropertyAccess => {
            Some(php_lsp_types::PhpSymbolKind::Property)
        }
        RefKind::ClassConstant => Some(php_lsp_types::PhpSymbolKind::ClassConstant),
        RefKind::GlobalConstant => Some(php_lsp_types::PhpSymbolKind::GlobalConstant),
        RefKind::Variable | RefKind::NamespaceName | RefKind::Unknown => None,
    }
}

fn format_signature_param(param: &php_lsp_types::ParamInfo) -> String {
    let mut label = String::new();
    if let Some(ref type_info) = param.type_info {
        label.push_str(&type_info.to_string());
        label.push(' ');
    }
    if param.is_variadic {
        label.push_str("...");
    }
    if param.is_by_ref {
        label.push('&');
    }
    if param.name.starts_with('$') {
        label.push_str(&param.name);
    } else {
        label.push('$');
        label.push_str(&param.name);
    }
    if let Some(ref default) = param.default_value {
        label.push_str(" = ");
        label.push_str(default);
    }
    label
}

fn build_signature_help(
    sym: &php_lsp_types::SymbolInfo,
    active_parameter: usize,
) -> Option<SignatureHelp> {
    let sig = sym.signature.as_ref()?;
    let param_labels: Vec<String> = sig.params.iter().map(format_signature_param).collect();

    let mut label = String::new();
    label.push_str(&sym.fqn);
    label.push('(');
    label.push_str(&param_labels.join(", "));
    label.push(')');
    if let Some(ref ret) = sig.return_type {
        label.push_str(": ");
        label.push_str(&ret.to_string());
    }

    let phpdoc = sym.doc_comment.as_ref().map(|doc| parse_phpdoc(doc));
    let documentation = phpdoc.as_ref().and_then(|doc| {
        let mut parts = Vec::new();
        if let Some(ref summary) = doc.summary {
            parts.push(summary.clone());
        }
        if let Some(ref ret) = doc.return_type {
            parts.push(format!("@return `{}`", ret));
        }
        if parts.is_empty() {
            None
        } else {
            Some(Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: parts.join("\n\n"),
            }))
        }
    });

    let parameters: Vec<ParameterInformation> = sig
        .params
        .iter()
        .zip(param_labels.iter())
        .map(|(param, label)| {
            let documentation = phpdoc.as_ref().and_then(|doc| {
                doc.params
                    .iter()
                    .find(|p| p.name == param.name)
                    .and_then(|p| {
                        let mut parts = Vec::new();
                        if let Some(ref type_info) = p.type_info {
                            parts.push(format!("`{}`", type_info));
                        }
                        if let Some(ref desc) = p.description {
                            parts.push(desc.clone());
                        }
                        if parts.is_empty() {
                            None
                        } else {
                            Some(Documentation::MarkupContent(MarkupContent {
                                kind: MarkupKind::Markdown,
                                value: parts.join(" — "),
                            }))
                        }
                    })
            });

            ParameterInformation {
                label: ParameterLabel::Simple(label.clone()),
                documentation,
            }
        })
        .collect();

    let active_parameter = if sig.params.is_empty() {
        None
    } else {
        Some(active_parameter.min(sig.params.len() - 1) as u32)
    };

    Some(SignatureHelp {
        signatures: vec![SignatureInformation {
            label,
            documentation,
            parameters: Some(parameters),
            active_parameter,
        }],
        active_signature: Some(0),
        active_parameter,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ImportKind {
    Class,
    Function,
    Constant,
}

fn code_action_kind_allowed(only: Option<&Vec<CodeActionKind>>, kind: &CodeActionKind) -> bool {
    only.map(|kinds| kinds.is_empty() || kinds.iter().any(|k| k == kind))
        .unwrap_or(true)
}

fn unknown_symbol_from_diagnostic(message: &str) -> Option<(ImportKind, String)> {
    if let Some(fqn) = message.strip_prefix("Unknown class: ") {
        return Some((ImportKind::Class, fqn.to_string()));
    }
    if let Some(fqn) = message.strip_prefix("Unknown function: ") {
        return Some((ImportKind::Function, fqn.to_string()));
    }
    None
}

fn short_name(fqn: &str) -> &str {
    fqn.trim_start_matches('\\')
        .rsplit('\\')
        .next()
        .unwrap_or(fqn)
}

fn use_kind_for_ref_kind(ref_kind: RefKind) -> Option<php_lsp_types::UseKind> {
    match ref_kind {
        RefKind::ClassName | RefKind::Constructor => Some(php_lsp_types::UseKind::Class),
        RefKind::FunctionCall => Some(php_lsp_types::UseKind::Function),
        RefKind::GlobalConstant => Some(php_lsp_types::UseKind::Constant),
        _ => None,
    }
}

fn import_target_fqn(sym: &SymbolAtPosition) -> &str {
    if sym.ref_kind == RefKind::Constructor {
        sym.fqn
            .strip_suffix("::__construct")
            .unwrap_or(sym.fqn.as_str())
    } else {
        sym.fqn.as_str()
    }
}

fn imported_use_statement_for_symbol<'a>(
    file_symbols: &'a php_lsp_types::FileSymbols,
    sym: &SymbolAtPosition,
) -> Option<&'a php_lsp_types::UseStatement> {
    let use_kind = use_kind_for_ref_kind(sym.ref_kind)?;
    let target_fqn = import_target_fqn(sym).trim_start_matches('\\');

    file_symbols.use_statements.iter().find(|use_stmt| {
        use_stmt.kind == use_kind && use_stmt.fqn.trim_start_matches('\\') == target_fqn
    })
}

fn is_builtin_type_name(name: &str) -> bool {
    matches!(
        name.trim_start_matches('\\').to_ascii_lowercase().as_str(),
        "int"
            | "float"
            | "string"
            | "bool"
            | "boolean"
            | "array"
            | "object"
            | "null"
            | "void"
            | "never"
            | "mixed"
            | "callable"
            | "iterable"
            | "true"
            | "false"
            | "resource"
    )
}

fn first_type_definition_fqn(
    type_info: &php_lsp_types::TypeInfo,
    file_symbols: &php_lsp_types::FileSymbols,
    current_class_fqn: Option<&str>,
) -> Option<String> {
    match type_info {
        php_lsp_types::TypeInfo::Simple(name) => {
            if is_builtin_type_name(name) {
                None
            } else {
                Some(resolve_class_name_pub(name, file_symbols))
            }
        }
        php_lsp_types::TypeInfo::Nullable(inner) => {
            first_type_definition_fqn(inner, file_symbols, current_class_fqn)
        }
        php_lsp_types::TypeInfo::Union(types) | php_lsp_types::TypeInfo::Intersection(types) => {
            types
                .iter()
                .find_map(|ty| first_type_definition_fqn(ty, file_symbols, current_class_fqn))
        }
        php_lsp_types::TypeInfo::Self_ | php_lsp_types::TypeInfo::Static_ => {
            current_class_fqn.map(str::to_string)
        }
        php_lsp_types::TypeInfo::Parent_ => current_class_fqn.and_then(|class_fqn| {
            file_symbols
                .symbols
                .iter()
                .find(|sym| sym.fqn == class_fqn)
                .and_then(|sym| sym.extends.first().cloned())
        }),
        php_lsp_types::TypeInfo::Void
        | php_lsp_types::TypeInfo::Never
        | php_lsp_types::TypeInfo::Mixed => None,
    }
}

fn use_kind_matches(import_kind: ImportKind, use_kind: php_lsp_types::UseKind) -> bool {
    matches!(
        (import_kind, use_kind),
        (ImportKind::Class, php_lsp_types::UseKind::Class)
            | (ImportKind::Function, php_lsp_types::UseKind::Function)
            | (ImportKind::Constant, php_lsp_types::UseKind::Constant)
    )
}

fn import_kind_from_use_kind(use_kind: php_lsp_types::UseKind) -> ImportKind {
    match use_kind {
        php_lsp_types::UseKind::Class => ImportKind::Class,
        php_lsp_types::UseKind::Function => ImportKind::Function,
        php_lsp_types::UseKind::Constant => ImportKind::Constant,
    }
}

fn existing_use_alias(use_stmt: &php_lsp_types::UseStatement) -> String {
    use_stmt
        .alias
        .clone()
        .unwrap_or_else(|| short_name(&use_stmt.fqn).to_string())
}

fn used_import_aliases(
    file_symbols: &php_lsp_types::FileSymbols,
    import_kind: ImportKind,
) -> std::collections::HashSet<String> {
    let mut aliases = std::collections::HashSet::new();
    for use_stmt in &file_symbols.use_statements {
        if use_kind_matches(import_kind, use_stmt.kind) {
            aliases.insert(existing_use_alias(use_stmt));
        }
    }
    if import_kind == ImportKind::Class {
        for sym in &file_symbols.symbols {
            if matches!(
                sym.kind,
                php_lsp_types::PhpSymbolKind::Class
                    | php_lsp_types::PhpSymbolKind::Interface
                    | php_lsp_types::PhpSymbolKind::Trait
                    | php_lsp_types::PhpSymbolKind::Enum
            ) {
                aliases.insert(sym.name.clone());
            }
        }
    }
    aliases
}

fn unique_import_alias(base: &str, used: &std::collections::HashSet<String>) -> String {
    let mut candidate = format!("{}Import", base);
    let mut suffix = 2usize;
    while used.contains(&candidate) {
        candidate = format!("{}Import{}", base, suffix);
        suffix += 1;
    }
    candidate
}

fn existing_import_for_fqn<'a>(
    file_symbols: &'a php_lsp_types::FileSymbols,
    fqn: &str,
    import_kind: ImportKind,
) -> Option<&'a php_lsp_types::UseStatement> {
    file_symbols
        .use_statements
        .iter()
        .find(|use_stmt| use_kind_matches(import_kind, use_stmt.kind) && use_stmt.fqn == fqn)
}

fn line_is_blank(source: &str, line: u32) -> bool {
    source
        .lines()
        .nth(line as usize)
        .map(|line| line.trim().is_empty())
        .unwrap_or(false)
}

fn find_use_insert_line(source: &str, file_symbols: &php_lsp_types::FileSymbols) -> u32 {
    if let Some(last_use_line) = file_symbols
        .use_statements
        .iter()
        .map(|use_stmt| use_stmt.range.2)
        .max()
    {
        return last_use_line + 1;
    }

    for (idx, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("namespace ") && (trimmed.contains(';') || trimmed.contains('{')) {
            return idx as u32 + 1;
        }
    }

    if source
        .lines()
        .next()
        .is_some_and(|line| line.trim() == "<?php")
    {
        1
    } else {
        0
    }
}

fn build_use_statement(import_fqn: &str, import_kind: ImportKind, alias: Option<&str>) -> String {
    let import_fqn = import_fqn.trim_start_matches('\\');
    let prefix = match import_kind {
        ImportKind::Class => "use",
        ImportKind::Function => "use function",
        ImportKind::Constant => "use const",
    };
    match alias {
        Some(alias) => format!("{} {} as {};", prefix, import_fqn, alias),
        None => format!("{} {};", prefix, import_fqn),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct OrganizableImport {
    fqn: String,
    alias: Option<String>,
    kind: ImportKind,
}

fn import_kind_sort_key(kind: ImportKind) -> u8 {
    match kind {
        ImportKind::Class => 0,
        ImportKind::Function => 1,
        ImportKind::Constant => 2,
    }
}

fn source_line(source: &str, line: u32) -> Option<&str> {
    source.lines().nth(line as usize)
}

fn is_simple_use_statement_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with("use ")
        && trimmed.ends_with(';')
        && !trimmed.contains('{')
        && !trimmed.contains('}')
}

fn find_organizable_use_block(
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
) -> Option<(u32, u32)> {
    let start_line = file_symbols
        .use_statements
        .iter()
        .map(|use_stmt| use_stmt.range.0)
        .min()?;
    let end_line = file_symbols
        .use_statements
        .iter()
        .map(|use_stmt| use_stmt.range.2)
        .max()?
        + 1;

    for use_stmt in &file_symbols.use_statements {
        if use_stmt.range.0 != use_stmt.range.2 {
            return None;
        }
        let line = source_line(source, use_stmt.range.0)?;
        if !is_simple_use_statement_line(line) {
            return None;
        }
    }

    for line_idx in start_line..end_line {
        let line = source_line(source, line_idx)?;
        let trimmed = line.trim();
        if !trimmed.is_empty() && !is_simple_use_statement_line(line) {
            return None;
        }
    }

    Some((start_line, end_line))
}

fn source_without_line_range(source: &str, start_line: u32, end_line: u32) -> String {
    let mut result = String::with_capacity(source.len());
    for (line_idx, line) in source.split_inclusive('\n').enumerate() {
        if (start_line as usize..end_line as usize).contains(&line_idx) {
            result.push('\n');
        } else {
            result.push_str(line);
        }
    }
    result
}

fn is_php_identifier_char(ch: char) -> bool {
    ch == '_' || ch == '$' || ch.is_ascii_alphanumeric() || !ch.is_ascii()
}

fn has_identifier_boundaries(source: &str, start: usize, end: usize) -> bool {
    let before_ok = source[..start]
        .chars()
        .next_back()
        .map(|ch| !is_php_identifier_char(ch))
        .unwrap_or(true);
    let after_ok = source[end..]
        .chars()
        .next()
        .map(|ch| !is_php_identifier_char(ch))
        .unwrap_or(true);
    before_ok && after_ok
}

fn contains_php_identifier(source: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }

    let mut offset = 0usize;
    while let Some(relative) = source[offset..].find(name) {
        let start = offset + relative;
        let end = start + name.len();
        if has_identifier_boundaries(source, start, end) {
            return true;
        }
        offset = end;
    }

    false
}

fn contains_php_function_call(source: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }

    let mut offset = 0usize;
    while let Some(relative) = source[offset..].find(name) {
        let start = offset + relative;
        let end = start + name.len();
        if has_identifier_boundaries(source, start, end) {
            let after_name = source[end..].trim_start();
            if after_name.starts_with('(') {
                return true;
            }
        }
        offset = end;
    }

    false
}

fn import_is_used(source_without_imports: &str, import: &OrganizableImport) -> bool {
    let alias = import
        .alias
        .as_deref()
        .unwrap_or_else(|| short_name(&import.fqn));

    match import.kind {
        ImportKind::Class => contains_php_identifier(source_without_imports, alias),
        ImportKind::Function => contains_php_function_call(source_without_imports, alias),
        ImportKind::Constant => contains_php_identifier(source_without_imports, alias),
    }
}

fn build_organize_imports_edit(
    uri: Uri,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
) -> Option<WorkspaceEdit> {
    if file_symbols.use_statements.is_empty() {
        return None;
    }

    let (start_line, end_line) = find_organizable_use_block(source, file_symbols)?;
    let source_without_imports = source_without_line_range(source, start_line, end_line);

    let mut imports: Vec<OrganizableImport> = file_symbols
        .use_statements
        .iter()
        .map(|use_stmt| OrganizableImport {
            fqn: use_stmt.fqn.trim_start_matches('\\').to_string(),
            alias: use_stmt.alias.clone(),
            kind: import_kind_from_use_kind(use_stmt.kind),
        })
        .filter(|import| import_is_used(&source_without_imports, import))
        .collect();

    imports.sort_by(|a, b| {
        import_kind_sort_key(a.kind)
            .cmp(&import_kind_sort_key(b.kind))
            .then_with(|| a.fqn.to_lowercase().cmp(&b.fqn.to_lowercase()))
            .then_with(|| a.alias.cmp(&b.alias))
    });
    imports.dedup();

    let mut groups = Vec::new();
    for kind in [
        ImportKind::Class,
        ImportKind::Function,
        ImportKind::Constant,
    ] {
        let lines: Vec<String> = imports
            .iter()
            .filter(|import| import.kind == kind)
            .map(|import| build_use_statement(&import.fqn, import.kind, import.alias.as_deref()))
            .collect();
        if !lines.is_empty() {
            groups.push(lines.join("\n"));
        }
    }

    let mut new_text = groups.join("\n\n");
    if !new_text.is_empty() {
        new_text.push('\n');
        if !line_is_blank(source, end_line) {
            new_text.push('\n');
        }
    }

    let range = Range {
        start: Position::new(start_line, 0),
        end: Position::new(end_line, 0),
    };
    if text_at_lsp_range(source, range)
        .map(|old_text| old_text == new_text)
        .unwrap_or(false)
    {
        return None;
    }

    let mut changes = std::collections::HashMap::new();
    changes.insert(uri, vec![TextEdit { range, new_text }]);
    Some(WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    })
}

fn lsp_range_to_byte_range(source: &str, range: Range) -> (u32, u32, u32, u32) {
    (
        range.start.line,
        utf16_col_to_byte(source, range.start.line, range.start.character),
        range.end.line,
        utf16_col_to_byte(source, range.end.line, range.end.character),
    )
}

fn simple_return_type_hint_is_supported(
    name: &str,
    php_version: PhpVersion,
    in_union: bool,
) -> bool {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('$')
        || trimmed.contains(['<', '>', '[', ']', '(', ')', ',', ' '])
    {
        return false;
    }

    let lower = trimmed.trim_start_matches('\\').to_ascii_lowercase();
    match lower.as_str() {
        "void" => false,
        "never" => php_version.at_least(8, 1),
        "mixed" => php_version.at_least(8, 0),
        "static" => php_version.at_least(8, 0),
        "false" | "null" => {
            if in_union {
                php_version.at_least(8, 0)
            } else {
                php_version.at_least(8, 2)
            }
        }
        "true" => php_version.at_least(8, 2),
        "resource" => false,
        _ => true,
    }
}

fn is_intersection_member_type(type_info: &php_lsp_types::TypeInfo) -> bool {
    let php_lsp_types::TypeInfo::Simple(name) = type_info else {
        return false;
    };
    let lower = name.trim_start_matches('\\').to_ascii_lowercase();
    !matches!(
        lower.as_str(),
        "array"
            | "bool"
            | "callable"
            | "false"
            | "float"
            | "int"
            | "iterable"
            | "mixed"
            | "never"
            | "null"
            | "object"
            | "resource"
            | "string"
            | "true"
            | "void"
    ) && simple_return_type_hint_is_supported(name, PhpVersion::DEFAULT, false)
}

fn return_type_hint_is_supported(
    type_info: &php_lsp_types::TypeInfo,
    php_version: PhpVersion,
    in_union: bool,
) -> bool {
    match type_info {
        php_lsp_types::TypeInfo::Simple(name) => {
            simple_return_type_hint_is_supported(name, php_version, in_union)
        }
        php_lsp_types::TypeInfo::Union(types) => {
            php_version.at_least(8, 0)
                && types
                    .iter()
                    .all(|t| !matches!(t, php_lsp_types::TypeInfo::Void))
                && types
                    .iter()
                    .all(|t| return_type_hint_is_supported(t, php_version, true))
        }
        php_lsp_types::TypeInfo::Intersection(types) => {
            php_version.at_least(8, 1) && types.iter().all(is_intersection_member_type)
        }
        php_lsp_types::TypeInfo::Nullable(inner) => {
            php_version.at_least(7, 1)
                && !matches!(
                    inner.as_ref(),
                    php_lsp_types::TypeInfo::Mixed
                        | php_lsp_types::TypeInfo::Never
                        | php_lsp_types::TypeInfo::Void
                        | php_lsp_types::TypeInfo::Union(_)
                        | php_lsp_types::TypeInfo::Intersection(_)
                )
                && return_type_hint_is_supported(inner, php_version, false)
        }
        php_lsp_types::TypeInfo::Void => php_version.at_least(7, 1),
        php_lsp_types::TypeInfo::Never => php_version.at_least(8, 1),
        php_lsp_types::TypeInfo::Mixed => php_version.at_least(8, 0),
        php_lsp_types::TypeInfo::Self_ | php_lsp_types::TypeInfo::Parent_ => true,
        php_lsp_types::TypeInfo::Static_ => php_version.at_least(8, 0),
    }
}

fn return_type_hint(
    type_info: &php_lsp_types::TypeInfo,
    php_version: PhpVersion,
) -> Option<String> {
    if return_type_hint_is_supported(type_info, php_version, false) {
        Some(type_info.to_string())
    } else {
        None
    }
}

fn build_add_return_type_action(
    uri: Uri,
    source: &str,
    candidate: &MissingReturnTypeCandidate,
    php_version: PhpVersion,
) -> Option<CodeActionOrCommand> {
    let hint = return_type_hint(&candidate.return_type, php_version)?;
    let utf16_index = Utf16LineIndex::new(source);
    let insert_position = Position::new(
        candidate.insert_position.0,
        utf16_index.byte_col_to_utf16(candidate.insert_position.0, candidate.insert_position.1),
    );

    let mut changes = std::collections::HashMap::new();
    changes.insert(
        uri,
        vec![TextEdit {
            range: Range {
                start: insert_position,
                end: insert_position,
            },
            new_text: format!(": {}", hint),
        }],
    );

    Some(CodeActionOrCommand::CodeAction(CodeAction {
        title: format!("Add return type `{}`", hint),
        kind: Some(CodeActionKind::REFACTOR_REWRITE),
        diagnostics: None,
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }),
        command: None,
        is_preferred: Some(false),
        disabled: None,
        data: None,
    }))
}

fn semantic_tokens_legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: SEMANTIC_TOKEN_TYPES
            .iter()
            .map(|token_type| SemanticTokenType::from(*token_type))
            .collect(),
        token_modifiers: SEMANTIC_TOKEN_MODIFIERS
            .iter()
            .map(|modifier| SemanticTokenModifier::from(*modifier))
            .collect(),
    }
}

fn semantic_tokens_for_parser(parser: &FileParser) -> Option<Vec<SemanticToken>> {
    let tree = parser.tree()?;
    let source = parser.source();
    Some(
        extract_semantic_tokens(tree, &source)
            .into_iter()
            .map(|token| SemanticToken {
                delta_line: token.delta_line,
                delta_start: token.delta_start,
                length: token.length,
                token_type: token.token_type,
                token_modifiers_bitset: token.token_modifiers_bitset,
            })
            .collect(),
    )
}

fn semantic_tokens_flat_len(token_count: usize) -> u32 {
    u32::try_from(token_count.saturating_mul(5)).unwrap_or(u32::MAX)
}

fn semantic_tokens_delta_edits(
    previous: &[SemanticToken],
    current: &[SemanticToken],
) -> Vec<SemanticTokensEdit> {
    if previous == current {
        return vec![];
    }

    let mut common_prefix = 0usize;
    while common_prefix < previous.len()
        && common_prefix < current.len()
        && previous[common_prefix] == current[common_prefix]
    {
        common_prefix += 1;
    }

    let mut common_suffix = 0usize;
    while common_suffix < previous.len().saturating_sub(common_prefix)
        && common_suffix < current.len().saturating_sub(common_prefix)
        && previous[previous.len() - 1 - common_suffix]
            == current[current.len() - 1 - common_suffix]
    {
        common_suffix += 1;
    }

    let delete_count = previous.len() - common_prefix - common_suffix;
    let insert_end = current.len() - common_suffix;
    let inserted = current[common_prefix..insert_end].to_vec();

    vec![SemanticTokensEdit {
        start: semantic_tokens_flat_len(common_prefix),
        delete_count: semantic_tokens_flat_len(delete_count),
        data: (!inserted.is_empty()).then_some(inserted),
    }]
}

fn full_document_range(source: &str) -> Range {
    let mut line = 0u32;
    let mut character = 0u32;

    for ch in source.chars() {
        if ch == '\n' {
            line += 1;
            character = 0;
        } else {
            character += ch.len_utf16() as u32;
        }
    }

    Range {
        start: Position::new(0, 0),
        end: Position::new(line, character),
    }
}

fn shell_escape(value: &str) -> String {
    if cfg!(windows) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn build_formatter_shell_command(template: &str, file_path: &Path) -> String {
    let escaped_file = shell_escape(&file_path.to_string_lossy());
    if template.contains("{file}") {
        template.replace("{file}", &escaped_file)
    } else {
        format!("{} {}", template, escaped_file)
    }
}

fn run_formatter_shell_command(
    command: &str,
    current_dir: Option<&Path>,
) -> std::io::Result<std::process::Output> {
    let mut process = if cfg!(windows) {
        let mut command_process = std::process::Command::new("cmd");
        command_process.arg("/C").arg(command);
        command_process
    } else {
        let mut command_process = std::process::Command::new("sh");
        command_process.arg("-c").arg(command);
        command_process
    };

    if let Some(current_dir) = current_dir {
        process.current_dir(current_dir);
    }

    process.output()
}

fn temp_format_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!("php-lsp-format-{}-{}", std::process::id(), nanos))
}

fn run_external_formatter(
    source: String,
    config: FormattingConfig,
    workspace_root: Option<PathBuf>,
) -> std::result::Result<Option<String>, String> {
    let Some(template) = config.command_template() else {
        return Ok(None);
    };

    let temp_dir = temp_format_dir();
    let file_path = temp_dir.join("input.php");
    std::fs::create_dir_all(&temp_dir)
        .map_err(|err| format!("failed to create formatter temp dir: {}", err))?;
    std::fs::write(&file_path, &source)
        .map_err(|err| format!("failed to write formatter temp file: {}", err))?;

    let command = build_formatter_shell_command(&template, &file_path);
    let output = run_formatter_shell_command(&command, workspace_root.as_deref())
        .map_err(|err| format!("failed to run formatter command: {}", err));
    let formatted = std::fs::read_to_string(&file_path)
        .map_err(|err| format!("failed to read formatter temp file: {}", err));
    let _ = std::fs::remove_dir_all(&temp_dir);

    let output = output?;
    let formatted = formatted?;

    if !output.status.success() && formatted == source {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let details = if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else {
            stdout.trim().to_string()
        };
        return Err(if details.is_empty() {
            format!("formatter command exited with {}", output.status)
        } else {
            format!(
                "formatter command exited with {}: {}",
                output.status, details
            )
        });
    }

    if formatted == source {
        Ok(None)
    } else {
        Ok(Some(formatted))
    }
}

fn range_formatter_input(fragment: &str) -> (String, bool) {
    if fragment.trim_start().starts_with("<?php") {
        (fragment.to_string(), false)
    } else {
        (format!("<?php\n{}", fragment), true)
    }
}

fn strip_range_formatter_wrapper(formatted: String, was_wrapped: bool) -> String {
    if !was_wrapped {
        return formatted;
    }

    formatted
        .strip_prefix("<?php\n")
        .or_else(|| formatted.strip_prefix("<?php\r\n"))
        .unwrap_or(&formatted)
        .to_string()
}

fn formatting_source_line(source: &str, line: u32) -> Option<&str> {
    source.split('\n').nth(line as usize)
}

fn leading_indent(line: &str) -> &str {
    let indent_end = line
        .char_indices()
        .find(|(_, ch)| !matches!(ch, ' ' | '\t'))
        .map(|(idx, _)| idx)
        .unwrap_or(line.len());
    &line[..indent_end]
}

fn utf16_len(text: &str) -> u32 {
    text.chars().map(|ch| ch.len_utf16() as u32).sum()
}

fn formatting_indent_unit(options: &FormattingOptions) -> String {
    if options.insert_spaces {
        " ".repeat(options.tab_size.max(1) as usize)
    } else {
        "\t".to_string()
    }
}

fn brace_delta(line: &str) -> isize {
    let mut delta = 0isize;
    for ch in line.chars() {
        match ch {
            '{' => delta += 1,
            '}' => delta -= 1,
            _ => {}
        }
    }
    delta
}

fn brace_depth_before_line(source: &str, line: u32) -> usize {
    let mut depth = 0isize;
    for row in source.split('\n').take(line as usize) {
        depth = (depth + brace_delta(row)).max(0);
    }
    depth as usize
}

fn on_type_indent_edit(source: &str, line: u32, options: &FormattingOptions) -> Option<TextEdit> {
    let current_line = formatting_source_line(source, line)?;
    let current_indent = leading_indent(current_line);
    let trimmed = current_line.trim_start_matches([' ', '\t']);
    let mut depth = brace_depth_before_line(source, line);
    if trimmed.starts_with('}') {
        depth = depth.saturating_sub(1);
    }

    let desired_indent = formatting_indent_unit(options).repeat(depth);
    if desired_indent == current_indent {
        return None;
    }

    Some(TextEdit {
        range: Range {
            start: Position::new(line, 0),
            end: Position::new(line, utf16_len(current_indent)),
        },
        new_text: desired_indent,
    })
}

fn lsp_position_to_byte(source: &str, position: Position) -> Option<usize> {
    let byte_col = utf16_col_to_byte(source, position.line, position.character) as usize;
    let mut offset = 0usize;

    for (current_line, row) in source.split_inclusive('\n').enumerate() {
        if current_line as u32 == position.line {
            return Some(offset + byte_col.min(row.len()));
        }
        offset += row.len();
    }

    if position.line as usize == source.lines().count() {
        Some(source.len())
    } else {
        None
    }
}

fn text_at_lsp_range(source: &str, range: Range) -> Option<&str> {
    let start = lsp_position_to_byte(source, range.start)?;
    let end = lsp_position_to_byte(source, range.end)?;
    source.get(start..end)
}

fn build_add_import_edit(
    uri: Uri,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    import_fqn: &str,
    import_kind: ImportKind,
    diagnostic_range: Range,
) -> Option<(WorkspaceEdit, Option<String>)> {
    if let Some(existing) = existing_import_for_fqn(file_symbols, import_fqn, import_kind) {
        if let Some(alias) = existing.alias.clone() {
            let edit = TextEdit {
                range: diagnostic_range,
                new_text: alias.clone(),
            };
            let mut changes = std::collections::HashMap::new();
            changes.insert(uri, vec![edit]);
            return Some((
                WorkspaceEdit {
                    changes: Some(changes),
                    document_changes: None,
                    change_annotations: None,
                },
                Some(alias),
            ));
        }
        return None;
    }

    let import_short_name = short_name(import_fqn);
    let used_aliases = used_import_aliases(file_symbols, import_kind);
    let alias = if used_aliases.contains(import_short_name) {
        Some(unique_import_alias(import_short_name, &used_aliases))
    } else {
        None
    };

    let insert_line = find_use_insert_line(source, file_symbols);
    let needs_spacing =
        file_symbols.use_statements.is_empty() && !line_is_blank(source, insert_line);
    let mut import_text = build_use_statement(import_fqn, import_kind, alias.as_deref());
    import_text.push('\n');
    if needs_spacing {
        import_text.push('\n');
    }

    let mut edits = vec![TextEdit {
        range: Range {
            start: Position::new(insert_line, 0),
            end: Position::new(insert_line, 0),
        },
        new_text: import_text,
    }];

    let replacement_name = alias.as_deref().unwrap_or(import_short_name);
    if alias.is_some()
        || text_at_lsp_range(source, diagnostic_range)
            .map(|text| text.trim_start_matches('\\') != replacement_name)
            .unwrap_or(false)
    {
        edits.push(TextEdit {
            range: diagnostic_range,
            new_text: replacement_name.to_string(),
        });
    }

    let mut changes = std::collections::HashMap::new();
    changes.insert(uri, edits);
    Some((
        WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        },
        alias,
    ))
}

fn range_overlaps(a: Range, b: Range) -> bool {
    a.start <= b.end && b.start <= a.end
}

/// Compute diagnostics for a file (syntax + semantic).
///
/// Extracted as a free function so it can be called both from
/// `publish_diagnostics` and from the post-indexing re-check in `initialized`.
fn compute_diagnostics(
    uri_str: &str,
    parser: &FileParser,
    index: &WorkspaceIndex,
) -> Vec<Diagnostic> {
    let tree = match parser.tree() {
        Some(t) => t,
        None => return vec![],
    };
    let source = parser.source();
    let utf16_index = Utf16LineIndex::new(&source);

    // Syntax errors (ERROR / MISSING nodes)
    let lsp_diags = extract_syntax_errors(tree, &source);
    let mut diagnostics: Vec<Diagnostic> = lsp_diags
        .into_iter()
        .map(|d| {
            // tree-sitter positions use byte columns; convert to UTF-16
            let start_char =
                utf16_index.byte_col_to_utf16(d.range.start.line, d.range.start.character);
            let end_char = utf16_index.byte_col_to_utf16(d.range.end.line, d.range.end.character);
            Diagnostic {
                range: Range {
                    start: Position::new(d.range.start.line, start_char),
                    end: Position::new(d.range.end.line, end_char),
                },
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("php-lsp".to_string()),
                message: d.message,
                ..Default::default()
            }
        })
        .collect();

    // Avoid semantic noise while the file has syntax errors.
    if !diagnostics.is_empty() {
        return diagnostics;
    }

    // Semantic diagnostics (unknown class, function, unresolved use)
    let file_symbols = index
        .file_symbols
        .get(uri_str)
        .map(|entry| entry.value().clone())
        .unwrap_or_default();

    let sem_diags =
        extract_semantic_diagnostics(tree, &source, &file_symbols, |fqn| index.resolve_fqn(fqn));

    for sd in sem_diags {
        diagnostics.push(Diagnostic {
            range: Range {
                start: Position::new(
                    sd.range.0,
                    utf16_index.byte_col_to_utf16(sd.range.0, sd.range.1),
                ),
                end: Position::new(
                    sd.range.2,
                    utf16_index.byte_col_to_utf16(sd.range.2, sd.range.3),
                ),
            },
            severity: Some(DiagnosticSeverity::WARNING),
            source: Some("php-lsp".to_string()),
            message: sd.message,
            ..Default::default()
        });
    }

    diagnostics
}

/// Collect all .php files from the given directories.
fn collect_php_files(directories: &[&Path], root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for dir in directories {
        let abs_dir = if dir.is_absolute() {
            dir.to_path_buf()
        } else {
            root.join(dir)
        };
        if abs_dir.is_dir() {
            collect_php_files_recursive(&abs_dir, &mut files);
        }
    }
    files
}

/// Recursively collect .php files from a directory.
fn collect_php_files_recursive(dir: &Path, files: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!("Failed to read directory {}: {}", dir.display(), e);
            return;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Skip hidden directories and vendor
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with('.') || name_str == "vendor" || name_str == "node_modules" {
                continue;
            }
            collect_php_files_recursive(&path, files);
        } else if path.extension().and_then(|e| e.to_str()) == Some("php") {
            files.push(path);
        }
    }
}

/// Convert a file:// URI to a filesystem path.
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    uri.strip_prefix("file://").map(PathBuf::from)
}

/// Convert a file path to a file:// URI.
fn path_to_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

/// Find composer.json in the workspace root or immediate subdirectories.
///
/// Searches the root first, then scans depth-1 subdirectories (skipping hidden
/// directories and common non-project dirs like `node_modules`, `vendor`).
fn find_composer_json(root: &Path) -> Option<PathBuf> {
    // Check root first
    let in_root = root.join("composer.json");
    if in_root.exists() {
        return Some(in_root);
    }

    // Scan immediate subdirectories (depth 1)
    let entries = std::fs::read_dir(root).ok()?;
    let skip_dirs = [
        "node_modules",
        "vendor",
        ".git",
        ".github",
        "docker",
        "cache",
        "logs",
        "tmp",
    ];

    let mut candidates: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if !ft.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        // Skip hidden dirs and known non-project dirs
        if name_str.starts_with('.') || skip_dirs.contains(&name_str.as_ref()) {
            continue;
        }
        let subdir_composer = entry.path().join("composer.json");
        if subdir_composer.exists() {
            candidates.push(subdir_composer);
        }
    }

    // If exactly one found, use it; if multiple, prefer the one with autoload section
    match candidates.len() {
        0 => None,
        1 => Some(candidates.into_iter().next().unwrap()),
        _ => {
            // Prefer the candidate with the most autoload entries
            for c in &candidates {
                if let Ok(content) = std::fs::read_to_string(c) {
                    if content.contains("\"autoload\"") || content.contains("\"psr-4\"") {
                        return Some(c.clone());
                    }
                }
            }
            // Fallback to first
            Some(candidates.into_iter().next().unwrap())
        }
    }
}

/// Try to resolve a FQN to file paths by scanning vendor/composer installed packages.
fn resolve_vendor_paths(fqn: &str, vendor_dir: &Path) -> Option<Vec<PathBuf>> {
    // Try to parse vendor/composer/installed.json for PSR-4 mappings
    let installed_json = vendor_dir.join("composer/installed.json");
    if !installed_json.exists() {
        return None;
    }

    let content = std::fs::read_to_string(&installed_json).ok()?;
    let data: serde_json::Value = serde_json::from_str(&content).ok()?;

    // installed.json can be {"packages": [...]} or just [...]
    let packages = data
        .get("packages")
        .and_then(|p| p.as_array())
        .or_else(|| data.as_array())?;

    let mut paths = Vec::new();

    for pkg in packages {
        // Each package has "autoload" -> "psr-4" -> { "Prefix\\": "src/" }
        if let Some(autoload) = pkg.get("autoload") {
            if let Some(psr4) = autoload.get("psr-4").and_then(|v| v.as_object()) {
                for (prefix, dirs) in psr4 {
                    if let Some(relative) = fqn.strip_prefix(prefix.as_str()) {
                        let relative_path = relative.replace('\\', "/") + ".php";

                        // Get install path (package directory)
                        let install_path = pkg
                            .get("install-path")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");

                        let pkg_dir = if install_path.starts_with("../") {
                            vendor_dir.join("composer").join(install_path)
                        } else {
                            vendor_dir.join(install_path)
                        };

                        match dirs {
                            serde_json::Value::String(dir) => {
                                paths.push(pkg_dir.join(dir).join(&relative_path));
                            }
                            serde_json::Value::Array(dir_list) => {
                                for dir in dir_list {
                                    if let Some(dir_str) = dir.as_str() {
                                        paths.push(pkg_dir.join(dir_str).join(&relative_path));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    if paths.is_empty() {
        None
    } else {
        Some(paths)
    }
}

/// Convert PhpSymbolKind to the ls_types SymbolKind used by tower-lsp.
fn php_kind_to_lsp(kind: php_lsp_types::PhpSymbolKind) -> SymbolKind {
    match kind {
        php_lsp_types::PhpSymbolKind::Class => SymbolKind::CLASS,
        php_lsp_types::PhpSymbolKind::Interface => SymbolKind::INTERFACE,
        php_lsp_types::PhpSymbolKind::Trait => SymbolKind::INTERFACE,
        php_lsp_types::PhpSymbolKind::Enum => SymbolKind::ENUM,
        php_lsp_types::PhpSymbolKind::Function => SymbolKind::FUNCTION,
        php_lsp_types::PhpSymbolKind::Method => SymbolKind::METHOD,
        php_lsp_types::PhpSymbolKind::Property => SymbolKind::PROPERTY,
        php_lsp_types::PhpSymbolKind::ClassConstant => SymbolKind::CONSTANT,
        php_lsp_types::PhpSymbolKind::GlobalConstant => SymbolKind::CONSTANT,
        php_lsp_types::PhpSymbolKind::EnumCase => SymbolKind::ENUM_MEMBER,
        php_lsp_types::PhpSymbolKind::Namespace => SymbolKind::NAMESPACE,
    }
}

/// Convert lsp_types::CompletionItemKind to ls_types::CompletionItemKind.
fn lsp_completion_kind_to_ls(kind: lsp_types::CompletionItemKind) -> CompletionItemKind {
    // Both crates use the same numeric values from the LSP spec
    match kind {
        lsp_types::CompletionItemKind::TEXT => CompletionItemKind::TEXT,
        lsp_types::CompletionItemKind::METHOD => CompletionItemKind::METHOD,
        lsp_types::CompletionItemKind::FUNCTION => CompletionItemKind::FUNCTION,
        lsp_types::CompletionItemKind::CONSTRUCTOR => CompletionItemKind::CONSTRUCTOR,
        lsp_types::CompletionItemKind::FIELD => CompletionItemKind::FIELD,
        lsp_types::CompletionItemKind::VARIABLE => CompletionItemKind::VARIABLE,
        lsp_types::CompletionItemKind::CLASS => CompletionItemKind::CLASS,
        lsp_types::CompletionItemKind::INTERFACE => CompletionItemKind::INTERFACE,
        lsp_types::CompletionItemKind::MODULE => CompletionItemKind::MODULE,
        lsp_types::CompletionItemKind::PROPERTY => CompletionItemKind::PROPERTY,
        lsp_types::CompletionItemKind::UNIT => CompletionItemKind::UNIT,
        lsp_types::CompletionItemKind::VALUE => CompletionItemKind::VALUE,
        lsp_types::CompletionItemKind::ENUM => CompletionItemKind::ENUM,
        lsp_types::CompletionItemKind::KEYWORD => CompletionItemKind::KEYWORD,
        lsp_types::CompletionItemKind::SNIPPET => CompletionItemKind::SNIPPET,
        lsp_types::CompletionItemKind::COLOR => CompletionItemKind::COLOR,
        lsp_types::CompletionItemKind::FILE => CompletionItemKind::FILE,
        lsp_types::CompletionItemKind::REFERENCE => CompletionItemKind::REFERENCE,
        lsp_types::CompletionItemKind::FOLDER => CompletionItemKind::FOLDER,
        lsp_types::CompletionItemKind::ENUM_MEMBER => CompletionItemKind::ENUM_MEMBER,
        lsp_types::CompletionItemKind::CONSTANT => CompletionItemKind::CONSTANT,
        lsp_types::CompletionItemKind::STRUCT => CompletionItemKind::STRUCT,
        lsp_types::CompletionItemKind::EVENT => CompletionItemKind::EVENT,
        lsp_types::CompletionItemKind::OPERATOR => CompletionItemKind::OPERATOR,
        lsp_types::CompletionItemKind::TYPE_PARAMETER => CompletionItemKind::TYPE_PARAMETER,
        _ => CompletionItemKind::TEXT,
    }
}

/// Background workspace indexing.
///
/// Scans PHP files in the workspace and adds their symbols to the index.
async fn index_workspace(
    client: &Client,
    index: &WorkspaceIndex,
    root: &Path,
    namespace_map: Option<&NamespaceMap>,
) -> std::result::Result<(), String> {
    // Create progress token
    let progress_token = ProgressToken::String("php-lsp-indexing".to_string());

    // Request progress support from client (with timeout to avoid hanging if client doesn't respond)
    let progress_supported = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.create_work_done_progress(progress_token.clone()),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false);

    // Start progress reporting (Bounded with percentage)
    let ongoing = if progress_supported {
        let progress = client
            .progress(progress_token, "Indexing PHP workspace")
            .with_percentage(0)
            .with_message("Discovering files...");
        Some(progress.begin().await)
    } else {
        None
    };

    // Collect PHP files
    let php_files = if let Some(ns_map) = namespace_map {
        let source_dirs = ns_map.source_directories();
        if source_dirs.is_empty() {
            collect_php_files(&[root], root)
        } else {
            collect_php_files(&source_dirs, root)
        }
    } else {
        collect_php_files(&[root], root)
    };

    // Also add explicit files from composer.json
    let mut all_files = php_files;
    if let Some(ns_map) = namespace_map {
        for file_path in &ns_map.files {
            let abs = if file_path.is_absolute() {
                file_path.clone()
            } else {
                root.join(file_path)
            };
            if abs.exists() && !all_files.contains(&abs) {
                all_files.push(abs);
            }
        }
    }

    let total = all_files.len();
    tracing::info!("Indexing {} PHP files", total);

    if let Some(ref p) = ongoing {
        p.report_with_message(format!("Indexing {} files...", total), 0)
            .await;
    }

    // Parse files with limited concurrency via semaphore
    let semaphore = Arc::new(Semaphore::new(4));
    let indexed = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    for (i, file_path) in all_files.iter().enumerate() {
        let _permit = semaphore
            .acquire()
            .await
            .map_err(|e| format!("Semaphore error: {}", e))?;

        // Read and parse file
        match std::fs::read_to_string(file_path) {
            Ok(source) => {
                let mut parser = FileParser::new();
                parser.parse_full(&source);

                if let Some(tree) = parser.tree() {
                    let uri = path_to_uri(file_path);
                    let file_symbols = extract_file_symbols(tree, &source, &uri);

                    let sym_count = file_symbols.symbols.len();
                    index.update_file(&uri, file_symbols);

                    if sym_count > 0 {
                        tracing::debug!("Indexed {}: {} symbols", file_path.display(), sym_count);
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Failed to read {}: {}", file_path.display(), e);
            }
        }

        let done = indexed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;

        // Report progress every 10 files or on last file
        if let Some(ref p) = ongoing {
            if done % 10 == 0 || done == total {
                let percentage = if total > 0 {
                    ((done as f64 / total as f64) * 100.0) as u32
                } else {
                    100
                };
                p.report_with_message(format!("Indexed {}/{} files", done, total), percentage)
                    .await;
            }
        }

        // Yield to allow other tasks to run
        if i % 50 == 0 {
            tokio::task::yield_now().await;
        }
    }

    // End progress
    if let Some(p) = ongoing {
        p.finish_with_message(format!("Indexed {} files", total))
            .await;
    }

    client
        .log_message(
            MessageType::INFO,
            format!("php-lsp: indexed {} PHP files", total),
        )
        .await;

    tracing::info!("Workspace indexing complete: {} files", total);

    Ok(())
}

impl LanguageServer for PhpLspBackend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        tracing::info!("php-lsp: initialize");

        // Store trace level from client
        if let Some(trace) = params.trace {
            *self.trace_level.lock().await = trace;
            tracing::info!("Trace level: {:?}", trace);
        }

        // Extract workspace root from InitializeParams
        #[allow(deprecated)]
        let root_path = params
            .root_uri
            .as_ref()
            .and_then(|uri| uri_to_path(uri.as_str()))
            .or_else(|| params.root_path.as_ref().map(PathBuf::from));

        // Extract stubsPath from client initializationOptions
        if let Some(ref opts) = params.initialization_options {
            if let Some(raw_php_version) = opts.get("phpVersion").and_then(|v| v.as_str()) {
                if let Some(parsed) = PhpVersion::parse(raw_php_version) {
                    tracing::info!("Client provided phpVersion: {}", raw_php_version);
                    *self.php_version.lock().await = parsed;
                } else {
                    tracing::warn!("Ignoring invalid phpVersion: {}", raw_php_version);
                }
            }

            if let Some(sp) = opts.get("stubsPath").and_then(|v| v.as_str()) {
                let p = PathBuf::from(sp);
                tracing::info!("Client provided stubsPath: {}", p.display());
                *self.stubs_path.lock().await = Some(p);
            }

            let formatting_provider = opts
                .get("formattingProvider")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    opts.get("formatting")
                        .and_then(|v| v.get("provider"))
                        .and_then(|v| v.as_str())
                });
            let formatting_command = opts
                .get("formattingCommand")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    opts.get("formatting")
                        .and_then(|v| v.get("command"))
                        .and_then(|v| v.as_str())
                });
            *self.formatting_config.lock().await =
                FormattingConfig::from_options(formatting_provider, formatting_command);
        }

        if let Some(ref root) = root_path {
            tracing::info!("Workspace root: {}", root.display());
            *self.workspace_root.lock().await = Some(root.clone());
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::INCREMENTAL),
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(false),
                        })),
                        ..Default::default()
                    },
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                declaration_provider: Some(DeclarationCapability::Simple(true)),
                type_definition_provider: Some(TypeDefinitionProviderCapability::Simple(true)),
                references_provider: Some(OneOf::Left(true)),
                document_highlight_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                })),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![
                        "$".to_string(),
                        ">".to_string(),
                        ":".to_string(),
                        "\\".to_string(),
                    ]),
                    resolve_provider: Some(true),
                    ..Default::default()
                }),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
                    retrigger_characters: Some(vec![",".to_string()]),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                }),
                code_action_provider: Some(CodeActionProviderCapability::Options(
                    CodeActionOptions {
                        code_action_kinds: Some(vec![
                            CodeActionKind::QUICKFIX,
                            CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
                            CodeActionKind::REFACTOR_REWRITE,
                        ]),
                        resolve_provider: Some(false),
                        work_done_progress_options: WorkDoneProgressOptions::default(),
                    },
                )),
                document_formatting_provider: Some(OneOf::Left(true)),
                document_range_formatting_provider: Some(OneOf::Left(true)),
                document_on_type_formatting_provider: Some(DocumentOnTypeFormattingOptions {
                    first_trigger_character: "\n".to_string(),
                    more_trigger_character: Some(vec![";".to_string(), "}".to_string()]),
                }),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            work_done_progress_options: WorkDoneProgressOptions::default(),
                            legend: semantic_tokens_legend(),
                            range: None,
                            full: Some(SemanticTokensFullOptions::Delta { delta: Some(true) }),
                        },
                    ),
                ),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "php-lsp".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        tracing::info!("php-lsp: initialized");
        self.client
            .log_message(MessageType::INFO, "php-lsp server initialized")
            .await;

        // Start background workspace indexing
        let workspace_root = self.workspace_root.lock().await.clone();
        if let Some(root) = workspace_root {
            let client = self.client.clone();
            let index = self.index.clone();

            // Auto-discover composer.json: check root, then immediate subdirectories
            let (ns_map_storage, project_root) = {
                let composer_path = find_composer_json(&root);
                if let Some(ref cp) = composer_path {
                    let effective_root = cp.parent().unwrap_or(&root).to_path_buf();
                    if effective_root != root {
                        tracing::info!(
                            "Found composer.json in subdirectory: {}",
                            effective_root.display()
                        );
                    }
                    match parse_composer_json(cp) {
                        Ok(ns_map) => {
                            tracing::info!(
                                "Parsed composer.json with {} PSR-4 entries",
                                ns_map.psr4.len()
                            );
                            (Some(ns_map), effective_root)
                        }
                        Err(e) => {
                            tracing::warn!("Failed to parse composer.json: {}", e);
                            (None, root.clone())
                        }
                    }
                } else {
                    tracing::info!("No composer.json found, will scan all PHP files");
                    (None, root.clone())
                }
            };

            // Update workspace root to the effective project root (where composer.json is)
            if project_root != root {
                *self.workspace_root.lock().await = Some(project_root.clone());
                tracing::info!("Effective project root: {}", project_root.display());
            }
            let root = project_root;

            // Store namespace map
            *self.namespace_map.lock().await = ns_map_storage.clone();

            // Load phpstorm-stubs for built-in PHP functions/classes
            let stubs_index = self.index.clone();
            let stubs_root = root.clone();
            let client_stubs_path = self.stubs_path.lock().await.clone();
            tokio::task::spawn_blocking(move || {
                // Build list of candidate paths, client-provided path first
                let mut candidate_paths: Vec<PathBuf> = Vec::new();

                // 1. Path from client initializationOptions (bundled with extension)
                if let Some(ref p) = client_stubs_path {
                    candidate_paths.push(p.clone());
                }

                // 2. Relative to workspace (development)
                candidate_paths.push(stubs_root.join("server/data/stubs"));

                // 3. Relative to binary location
                if let Ok(exe) = std::env::current_exe() {
                    if let Some(dir) = exe.parent() {
                        candidate_paths.push(dir.join("data/stubs"));
                        // Also check sibling stubs/ dir (for extension layout: bin/php-lsp + stubs/)
                        candidate_paths.push(
                            dir.join("../stubs")
                                .canonicalize()
                                .unwrap_or_else(|_| dir.join("../stubs")),
                        );
                    }
                }

                // 4. Common install paths
                candidate_paths.push(PathBuf::from("/usr/share/php-lsp/stubs"));

                for stubs_path in &candidate_paths {
                    if stubs_path.is_dir() {
                        tracing::info!("Loading phpstorm-stubs from {}", stubs_path.display());
                        let loaded =
                            stubs::load_stubs(&stubs_index, stubs_path, stubs::DEFAULT_EXTENSIONS);
                        tracing::info!("Loaded {} stub files", loaded);
                        return;
                    }
                }
                tracing::warn!("phpstorm-stubs not found, built-in completions will be limited");
            })
            .await
            .ok();

            let open_files = self.open_files.clone();
            let reindex_index = self.index.clone();
            let reindex_client = self.client.clone();
            tokio::spawn(async move {
                if let Err(e) =
                    index_workspace(&client, &index, &root, ns_map_storage.as_ref()).await
                {
                    tracing::error!("Background indexing failed: {}", e);
                    client
                        .log_message(MessageType::ERROR, format!("Indexing failed: {}", e))
                        .await;
                    return;
                }

                // Re-publish diagnostics for all open files now that the index is populated
                for entry in open_files.iter() {
                    let uri_str = entry.key().clone();
                    if let Ok(uri) = uri_str.parse::<Uri>() {
                        let diags = compute_diagnostics(&uri_str, &entry, &reindex_index);
                        reindex_client.publish_diagnostics(uri, diags, None).await;
                    }
                }
            });
        } else {
            tracing::warn!("No workspace root, skipping indexing");
        }
    }

    async fn shutdown(&self) -> Result<()> {
        tracing::info!("php-lsp: shutdown");
        Ok(())
    }

    // --- Document Synchronization ---

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let uri_str = uri.as_str().to_string();
        let text = &params.text_document.text;

        tracing::debug!("didOpen: {}", uri_str);
        self.log_trace(&format!("didOpen: {}", uri_str)).await;

        let mut parser = FileParser::new();
        parser.parse_full(text);

        // Update index with symbols from this file
        if let Some(tree) = parser.tree() {
            let file_symbols = extract_file_symbols(tree, text, &uri_str);
            let sym_count = file_symbols.symbols.len();
            self.index.update_file(&uri_str, file_symbols);
            self.log_trace(&format!("Indexed {} symbols from {}", sym_count, uri_str))
                .await;
        }

        self.open_files.insert(uri_str, parser);

        self.publish_diagnostics(&uri).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let uri_str = uri.as_str().to_string();

        tracing::debug!("didChange: {}", uri_str);

        if let Some(mut parser) = self.open_files.get_mut(&uri_str) {
            for change in &params.content_changes {
                if let Some(range) = change.range {
                    parser.apply_edit(
                        range.start.line,
                        range.start.character,
                        range.end.line,
                        range.end.character,
                        &change.text,
                    );
                } else {
                    // Full content replacement
                    parser.parse_full(&change.text);
                }
            }

            // Update index with new symbols
            if let Some(tree) = parser.tree() {
                let source = parser.source();
                let file_symbols = extract_file_symbols(tree, &source, &uri_str);
                self.index.update_file(&uri_str, file_symbols);
            }
        }

        self.publish_diagnostics(&uri).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        let uri_str = uri.as_str().to_string();
        tracing::debug!("didClose: {}", uri_str);
        self.open_files.remove(&uri_str);
        self.semantic_tokens_cache.lock().await.remove(&uri_str);
        // Clear diagnostics for closed file
        self.client.publish_diagnostics(uri, vec![], None).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        tracing::debug!("didSave: {}", params.text_document.uri.as_str());
    }

    // --- Language Features ---

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let uri_str = params.text_document.uri.as_str().to_string();
        tracing::debug!("formatting: {}", uri_str);

        let source = {
            let parser = match self.open_files.get(&uri_str) {
                Some(parser) => parser,
                None => return Ok(None),
            };
            parser.source()
        };

        let config = self.formatting_config.lock().await.clone();
        if config.command_template().is_none() {
            return Ok(None);
        }

        let workspace_root = self.workspace_root.lock().await.clone();
        let source_for_formatter = source.clone();
        let formatted = tokio::task::spawn_blocking(move || {
            run_external_formatter(source_for_formatter, config, workspace_root)
        })
        .await
        .map_err(|err| {
            tracing::error!("Formatter task failed: {}", err);
            tower_lsp::jsonrpc::Error::internal_error()
        })?;

        let formatted = match formatted {
            Ok(Some(formatted)) => formatted,
            Ok(None) => return Ok(Some(vec![])),
            Err(message) => {
                self.client
                    .log_message(
                        MessageType::WARNING,
                        format!("php-lsp formatter failed: {}", message),
                    )
                    .await;
                return Ok(Some(vec![]));
            }
        };

        Ok(Some(vec![TextEdit {
            range: full_document_range(&source),
            new_text: formatted,
        }]))
    }

    async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let uri_str = params.text_document.uri.as_str().to_string();
        tracing::debug!("rangeFormatting: {}", uri_str);

        let source = {
            let parser = match self.open_files.get(&uri_str) {
                Some(parser) => parser,
                None => return Ok(None),
            };
            parser.source()
        };

        let Some(fragment) = text_at_lsp_range(&source, params.range) else {
            return Ok(Some(vec![]));
        };
        if fragment.is_empty() {
            return Ok(Some(vec![]));
        }

        let config = self.formatting_config.lock().await.clone();
        if config.command_template().is_none() {
            return Ok(None);
        }

        let (formatter_input, was_wrapped) = range_formatter_input(fragment);
        let workspace_root = self.workspace_root.lock().await.clone();
        let formatted = tokio::task::spawn_blocking(move || {
            run_external_formatter(formatter_input, config, workspace_root)
        })
        .await
        .map_err(|err| {
            tracing::error!("Range formatter task failed: {}", err);
            tower_lsp::jsonrpc::Error::internal_error()
        })?;

        let formatted = match formatted {
            Ok(Some(formatted)) => strip_range_formatter_wrapper(formatted, was_wrapped),
            Ok(None) => return Ok(Some(vec![])),
            Err(message) => {
                self.client
                    .log_message(
                        MessageType::WARNING,
                        format!("php-lsp range formatter failed: {}", message),
                    )
                    .await;
                return Ok(Some(vec![]));
            }
        };

        if formatted == fragment {
            return Ok(Some(vec![]));
        }

        Ok(Some(vec![TextEdit {
            range: params.range,
            new_text: formatted,
        }]))
    }

    async fn on_type_formatting(
        &self,
        params: DocumentOnTypeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let uri_str = params
            .text_document_position
            .text_document
            .uri
            .as_str()
            .to_string();
        let position = params.text_document_position.position;
        tracing::debug!(
            "onTypeFormatting: {}:{}:{} trigger={:?}",
            uri_str,
            position.line,
            position.character,
            params.ch
        );

        if !matches!(params.ch.as_str(), "\n" | ";" | "}") {
            return Ok(Some(vec![]));
        }

        let source = {
            let parser = match self.open_files.get(&uri_str) {
                Some(parser) => parser,
                None => return Ok(None),
            };
            parser.source()
        };

        let Some(current_line) = formatting_source_line(&source, position.line) else {
            return Ok(Some(vec![]));
        };
        if params.ch == "}"
            && !current_line
                .trim_start_matches([' ', '\t'])
                .starts_with('}')
        {
            return Ok(Some(vec![]));
        }

        Ok(Some(
            on_type_indent_edit(&source, position.line, &params.options)
                .into_iter()
                .collect(),
        ))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let uri_str = uri.as_str().to_string();
        let pos = params.text_document_position_params.position;
        tracing::debug!("hover: {}:{}:{}", uri_str, pos.line, pos.character);

        // Extract symbol-at-position and local variable hover info inside a block so DashMap guard is dropped.
        let (sym_at_pos, var_hover_info) = {
            let parser = match self.open_files.get(&uri_str) {
                Some(p) => p,
                None => return Ok(None),
            };

            let tree = match parser.tree() {
                Some(t) => t,
                None => return Ok(None),
            };

            let source = parser.source();
            let byte_col = utf16_col_to_byte(&source, pos.line, pos.character);

            // Get file symbols for name resolution
            let file_symbols = self
                .index
                .file_symbols
                .get(&uri_str)
                .map(|entry| entry.value().clone())
                .unwrap_or_default();

            // Build a cross-file type resolver for method chain resolution
            let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
                self.resolve_member_type(class_fqn, member_name)
            };

            // Find symbol at cursor position (with resolver for chains)
            let sym_at_pos = match symbol_at_position_with_resolver(
                tree,
                &source,
                pos.line,
                byte_col,
                &file_symbols,
                Some(&resolver),
            ) {
                Some(s) => s,
                None => return Ok(None),
            };
            let var_hover_info = if sym_at_pos.ref_kind == RefKind::Variable {
                variable_hover_info_at_position(tree, &source, &file_symbols, pos.line, byte_col)
            } else {
                None
            };

            (sym_at_pos, var_hover_info)
        };

        // Look up symbol in index (with lazy vendor fallback)
        let symbol_info = match sym_at_pos.ref_kind {
            RefKind::Variable => None, // Variables are local, handled by gotoDefinition.
            _ => {
                let info = self
                    .resolve_fqn_lazy_with_fallback(&sym_at_pos.fqn, sym_at_pos.ref_kind)
                    .await;
                // For constructor refs, fall back to the class if __construct is
                // not explicitly defined.
                if info.is_none() && sym_at_pos.ref_kind == RefKind::Constructor {
                    if let Some(class_fqn) = sym_at_pos.fqn.strip_suffix("::__construct") {
                        self.resolve_fqn_lazy_with_fallback(class_fqn, RefKind::ClassName)
                            .await
                    } else {
                        None
                    }
                } else {
                    info
                }
            }
        };

        let result = if let Some(sym) = symbol_info {
            // Build hover content
            let mut content = String::new();

            // Symbol kind label
            let kind_label = match sym.kind {
                php_lsp_types::PhpSymbolKind::Class => "class",
                php_lsp_types::PhpSymbolKind::Interface => "interface",
                php_lsp_types::PhpSymbolKind::Trait => "trait",
                php_lsp_types::PhpSymbolKind::Enum => "enum",
                php_lsp_types::PhpSymbolKind::Function => "function",
                php_lsp_types::PhpSymbolKind::Method => "method",
                php_lsp_types::PhpSymbolKind::Property => "property",
                php_lsp_types::PhpSymbolKind::ClassConstant => "const",
                php_lsp_types::PhpSymbolKind::GlobalConstant => "const",
                php_lsp_types::PhpSymbolKind::EnumCase => "case",
                php_lsp_types::PhpSymbolKind::Namespace => "namespace",
            };

            // PHP code block with signature
            content.push_str("```php\n");
            if let Some(ref sig) = sym.signature {
                // Function/method signature
                content.push_str(kind_label);
                content.push(' ');
                content.push_str(&sym.fqn);
                content.push('(');
                for (i, param) in sig.params.iter().enumerate() {
                    if i > 0 {
                        content.push_str(", ");
                    }
                    if let Some(ref t) = param.type_info {
                        content.push_str(&t.to_string());
                        content.push(' ');
                    }
                    if param.is_variadic {
                        content.push_str("...");
                    }
                    if param.is_by_ref {
                        content.push('&');
                    }
                    content.push('$');
                    content.push_str(&param.name);
                    if let Some(ref def) = param.default_value {
                        content.push_str(" = ");
                        content.push_str(def);
                    }
                }
                content.push(')');
                if let Some(ref ret) = sig.return_type {
                    content.push_str(": ");
                    content.push_str(&ret.to_string());
                }
            } else {
                content.push_str(kind_label);
                content.push(' ');
                content.push_str(&sym.fqn);
            }
            content.push_str("\n```\n");

            // PHPDoc summary
            if let Some(ref doc) = sym.doc_comment {
                let phpdoc = parse_phpdoc(doc);
                if let Some(ref summary) = phpdoc.summary {
                    content.push_str("\n---\n\n");
                    content.push_str(summary);
                    content.push('\n');
                }

                // @param descriptions
                if !phpdoc.params.is_empty() {
                    content.push_str("\n**Parameters:**\n\n");
                    for p in &phpdoc.params {
                        content.push_str("- `$");
                        content.push_str(&p.name);
                        content.push('`');
                        if let Some(ref t) = p.type_info {
                            content.push_str(" — `");
                            content.push_str(&t.to_string());
                            content.push('`');
                        }
                        if let Some(ref desc) = p.description {
                            content.push_str(" — ");
                            content.push_str(desc);
                        }
                        content.push('\n');
                    }
                }

                // @return
                if let Some(ref ret) = phpdoc.return_type {
                    content.push_str("\n**Returns:** `");
                    content.push_str(&ret.to_string());
                    content.push_str("`\n");
                }

                // @deprecated
                if let Some(ref dep) = phpdoc.deprecated {
                    content.push_str("\n⚠️ **Deprecated**");
                    if !dep.is_empty() {
                        content.push_str(": ");
                        content.push_str(dep);
                    }
                    content.push('\n');
                }
            }

            let range = Range {
                start: Position::new(sym_at_pos.range.0, sym_at_pos.range.1),
                end: Position::new(sym_at_pos.range.2, sym_at_pos.range.3),
            };

            Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: content,
                }),
                range: Some(range),
            })
        } else if let Some(var_info) = var_hover_info {
            let mut content = String::new();
            content.push_str("```php\n");
            if let Some(ref t) = var_info.type_display {
                content.push_str(t);
                content.push(' ');
                content.push_str(&var_info.variable_name);
            } else if let Some(ref fqn) = var_info.resolved_type_fqn {
                content.push_str(fqn);
                content.push(' ');
                content.push_str(&var_info.variable_name);
            } else {
                content.push_str("variable ");
                content.push_str(&var_info.variable_name);
            }
            content.push_str("\n```\n");

            if let Some(ref doc) = var_info.phpdoc_comment {
                let phpdoc = parse_phpdoc(doc);
                if let Some(ref summary) = phpdoc.summary {
                    content.push_str("\n---\n\n");
                    content.push_str(summary);
                    content.push('\n');
                }
                if let Some(ref var_type) = phpdoc.var_type {
                    content.push_str("\n**@var** `");
                    content.push_str(&var_type.to_string());
                    content.push_str("`\n");
                }
            }

            let range = Range {
                start: Position::new(sym_at_pos.range.0, sym_at_pos.range.1),
                end: Position::new(sym_at_pos.range.2, sym_at_pos.range.3),
            };
            Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: content,
                }),
                range: Some(range),
            })
        } else {
            None
        };

        Ok(result)
    }

    async fn goto_declaration(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .clone();
        let pos = params.text_document_position_params.position;
        tracing::debug!(
            "gotoDeclaration: {}:{}:{}",
            uri.as_str(),
            pos.line,
            pos.character
        );

        if let Some(import_declaration) = self.import_declaration_at_position(&uri, pos) {
            return Ok(Some(import_declaration));
        }

        self.goto_definition(params).await
    }

    async fn goto_type_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let uri_str = uri.as_str().to_string();
        let pos = params.text_document_position_params.position;
        tracing::debug!(
            "gotoTypeDefinition: {}:{}:{}",
            uri_str,
            pos.line,
            pos.character
        );

        let (sym_at_pos, variable_type_fqn, file_symbols) = {
            let parser = match self.open_files.get(&uri_str) {
                Some(parser) => parser,
                None => return Ok(None),
            };
            let tree = match parser.tree() {
                Some(tree) => tree,
                None => return Ok(None),
            };
            let source = parser.source();
            let byte_col = utf16_col_to_byte(&source, pos.line, pos.character);
            let file_symbols = self
                .index
                .file_symbols
                .get(&uri_str)
                .map(|entry| entry.value().clone())
                .unwrap_or_else(|| extract_file_symbols(tree, &source, &uri_str));

            let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
                self.resolve_member_type(class_fqn, member_name)
            };

            let sym_at_pos = symbol_at_position_with_resolver(
                tree,
                &source,
                pos.line,
                byte_col,
                &file_symbols,
                Some(&resolver),
            );
            let variable_type_fqn = if let Some(sym) = &sym_at_pos {
                if sym.ref_kind == RefKind::Variable {
                    variable_hover_info_at_position(
                        tree,
                        &source,
                        &file_symbols,
                        pos.line,
                        byte_col,
                    )
                    .and_then(|info| info.resolved_type_fqn)
                    .or_else(|| {
                        infer_variable_type_at_position(
                            tree,
                            &source,
                            &file_symbols,
                            pos.line,
                            byte_col,
                            &sym.name,
                        )
                    })
                } else {
                    None
                }
            } else {
                None
            };

            (sym_at_pos, variable_type_fqn, file_symbols)
        };

        if let Some(type_fqn) = variable_type_fqn {
            return Ok(self
                .location_for_type_fqn(&type_fqn)
                .await
                .map(GotoDefinitionResponse::Scalar));
        }

        let Some(sym_at_pos) = sym_at_pos else {
            return Ok(None);
        };

        if matches!(
            sym_at_pos.ref_kind,
            RefKind::ClassName | RefKind::Constructor
        ) {
            let type_fqn = import_target_fqn(&sym_at_pos);
            return Ok(self
                .location_for_type_fqn(type_fqn)
                .await
                .map(GotoDefinitionResponse::Scalar));
        }

        let symbol_info = self
            .resolve_fqn_lazy_with_fallback(&sym_at_pos.fqn, sym_at_pos.ref_kind)
            .await;

        let Some(symbol_info) = symbol_info else {
            return Ok(None);
        };
        let Some(type_fqn) = self.type_definition_fqn_for_symbol(&symbol_info, &file_symbols)
        else {
            return Ok(None);
        };

        Ok(self
            .location_for_type_fqn(&type_fqn)
            .await
            .map(GotoDefinitionResponse::Scalar))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let uri_str = uri.as_str().to_string();
        let pos = params.text_document_position_params.position;
        tracing::debug!("gotoDefinition: {}:{}:{}", uri_str, pos.line, pos.character);

        // Extract symbol-at-position inside a block so DashMap guard is dropped
        let (sym_at_pos, local_var_def) = {
            let parser = match self.open_files.get(&uri_str) {
                Some(p) => p,
                None => return Ok(None),
            };

            let tree = match parser.tree() {
                Some(t) => t,
                None => return Ok(None),
            };

            let source = parser.source();
            let byte_col = utf16_col_to_byte(&source, pos.line, pos.character);

            let file_symbols = self
                .index
                .file_symbols
                .get(&uri_str)
                .map(|entry| entry.value().clone())
                .unwrap_or_default();

            // Build a cross-file type resolver that uses the workspace index
            let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
                self.resolve_member_type(class_fqn, member_name)
            };

            let local_var_def = variable_definition_at_position(tree, &source, pos.line, byte_col)
                .map(|d| range_byte_to_utf16(&source, d));
            let sym = symbol_at_position_with_resolver(
                tree,
                &source,
                pos.line,
                byte_col,
                &file_symbols,
                Some(&resolver),
            );
            (sym, local_var_def)
        };

        // Local variable definition (same file/scope).
        if let Some(def) = local_var_def {
            let range = Range {
                start: Position::new(def.0, def.1),
                end: Position::new(def.2, def.3),
            };
            return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                uri,
                range,
            })));
        }

        let sym_at_pos = match sym_at_pos {
            Some(s) => {
                tracing::debug!(
                    "goto_definition: sym_at_pos fqn='{}', name='{}', ref_kind={:?}",
                    s.fqn,
                    s.name,
                    s.ref_kind
                );
                s
            }
            None => {
                tracing::debug!("goto_definition: no symbol at position");
                return Ok(None);
            }
        };

        // Look up symbol in index (with lazy vendor fallback)
        let symbol_info = self
            .resolve_fqn_lazy_with_fallback(&sym_at_pos.fqn, sym_at_pos.ref_kind)
            .await;

        // For constructor refs (`new ClassName()`), fall back to the class
        // declaration when `__construct` is not explicitly defined.
        let symbol_info = if symbol_info.is_none() && sym_at_pos.ref_kind == RefKind::Constructor {
            if let Some(class_fqn) = sym_at_pos.fqn.strip_suffix("::__construct") {
                self.resolve_fqn_lazy_with_fallback(class_fqn, RefKind::ClassName)
                    .await
            } else {
                None
            }
        } else {
            symbol_info
        };

        let result = if let Some(sym) = symbol_info {
            // Convert URI string to lsp_types::Uri
            if let Ok(target_uri) = sym.uri.parse::<Uri>() {
                let range = Range {
                    start: Position::new(sym.selection_range.0, sym.selection_range.1),
                    end: Position::new(sym.selection_range.2, sym.selection_range.3),
                };
                Some(GotoDefinitionResponse::Scalar(Location {
                    uri: target_uri,
                    range,
                }))
            } else {
                None
            }
        } else {
            None
        };

        // Fallback: when a member call on `$this->prop` fails because the declared
        // property type doesn't have that member, try resolving from the actual
        // assignment (e.g., `$this->em = $this->createStub(...)` → Stub type).
        let result = if result.is_none()
            && (sym_at_pos.ref_kind == RefKind::MethodCall
                || sym_at_pos.ref_kind == RefKind::PropertyAccess)
        {
            tracing::debug!(
                "goto_definition: primary resolution failed, trying property assignment fallback for obj_expr={:?}",
                sym_at_pos.object_expr
            );
            if let Some(ref obj_expr) = sym_at_pos.object_expr {
                if let Some(prop_name) = obj_expr.strip_prefix("$this->") {
                    // Only handle simple property access (no chaining)
                    if !prop_name.contains("->") {
                        self.try_property_assignment_type_fallback(
                            &uri_str,
                            prop_name,
                            &sym_at_pos.name,
                        )
                        .await
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            result
        };

        Ok(result)
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let uri_str = params
            .text_document_position_params
            .text_document
            .uri
            .as_str()
            .to_string();
        let pos = params.text_document_position_params.position;

        let parser = match self.open_files.get(&uri_str) {
            Some(parser) => parser,
            None => return Ok(None),
        };
        let tree = match parser.tree() {
            Some(tree) => tree,
            None => return Ok(None),
        };
        let source = parser.source();
        let byte_col = utf16_col_to_byte(&source, pos.line, pos.character);
        let file_symbols = extract_file_symbols(tree, &source, &uri_str);
        let sym = match symbol_at_position(tree, &source, pos.line, byte_col, &file_symbols) {
            Some(sym) => sym,
            None => return Ok(None),
        };

        if sym.ref_kind == RefKind::Variable {
            let highlights: Vec<DocumentHighlight> =
                find_variable_references_at_position(tree, &source, pos.line, byte_col, true)
                    .into_iter()
                    .map(|reference| document_highlight_from_range(&source, reference.range, true))
                    .collect();
            return if highlights.is_empty() {
                Ok(None)
            } else {
                Ok(Some(highlights))
            };
        }

        let Some(kind) = php_symbol_kind_for_ref_kind(sym.ref_kind) else {
            return Ok(None);
        };
        let resolved = self.resolve_fqn_with_fallback(&sym.fqn, sym.ref_kind);
        let (target_fqn, target_kind) = if let Some(resolved) = resolved {
            (resolved.fqn.clone(), resolved.kind)
        } else {
            (sym.fqn.clone(), kind)
        };
        let read_write_capable = target_kind == php_lsp_types::PhpSymbolKind::Property;

        let highlights: Vec<DocumentHighlight> =
            find_references_in_file(tree, &source, &file_symbols, &target_fqn, target_kind, true)
                .into_iter()
                .map(|reference| {
                    document_highlight_from_range(&source, reference.range, read_write_capable)
                })
                .collect();

        if highlights.is_empty() {
            Ok(None)
        } else {
            Ok(Some(highlights))
        }
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri_str = params
            .text_document_position
            .text_document
            .uri
            .as_str()
            .to_string();
        let pos = params.text_document_position.position;
        let include_declaration = params.context.include_declaration;

        // Resolve symbol under cursor to get FQN
        let (target_fqn, target_kind) = {
            let parser = match self.open_files.get(&uri_str) {
                Some(p) => p,
                None => return Ok(None),
            };
            let tree = match parser.tree() {
                Some(t) => t,
                None => return Ok(None),
            };
            let source = parser.source();
            let byte_col = utf16_col_to_byte(&source, pos.line, pos.character);
            let file_symbols = extract_file_symbols(tree, &source, &uri_str);

            match symbol_at_position(tree, &source, pos.line, byte_col, &file_symbols) {
                Some(sym) => {
                    if sym.ref_kind == RefKind::Variable {
                        let refs = find_variable_references_at_position(
                            tree,
                            &source,
                            pos.line,
                            byte_col,
                            include_declaration,
                        );
                        if refs.is_empty() {
                            return Ok(None);
                        }
                        let uri = match uri_str.parse::<Uri>() {
                            Ok(u) => u,
                            Err(_) => return Ok(None),
                        };
                        let locations: Vec<Location> = refs
                            .into_iter()
                            .map(|r| {
                                let rng = range_byte_to_utf16(&source, r.range);
                                Location {
                                    uri: uri.clone(),
                                    range: Range {
                                        start: Position::new(rng.0, rng.1),
                                        end: Position::new(rng.2, rng.3),
                                    },
                                }
                            })
                            .collect();
                        return Ok(Some(locations));
                    }

                    let kind = match sym.ref_kind {
                        RefKind::ClassName | RefKind::Constructor => {
                            php_lsp_types::PhpSymbolKind::Class
                        }
                        RefKind::FunctionCall => php_lsp_types::PhpSymbolKind::Function,
                        RefKind::MethodCall => php_lsp_types::PhpSymbolKind::Method,
                        RefKind::PropertyAccess | RefKind::StaticPropertyAccess => {
                            php_lsp_types::PhpSymbolKind::Property
                        }
                        RefKind::ClassConstant => php_lsp_types::PhpSymbolKind::ClassConstant,
                        RefKind::GlobalConstant => php_lsp_types::PhpSymbolKind::GlobalConstant,
                        RefKind::Variable => return Ok(None),
                        RefKind::NamespaceName | RefKind::Unknown => return Ok(None),
                    };

                    // Try to canonicalize symbol via index lookup.
                    let resolved = self.resolve_fqn_with_fallback(&sym.fqn, sym.ref_kind);
                    if let Some(resolved) = resolved {
                        (resolved.fqn.clone(), resolved.kind)
                    } else {
                        (sym.fqn.clone(), kind)
                    }
                }
                None => return Ok(None),
            }
        };

        // Search all indexed files for references
        let mut locations = Vec::new();

        for entry in self.index.file_symbols.iter() {
            let file_uri = entry.key().clone();
            let file_syms = entry.value().clone();

            // We need to parse the file to walk the CST
            // First check if it's an open file
            let refs = if let Some(parser) = self.open_files.get(&file_uri) {
                if let Some(tree) = parser.tree() {
                    let source = parser.source();
                    let raw = find_references_in_file(
                        tree,
                        &source,
                        &file_syms,
                        &target_fqn,
                        target_kind,
                        include_declaration,
                    );
                    raw.into_iter()
                        .map(|mut r| {
                            r.range = range_byte_to_utf16(&source, r.range);
                            r
                        })
                        .collect::<Vec<_>>()
                } else {
                    continue;
                }
            } else {
                // For non-open files, we need to re-parse
                let path = match uri_to_path(&file_uri) {
                    Some(p) => p,
                    None => continue,
                };
                let source = match std::fs::read_to_string(&path) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let mut parser = FileParser::new();
                parser.parse_full(&source);
                let tree = match parser.tree() {
                    Some(t) => t,
                    None => continue,
                };
                let raw = find_references_in_file(
                    tree,
                    &source,
                    &file_syms,
                    &target_fqn,
                    target_kind,
                    include_declaration,
                );
                raw.into_iter()
                    .map(|mut r| {
                        r.range = range_byte_to_utf16(&source, r.range);
                        r
                    })
                    .collect::<Vec<_>>()
            };

            for r in refs {
                if let Ok(uri) = file_uri.parse::<Uri>() {
                    locations.push(Location {
                        uri,
                        range: Range {
                            start: Position::new(r.range.0, r.range.1),
                            end: Position::new(r.range.2, r.range.3),
                        },
                    });
                }
            }
        }

        if locations.is_empty() {
            Ok(None)
        } else {
            Ok(Some(locations))
        }
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri_str = params
            .text_document_position
            .text_document
            .uri
            .as_str()
            .to_string();
        let pos = params.text_document_position.position;
        let new_name = &params.new_name;

        // Validate new name
        if new_name.is_empty() || new_name.contains(' ') || new_name.contains('\\') {
            return Err(tower_lsp::jsonrpc::Error::invalid_params(
                "Invalid new name",
            ));
        }

        let parser = match self.open_files.get(&uri_str) {
            Some(p) => p,
            None => return Ok(None),
        };
        let tree = match parser.tree() {
            Some(t) => t,
            None => return Ok(None),
        };
        let source = parser.source();
        let byte_col = utf16_col_to_byte(&source, pos.line, pos.character);
        let file_symbols = extract_file_symbols(tree, &source, &uri_str);

        let sym = match symbol_at_position(tree, &source, pos.line, byte_col, &file_symbols) {
            Some(s) => s,
            None => return Ok(None),
        };

        if sym.ref_kind == RefKind::Variable {
            if !is_renameable_variable(&sym.name) {
                return Err(tower_lsp::jsonrpc::Error::invalid_params(
                    "Cannot rename this variable",
                ));
            }
            let replacement = normalize_variable_new_name(new_name).ok_or_else(|| {
                tower_lsp::jsonrpc::Error::invalid_params("Invalid variable name")
            })?;
            let refs =
                find_variable_references_at_position(tree, &source, pos.line, byte_col, true);
            if refs.is_empty() {
                return Ok(None);
            }
            let uri = match uri_str.parse::<Uri>() {
                Ok(u) => u,
                Err(_) => return Ok(None),
            };
            let edits: Vec<TextEdit> = refs
                .into_iter()
                .map(|r| {
                    let rng = range_byte_to_utf16(&source, r.range);
                    TextEdit {
                        range: Range {
                            start: Position::new(rng.0, rng.1),
                            end: Position::new(rng.2, rng.3),
                        },
                        new_text: replacement.clone(),
                    }
                })
                .collect();
            let mut changes: std::collections::HashMap<Uri, Vec<TextEdit>> =
                std::collections::HashMap::new();
            changes.insert(uri, edits);
            return Ok(Some(WorkspaceEdit {
                changes: Some(changes),
                document_changes: None,
                change_annotations: None,
            }));
        }

        if sym.ref_kind == RefKind::Unknown || sym.ref_kind == RefKind::NamespaceName {
            return Ok(None);
        }

        // Resolve symbol under cursor
        let (target_fqn, target_kind, _old_name) = {
            let kind = match sym.ref_kind {
                RefKind::ClassName | RefKind::Constructor => php_lsp_types::PhpSymbolKind::Class,
                RefKind::FunctionCall => php_lsp_types::PhpSymbolKind::Function,
                RefKind::MethodCall => php_lsp_types::PhpSymbolKind::Method,
                RefKind::PropertyAccess | RefKind::StaticPropertyAccess => {
                    php_lsp_types::PhpSymbolKind::Property
                }
                RefKind::ClassConstant => php_lsp_types::PhpSymbolKind::ClassConstant,
                RefKind::GlobalConstant => php_lsp_types::PhpSymbolKind::GlobalConstant,
                _ => return Ok(None),
            };

            let resolved = self.resolve_fqn_with_fallback(&sym.fqn, sym.ref_kind);
            if let Some(resolved) = resolved {
                (resolved.fqn.clone(), resolved.kind, sym.name.clone())
            } else {
                (sym.fqn.clone(), kind, sym.name.clone())
            }
        };

        let property_new_name = if target_kind == php_lsp_types::PhpSymbolKind::Property {
            Some(normalize_property_new_name(new_name).ok_or_else(|| {
                tower_lsp::jsonrpc::Error::invalid_params("Invalid property name")
            })?)
        } else {
            None
        };

        // Don't rename built-in symbols
        if let Some(sym) = self.index.resolve_fqn(&target_fqn) {
            if sym.modifiers.is_builtin {
                return Err(tower_lsp::jsonrpc::Error::invalid_params(
                    "Cannot rename built-in symbols",
                ));
            }
        }

        // Find all references (including declaration)
        let mut changes: std::collections::HashMap<Uri, Vec<TextEdit>> =
            std::collections::HashMap::new();

        for entry in self.index.file_symbols.iter() {
            let file_uri = entry.key().clone();
            let file_syms = entry.value().clone();

            let (refs, source_text) = if let Some(parser) = self.open_files.get(&file_uri) {
                if let Some(tree) = parser.tree() {
                    let source = parser.source();
                    let refs = find_references_in_file(
                        tree,
                        &source,
                        &file_syms,
                        &target_fqn,
                        target_kind,
                        true, // include declaration
                    );
                    (refs, source)
                } else {
                    continue;
                }
            } else {
                let path = match uri_to_path(&file_uri) {
                    Some(p) => p,
                    None => continue,
                };
                let source = match std::fs::read_to_string(&path) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let mut parser = FileParser::new();
                parser.parse_full(&source);
                let tree = match parser.tree() {
                    Some(t) => t,
                    None => continue,
                };
                let refs = find_references_in_file(
                    tree,
                    &source,
                    &file_syms,
                    &target_fqn,
                    target_kind,
                    true,
                );
                (refs, source)
            };

            if !refs.is_empty() {
                if let Ok(uri) = file_uri.parse::<Uri>() {
                    let edits: Vec<TextEdit> = refs
                        .into_iter()
                        .map(|r| {
                            let rng = range_byte_to_utf16(&source_text, r.range);
                            TextEdit {
                                range: Range {
                                    start: Position::new(rng.0, rng.1),
                                    end: Position::new(rng.2, rng.3),
                                },
                                new_text: if target_kind == php_lsp_types::PhpSymbolKind::Property
                                    && range_starts_with_dollar(&source_text, r.range)
                                {
                                    format!(
                                        "${}",
                                        property_new_name
                                            .as_deref()
                                            .unwrap_or(new_name.trim_start_matches('$'))
                                    )
                                } else {
                                    property_new_name.as_deref().unwrap_or(new_name).to_string()
                                },
                            }
                        })
                        .collect();
                    changes.entry(uri).or_default().extend(edits);
                }
            }
        }

        if changes.is_empty() {
            Ok(None)
        } else {
            Ok(Some(WorkspaceEdit {
                changes: Some(changes),
                document_changes: None,
                change_annotations: None,
            }))
        }
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let uri_str = params.text_document.uri.as_str().to_string();
        let pos = params.position;

        let parser = match self.open_files.get(&uri_str) {
            Some(p) => p,
            None => return Ok(None),
        };
        let tree = match parser.tree() {
            Some(t) => t,
            None => return Ok(None),
        };
        let source = parser.source();
        let byte_col = utf16_col_to_byte(&source, pos.line, pos.character);
        let file_symbols = extract_file_symbols(tree, &source, &uri_str);

        match symbol_at_position(tree, &source, pos.line, byte_col, &file_symbols) {
            Some(sym) => {
                // Variable rename support is local-scope only.
                if sym.ref_kind == RefKind::Variable {
                    if !is_renameable_variable(&sym.name) {
                        return Ok(None);
                    }
                    let rng = range_byte_to_utf16(&source, sym.range);
                    let range = Range {
                        start: Position::new(rng.0, rng.1),
                        end: Position::new(rng.2, rng.3),
                    };
                    return Ok(Some(PrepareRenameResponse::Range(range)));
                }
                if sym.ref_kind == RefKind::Unknown || sym.ref_kind == RefKind::NamespaceName {
                    return Ok(None);
                }

                // Don't rename built-in symbols
                if let Some(resolved) = self.resolve_fqn_with_fallback(&sym.fqn, sym.ref_kind) {
                    if resolved.modifiers.is_builtin {
                        return Ok(None);
                    }
                }

                let rng2 = range_byte_to_utf16(&source, sym.range);
                let range = Range {
                    start: Position::new(rng2.0, rng2.1),
                    end: Position::new(rng2.2, rng2.3),
                };

                Ok(Some(PrepareRenameResponse::Range(range)))
            }
            None => Ok(None),
        }
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri_str = params.text_document.uri.as_str().to_string();

        // Try open files first, then fall back to index
        let file_symbols = if let Some(parser) = self.open_files.get(&uri_str) {
            if let Some(tree) = parser.tree() {
                extract_file_symbols(tree, &parser.source(), &uri_str)
            } else {
                return Ok(None);
            }
        } else if let Some(fs) = self.index.file_symbols.get(&uri_str) {
            fs.value().clone()
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
        let to_range = |r: (u32, u32, u32, u32)| -> Range {
            Range {
                start: Position::new(r.0, r.1),
                end: Position::new(r.2, r.3),
            }
        };

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

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri_str = params.text_document.uri.as_str().to_string();
        tracing::debug!("semanticTokens/full: {}", uri_str);

        let data = {
            let parser = match self.open_files.get(&uri_str) {
                Some(parser) => parser,
                None => return Ok(None),
            };
            match semantic_tokens_for_parser(&parser) {
                Some(data) => data,
                None => return Ok(None),
            }
        };
        let snapshot = self
            .semantic_tokens_cache
            .lock()
            .await
            .store(&uri_str, data);

        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: Some(snapshot.result_id),
            data: snapshot.data,
        })))
    }

    async fn semantic_tokens_full_delta(
        &self,
        params: SemanticTokensDeltaParams,
    ) -> Result<Option<SemanticTokensFullDeltaResult>> {
        let uri_str = params.text_document.uri.as_str().to_string();
        tracing::debug!(
            "semanticTokens/full/delta: {} previous={}",
            uri_str,
            params.previous_result_id
        );

        let data = {
            let parser = match self.open_files.get(&uri_str) {
                Some(parser) => parser,
                None => return Ok(None),
            };
            match semantic_tokens_for_parser(&parser) {
                Some(data) => data,
                None => return Ok(None),
            }
        };

        let mut cache = self.semantic_tokens_cache.lock().await;
        let previous = cache.previous_data(&uri_str, &params.previous_result_id);
        let snapshot = cache.store(&uri_str, data);

        let Some(previous) = previous else {
            return Ok(Some(SemanticTokensFullDeltaResult::Tokens(
                SemanticTokens {
                    result_id: Some(snapshot.result_id),
                    data: snapshot.data,
                },
            )));
        };

        Ok(Some(SemanticTokensFullDeltaResult::TokensDelta(
            SemanticTokensDelta {
                result_id: Some(snapshot.result_id),
                edits: semantic_tokens_delta_edits(&previous, &snapshot.data),
            },
        )))
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<WorkspaceSymbolResponse>> {
        let query = &params.query;

        // Empty query returns nothing (avoid overwhelming results)
        if query.is_empty() {
            return Ok(Some(WorkspaceSymbolResponse::Flat(vec![])));
        }

        let results = self.index.search(query);

        // Limit results to avoid overwhelming the client
        let symbols: Vec<SymbolInformation> = results
            .into_iter()
            .filter(|sym| !sym.modifiers.is_builtin) // Exclude built-in symbols from stubs
            .take(200)
            .filter_map(|sym| {
                let uri: Uri = sym.uri.parse().ok()?;
                #[allow(deprecated)]
                Some(SymbolInformation {
                    name: sym.name.clone(),
                    kind: php_kind_to_lsp(sym.kind),
                    tags: if sym.modifiers.is_deprecated {
                        Some(vec![SymbolTag::DEPRECATED])
                    } else {
                        None
                    },
                    deprecated: None,
                    location: Location {
                        uri,
                        range: Range {
                            start: Position::new(sym.range.0, sym.range.1),
                            end: Position::new(sym.range.2, sym.range.3),
                        },
                    },
                    container_name: sym.parent_fqn.clone().or_else(|| {
                        // For top-level symbols, use namespace as container
                        let fqn = &sym.fqn;
                        fqn.rfind('\\').map(|i| fqn[..i].to_string())
                    }),
                })
            })
            .collect();

        Ok(Some(WorkspaceSymbolResponse::Flat(symbols)))
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let wants_quickfix =
            code_action_kind_allowed(params.context.only.as_ref(), &CodeActionKind::QUICKFIX);
        let wants_organize_imports = code_action_kind_allowed(
            params.context.only.as_ref(),
            &CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
        );
        let wants_add_return_type = code_action_kind_allowed(
            params.context.only.as_ref(),
            &CodeActionKind::REFACTOR_REWRITE,
        );

        if !wants_quickfix && !wants_organize_imports && !wants_add_return_type {
            return Ok(Some(vec![]));
        }

        let uri = params.text_document.uri;
        let uri_str = uri.as_str().to_string();
        let php_version = *self.php_version.lock().await;

        let (source, file_symbols, add_return_type_actions) = {
            let parser = match self.open_files.get(&uri_str) {
                Some(p) => p,
                None => return Ok(Some(vec![])),
            };
            let tree = match parser.tree() {
                Some(t) => t,
                None => return Ok(Some(vec![])),
            };
            let source = parser.source();
            let file_symbols = self
                .index
                .file_symbols
                .get(&uri_str)
                .map(|entry| entry.value().clone())
                .unwrap_or_else(|| extract_file_symbols(tree, &source, &uri_str));
            let add_return_type_actions = if wants_add_return_type {
                let range = lsp_range_to_byte_range(&source, params.range);
                find_missing_return_type_candidates(tree, &source, range)
                    .into_iter()
                    .filter_map(|candidate| {
                        build_add_return_type_action(uri.clone(), &source, &candidate, php_version)
                    })
                    .collect()
            } else {
                Vec::new()
            };
            (source, file_symbols, add_return_type_actions)
        };

        let mut actions = Vec::new();
        actions.extend(add_return_type_actions);

        if wants_organize_imports {
            if let Some(edit) = build_organize_imports_edit(uri.clone(), &source, &file_symbols) {
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: "Organize imports".to_string(),
                    kind: Some(CodeActionKind::SOURCE_ORGANIZE_IMPORTS),
                    diagnostics: None,
                    edit: Some(edit),
                    command: None,
                    is_preferred: Some(false),
                    disabled: None,
                    data: None,
                }));
            }
        }

        if !wants_quickfix {
            return Ok(Some(actions));
        }

        let diagnostics = if params.context.diagnostics.is_empty() {
            let parser = match self.open_files.get(&uri_str) {
                Some(p) => p,
                None => return Ok(Some(vec![])),
            };
            compute_diagnostics(&uri_str, &parser, &self.index)
                .into_iter()
                .filter(|diag| range_overlaps(diag.range, params.range))
                .collect()
        } else {
            params.context.diagnostics
        };

        let mut quickfix_count = 0usize;

        for diagnostic in diagnostics {
            let Some((import_kind, unresolved_fqn)) =
                unknown_symbol_from_diagnostic(&diagnostic.message)
            else {
                continue;
            };
            let unresolved_short = short_name(&unresolved_fqn);

            let mut candidates: Vec<std::sync::Arc<php_lsp_types::SymbolInfo>> = match import_kind {
                ImportKind::Class => self
                    .index
                    .types
                    .iter()
                    .filter(|entry| {
                        let sym = entry.value();
                        !sym.modifiers.is_builtin
                            && (sym.name == unresolved_short
                                || short_name(&sym.fqn) == unresolved_short)
                    })
                    .map(|entry| entry.value().clone())
                    .collect(),
                ImportKind::Function => self
                    .index
                    .functions
                    .iter()
                    .filter(|entry| {
                        let sym = entry.value();
                        !sym.modifiers.is_builtin
                            && (sym.name == unresolved_short
                                || short_name(&sym.fqn) == unresolved_short)
                    })
                    .map(|entry| entry.value().clone())
                    .collect(),
                ImportKind::Constant => Vec::new(),
            };
            candidates.sort_by(|a, b| a.fqn.cmp(&b.fqn));
            candidates.dedup_by(|a, b| a.fqn == b.fqn);
            candidates.truncate(5);

            for candidate in candidates {
                let Some((edit, alias)) = build_add_import_edit(
                    uri.clone(),
                    &source,
                    &file_symbols,
                    &candidate.fqn,
                    import_kind,
                    diagnostic.range,
                ) else {
                    continue;
                };

                let title = if let Some(alias) = alias {
                    format!("Import {} as {}", candidate.fqn, alias)
                } else {
                    format!("Import {}", candidate.fqn)
                };

                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title,
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: Some(vec![diagnostic.clone()]),
                    edit: Some(edit),
                    command: None,
                    is_preferred: Some(quickfix_count == 0),
                    disabled: None,
                    data: None,
                }));
                quickfix_count += 1;
            }
        }

        Ok(Some(actions))
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let uri_str = params
            .text_document_position_params
            .text_document
            .uri
            .as_str()
            .to_string();
        let pos = params.text_document_position_params.position;
        tracing::debug!("signatureHelp: {}:{}:{}", uri_str, pos.line, pos.character);

        let (sym_at_pos, active_parameter) = {
            let parser = match self.open_files.get(&uri_str) {
                Some(p) => p,
                None => return Ok(None),
            };
            let tree = match parser.tree() {
                Some(t) => t,
                None => return Ok(None),
            };
            let source = parser.source();
            let byte_col = utf16_col_to_byte(&source, pos.line, pos.character);
            let file_symbols = self
                .index
                .file_symbols
                .get(&uri_str)
                .map(|entry| entry.value().clone())
                .unwrap_or_else(|| extract_file_symbols(tree, &source, &uri_str));

            let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
                self.resolve_member_type(class_fqn, member_name)
            };

            let context = match signature_help_context_at_position(
                tree,
                &source,
                pos.line,
                byte_col,
                &file_symbols,
                Some(&resolver),
            ) {
                Some(context) => context,
                None => return Ok(None),
            };

            (context.symbol, context.active_parameter)
        };

        let symbol_info = self
            .resolve_fqn_lazy_with_fallback(&sym_at_pos.fqn, sym_at_pos.ref_kind)
            .await;

        let symbol_info = if symbol_info.is_none() && sym_at_pos.ref_kind == RefKind::Constructor {
            if let Some(class_fqn) = sym_at_pos.fqn.strip_suffix("::__construct") {
                self.resolve_fqn_lazy_with_fallback(class_fqn, RefKind::ClassName)
                    .await
            } else {
                None
            }
        } else {
            symbol_info
        };

        Ok(symbol_info.and_then(|sym| build_signature_help(&sym, active_parameter)))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri_str = params
            .text_document_position
            .text_document
            .uri
            .as_str()
            .to_string();
        let pos = params.text_document_position.position;
        tracing::debug!("completion: {}:{}:{}", uri_str, pos.line, pos.character);

        let parser = match self.open_files.get(&uri_str) {
            Some(p) => p,
            None => return Ok(None),
        };
        let tree = match parser.tree() {
            Some(t) => t,
            None => return Ok(None),
        };
        let source = parser.source();
        let byte_col = utf16_col_to_byte(&source, pos.line, pos.character);
        let file_symbols = extract_file_symbols(tree, &source, &uri_str);

        // Detect completion context
        let context = detect_context(tree, &source, pos.line, byte_col, &file_symbols);
        let context = match context {
            php_lsp_completion::context::CompletionContext::MemberAccess {
                object_expr,
                class_fqn,
            } => {
                let inferred = class_fqn.or_else(|| {
                    if object_expr.starts_with('$') {
                        infer_variable_type_at_position(
                            tree,
                            &source,
                            &file_symbols,
                            pos.line,
                            byte_col,
                            &object_expr,
                        )
                    } else {
                        None
                    }
                });

                php_lsp_completion::context::CompletionContext::MemberAccess {
                    object_expr,
                    class_fqn: inferred,
                }
            }
            other => other,
        };

        if context == php_lsp_completion::context::CompletionContext::None {
            return Ok(None);
        }

        // Get completion items from the provider
        let lsp_items = provide_completions(&context, &self.index, &file_symbols);

        // Convert lsp_types::CompletionItem to ls_types::CompletionItem
        // We need to map between the two different type systems
        let items: Vec<CompletionItem> = lsp_items
            .into_iter()
            .map(|item| {
                let kind = item.kind.map(lsp_completion_kind_to_ls);

                let tags = item.tags.map(|tags| {
                    tags.into_iter()
                        .filter_map(|t| {
                            if t == lsp_types::CompletionItemTag::DEPRECATED {
                                Some(CompletionItemTag::DEPRECATED)
                            } else {
                                None
                            }
                        })
                        .collect()
                });

                CompletionItem {
                    label: item.label,
                    kind,
                    detail: item.detail,
                    tags,
                    data: item.data,
                    ..Default::default()
                }
            })
            .collect();

        if items.is_empty() {
            Ok(None)
        } else {
            Ok(Some(CompletionResponse::Array(items)))
        }
    }

    async fn completion_resolve(&self, mut item: CompletionItem) -> Result<CompletionItem> {
        // Try to resolve more details for the completion item
        // The FQN is stored in item.data
        if let Some(ref data) = item.data {
            if let Some(fqn) = data.as_str() {
                if let Some(sym) = self.resolve_fqn_lazy(fqn).await {
                    // Add full documentation
                    let mut doc_parts = Vec::new();

                    // Signature
                    if let Some(ref sig) = sym.signature {
                        let params_str: Vec<String> = sig
                            .params
                            .iter()
                            .map(|p| {
                                let mut s = String::new();
                                if let Some(ref t) = p.type_info {
                                    s.push_str(&t.to_string());
                                    s.push(' ');
                                }
                                if p.is_variadic {
                                    s.push_str("...");
                                }
                                if p.is_by_ref {
                                    s.push('&');
                                }
                                s.push('$');
                                s.push_str(&p.name);
                                if let Some(ref default) = p.default_value {
                                    s.push_str(" = ");
                                    s.push_str(default);
                                }
                                s
                            })
                            .collect();
                        let mut sig_str = format!("({})", params_str.join(", "));
                        if let Some(ref ret) = sig.return_type {
                            sig_str.push_str(&format!(": {}", ret));
                        }
                        item.detail = Some(sig_str);
                    }

                    // PHPDoc
                    if let Some(ref doc) = sym.doc_comment {
                        let phpdoc = parse_phpdoc(doc);
                        if let Some(ref summary) = phpdoc.summary {
                            doc_parts.push(summary.clone());
                        }

                        if phpdoc.deprecated.is_some() {
                            doc_parts.push("**@deprecated**".to_string());
                            if let Some(ref tags) = item.tags {
                                if !tags.contains(&CompletionItemTag::DEPRECATED) {
                                    let mut tags = tags.clone();
                                    tags.push(CompletionItemTag::DEPRECATED);
                                    item.tags = Some(tags);
                                }
                            } else {
                                item.tags = Some(vec![CompletionItemTag::DEPRECATED]);
                            }
                        }

                        // Param docs
                        if !phpdoc.params.is_empty() {
                            doc_parts.push(String::new());
                            for param in &phpdoc.params {
                                let type_str = param
                                    .type_info
                                    .as_ref()
                                    .map(|t| format!(" `{}`", t))
                                    .unwrap_or_default();
                                let desc = param
                                    .description
                                    .as_ref()
                                    .map(|d| format!(" — {}", d))
                                    .unwrap_or_default();
                                doc_parts
                                    .push(format!("@param{} `${}`{}", type_str, param.name, desc));
                            }
                        }

                        // Return type
                        if let Some(ref ret) = phpdoc.return_type {
                            doc_parts.push(format!("\n@return `{}`", ret));
                        }
                    }

                    if !doc_parts.is_empty() {
                        item.documentation = Some(Documentation::MarkupContent(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: doc_parts.join("\n"),
                        }));
                    }
                }
            }
        }

        Ok(item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use php_lsp_types::*;

    fn make_symbol(
        name: &str,
        fqn: &str,
        kind: PhpSymbolKind,
        range: (u32, u32, u32, u32),
        parent_fqn: Option<&str>,
    ) -> SymbolInfo {
        SymbolInfo {
            name: name.to_string(),
            fqn: fqn.to_string(),
            kind,
            uri: "file:///test.php".to_string(),
            range,
            selection_range: range,
            visibility: Visibility::Public,
            modifiers: SymbolModifiers::default(),
            doc_comment: None,
            signature: None,
            parent_fqn: parent_fqn.map(|s| s.to_string()),
            extends: vec![],
            implements: vec![],
        }
    }

    #[test]
    fn test_php_kind_to_lsp() {
        assert_eq!(php_kind_to_lsp(PhpSymbolKind::Class), SymbolKind::CLASS);
        assert_eq!(
            php_kind_to_lsp(PhpSymbolKind::Function),
            SymbolKind::FUNCTION
        );
        assert_eq!(php_kind_to_lsp(PhpSymbolKind::Method), SymbolKind::METHOD);
        assert_eq!(
            php_kind_to_lsp(PhpSymbolKind::Property),
            SymbolKind::PROPERTY
        );
        assert_eq!(
            php_kind_to_lsp(PhpSymbolKind::EnumCase),
            SymbolKind::ENUM_MEMBER
        );
        assert_eq!(
            php_kind_to_lsp(PhpSymbolKind::Namespace),
            SymbolKind::NAMESPACE
        );
    }

    #[test]
    fn test_document_symbol_hierarchy() {
        // Simulate file with namespace → class → methods
        let file_symbols = FileSymbols {
            namespace: Some("App\\Service".to_string()),
            use_statements: vec![],
            symbols: vec![
                make_symbol(
                    "App\\Service",
                    "App\\Service",
                    PhpSymbolKind::Namespace,
                    (0, 0, 20, 0),
                    None,
                ),
                make_symbol(
                    "UserService",
                    "App\\Service\\UserService",
                    PhpSymbolKind::Class,
                    (2, 0, 18, 1),
                    None,
                ),
                make_symbol(
                    "getUser",
                    "App\\Service\\UserService::getUser",
                    PhpSymbolKind::Method,
                    (4, 4, 8, 5),
                    Some("App\\Service\\UserService"),
                ),
                make_symbol(
                    "$name",
                    "App\\Service\\UserService::$name",
                    PhpSymbolKind::Property,
                    (3, 4, 3, 30),
                    Some("App\\Service\\UserService"),
                ),
            ],
        };

        // Index file
        let index = WorkspaceIndex::new();
        index.update_file("file:///test.php", file_symbols);

        // Retrieve and verify structure
        let fs = index.file_symbols.get("file:///test.php").unwrap();
        let symbols = &fs.symbols;

        // Should have 4 symbols total
        assert_eq!(symbols.len(), 4);

        // Verify the class has proper kind
        let class = symbols
            .iter()
            .find(|s| s.kind == PhpSymbolKind::Class)
            .unwrap();
        assert_eq!(class.name, "UserService");

        // Verify members belong to the class
        let members: Vec<_> = symbols
            .iter()
            .filter(|s| s.parent_fqn.as_deref() == Some("App\\Service\\UserService"))
            .collect();
        assert_eq!(members.len(), 2); // getUser + $name
    }

    #[test]
    fn test_workspace_symbol_search() {
        let index = WorkspaceIndex::new();
        let file_symbols = FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![
                make_symbol(
                    "FooController",
                    "App\\FooController",
                    PhpSymbolKind::Class,
                    (0, 0, 10, 0),
                    None,
                ),
                make_symbol(
                    "BarService",
                    "App\\BarService",
                    PhpSymbolKind::Class,
                    (12, 0, 20, 0),
                    None,
                ),
                make_symbol(
                    "helper_foo",
                    "App\\helper_foo",
                    PhpSymbolKind::Function,
                    (22, 0, 25, 0),
                    None,
                ),
            ],
        };
        index.update_file("file:///app.php", file_symbols);

        // Search for "foo" should find FooController + helper_foo
        let results = index.search("foo");
        assert_eq!(results.len(), 2);

        // Search for "Service" should find BarService
        let results = index.search("Service");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "BarService");

        // Search for "xyz" should find nothing
        let results = index.search("xyz");
        assert!(results.is_empty());
    }

    #[test]
    fn test_uri_to_path_and_back() {
        let path = PathBuf::from("/home/user/project/src/Foo.php");
        let uri = path_to_uri(&path);
        assert_eq!(uri, "file:///home/user/project/src/Foo.php");

        let back = uri_to_path(&uri).unwrap();
        assert_eq!(back, path);
    }

    #[test]
    fn test_resolve_vendor_paths() {
        // Create temp dir with fake vendor/composer/installed.json
        let tmp = std::env::temp_dir().join("php-lsp-test-vendor");
        let vendor_dir = tmp.join("vendor");
        let composer_dir = vendor_dir.join("composer");
        std::fs::create_dir_all(&composer_dir).unwrap();

        let installed_json = serde_json::json!({
            "packages": [
                {
                    "name": "acme/library",
                    "install-path": "../acme/library",
                    "autoload": {
                        "psr-4": {
                            "Acme\\Library\\": "src/"
                        }
                    }
                }
            ]
        });

        std::fs::write(
            composer_dir.join("installed.json"),
            serde_json::to_string(&installed_json).unwrap(),
        )
        .unwrap();

        // Test resolving a FQN
        let paths = resolve_vendor_paths("Acme\\Library\\Http\\Client", &vendor_dir);
        assert!(paths.is_some());
        let paths = paths.unwrap();
        assert_eq!(paths.len(), 1);
        // The path should resolve to vendor/composer/../acme/library/src/Http/Client.php
        let expected_end = "src/Http/Client.php";
        assert!(
            paths[0].to_string_lossy().ends_with(expected_end),
            "Expected path to end with {}, got: {}",
            expected_end,
            paths[0].display()
        );

        // Test FQN that doesn't match any prefix
        let no_match = resolve_vendor_paths("Other\\Namespace\\Foo", &vendor_dir);
        // Should return Some(empty vec) or None — no paths match
        assert!(no_match.is_none() || no_match.unwrap().is_empty());

        // Cleanup
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
