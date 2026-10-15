//! Source-backed quick-fix metadata for the normal analyzer publication path.
use super::*;
use php_lsp_types::{FileSymbols, PhpSymbolKind, SymbolInfo, SymbolReference, TypeInfo};

#[cfg(test)]
thread_local! {static RANGE_INDEX_BUILDS: std::cell::Cell<usize> = const {std::cell::Cell::new(0)};}
fn node_range(positions: &Utf16LineIndex, node: tree_sitter::Node) -> Range {
    let start = node.start_position();
    let end = node.end_position();
    Range::new(
        Position::new(
            start.row as u32,
            positions.byte_col_to_utf16(start.row as u32, start.column as u32),
        ),
        Position::new(
            end.row as u32,
            positions.byte_col_to_utf16(end.row as u32, end.column as u32),
        ),
    )
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FixSnapshot {
    uri: String,
    size: u64,
    hash: u64,
    version: Option<i32>,
    generation: Option<u64>,
    runtime_generation: u64,
}

struct FixContext<'a> {
    source: &'a str,
    tree: &'a tree_sitter::Tree,
    file: &'a FileSymbols,
    index: &'a WorkspaceIndex,
    references: Vec<SymbolReference>,
    callables: Vec<(tree_sitter::Node<'a>, Range)>,
    positions: Utf16LineIndex,
    line_starts: Vec<usize>,
    cancel: &'a OperationCancellationToken,
    snapshot: FixSnapshot,
}

impl<'a> FixContext<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        uri: &str,
        source: &'a str,
        tree: &'a tree_sitter::Tree,
        file: &'a FileSymbols,
        index: &'a WorkspaceIndex,
        state: Option<OpenDocumentState>,
        runtime_generation: u64,
        cancel: &'a OperationCancellationToken,
    ) -> Option<Self> {
        if cancel.is_cancelled() {
            return None;
        }
        #[cfg(test)]
        RANGE_INDEX_BUILDS.with(|count| count.set(count.get() + 1));
        let positions = Utf16LineIndex::new(source);
        let line_starts = std::iter::once(0)
            .chain(
                source
                    .bytes()
                    .enumerate()
                    .filter_map(|(i, b)| (b == b'\n').then_some(i + 1)),
            )
            .collect();
        let mut callables = Vec::new();
        let mut cursor = tree.walk();
        loop {
            if cancel.is_cancelled() {
                return None;
            }
            let node = cursor.node();
            if php_lsp_parser::variable_scope::is_callable_scope(node) {
                callables.push((node, node_range(&positions, node)));
            }
            if cursor.goto_first_child() {
                continue;
            }
            loop {
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    break;
                }
            }
            if cursor.node().id() == tree.root_node().id() {
                break;
            }
        }
        let fingerprint = SourceFingerprint::from_bytes(source.as_bytes());
        let references = php_lsp_parser::references::collect_symbol_references_in_file_cancellable(
            tree,
            source,
            file,
            None,
            None,
            &|| cancel.is_cancelled(),
        )?;
        if cancel.is_cancelled() {
            return None;
        }
        Some(Self {
            source,
            tree,
            file,
            index,
            callables,
            references,
            positions,
            line_starts,
            cancel,
            snapshot: FixSnapshot {
                uri: uri.into(),
                size: fingerprint.size,
                hash: fingerprint.content_hash,
                version: state.map(|s| s.version),
                generation: state.map(|s| s.generation),
                runtime_generation,
            },
        })
    }

    fn line_only(diagnostic: &Diagnostic) -> bool {
        diagnostic.source.as_deref() == Some("phpstan")
            && diagnostic.range.start.character == 0
            && diagnostic.range.end == Position::new(diagnostic.range.start.line, 1)
    }

    fn owner(&self, diagnostic: &Diagnostic) -> Option<&'a SymbolInfo> {
        let mut candidates = self
            .callables
            .iter()
            .copied()
            .filter(|(_, range)| {
                if self.cancel.is_cancelled() {
                    return false;
                }
                if Self::line_only(diagnostic) {
                    range.start.line <= diagnostic.range.start.line
                        && diagnostic.range.start.line <= range.end.line
                } else {
                    range.start <= diagnostic.range.start && diagnostic.range.end <= range.end
                }
            })
            .map(|(node, _)| node)
            .collect::<Vec<_>>();
        candidates.sort_by_key(|n| n.end_byte() - n.start_byte());
        let node = *candidates.first()?;
        if candidates
            .iter()
            .any(|other| !node_contains_node(*other, node))
        {
            return None;
        }
        if !matches!(node.kind(), "function_definition" | "method_declaration") || node.has_error()
        {
            return None;
        }
        let a = node.start_position();
        let b = node.end_position();
        let symbol = self.file.symbols.iter().find(|s| {
            matches!(s.kind, PhpSymbolKind::Function | PhpSymbolKind::Method)
                && s.range == (a.row as u32, a.column as u32, b.row as u32, b.column as u32)
        })?;
        if let Some(doc) = symbol.doc_comment.as_deref() {
            let comment = node.prev_named_sibling()?;
            if comment.kind() != "comment"
                || self.source.get(comment.byte_range())? != doc
                || !self.source[comment.end_byte()..node.start_byte()]
                    .trim()
                    .is_empty()
            {
                return None;
            }
        }
        Some(symbol)
    }

    pub(in crate::server) fn metadata(&self, diagnostic: &Diagnostic) -> Option<serde_json::Value> {
        if self.cancel.is_cancelled() || self.tree.root_node().has_error() {
            return None;
        }
        let analyzer = diagnostic_external_analyzer(diagnostic)?;
        let code = diagnostic_code_str(diagnostic)?;
        let mut data = PhpLspDiagnosticData {
            snapshot: Some(self.snapshot.clone()),
            ..Default::default()
        };
        match (analyzer, code) {
            (ExternalAnalyzer::PhpStan, "missingType.checkedException")
            | (ExternalAnalyzer::Psalm, "MissingThrowsDocblock") => {
                let owner = self.owner(diagnostic)?;
                let message = diagnostic.message.split("\n\n").next()?;
                let exception = if analyzer == ExternalAnalyzer::PhpStan {
                    let (caller, exception) = message.split_once(" throws checked exception ")?;
                    let caller = caller
                        .strip_prefix("Function ")
                        .or_else(|| caller.strip_prefix("Method "))?
                        .strip_suffix("()")?;
                    if !caller.eq_ignore_ascii_case(&owner.fqn) {
                        return None;
                    }
                    exception.strip_suffix(" but it's missing from the PHPDoc @throws tag.")?
                } else {
                    message.strip_suffix(" is thrown but not caught - please either catch or add a @throws annotation")?
                };
                let exceptions = exception
                    .split('|')
                    .map(absolute_name)
                    .collect::<Option<Vec<_>>>()?;
                data.owner_range = Some(owner.range);
                data.analyzer_fixes = exceptions
                    .into_iter()
                    .map(|exception| serde_json::json!({"kind":"addThrows","exception":exception}))
                    .collect();
            }
            (ExternalAnalyzer::PhpStan, "missingType.iterableValue") => {
                let owner = self.owner(diagnostic)?;
                let (caller, rest) = diagnostic
                    .message
                    .split("\n\n")
                    .next()?
                    .split_once(" has parameter $")?;
                let caller = caller
                    .strip_prefix("Function ")
                    .or_else(|| caller.strip_prefix("Method "))?
                    .strip_suffix("()")?;
                if !caller.eq_ignore_ascii_case(&owner.fqn) {
                    return None;
                }
                let (variable, reported) =
                    rest.split_once(" with no value type specified in iterable type ")?;
                let reported = reported.strip_suffix('.')?;
                if !matches!(reported, "array" | "iterable") {
                    return None;
                }
                let param = owner
                    .signature
                    .as_ref()?
                    .params
                    .iter()
                    .find(|p| p.name.trim_start_matches('$') == variable)?;
                if param.is_variadic || param.is_promoted {
                    return None;
                }
                let native = param.declared_native_type()?;
                let (base, nullable) = match native {
                    TypeInfo::Simple(name) => (name.as_str(), false),
                    TypeInfo::Nullable(inner) => match inner.as_ref() {
                        TypeInfo::Simple(name) => (name.as_str(), true),
                        _ => return None,
                    },
                    _ => return None,
                };
                if base != reported {
                    return None;
                }
                if param
                    .phpdoc_type_info
                    .as_ref()
                    .is_some_and(|doc| doc != native)
                {
                    return None;
                }
                // Do not replace analyzer-specific refinements hidden by native precedence.
                if owner.doc_comment.as_deref().is_some_and(|doc| {
                    action_phpdoc::blocks(doc)
                        .iter()
                        .any(|(tag, _)| matches!(tag.as_str(), "@phpstan-param" | "@psalm-param"))
                }) {
                    return None;
                }
                let nullable = nullable || self.null_default(owner, param)?;
                let key_type = if base == "array" {
                    "array-key"
                } else {
                    "mixed"
                };
                let type_text = format!(
                    "{base}<{key_type}, mixed>{}",
                    if nullable { "|null" } else { "" }
                );
                data.owner_range = Some(owner.range);
                data.analyzer_fixes.push(serde_json::json!({"kind":"addIterableValueType","variable":variable,"typeText":type_text}));
            }
            (
                ExternalAnalyzer::PhpStan,
                "class.notFound" | "class.nameCase" | "interface.nameCase" | "trait.nameCase"
                | "enum.nameCase",
            )
            | (
                ExternalAnalyzer::Psalm,
                "UndefinedClass" | "UndefinedInterface" | "UndefinedTrait" | "InvalidClass",
            ) => {
                let (range, replacement) = self.name_fix(diagnostic, false)?;
                data.analyzer_fixes.push(serde_json::json!({"kind":"replacePrefixedClassName","range":range,"replacement":replacement}));
            }
            (ExternalAnalyzer::PhpStan, "function.nameCase") => {
                let (range, replacement) = self.name_fix(diagnostic, true)?;
                data.replacement = Some(DiagnosticReplacement {
                    new_text: replacement,
                    range: Some(range),
                    title: None,
                });
            }
            _ => return None,
        }
        Some(serde_json::json!({"phpLsp":data}))
    }

    fn name_fix(&self, diagnostic: &Diagnostic, function: bool) -> Option<(Range, String)> {
        let mut fixes = Vec::new();
        for reference in &self.references {
            if self.cancel.is_cancelled() {
                return None;
            }
            if reference.is_declaration
                || reference.is_import_target
                || reference.preserve_spelling_on_rename
                || if function {
                    reference.target_kind != PhpSymbolKind::Function
                } else {
                    !is_class_like_kind(reference.target_kind)
                }
            {
                continue;
            }
            let range = Range::new(
                Position::new(reference.range.0, reference.range.1),
                Position::new(reference.range.2, reference.range.3),
            );
            if if Self::line_only(diagnostic) {
                range.start.line != diagnostic.range.start.line
            } else {
                !range_overlaps(range, diagnostic.range)
            } {
                continue;
            }
            let start = self.offset(range.start)?;
            let end = self.offset(range.end)?;
            let mut name = self
                .tree
                .root_node()
                .descendant_for_byte_range(start, end)?;
            while let Some(parent) = name.parent().filter(|p| {
                matches!(
                    p.kind(),
                    "qualified_name" | "relative_name" | "namespace_name"
                )
            }) {
                name = parent;
            }
            if !matches!(name.kind(), "name" | "qualified_name" | "relative_name") {
                continue;
            }
            let raw = &self.source[name.byte_range()];
            if absolute_name(raw).is_none() {
                continue;
            }
            let target = reference.target_fqn.trim_start_matches('\\');
            if !diagnostic
                .message
                .split(|c: char| {
                    c.is_ascii() && !(c.is_ascii_alphanumeric() || c == '_' || c == '\\')
                })
                .any(|token| token.trim_start_matches('\\').eq_ignore_ascii_case(target))
            {
                continue;
            }
            let desired = if function {
                let symbol = self
                    .index
                    .resolve_fqn_matching_kinds(target, &[PhpSymbolKind::Function])?;
                if symbol.fqn == target || !symbol.fqn.eq_ignore_ascii_case(target) {
                    continue;
                }
                symbol.fqn.clone()
            } else if diagnostic_code_str(diagnostic)?.ends_with("nameCase")
                || diagnostic_code_str(diagnostic) == Some("InvalidClass")
            {
                let symbol = self.index.get_type(target)?;
                if !class_position_accepts(name, &symbol) {
                    continue;
                }
                if symbol.fqn == target || !symbol.fqn.eq_ignore_ascii_case(target) {
                    continue;
                }
                symbol.fqn.clone()
            } else {
                if self.index.get_type(target).is_some()
                    || raw.starts_with('\\')
                    || !raw.contains('\\')
                    || raw.to_ascii_lowercase().starts_with("namespace\\")
                {
                    continue;
                }
                let scope = self.file.scoped_at_byte_position(
                    name.start_position().row as u32,
                    name.start_position().column as u32,
                );
                if scope.use_statements.iter().any(|u| {
                    u.kind == php_lsp_types::UseKind::Class
                        && u.alias
                            .as_deref()
                            .unwrap_or_else(|| short_name(&u.fqn))
                            .eq_ignore_ascii_case(raw.split('\\').next().unwrap_or(""))
                }) {
                    continue;
                }
                let symbol = self.index.get_type(raw)?;
                if !class_position_accepts(name, &symbol) {
                    continue;
                }
                symbol.fqn.clone()
            };
            let replacement = absolute_name(&desired)?;
            if replacement != raw {
                fixes.push((node_range(&self.positions, name), replacement));
            }
        }
        fixes.sort_by_key(|(range, _)| range.start);
        fixes.dedup();
        (fixes.len() == 1).then(|| fixes.remove(0))
    }

    fn offset(&self, position: Position) -> Option<usize> {
        let start = *self.line_starts.get(position.line as usize)?;
        let end = self
            .line_starts
            .get(position.line as usize + 1)
            .copied()
            .unwrap_or(self.source.len());
        Some(
            start
                + php_lsp_parser::utf16::utf16_col_to_byte(
                    &self.source[start..end],
                    0,
                    position.character,
                ) as usize,
        )
    }

    fn null_default(&self, owner: &SymbolInfo, param: &php_lsp_types::ParamInfo) -> Option<bool> {
        if param.default_value.is_none() {
            return Some(false);
        }
        let (node, _) = self.callables.iter().find(|(node, _)| {
            let a = node.start_position();
            let b = node.end_position();
            owner.range == (a.row as u32, a.column as u32, b.row as u32, b.column as u32)
        })?;
        let parameters = node.child_by_field_name("parameters")?;
        let mut cursor = parameters.walk();
        let parameter = parameters.named_children(&mut cursor).find(|candidate| {
            candidate.child_by_field_name("name").is_some_and(|name| {
                self.source[name.byte_range()].trim_start_matches('$')
                    == param.name.trim_start_matches('$')
            })
        })?;
        let mut value = parameter.child_by_field_name("default_value")?;
        while value.kind() == "parenthesized_expression" {
            value = first_non_extra_named_child(value)?;
        }
        match value.kind() {
            "null" => Some(true),
            "array_creation_expression" | "object_creation_expression" => Some(false),
            // Unknown constants/expressions may evaluate to null; do not narrow.
            _ => None,
        }
    }
}

fn class_position_accepts(name: tree_sitter::Node, symbol: &SymbolInfo) -> bool {
    let Some(parent) = name.parent() else {
        return false;
    };
    match parent.kind() {
        "object_creation_expression" => {
            symbol.kind == PhpSymbolKind::Class && !symbol.modifiers.is_abstract
        }
        "class_interface_clause" => symbol.kind == PhpSymbolKind::Interface,
        "base_clause"
            if parent
                .parent()
                .is_some_and(|p| p.kind() == "interface_declaration") =>
        {
            symbol.kind == PhpSymbolKind::Interface
        }
        "base_clause" => symbol.kind == PhpSymbolKind::Class && !symbol.modifiers.is_final,
        "use_declaration" => symbol.kind == PhpSymbolKind::Trait,
        "named_type"
        | "catch_clause"
        | "binary_expression"
        | "class_constant_access_expression"
        | "scoped_call_expression"
        | "scoped_property_access_expression" => matches!(
            symbol.kind,
            PhpSymbolKind::Class | PhpSymbolKind::Interface | PhpSymbolKind::Enum
        ),
        _ => false,
    }
}

fn absolute_name(name: &str) -> Option<String> {
    let name = name.strip_prefix('\\').unwrap_or(name);
    if name.is_empty()
        || name.split('\\').any(|part| {
            part.is_empty()
                || part.as_bytes()[0].is_ascii_digit()
                || part
                    .chars()
                    .any(|c| c.is_ascii() && !(c.is_ascii_alphanumeric() || c == '_'))
        })
    {
        return None;
    }
    Some(format!("\\{name}"))
}

impl PhpLspBackend {
    pub(in crate::server) async fn attach_analyzer_fix_metadata(
        &self,
        request: &WorkspaceRequestContext,
        snapshot: &OpenDocumentSnapshot,
        uri: &str,
        diagnostics: Vec<Diagnostic>,
        token: &OperationCancellationToken,
    ) -> Vec<Diagnostic> {
        if !request.runtime_config().analyzer_code_actions.enabled {
            return diagnostics;
        }
        let captured = snapshot.clone();
        let index = request.index(&self.index);
        let revision = index.revision_snapshot();
        let worker_index = index.clone();
        let generation = request.state.generation;
        let uri = uri.to_string();
        let mut fallback = diagnostics.clone();
        for diagnostic in &mut fallback {
            diagnostic.data = None;
        }
        let task = run_file_io_blocking_cancellable(
            "analyzer quick-fix metadata",
            uri.clone(),
            move |cancel| {
                #[cfg(test)]
                if !wait_metadata_gate(&uri, &cancel) {
                    return None;
                }
                let context = FixContext::new(
                    &uri,
                    &captured.source,
                    &captured.tree,
                    &captured.file_symbols,
                    &worker_index,
                    captured.document_state,
                    generation,
                    &cancel,
                )?;
                let mut diagnostics = diagnostics;
                for diagnostic in &mut diagnostics {
                    if cancel.is_cancelled() {
                        return None;
                    }
                    diagnostic.data = context.metadata(diagnostic);
                }
                Some(diagnostics)
            },
        );
        let result =
            tokio::select! {biased; _=token.cancelled()=>None, result=task=>result.ok().flatten()};
        if index.revision_snapshot() != revision {
            return fallback;
        }
        result.unwrap_or(fallback)
    }
}

#[cfg(test)]
struct MetadataGate {
    entered: std::sync::mpsc::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
    cancelled: std::sync::mpsc::Sender<()>,
}
#[cfg(test)]
static METADATA_GATES: std::sync::OnceLock<StdMutex<HashMap<String, MetadataGate>>> =
    std::sync::OnceLock::new();
#[cfg(test)]
fn wait_metadata_gate(uri: &str, cancel: &OperationCancellationToken) -> bool {
    let gate = METADATA_GATES
        .get_or_init(StdMutex::default)
        .lock()
        .unwrap()
        .remove(uri);
    if let Some(gate) = gate {
        gate.entered.send(()).unwrap();
        loop {
            if cancel.is_cancelled() {
                let _ = gate.cancelled.send(());
                return false;
            }
            match gate.resume.recv_timeout(Duration::from_millis(5)) {
                Ok(()) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => return false,
            }
        }
    }
    !cancel.is_cancelled()
}

#[cfg(test)]
#[path = "diagnostic_fix_tests.rs"]
mod tests;
