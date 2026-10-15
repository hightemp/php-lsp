//! Source-backed method contracts relocated into the target declaration scope.
use super::*;
use php_lsp_types::{FileSymbols, PhpSymbolKind, SymbolInfo, Visibility};

struct Requirements {
    methods: Vec<(Arc<SymbolInfo>, Arc<SymbolInfo>)>,
    declarations: Vec<Arc<SymbolInfo>>,
}
#[derive(Default)]
struct SourceGroup {
    methods: Vec<(Arc<SymbolInfo>, Arc<SymbolInfo>)>,
    declarations: Vec<Arc<SymbolInfo>>,
}

pub(super) struct ImplementationContracts {
    pub metadata: HashMap<String, MethodContractMetadata>,
    revision: php_lsp_index::workspace::WorkspaceIndexRevision,
    states: Vec<(String, Option<OpenDocumentState>, SourceFingerprint)>,
}
impl ImplementationContracts {
    pub(super) fn is_current(&self, backend: &PhpLspBackend, index: &WorkspaceIndex) -> bool {
        index.revision_snapshot() == self.revision
            && self
                .states
                .iter()
                .all(|(uri, state, _)| backend.current_document_state(uri) == *state)
    }
}

impl PhpLspBackend {
    pub(super) async fn implementation_contracts(
        &self,
        request: &WorkspaceRequestContext,
        target_file: &FileSymbols,
        target: &SymbolInfo,
        methods: &[Arc<SymbolInfo>],
    ) -> Option<ImplementationContracts> {
        tokio::time::timeout(
            Duration::from_millis(FILE_IO_TIMEOUT_MS),
            self.implementation_contracts_inner(request, target_file, target, methods),
        )
        .await
        .ok()
        .flatten()
    }

    async fn implementation_contracts_inner(
        &self,
        request: &WorkspaceRequestContext,
        target_file: &FileSymbols,
        target: &SymbolInfo,
        methods: &[Arc<SymbolInfo>],
    ) -> Option<ImplementationContracts> {
        let index = request.index(&self.index);
        let revision = index.revision_snapshot();
        let graph_index = index.clone();
        let graph_file = target_file.clone();
        let graph_target = target.clone();
        let missing = methods.to_vec();
        let requirements = run_file_io_blocking_cancellable(
            "implementation hierarchy",
            target.uri.clone(),
            move |cancel| {
                all_requirements(&graph_index, &graph_file, &graph_target, &missing, &cancel)
            },
        )
        .await
        .ok()??;
        let mut groups = std::collections::BTreeMap::<String, SourceGroup>::new();
        for (method, owner) in requirements.methods {
            groups
                .entry(method.uri.clone())
                .or_default()
                .methods
                .push((method, owner));
        }
        for declaration in requirements.declarations {
            groups
                .entry(declaration.uri.clone())
                .or_default()
                .declarations
                .push(declaration);
        }
        let target_scope = Arc::new(declaration_scope(target_file, target.range));
        let mut states = Vec::new();
        let mut result = HashMap::new();
        let mut contracts = HashMap::<String, (String, Vec<String>)>::new();
        for (uri, group) in groups {
            if index.revision_snapshot() != revision {
                return None;
            }
            let state = self.current_document_state(&uri);
            let source = self
                .source_for_uri_in_request(request, &uri, "implementation contract source")
                .await?;
            if self.current_document_state(&uri) != state
                || self.template_documents.contains_key(&uri)
            {
                return None;
            }
            let fingerprint = SourceFingerprint::from_bytes(source.as_bytes());
            if state.is_none()
                && index
                    .read()
                    .source_fingerprints()
                    .get(&uri)
                    .map(|entry| *entry.value())
                    != Some(fingerprint)
            {
                return None;
            }
            states.push((uri.clone(), state, fingerprint));
            let worker_index = index.clone();
            let target_scope = target_scope.clone();
            let version = request.runtime_config().php_version;
            let target_uri = target.uri.clone();
            // Only one donor's text/tree is retained at a time. All declarations
            // and methods of that URI share this parse, including trait consumers.
            let chunk = run_file_io_blocking_cancellable(
                "implementation contracts",
                uri.clone(),
                move |cancel| {
                    if cancel.is_cancelled() {
                        return None;
                    }
                    #[cfg(test)]
                    if !group.methods.is_empty() && !pause_plan(&target_uri, &cancel) {
                        return None;
                    }
                    let _ = target_uri;
                    let mut parser = FileParser::new();
                    parser.parse_full(&source);
                    let tree = parser.tree()?;
                    let file = extract_file_symbols(tree, &source, &uri);
                    for expected in group.declarations {
                        if cancel.is_cancelled() {
                            return None;
                        }
                        let actual = file.symbols.iter().find(|symbol| {
                            symbol.fqn == expected.fqn
                                && symbol.range == expected.range
                                && symbol.kind == expected.kind
                        })?;
                        if actual.extends != expected.extends
                            || actual.implements != expected.implements
                            || actual.traits != expected.traits
                            || actual.templates != expected.templates
                        {
                            return None;
                        }
                        let (start, end) = byte_offsets_for_range(&source, actual.range)?;
                        let declaration = tree.root_node().descendant_for_byte_range(start, end)?;
                        if !hierarchy_matches(declaration, &source, &file, actual) {
                            return None;
                        }
                        let mut pending = vec![declaration];
                        while let Some(node) = pending.pop() {
                            if cancel.is_cancelled() {
                                return None;
                            }
                            if matches!(node.kind(), "use_as_clause" | "use_instead_of_clause") {
                                return None;
                            }
                            if node.start_byte() != start
                                && (php_lsp_parser::variable_scope::is_callable_scope(node)
                                    || matches!(
                                        node.kind(),
                                        "class_declaration"
                                            | "interface_declaration"
                                            | "trait_declaration"
                                            | "enum_declaration"
                                            | "anonymous_class"
                                    ))
                            {
                                continue;
                            }
                            let mut cursor = node.walk();
                            pending.extend(node.named_children(&mut cursor));
                        }
                    }
                    let mut chunk = Vec::new();
                    for (method, owner) in group.methods {
                        if cancel.is_cancelled() {
                            return None;
                        }
                        let actual = file
                            .symbols
                            .iter()
                            .find(|s| s.fqn == method.fqn && s.range == method.range)?;
                        if actual.signature != method.signature
                            || actual.doc_comment != method.doc_comment
                            || actual.attributes != method.attributes
                            || actual.visibility != method.visibility
                        {
                            return None;
                        }
                        let metadata = contract_metadata(
                            tree,
                            &source,
                            &file,
                            &method,
                            &owner,
                            &target_scope,
                            &worker_index,
                            version,
                            &cancel,
                        )?;
                        chunk.push((method, metadata));
                    }
                    Some(chunk)
                },
            )
            .await
            .ok()??;
            for (method, metadata) in chunk {
                let identity = metadata.declaration.as_ref()?;
                let doc_identity =
                    doc_contract_identity(metadata.doc_comment.as_deref().unwrap_or(""))?;
                let key = method.name.to_ascii_lowercase();
                if contracts
                    .get(&key)
                    .is_some_and(|(old, doc)| old != identity || *doc != doc_identity)
                {
                    return None;
                }
                contracts.insert(key, (identity.clone(), doc_identity));
                result.insert(method.fqn.clone(), metadata);
            }
        }
        for (uri, state, fingerprint) in &states {
            if self.current_document_state(uri) != *state {
                return None;
            }
            if state.is_none() {
                let source = self
                    .source_for_uri_in_request(request, uri, "implementation contract recheck")
                    .await?;
                if SourceFingerprint::from_bytes(source.as_bytes()) != *fingerprint {
                    return None;
                }
            }
        }
        (index.revision_snapshot() == revision
            && states
                .iter()
                .all(|(uri, state, _)| self.current_document_state(uri) == *state))
        .then_some(ImplementationContracts {
            metadata: result,
            revision,
            states,
        })
    }
}

fn all_requirements(
    index: &WorkspaceIndex,
    file: &FileSymbols,
    target: &SymbolInfo,
    missing: &[Arc<SymbolInfo>],
    cancel: &OperationCancellationToken,
) -> Option<Requirements> {
    let names = missing
        .iter()
        .map(|m| m.name.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    let mut local_types = HashMap::new();
    let mut local_members = HashMap::<String, Vec<Arc<SymbolInfo>>>::new();
    for symbol in &file.symbols {
        if cancel.is_cancelled() {
            return None;
        }
        if is_class_like_kind(symbol.kind) {
            if local_types
                .insert(symbol.fqn.to_ascii_lowercase(), Arc::new(symbol.clone()))
                .is_some()
            {
                return None;
            }
        } else if let Some(parent) = &symbol.parent_fqn {
            local_members
                .entry(parent.to_ascii_lowercase())
                .or_default()
                .push(Arc::new(symbol.clone()));
        }
    }
    let mut pending = vec![(target.fqn.clone(), Arc::new(target.clone()), false)];
    let mut seen = HashSet::new();
    let mut active = HashSet::new();
    let mut result = Requirements {
        methods: Vec::new(),
        declarations: Vec::new(),
    };
    while let Some((fqn, consumer, leaving)) = pending.pop() {
        if cancel.is_cancelled() {
            return None;
        }
        let key = fqn.trim_start_matches('\\').to_ascii_lowercase();
        if leaving {
            active.remove(&key);
            continue;
        }
        if active.contains(&key) {
            return None;
        }
        if !seen.insert((key.clone(), consumer.fqn.to_ascii_lowercase())) {
            continue;
        }
        let ty = local_types
            .get(&key)
            .cloned()
            .or_else(|| index.get_type(&fqn))?;
        active.insert(key);
        pending.push((fqn, consumer.clone(), true));
        let semantic_owner = if ty.kind == PhpSymbolKind::Trait {
            consumer.clone()
        } else {
            ty.clone()
        };
        let members = if ty.uri == target.uri {
            local_members
                .get(&ty.fqn.to_ascii_lowercase())
                .cloned()
                .unwrap_or_default()
        } else {
            index.get_direct_members(&ty.fqn)
        };
        for method in members {
            if method.kind == PhpSymbolKind::Method
                && (ty.kind == PhpSymbolKind::Interface || method.modifiers.is_abstract)
                && names.contains(&method.name.to_ascii_lowercase())
            {
                if method.uri != ty.uri || !byte_range_contains(ty.range, method.range) {
                    return None;
                }
                result.methods.push((method, semantic_owner.clone()));
            }
        }
        let next_consumer = if ty.kind == PhpSymbolKind::Class {
            ty.clone()
        } else {
            consumer
        };
        for parent in ty.extends.iter().chain(&ty.implements).chain(&ty.traits) {
            pending.push((parent.clone(), next_consumer.clone(), false));
        }
        result.declarations.push(ty);
    }
    Some(result)
}

fn class_name(
    raw: &str,
    source: &FileSymbols,
    target: &FileSymbols,
    owner: &SymbolInfo,
) -> Option<String> {
    if let Some((class, member)) = raw.rsplit_once("::") {
        return Some(format!(
            "{}::{member}",
            class_name(class, source, target, owner)?
        ));
    }
    if raw.is_empty()
        || raw
            .chars()
            .any(|c| c.is_ascii() && !(c.is_ascii_alphanumeric() || c == '_' || c == '\\'))
        || raw.trim_start_matches('\\').split('\\').any(str::is_empty)
    {
        return None;
    }
    let fqn = match raw.to_ascii_lowercase().as_str() {
        "self" if owner.kind != PhpSymbolKind::Trait => owner.fqn.clone(),
        "parent" if owner.kind != PhpSymbolKind::Trait => owner.extends.first()?.clone(),
        "self" | "parent" | "static" => return None,
        _ => resolve_class_name_pub(raw, source),
    };
    let resolved_target = resolve_class_name_pub(raw, target);
    if !matches!(raw.to_ascii_lowercase().as_str(), "self" | "parent")
        && php_lsp_types::symbol_fqn_eq(&resolved_target, &fqn, PhpSymbolKind::Class)
    {
        Some(raw.to_string())
    } else {
        Some(format!("\\{}", fqn.trim_start_matches('\\')))
    }
}

fn declaration_scope(file: &FileSymbols, range: (u32, u32, u32, u32)) -> FileSymbols {
    let mut scope = file.scoped_at_byte_position(range.0, range.1).into_owned();
    // Namespace imports take effect at their declaration, not retroactively.
    scope
        .use_statements
        .retain(|import| (import.range.0, import.range.1) < (range.0, range.1));
    scope
}

fn hierarchy_matches(
    node: tree_sitter::Node,
    source: &str,
    file: &FileSymbols,
    symbol: &SymbolInfo,
) -> bool {
    let scope = declaration_scope(file, symbol.range);
    let names = |node: tree_sitter::Node| {
        let mut cursor = node.walk();
        node.named_children(&mut cursor)
            .filter(|child| matches!(child.kind(), "name" | "qualified_name" | "relative_name"))
            .map(|child| resolve_class_name_pub(&source[child.byte_range()], &scope))
            .collect::<Vec<_>>()
    };
    let mut extends = Vec::new();
    let mut implements = Vec::new();
    let mut traits = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "base_clause" => extends.extend(names(child)),
            "class_interface_clause" => implements.extend(names(child)),
            "declaration_list" => {
                let mut cursor = child.walk();
                for child in child
                    .named_children(&mut cursor)
                    .filter(|child| child.kind() == "use_declaration")
                {
                    traits.extend(names(child));
                }
            }
            _ => {}
        }
    }
    let equal = |left: &[String], right: &[String]| {
        left.len() == right.len()
            && left
                .iter()
                .zip(right)
                .all(|(a, b)| php_lsp_types::symbol_fqn_eq(a, b, PhpSymbolKind::Class))
    };
    equal(&extends, &symbol.extends)
        && equal(&implements, &symbol.implements)
        && equal(&traits, &symbol.traits)
}

fn doc_contract_identity(doc: &str) -> Option<Vec<String>> {
    action_phpdoc::contract_identity(doc)
}

#[cfg(test)]
struct PlanPause {
    entered: std::sync::mpsc::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
    cancelled: std::sync::mpsc::Sender<()>,
    finish: Option<std::sync::mpsc::Receiver<()>>,
}
#[cfg(test)]
type PublicationPause = (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);
#[cfg(test)]
static PUBLICATION_PAUSES: std::sync::OnceLock<StdMutex<HashMap<String, PublicationPause>>> =
    std::sync::OnceLock::new();
#[cfg(test)]
pub(super) async fn pause_publication(uri: &str) {
    let pause = PUBLICATION_PAUSES
        .get_or_init(StdMutex::default)
        .lock()
        .unwrap()
        .remove(uri);
    if let Some((entered, resume)) = pause {
        let _ = entered.send(());
        let _ = resume.await;
    }
}
#[cfg(test)]
static PLAN_PAUSES: std::sync::OnceLock<StdMutex<HashMap<String, PlanPause>>> =
    std::sync::OnceLock::new();
#[cfg(test)]
fn pause_plan(uri: &str, cancel: &OperationCancellationToken) -> bool {
    let pause = PLAN_PAUSES
        .get_or_init(StdMutex::default)
        .lock()
        .unwrap()
        .remove(uri);
    if let Some(pause) = pause {
        pause.entered.send(()).unwrap();
        loop {
            if cancel.is_cancelled() {
                let _ = pause.cancelled.send(());
                if let Some(finish) = pause.finish {
                    let _ = finish.recv_timeout(Duration::from_secs(5));
                }
                return false;
            }
            match pause.resume.recv_timeout(Duration::from_millis(5)) {
                Ok(()) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => return false,
            }
        }
    }
    !cancel.is_cancelled()
}

#[allow(clippy::too_many_arguments)]
fn contract_metadata(
    tree: &tree_sitter::Tree,
    source: &str,
    file: &FileSymbols,
    method: &SymbolInfo,
    owner: &SymbolInfo,
    target: &FileSymbols,
    index: &WorkspaceIndex,
    version: PhpVersion,
    cancel: &OperationCancellationToken,
) -> Option<MethodContractMetadata> {
    let (start, end) = byte_offsets_for_range(source, method.range)?;
    let node = node_for_exact_byte_span(tree.root_node(), start, end, "method_declaration")?;
    if node.has_error() {
        return None;
    }
    let scope = declaration_scope(file, method.range);
    let header_end = node
        .child_by_field_name("body")
        .map(|body| body.start_byte())
        .unwrap_or_else(|| node.end_byte() - 1);
    let mut edits = Vec::<(usize, usize, String)>::new();
    let mut pending = vec![(node, false)];
    let mut explicit_visibility = false;
    while let Some((current, constant)) = pending.pop() {
        if cancel.is_cancelled() {
            return None;
        }
        if current.start_byte() >= header_end || current.is_extra() {
            continue;
        }
        let raw = source.get(current.byte_range())?;
        if constant
            && !matches!(
                current.kind(),
                "arguments"
                    | "argument"
                    | "array_creation_expression"
                    | "array_element_initializer"
                    | "integer"
                    | "float"
                    | "boolean"
                    | "null"
                    | "string"
                    | "encapsed_string"
                    | "unary_op_expression"
                    | "binary_expression"
                    | "parenthesized_expression"
                    | "name"
                    | "qualified_name"
                    | "relative_name"
                    | "class_constant_access_expression"
            )
        {
            return None;
        }
        if matches!(current.kind(), "string" | "encapsed_string") {
            if !is_static_string_literal_node(current) {
                return None;
            }
            continue;
        }
        let mut replacement = None;
        match current.kind() {
            "abstract_modifier" => {
                let suffix = &source[current.end_byte()..header_end];
                let spaces = suffix.len() - suffix.trim_start_matches([' ', '\t']).len();
                edits.push((
                    current.start_byte(),
                    current.end_byte() + spaces,
                    String::new(),
                ));
                continue;
            }
            "visibility_modifier" => explicit_visibility = true,
            "property_promotion_parameter" | "object_creation_expression" => return None,
            "attribute_list" if !version.at_least(8, 0) => return None,
            "formal_parameters" if !version.at_least(8, 0) => {
                let mut cursor = current.walk();
                let mut previous = None;
                for child in current
                    .children(&mut cursor)
                    .filter(|child| !child.is_extra())
                {
                    if child.kind() == ")" && previous == Some(",") {
                        return None;
                    }
                    previous = Some(child.kind());
                }
            }
            "union_type" if !version.at_least(8, 0) => return None,
            "intersection_type" if !version.at_least(8, 1) => return None,
            "disjunctive_normal_form_type" if !version.at_least(8, 2) => return None,
            "named_type" => {
                if raw.eq_ignore_ascii_case("object") {
                    continue;
                }
                if raw.eq_ignore_ascii_case("static") {
                    if !version.at_least(8, 0)
                        || !node
                            .child_by_field_name("return_type")
                            .is_some_and(|ret| node_contains_node(ret, current))
                    {
                        return None;
                    }
                } else {
                    replacement = Some(class_name(raw, &scope, target, owner)?);
                }
            }
            "primitive_type" | "bottom_type" => {
                let lower = raw.to_ascii_lowercase();
                let required = match lower.as_str() {
                    "mixed" => (8, 0),
                    "never" => (8, 1),
                    "true" => (8, 2),
                    "null" | "false"
                        if current
                            .parent()
                            .is_some_and(|parent| parent.kind() == "union_type") =>
                    {
                        (8, 0)
                    }
                    "null" | "false" => (8, 2),
                    _ => (7, 4),
                };
                if !version.at_least(required.0, required.1) {
                    return None;
                }
            }
            "class_constant_access_expression" => {
                let mut cursor = current.walk();
                let parts = current
                    .named_children(&mut cursor)
                    .filter(|n| !n.is_extra())
                    .collect::<Vec<_>>();
                if parts.len() != 2 {
                    return None;
                }
                let class = parts[0];
                let member = source.get(parts[1].byte_range())?;
                let qualified = class_name(source.get(class.byte_range())?, &scope, target, owner)?;
                let fqn = resolve_class_name_pub(source.get(class.byte_range())?, &scope);
                let fqn = match source
                    .get(class.byte_range())?
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "self" => owner.fqn.clone(),
                    "parent" => owner.extends.first()?.clone(),
                    _ => fqn,
                };
                if !member.eq_ignore_ascii_case("class") {
                    let constant = index.resolve_member_matching_kinds(
                        &format!("{fqn}::{member}"),
                        &[PhpSymbolKind::ClassConstant, PhpSymbolKind::EnumCase],
                    )?;
                    if constant.visibility != Visibility::Public
                        || (constant.kind == PhpSymbolKind::EnumCase && !version.at_least(8, 1))
                    {
                        return None;
                    }
                }
                edits.push((class.start_byte(), class.end_byte(), qualified));
                continue;
            }
            "name" | "qualified_name"
                if current.parent().is_some_and(|p| p.kind() == "attribute") =>
            {
                replacement = Some(class_name(raw, &scope, target, owner)?)
            }
            "name" | "qualified_name" | "relative_name" if constant => {
                let fqn = php_lsp_parser::resolve::resolve_constant_name_pub(raw, &scope);
                index.resolve_fqn_matching_kinds(&fqn, &[PhpSymbolKind::GlobalConstant])?;
                replacement = Some(format!("\\{}", fqn.trim_start_matches('\\')));
            }
            _ => {}
        }
        if let Some(replacement) = replacement {
            edits.push((current.start_byte(), current.end_byte(), replacement));
            continue;
        }
        let mut cursor = current.walk();
        for child in current.named_children(&mut cursor) {
            if current.kind() == "argument"
                && current
                    .child_by_field_name("name")
                    .is_some_and(|label| label.id() == child.id())
            {
                continue;
            }
            let child_constant = constant
                || current
                    .child_by_field_name("default_value")
                    .is_some_and(|value| value.id() == child.id())
                || (current.kind() == "attribute" && child.kind() == "arguments");
            pending.push((child, child_constant));
        }
    }
    if !explicit_visibility {
        let mut cursor = node.walk();
        let first = node
            .children(&mut cursor)
            .find(|child| !child.is_extra() && child.kind() != "attribute_list")?;
        edits.push((first.start_byte(), first.start_byte(), "public ".into()));
    }
    let mut declaration = source.get(start..header_end)?.to_string();
    edits.sort_by_key(|e| std::cmp::Reverse(e.0));
    for (a, b, text) in edits {
        declaration.replace_range(a - start..b - start, &text);
    }
    let doc_comment = match method.doc_comment.as_deref() {
        Some(doc) => Some(qualify_doc(doc, method, file, target, owner)?),
        None => None,
    };
    Some(MethodContractMetadata {
        declaration: Some(declaration.trim().to_string()),
        doc_comment,
        attributes: Vec::new(),
    })
}

fn qualify_doc(
    doc: &str,
    method: &SymbolInfo,
    file: &FileSymbols,
    target: &FileSymbols,
    owner: &SymbolInfo,
) -> Option<String> {
    let scope = declaration_scope(file, method.range);
    let parsed = parse_phpdoc(doc);
    if !parsed.type_aliases.is_empty() || !parsed.type_alias_imports.is_empty() {
        return None;
    }
    let templates = parsed
        .templates
        .iter()
        .map(|t| t.name.as_str())
        .collect::<HashSet<_>>();
    // A type can continue on the next PHPDoc line (including a union after
    // an already complete first type). Qualify the whole tag, not each line.
    let mut lines: Vec<String> = Vec::new();
    let mut type_tag = false;
    for line in phpdoc_content_lines(doc) {
        let trimmed = line.trim();
        if trimmed.starts_with('@') {
            let tag = trimmed.split_whitespace().next()?;
            type_tag = matches!(
                action_phpdoc::base_tag(tag),
                "param"
                    | "return"
                    | "throws"
                    | "var"
                    | "template"
                    | "template-covariant"
                    | "template-contravariant"
            );
            lines.push(line);
        } else if type_tag {
            let previous = lines.last_mut()?;
            previous.push(' ');
            previous.push_str(trimmed);
        } else {
            lines.push(line);
        }
    }
    for line in &mut lines {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('@') {
            continue;
        }
        let (tag, rest) = trimmed
            .split_once(char::is_whitespace)
            .unwrap_or((trimmed, ""));
        if action_phpdoc::class_annotation(tag) {
            return None;
        }
        let mut prefix = String::new();
        let base = action_phpdoc::base_tag(tag);
        if matches!(
            base,
            "param-out" | "param-closure-this" | "assert" | "assert-if-true" | "assert-if-false"
        ) {
            return None;
        }
        let rest = if matches!(
            base,
            "template" | "template-covariant" | "template-contravariant"
        ) {
            let Some((name, bound)) = rest.trim_start().split_once(char::is_whitespace) else {
                continue;
            };
            let (operator, ty) = bound.trim_start().split_once(char::is_whitespace)?;
            if !matches!(operator, "of" | "as") {
                return None;
            }
            prefix = format!("{name} {operator} ");
            ty
        } else if !matches!(base, "param" | "return" | "throws" | "var") {
            // Unknown annotations may contain source-relative types or a DSL.
            // Only known metadata/flags are portable without type relocation.
            if matches!(
                base,
                "deprecated"
                    | "since"
                    | "author"
                    | "version"
                    | "copyright"
                    | "license"
                    | "link"
                    | "api"
                    | "internal"
                    | "pure"
                    | "impure"
                    | "mutation-free"
                    | "external-mutation-free"
                    | "immutable"
                    | "readonly"
                    | "override"
                    | "inheritdoc"
                    | "inheritDoc"
            ) {
                continue;
            }
            return None;
        } else {
            rest
        };
        let rest = rest.trim_start();
        let end = consume_phpdoc_type_expr(rest)?;
        let ty = parse_phpdoc(&format!("/** @var {} */", &rest[..end])).var_type?;
        let declaring_owner = file
            .symbols
            .iter()
            .find(|symbol| Some(&symbol.fqn) == method.parent_fqn.as_ref());
        let declaring_doc = declaring_owner
            .and_then(|symbol| symbol.doc_comment.as_deref())
            .map(parse_phpdoc)
            .unwrap_or_default();
        let qualify = |name: &str| -> Option<String> {
            if templates.contains(name)
                || name == "$this"
                || is_builtin_type_name(name)
                || php_lsp_types::type_refinement::is_primitive(name)
                || name.contains('-')
                || matches!(name, "list" | "scalar" | "numeric")
            {
                return Some(name.into());
            }
            if scope.type_aliases.iter().any(|alias| alias.name == name)
                || scope
                    .type_alias_imports
                    .iter()
                    .any(|alias| alias.name == name)
                || owner.templates.iter().any(|template| template.name == name)
                || declaring_doc
                    .templates
                    .iter()
                    .any(|template| template.name == name)
                || declaring_doc
                    .type_aliases
                    .iter()
                    .any(|alias| alias.name == name)
                || declaring_doc
                    .type_alias_imports
                    .iter()
                    .any(|alias| alias.name == name)
            {
                return None;
            }
            class_name(name, &scope, target, owner)
        };
        let mapped = php_lsp_types::conditional::try_transform_type(&ty, &mut |ty| {
            use php_lsp_types::TypeInfo;
            Some(match ty {
                TypeInfo::Self_ => {
                    TypeInfo::Simple(format!("\\{}", owner.fqn.trim_start_matches('\\')))
                }
                TypeInfo::Parent_ => TypeInfo::Simple(format!(
                    "\\{}",
                    owner.extends.first()?.trim_start_matches('\\')
                )),
                TypeInfo::Simple(name) => TypeInfo::Simple(qualify(&name)?),
                TypeInfo::Generic { base, args } => TypeInfo::Generic {
                    base: qualify(&base)?,
                    args,
                },
                TypeInfo::Conditional {
                    subject,
                    target,
                    if_type,
                    else_type,
                } => {
                    let subject = if subject.starts_with('$')
                        || templates.contains(subject.as_str())
                        || php_lsp_types::conditional::literal_default_type(&subject).is_some()
                    {
                        subject
                    } else {
                        qualify(&subject)?
                    };
                    TypeInfo::Conditional {
                        subject,
                        target,
                        if_type,
                        else_type,
                    }
                }
                other => other,
            })
        })?;
        let plain = mapped.to_string();
        let rendered = if parse_phpdoc(&format!("/** @var {plain} */"))
            .var_type
            .as_ref()
            == Some(&mapped)
        {
            plain
        } else {
            php_lsp_parser::resolve::receiver_type_text(&mapped)
        };
        if parse_phpdoc(&format!("/** @var {rendered} */"))
            .var_type
            .as_ref()
            != Some(&mapped)
        {
            return None;
        }
        *line = format!("{tag} {prefix}{rendered}{}", &rest[end..]);
    }
    Some(render_phpdoc_comment("", &lines))
}

#[cfg(test)]
#[path = "inherited_method_contracts_tests.rs"]
mod tests;
