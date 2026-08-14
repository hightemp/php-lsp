//! Extract PHP symbols from tree-sitter CST.
//!
//! Walks the CST and produces `FileSymbols` containing all classes, interfaces,
//! traits, enums, functions, methods, properties, constants, namespace and use statements.

use php_lsp_types::*;
use std::collections::HashSet;
use tree_sitter::{Node, Tree};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PhpSymbolExtractionVersion {
    pub major: u16,
    pub minor: u16,
}

/// Extract all symbols from a parsed PHP file.
pub fn extract_file_symbols(tree: &Tree, source: &str, uri: &str) -> FileSymbols {
    extract_file_symbols_with_php_version(tree, source, uri, None)
}

/// Extract symbols while filtering phpstorm-stubs availability attributes.
pub fn extract_file_symbols_for_php_version(
    tree: &Tree,
    source: &str,
    uri: &str,
    php_version: PhpSymbolExtractionVersion,
) -> FileSymbols {
    extract_file_symbols_with_php_version(tree, source, uri, Some(php_version))
}

fn extract_file_symbols_with_php_version(
    tree: &Tree,
    source: &str,
    uri: &str,
    php_version: Option<PhpSymbolExtractionVersion>,
) -> FileSymbols {
    let mut result = FileSymbols::default();
    let root = tree.root_node();
    extract_file_level_phpdoc_aliases(root, source, &mut result);

    let mut root_cursor = root.walk();
    let children: Vec<Node<'_>> = root.children(&mut root_cursor).collect();
    let has_namespace_declarations = children.iter().any(|child| {
        child.kind() == "namespace_definition"
            && crate::resolve::namespace_relative_function_call(*child, source).is_none()
    });

    if !has_namespace_declarations {
        result.namespace_scopes.push(NamespaceScope {
            namespace: None,
            range: node_range(root),
        });
    }

    // Handle both bracketed namespace bodies and unbracketed namespace
    // sections. An unbracketed section extends to the next namespace
    // declaration (or EOF), not merely to the declaration's semicolon.
    let mut current_ns: Option<String> = None;
    for (index, child) in children.iter().copied().enumerate() {
        match child.kind() {
            "namespace_definition"
                if crate::resolve::namespace_relative_function_call(child, source).is_none() =>
            {
                let ns_name = find_namespace_name(child, source);
                result.namespace = ns_name.clone();

                if let Some(body) = child.child_by_field_name("body") {
                    result.namespace_scopes.push(NamespaceScope {
                        namespace: ns_name.clone(),
                        range: node_range(child),
                    });
                    extract_children(body, source, uri, &mut result, &ns_name, php_version);
                    current_ns = None;
                } else {
                    let end = children[index + 1..]
                        .iter()
                        .find(|candidate| {
                            candidate.kind() == "namespace_definition"
                                && crate::resolve::namespace_relative_function_call(
                                    **candidate,
                                    source,
                                )
                                .is_none()
                        })
                        .map(|candidate| candidate.start_position())
                        .unwrap_or_else(|| root.end_position());
                    let start = child.start_position();
                    result.namespace_scopes.push(NamespaceScope {
                        namespace: ns_name.clone(),
                        range: (
                            start.row as u32,
                            start.column as u32,
                            end.row as u32,
                            end.column as u32,
                        ),
                    });
                    current_ns = ns_name;
                }
            }
            _ => {
                extract_from_node(child, source, uri, &mut result, &current_ns, php_version);
            }
        }
    }

    result
}

fn extract_file_level_phpdoc_aliases(root: Node, source: &str, result: &mut FileSymbols) {
    collect_file_level_phpdoc_aliases(root, source, result);

    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "namespace_definition" {
            collect_file_level_phpdoc_aliases(child, source, result);
        }
    }
}

fn collect_file_level_phpdoc_aliases(node: Node, source: &str, result: &mut FileSymbols) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "comment" {
            continue;
        }

        let text = node_text(child, source);
        if !text.starts_with("/**") || phpdoc_comment_belongs_to_declaration(child) {
            continue;
        }

        let phpdoc = crate::phpdoc::parse_phpdoc(text);
        result.type_aliases.extend(phpdoc.type_aliases);
        result.type_alias_imports.extend(phpdoc.type_alias_imports);
    }
}

fn phpdoc_comment_belongs_to_declaration(comment: Node) -> bool {
    let mut next = comment.next_sibling();
    while let Some(node) = next {
        match node.kind() {
            "class_declaration"
            | "interface_declaration"
            | "trait_declaration"
            | "enum_declaration"
            | "function_definition"
            | "const_declaration" => return true,
            "namespace_definition" | "namespace_use_declaration" | "declare_statement" => {
                return false;
            }
            "comment" => return false,
            _ => {
                next = node.next_sibling();
            }
        }
    }
    false
}

/// Find the namespace name from a namespace_definition node.
/// The name is in a `namespace_name` child (not field "name").
fn find_namespace_name(node: Node, source: &str) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "namespace_name" {
            return Some(node_text(child, source).to_string());
        }
    }
    None
}

fn extract_from_node(
    node: Node,
    source: &str,
    uri: &str,
    result: &mut FileSymbols,
    current_ns: &Option<String>,
    php_version: Option<PhpSymbolExtractionVersion>,
) {
    match node.kind() {
        "namespace_use_declaration" => {
            extract_use_statements(node, source, result, current_ns);
        }
        "class_declaration" => {
            extract_class_like(
                node,
                source,
                uri,
                result,
                current_ns,
                PhpSymbolKind::Class,
                php_version,
            );
        }
        "interface_declaration" => {
            extract_class_like(
                node,
                source,
                uri,
                result,
                current_ns,
                PhpSymbolKind::Interface,
                php_version,
            );
        }
        "trait_declaration" => {
            extract_class_like(
                node,
                source,
                uri,
                result,
                current_ns,
                PhpSymbolKind::Trait,
                php_version,
            );
        }
        "enum_declaration" => {
            extract_class_like(
                node,
                source,
                uri,
                result,
                current_ns,
                PhpSymbolKind::Enum,
                php_version,
            );
        }
        "function_definition" => {
            extract_function(node, source, uri, result, current_ns, php_version);
        }
        "const_declaration" => {
            extract_global_constants(node, source, uri, result, current_ns, php_version);
        }
        _ => {
            // Recurse into children
            extract_children(node, source, uri, result, current_ns, php_version);
        }
    }
}

fn extract_children(
    node: Node,
    source: &str,
    uri: &str,
    result: &mut FileSymbols,
    current_ns: &Option<String>,
    php_version: Option<PhpSymbolExtractionVersion>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        extract_from_node(child, source, uri, result, current_ns, php_version);
    }
}

/// Extract use statements from a `namespace_use_declaration`.
fn extract_use_statements(
    node: Node,
    source: &str,
    result: &mut FileSymbols,
    current_ns: &Option<String>,
) {
    let declaration_kind = determine_use_kind(node, source);

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "namespace_use_clause" {
            let clause_kind = use_clause_kind(child, source).unwrap_or(declaration_kind);
            extract_single_use_clause(child, source, result, clause_kind, current_ns);
        } else if child.kind() == "namespace_use_group" {
            extract_use_group(child, node, source, result, declaration_kind, current_ns);
        }
    }
}

/// Extract a single use clause. The CST structure is:
/// namespace_use_clause -> qualified_name, [as, name(alias)]
fn extract_single_use_clause(
    clause: Node,
    source: &str,
    result: &mut FileSymbols,
    kind: UseKind,
    current_ns: &Option<String>,
) {
    let mut fqn: Option<String> = None;
    let mut alias: Option<String> = None;
    let mut saw_as = false;

    let mut cursor = clause.walk();
    for child in clause.children(&mut cursor) {
        match child.kind() {
            "qualified_name" | "namespace_name" | "name" if !saw_as => {
                fqn = Some(node_text(child, source).to_string());
            }
            "as" => {
                saw_as = true;
            }
            "name" if saw_as => {
                alias = Some(node_text(child, source).to_string());
            }
            _ => {}
        }
    }

    if let Some(fqn) = fqn {
        let sp = clause.start_position();
        let ep = clause.end_position();
        let range = (
            sp.row as u32,
            sp.column as u32,
            ep.row as u32,
            ep.column as u32,
        );
        result.use_statements.push(UseStatement {
            fqn,
            alias,
            kind,
            namespace: current_ns.clone(),
            range,
        });
    }
}

fn extract_use_group(
    group: Node,
    parent: Node,
    source: &str,
    result: &mut FileSymbols,
    declaration_kind: UseKind,
    current_ns: &Option<String>,
) {
    // tree-sitter exposes the group prefix as the declaration's
    // `namespace_name` child (the group itself only contains clauses).
    let mut parent_cursor = parent.walk();
    let prefix = parent
        .named_children(&mut parent_cursor)
        .find(|child| child.kind() == "namespace_name")
        .map(|node| node_text(node, source).trim_matches('\\').to_string())
        .unwrap_or_default();

    let mut cursor = group.walk();
    for clause in group.named_children(&mut cursor) {
        if clause.kind() != "namespace_use_clause" {
            continue;
        }

        let alias_node = clause.child_by_field_name("alias");
        let mut clause_cursor = clause.walk();
        let name_node = clause.named_children(&mut clause_cursor).find(|child| {
            Some(child.id()) != alias_node.map(|alias| alias.id())
                && matches!(child.kind(), "name" | "qualified_name")
        });
        let Some(name_node) = name_node else {
            continue;
        };

        let name = node_text(name_node, source).trim_matches('\\');
        let fqn = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}\\{name}")
        };
        let alias = alias_node.map(|node| node_text(node, source).to_string());
        let kind = use_clause_kind(clause, source).unwrap_or(declaration_kind);

        result.use_statements.push(UseStatement {
            fqn,
            alias,
            kind,
            namespace: current_ns.clone(),
            range: node_range(clause),
        });
    }
}

fn use_clause_kind(clause: Node, source: &str) -> Option<UseKind> {
    let type_node = clause.child_by_field_name("type")?;
    match node_text(type_node, source).to_ascii_lowercase().as_str() {
        "function" => Some(UseKind::Function),
        "const" => Some(UseKind::Constant),
        _ => None,
    }
}

fn determine_use_kind(node: Node, source: &str) -> UseKind {
    // In tree-sitter-php, "use function ..." and "use const ..." have the
    // "function"/"const" keyword as a child of the namespace_use_declaration
    // OR as part of the text. Check full node text for the pattern.
    let text = node_text(node, source);
    if text.starts_with("use function ") || text.starts_with("use function\t") {
        return UseKind::Function;
    }
    if text.starts_with("use const ") || text.starts_with("use const\t") {
        return UseKind::Constant;
    }
    // Also check children in case grammar has explicit keyword nodes
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            let kind = child.kind();
            if kind == "function" {
                return UseKind::Function;
            }
            if kind == "const" {
                return UseKind::Constant;
            }
            if kind == "namespace_use_clause" || kind == "namespace_use_group" {
                break;
            }
        }
    }
    UseKind::Class
}

/// Resolve a class name from a `base_clause` or `class_interface_clause` child
/// using the file's namespace and use statements.
fn resolve_class_name_in_file(name: &str, file_symbols: &FileSymbols) -> String {
    crate::resolve::resolve_class_name_pub(name, file_symbols)
}

/// Extract FQNs from a `base_clause` (extends) child of a class/interface node.
fn extract_base_clause(node: Node, source: &str, file_symbols: &FileSymbols) -> Vec<String> {
    let mut result = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "base_clause" {
            let mut inner_cursor = child.walk();
            for inner in child.children(&mut inner_cursor) {
                match inner.kind() {
                    "name" | "qualified_name" => {
                        let name = node_text(inner, source);
                        result.push(resolve_class_name_in_file(name, file_symbols));
                    }
                    _ => {}
                }
            }
        }
    }
    result
}

/// Extract FQNs from a `class_interface_clause` (implements) child of a class node.
fn extract_interface_clause(node: Node, source: &str, file_symbols: &FileSymbols) -> Vec<String> {
    let mut result = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "class_interface_clause" {
            let mut inner_cursor = child.walk();
            for inner in child.children(&mut inner_cursor) {
                match inner.kind() {
                    "name" | "qualified_name" => {
                        let name = node_text(inner, source);
                        result.push(resolve_class_name_in_file(name, file_symbols));
                    }
                    _ => {}
                }
            }
        }
    }
    result
}

fn class_body_node(node: Node) -> Option<Node> {
    node.child_by_field_name("body").or_else(|| {
        let count = node.child_count();
        for i in 0..count {
            if let Some(child) = node.child(i) {
                let kind = child.kind();
                if kind == "declaration_list"
                    || kind == "enum_declaration_list"
                    || kind == "class_body"
                {
                    return Some(child);
                }
            }
        }
        None
    })
}

/// Extract FQNs from trait `use SomeTrait;` declarations inside a class/trait body.
fn extract_trait_use_clauses(body: Node, source: &str, file_symbols: &FileSymbols) -> Vec<String> {
    let mut result = Vec::new();
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if child.kind() != "use_declaration" {
            continue;
        }

        let mut inner_cursor = child.walk();
        for inner in child.children(&mut inner_cursor) {
            match inner.kind() {
                "name" | "qualified_name" => {
                    let name = node_text(inner, source);
                    result.push(resolve_class_name_in_file(name, file_symbols));
                }
                _ => {}
            }
        }
    }
    result
}

/// Extract a class-like declaration (class, interface, trait, enum).
fn extract_class_like(
    node: Node,
    source: &str,
    uri: &str,
    result: &mut FileSymbols,
    current_ns: &Option<String>,
    kind: PhpSymbolKind,
    php_version: Option<PhpSymbolExtractionVersion>,
) {
    if !node_is_available_for_php_version(node, source, php_version) {
        return;
    }

    let name_node = match node.child_by_field_name("name") {
        Some(n) => n,
        None => return,
    };
    let name = node_text(name_node, source).to_string();
    let fqn = make_fqn(current_ns, &name);
    let start = node.start_position();
    let resolution_symbols = result
        .scoped_at_byte_position(start.row as u32, start.column as u32)
        .into_owned();

    let modifiers = extract_modifiers(node, source);
    let attributes = attribute_groups_for_node(node, source);
    let doc_comment_node = find_doc_comment_node(node, source);
    let doc_comment = doc_comment_node
        .as_ref()
        .map(|doc_node| node_text(*doc_node, source).to_string());
    let templates = phpdoc_templates(doc_comment.as_deref());
    let mut template_bindings =
        phpdoc_template_bindings(doc_comment.as_deref(), &resolution_symbols);
    if let Some(repository_name) =
        doctrine_repository_class_name_from_attribute_text(&attribute_prefix_for_node(node, source))
    {
        let repository_fqn = resolve_class_name_in_file(&repository_name, &resolution_symbols)
            .trim_start_matches('\\')
            .to_string();
        if !repository_fqn.is_empty() {
            template_bindings.push(TemplateBinding {
                kind: TemplateBindingKind::RepositoryClass,
                target: repository_fqn,
                args: vec![],
            });
        }
    }

    // Extract extends (base_clause) and implements (class_interface_clause)
    let extends_fqns = extract_base_clause(node, source, &resolution_symbols);
    let implements_fqns = extract_interface_clause(node, source, &resolution_symbols);
    let body_node = class_body_node(node);
    let trait_fqns = body_node
        .map(|body| extract_trait_use_clauses(body, source, &resolution_symbols))
        .unwrap_or_default();

    let sym = SymbolInfo {
        name: name.clone(),
        fqn: fqn.clone(),
        kind,
        uri: uri.to_string(),
        range: node_range(node),
        selection_range: node_range(name_node),
        visibility: Visibility::Public,
        modifiers,
        attributes,
        doc_comment: doc_comment.clone(),
        signature: None,
        parent_fqn: None,
        extends: extends_fqns,
        implements: implements_fqns,
        traits: trait_fqns,
        templates,
        template_bindings,
    };
    result.symbols.push(sym);

    // Extract members from body (declaration_list)
    if let Some(body) = body_node {
        extract_class_body(body, source, uri, result, &fqn, php_version);
    }

    if kind == PhpSymbolKind::Enum {
        extract_enum_builtin_properties(node, source, uri, result, &fqn, name_node, body_node);
    }

    if let (Some(doc), Some(doc_node)) = (doc_comment.as_deref(), doc_comment_node) {
        extract_phpdoc_virtual_properties(
            doc,
            uri,
            result,
            &fqn,
            node_range(name_node),
            doc_node.start_position(),
        );
        extract_phpdoc_virtual_methods(
            doc,
            uri,
            result,
            &fqn,
            node_range(name_node),
            doc_node.start_position(),
        );
    }
}

fn extract_phpdoc_virtual_properties(
    doc_comment: &str,
    uri: &str,
    result: &mut FileSymbols,
    parent_fqn: &str,
    fallback_range: (u32, u32, u32, u32),
    doc_start: tree_sitter::Point,
) {
    let phpdoc = crate::phpdoc::parse_phpdoc(doc_comment);
    let template_names: HashSet<String> = phpdoc
        .templates
        .iter()
        .map(|template| template.name.clone())
        .collect();

    for property in phpdoc.properties {
        if !property.access.is_readable() {
            continue;
        }
        if result.symbols.iter().any(|symbol| {
            symbol.kind == PhpSymbolKind::Property
                && symbol.parent_fqn.as_deref() == Some(parent_fqn)
                && symbol.name == property.name
        }) {
            continue;
        }

        let property_range = phpdoc_property_name_range(doc_comment, &property.name, doc_start)
            .unwrap_or(fallback_range);
        let type_info = property
            .type_info
            .clone()
            .map(|type_info| resolve_template_type_info_in_file(type_info, result, &template_names))
            .unwrap_or(TypeInfo::Mixed);

        result.symbols.push(SymbolInfo {
            name: property.name.clone(),
            fqn: format!("{}::${}", parent_fqn, property.name),
            kind: PhpSymbolKind::Property,
            uri: uri.to_string(),
            range: property_range,
            selection_range: property_range,
            visibility: Visibility::Public,
            modifiers: SymbolModifiers::default(),
            attributes: vec![],
            doc_comment: Some(doc_comment.to_string()),
            signature: Some(Signature {
                params: vec![],
                return_type: Some(type_info),
            }),
            parent_fqn: Some(parent_fqn.to_string()),
            extends: vec![],
            implements: vec![],
            traits: vec![],
            templates: vec![],
            template_bindings: vec![],
        });
    }
}

fn extract_phpdoc_virtual_methods(
    doc_comment: &str,
    uri: &str,
    result: &mut FileSymbols,
    parent_fqn: &str,
    fallback_range: (u32, u32, u32, u32),
    doc_start: tree_sitter::Point,
) {
    let phpdoc = crate::phpdoc::parse_phpdoc(doc_comment);
    let template_names: HashSet<String> = phpdoc
        .templates
        .iter()
        .map(|template| template.name.clone())
        .collect();

    for method in phpdoc.methods {
        if result.symbols.iter().any(|symbol| {
            symbol.kind == PhpSymbolKind::Method
                && symbol.parent_fqn.as_deref() == Some(parent_fqn)
                && symbol.name.eq_ignore_ascii_case(&method.name)
        }) {
            continue;
        }

        let return_type = method.return_type.map(|type_info| {
            resolve_template_type_info_in_file(type_info, result, &template_names)
        });

        let method_range = phpdoc_method_name_range(doc_comment, &method.name, doc_start)
            .unwrap_or(fallback_range);

        result.symbols.push(SymbolInfo {
            name: method.name.clone(),
            fqn: format!("{}::{}", parent_fqn, method.name),
            kind: PhpSymbolKind::Method,
            uri: uri.to_string(),
            range: method_range,
            selection_range: method_range,
            visibility: Visibility::Public,
            modifiers: SymbolModifiers {
                is_static: method.is_static,
                ..SymbolModifiers::default()
            },
            attributes: vec![],
            doc_comment: Some(doc_comment.to_string()),
            signature: Some(Signature {
                params: method.params,
                return_type,
            }),
            parent_fqn: Some(parent_fqn.to_string()),
            extends: vec![],
            implements: vec![],
            traits: vec![],
            templates: vec![],
            template_bindings: vec![],
        });
    }
}

fn extract_enum_builtin_properties(
    node: Node,
    source: &str,
    uri: &str,
    result: &mut FileSymbols,
    parent_fqn: &str,
    name_node: Node,
    body_node: Option<Node>,
) {
    let fallback_range = node_range(name_node);
    push_enum_builtin_property(
        result,
        uri,
        parent_fqn,
        "name",
        TypeInfo::Simple("string".to_string()),
        fallback_range,
    );

    let Some(body) = body_node else {
        return;
    };
    let header = source
        .get(node.start_byte()..body.start_byte())
        .unwrap_or_default();
    let Some((_, backing_type_text)) = header.rsplit_once(':') else {
        return;
    };
    let backing_type = backing_type_text
        .split_whitespace()
        .next()
        .unwrap_or_default();
    if backing_type.is_empty() {
        return;
    }

    push_enum_builtin_property(
        result,
        uri,
        parent_fqn,
        "value",
        TypeInfo::Simple(backing_type.to_string()),
        fallback_range,
    );
}

fn push_enum_builtin_property(
    result: &mut FileSymbols,
    uri: &str,
    parent_fqn: &str,
    name: &str,
    type_info: TypeInfo,
    fallback_range: (u32, u32, u32, u32),
) {
    if result.symbols.iter().any(|symbol| {
        symbol.kind == PhpSymbolKind::Property
            && symbol.parent_fqn.as_deref() == Some(parent_fqn)
            && symbol.name == name
    }) {
        return;
    }

    result.symbols.push(SymbolInfo {
        name: name.to_string(),
        fqn: format!("{}::${}", parent_fqn, name),
        kind: PhpSymbolKind::Property,
        uri: uri.to_string(),
        range: fallback_range,
        selection_range: fallback_range,
        visibility: Visibility::Public,
        modifiers: SymbolModifiers {
            is_readonly: true,
            ..SymbolModifiers::default()
        },
        attributes: vec![],
        doc_comment: None,
        signature: Some(Signature {
            params: vec![],
            return_type: Some(type_info),
        }),
        parent_fqn: Some(parent_fqn.to_string()),
        extends: vec![],
        implements: vec![],
        traits: vec![],
        templates: vec![],
        template_bindings: vec![],
    });
}

fn phpdoc_method_name_range(
    doc_comment: &str,
    method_name: &str,
    doc_start: tree_sitter::Point,
) -> Option<(u32, u32, u32, u32)> {
    for (line_idx, raw_line) in doc_comment.lines().enumerate() {
        if !raw_line.contains("@method") {
            continue;
        }

        let mut search_from = 0usize;
        while let Some(relative_pos) = raw_line[search_from..].find(method_name) {
            let name_start = search_from + relative_pos;
            let after_name = &raw_line[name_start + method_name.len()..];
            if after_name.trim_start().starts_with('(') {
                let line = doc_start.row as u32 + line_idx as u32;
                let line_base_col = if line_idx == 0 {
                    doc_start.column as u32
                } else {
                    0
                };
                let start_col = line_base_col + name_start as u32;
                let end_col = start_col + method_name.len() as u32;
                return Some((line, start_col, line, end_col));
            }
            search_from = name_start + method_name.len();
        }
    }

    None
}

fn phpdoc_property_name_range(
    doc_comment: &str,
    property_name: &str,
    doc_start: tree_sitter::Point,
) -> Option<(u32, u32, u32, u32)> {
    for (line_idx, raw_line) in doc_comment.lines().enumerate() {
        if !raw_line.contains("@property") {
            continue;
        }

        let needle = format!("${property_name}");
        let Some(name_start) = raw_line.find(&needle) else {
            continue;
        };
        let line = doc_start.row as u32 + line_idx as u32;
        let line_base_col = if line_idx == 0 {
            doc_start.column as u32
        } else {
            0
        };
        let start_col = line_base_col + name_start as u32 + 1;
        let end_col = start_col + property_name.len() as u32;
        return Some((line, start_col, line, end_col));
    }

    None
}

/// Extract members from a class/interface/trait/enum body.
fn extract_class_body(
    body: Node,
    source: &str,
    uri: &str,
    result: &mut FileSymbols,
    parent_fqn: &str,
    php_version: Option<PhpSymbolExtractionVersion>,
) {
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        match child.kind() {
            "method_declaration" => {
                extract_method(child, source, uri, result, parent_fqn, php_version);
            }
            "property_declaration" => {
                extract_properties(child, source, uri, result, parent_fqn, php_version);
            }
            "class_const_declaration" | "const_declaration" => {
                extract_class_constants(child, source, uri, result, parent_fqn, php_version);
            }
            "enum_case" => {
                extract_enum_case(child, source, uri, result, parent_fqn, php_version);
            }
            "use_declaration" => {
                // Trait use — ignore for now (could track trait usage)
            }
            _ => {}
        }
    }
}

fn extract_method(
    node: Node,
    source: &str,
    uri: &str,
    result: &mut FileSymbols,
    parent_fqn: &str,
    php_version: Option<PhpSymbolExtractionVersion>,
) {
    if !node_is_available_for_php_version(node, source, php_version) {
        return;
    }

    let name_node = match node.child_by_field_name("name") {
        Some(n) => n,
        None => return,
    };
    let name = node_text(name_node, source).to_string();
    let fqn = format!("{}::{}", parent_fqn, name);

    let visibility = extract_visibility(node, source);
    let modifiers = extract_modifiers(node, source);
    let attributes = attribute_groups_for_node(node, source);
    let doc_comment = find_doc_comment(node, source);
    let templates = phpdoc_templates(doc_comment.as_deref());
    let mut signature = extract_signature(node, source, php_version);

    // Apply PHPDoc fallbacks: @return type and [optional] params
    if let Some(ref doc) = doc_comment {
        apply_phpdoc_to_signature(&mut signature, doc);
    }

    result.symbols.push(SymbolInfo {
        name,
        fqn,
        kind: PhpSymbolKind::Method,
        uri: uri.to_string(),
        range: node_range(node),
        selection_range: node_range(name_node),
        visibility,
        modifiers,
        attributes,
        doc_comment,
        signature: Some(signature),
        parent_fqn: Some(parent_fqn.to_string()),
        extends: vec![],
        implements: vec![],
        traits: vec![],
        templates,
        template_bindings: vec![],
    });

    // Emit Property symbols for promoted constructor parameters.
    // PHP constructor promotion (`public readonly Type $prop`) creates both a
    // constructor parameter AND a class property. We need the Property symbol
    // so that `$this->prop` can be resolved to its type.
    if let Some(param_list) = node.child_by_field_name("parameters") {
        let mut cursor = param_list.walk();
        for child in param_list.children(&mut cursor) {
            if child.kind() == "property_promotion_parameter" {
                if !node_is_available_for_php_version(child, source, php_version) {
                    continue;
                }
                let prop_vis = extract_visibility(child, source);
                let prop_mods = extract_modifiers(child, source);
                let prop_attributes = attribute_groups_for_node(child, source);
                let prop_type = child
                    .child_by_field_name("type")
                    .map(|t| parse_type_node(t, source));
                if let Some(name_node) = child.child_by_field_name("name") {
                    let raw_name = node_text(name_node, source);
                    let prop_name = raw_name.strip_prefix('$').unwrap_or(raw_name).to_string();
                    let prop_fqn = format!("{}::${}", parent_fqn, prop_name);

                    result.symbols.push(SymbolInfo {
                        name: prop_name,
                        fqn: prop_fqn,
                        kind: PhpSymbolKind::Property,
                        uri: uri.to_string(),
                        range: node_range(child),
                        selection_range: node_range(name_node),
                        visibility: prop_vis,
                        modifiers: prop_mods,
                        attributes: prop_attributes,
                        doc_comment: None,
                        signature: prop_type.map(|t| Signature {
                            params: vec![],
                            return_type: Some(t),
                        }),
                        parent_fqn: Some(parent_fqn.to_string()),
                        extends: vec![],
                        implements: vec![],
                        traits: vec![],
                        templates: vec![],
                        template_bindings: vec![],
                    });
                }
            }
        }
    }
}

fn extract_function(
    node: Node,
    source: &str,
    uri: &str,
    result: &mut FileSymbols,
    current_ns: &Option<String>,
    php_version: Option<PhpSymbolExtractionVersion>,
) {
    if !node_is_available_for_php_version(node, source, php_version) {
        return;
    }

    let name_node = match node.child_by_field_name("name") {
        Some(n) => n,
        None => return,
    };
    let name = node_text(name_node, source).to_string();
    let fqn = make_fqn(current_ns, &name);
    let attributes = attribute_groups_for_node(node, source);
    let doc_comment = find_doc_comment(node, source);
    let templates = phpdoc_templates(doc_comment.as_deref());
    let mut signature = extract_signature(node, source, php_version);

    // Apply PHPDoc fallbacks: @return type and [optional] params
    if let Some(ref doc) = doc_comment {
        apply_phpdoc_to_signature(&mut signature, doc);
    }

    result.symbols.push(SymbolInfo {
        name,
        fqn,
        kind: PhpSymbolKind::Function,
        uri: uri.to_string(),
        range: node_range(node),
        selection_range: node_range(name_node),
        visibility: Visibility::Public,
        modifiers: SymbolModifiers::default(),
        attributes,
        doc_comment,
        signature: Some(signature),
        parent_fqn: None,
        extends: vec![],
        implements: vec![],
        traits: vec![],
        templates,
        template_bindings: vec![],
    });
}

fn extract_properties(
    node: Node,
    source: &str,
    uri: &str,
    result: &mut FileSymbols,
    parent_fqn: &str,
    php_version: Option<PhpSymbolExtractionVersion>,
) {
    if !node_is_available_for_php_version(node, source, php_version) {
        return;
    }

    let visibility = extract_visibility(node, source);
    let modifiers = extract_modifiers(node, source);
    let doc_comment = find_doc_comment(node, source);
    let attribute_text = attribute_prefix_for_node(node, source);
    let attributes = attribute_groups_for_node(node, source);

    // Extract type annotation if present
    let native_type_info = node
        .child_by_field_name("type")
        .map(|t| parse_type_node(t, source))
        .map(|type_info| {
            refine_doctrine_collection_type_from_target_entity(type_info, &attribute_text, result)
        });

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "property_element" {
            if let Some(name_node) = child.child_by_field_name("name") {
                let raw_name = node_text(name_node, source);
                // Remove leading $ from property name
                let name = raw_name.strip_prefix('$').unwrap_or(raw_name).to_string();
                let fqn = format!("{}::${}", parent_fqn, name);
                let type_info = native_type_info.clone().or_else(|| {
                    doc_comment
                        .as_deref()
                        .and_then(|doc| phpdoc_var_type_for_property(doc, raw_name))
                });

                result.symbols.push(SymbolInfo {
                    name,
                    fqn,
                    kind: PhpSymbolKind::Property,
                    uri: uri.to_string(),
                    range: node_range(node),
                    selection_range: node_range(name_node),
                    visibility,
                    modifiers,
                    attributes: attributes.clone(),
                    doc_comment: doc_comment.clone(),
                    signature: type_info.as_ref().map(|t| Signature {
                        params: vec![],
                        return_type: Some(t.clone()),
                    }),
                    parent_fqn: Some(parent_fqn.to_string()),
                    extends: vec![],
                    implements: vec![],
                    traits: vec![],
                    templates: vec![],
                    template_bindings: vec![],
                });
            }
        }
    }
}

fn refine_doctrine_collection_type_from_target_entity(
    type_info: TypeInfo,
    attribute_text: &str,
    file_symbols: &FileSymbols,
) -> TypeInfo {
    let Some(collection_base) = collection_base_type_name(&type_info) else {
        return type_info;
    };
    let Some(target_name) = doctrine_target_entity_class_name_from_attribute_text(attribute_text)
    else {
        return type_info;
    };
    let target_fqn = resolve_class_name_in_file(&target_name, file_symbols)
        .trim_start_matches('\\')
        .to_string();
    if target_fqn.is_empty() {
        return type_info;
    }

    TypeInfo::Generic {
        base: collection_base,
        args: vec![
            TypeInfo::Simple("int".to_string()),
            TypeInfo::Simple(target_fqn),
        ],
    }
}

fn attribute_prefix_for_node(node: Node, source: &str) -> String {
    attribute_groups_for_node(node, source)
        .into_iter()
        .map(|attribute| attribute.text)
        .collect::<Vec<_>>()
        .join(" ")
}

fn attribute_groups_for_node(node: Node, source: &str) -> Vec<SymbolAttribute> {
    let mut groups = immediate_attribute_groups(source, node.start_byte());
    groups.extend(leading_attribute_groups_for_node(node, source));
    groups.sort_by_key(|attribute| {
        (
            attribute.range.0,
            attribute.range.1,
            attribute.range.2,
            attribute.range.3,
            attribute.text.clone(),
        )
    });
    groups.dedup_by(|left, right| left.range == right.range && left.text == right.text);
    groups
}

fn immediate_attribute_groups(source: &str, start_byte: usize) -> Vec<SymbolAttribute> {
    let mut end = start_byte.min(source.len());
    let mut ranges = Vec::new();

    loop {
        end = trim_end_ascii_whitespace(source, end);
        if end == 0 || !source[..end].ends_with(']') {
            break;
        }

        let Some(start) = find_attribute_group_start(source, end) else {
            break;
        };
        ranges.push((start, end));
        end = start;
    }

    ranges.reverse();
    ranges
        .into_iter()
        .map(|(start, end)| symbol_attribute_from_byte_range(source, start, end))
        .collect()
}

fn leading_attribute_groups_for_node(node: Node, source: &str) -> Vec<SymbolAttribute> {
    let text = node_text(node, source);
    let mut offset = 0usize;
    let mut groups = Vec::new();

    loop {
        while text
            .as_bytes()
            .get(offset)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            offset += 1;
        }
        if !text[offset..].starts_with("#[") {
            break;
        }

        let Some(end_relative) = matching_attribute_group_end(&text[offset..]) else {
            break;
        };
        let start = node.start_byte() + offset;
        let end = start + end_relative;
        groups.push(symbol_attribute_from_byte_range(source, start, end));
        offset += end_relative;
    }

    groups
}

fn symbol_attribute_from_byte_range(source: &str, start: usize, end: usize) -> SymbolAttribute {
    let (start_line, start_col) = byte_offset_to_point(source, start);
    let (end_line, end_col) = byte_offset_to_point(source, end);
    SymbolAttribute {
        text: source[start..end].to_string(),
        range: (start_line, start_col, end_line, end_col),
    }
}

fn byte_offset_to_point(source: &str, offset: usize) -> (u32, u32) {
    let mut line = 0u32;
    let mut line_start = 0usize;
    let offset = offset.min(source.len());

    for (idx, byte) in source.as_bytes().iter().take(offset).enumerate() {
        if *byte == b'\n' {
            line += 1;
            line_start = idx + 1;
        }
    }

    (line, (offset - line_start) as u32)
}

fn collection_base_type_name(type_info: &TypeInfo) -> Option<String> {
    match type_info {
        TypeInfo::Simple(name) if is_collection_type_name(name) => Some(name.clone()),
        TypeInfo::Nullable(inner) => collection_base_type_name(inner),
        _ => None,
    }
}

fn is_collection_type_name(name: &str) -> bool {
    let lower = name.trim_start_matches('\\').to_ascii_lowercase();
    lower == "collection"
        || lower.ends_with("\\collection")
        || lower == "doctrine\\common\\collections\\collection"
}

fn doctrine_target_entity_class_name_from_attribute_text(text: &str) -> Option<String> {
    let marker_start = text.rfind("targetEntity")?;
    doctrine_class_name_argument_from_attribute_text(&text[marker_start..], "targetEntity")
}

fn doctrine_repository_class_name_from_attribute_text(text: &str) -> Option<String> {
    let marker_start = text.rfind("repositoryClass")?;
    doctrine_class_name_argument_from_attribute_text(&text[marker_start..], "repositoryClass")
}

fn doctrine_class_name_argument_from_attribute_text(text: &str, argument: &str) -> Option<String> {
    let marker_start = text.find(argument)?;
    let after_marker = &text[marker_start + argument.len()..];
    let separator = after_marker
        .char_indices()
        .find_map(|(idx, ch)| matches!(ch, ':' | '=').then_some(idx))?;
    let after_separator = after_marker[separator + 1..].trim_start();
    let mut end = 0usize;
    for (idx, ch) in after_separator.char_indices() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '\\') {
            end = idx + ch.len_utf8();
        } else {
            break;
        }
    }

    let class_name = after_separator[..end].trim();
    if class_name.is_empty() || !after_separator[end..].trim_start().starts_with("::class") {
        return None;
    }

    Some(class_name.to_string())
}

fn extract_class_constants(
    node: Node,
    source: &str,
    uri: &str,
    result: &mut FileSymbols,
    parent_fqn: &str,
    php_version: Option<PhpSymbolExtractionVersion>,
) {
    if !node_is_available_for_php_version(node, source, php_version) {
        return;
    }

    let visibility = extract_visibility(node, source);
    let modifiers = extract_modifiers(node, source);
    let doc_comment = find_doc_comment(node, source);
    let attributes = attribute_groups_for_node(node, source);

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "const_element" {
            // Find name: first "name" child (may not be a field)
            let name_node = child.child_by_field_name("name").or_else(|| {
                // Fallback: find first child with kind "name"
                (0..child.child_count())
                    .filter_map(|i| child.child(i))
                    .find(|c| c.kind() == "name")
            });
            if let Some(name_node) = name_node {
                let name = node_text(name_node, source).to_string();
                let fqn = format!("{}::{}", parent_fqn, name);

                result.symbols.push(SymbolInfo {
                    name,
                    fqn,
                    kind: PhpSymbolKind::ClassConstant,
                    uri: uri.to_string(),
                    range: node_range(node),
                    selection_range: node_range(name_node),
                    visibility,
                    modifiers,
                    attributes: attributes.clone(),
                    doc_comment: doc_comment.clone(),
                    signature: None,
                    parent_fqn: Some(parent_fqn.to_string()),
                    extends: vec![],
                    implements: vec![],
                    traits: vec![],
                    templates: vec![],
                    template_bindings: vec![],
                });
            }
        }
    }
}

fn extract_global_constants(
    node: Node,
    source: &str,
    uri: &str,
    result: &mut FileSymbols,
    current_ns: &Option<String>,
    php_version: Option<PhpSymbolExtractionVersion>,
) {
    if !node_is_available_for_php_version(node, source, php_version) {
        return;
    }

    let doc_comment = find_doc_comment(node, source);
    let attributes = attribute_groups_for_node(node, source);

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "const_element" {
            let name_node = child.child_by_field_name("name").or_else(|| {
                (0..child.child_count())
                    .filter_map(|i| child.child(i))
                    .find(|c| c.kind() == "name")
            });
            if let Some(name_node) = name_node {
                let name = node_text(name_node, source).to_string();
                let fqn = make_fqn(current_ns, &name);

                result.symbols.push(SymbolInfo {
                    name,
                    fqn,
                    kind: PhpSymbolKind::GlobalConstant,
                    uri: uri.to_string(),
                    range: node_range(node),
                    selection_range: node_range(name_node),
                    visibility: Visibility::Public,
                    modifiers: SymbolModifiers::default(),
                    attributes: attributes.clone(),
                    doc_comment: doc_comment.clone(),
                    signature: None,
                    parent_fqn: None,
                    extends: vec![],
                    implements: vec![],
                    traits: vec![],
                    templates: vec![],
                    template_bindings: vec![],
                });
            }
        }
    }
}

fn extract_enum_case(
    node: Node,
    source: &str,
    uri: &str,
    result: &mut FileSymbols,
    parent_fqn: &str,
    php_version: Option<PhpSymbolExtractionVersion>,
) {
    if !node_is_available_for_php_version(node, source, php_version) {
        return;
    }

    let name_node = match node.child_by_field_name("name") {
        Some(n) => n,
        None => return,
    };
    let name = node_text(name_node, source).to_string();
    let fqn = format!("{}::{}", parent_fqn, name);
    let doc_comment = find_doc_comment(node, source);
    let attributes = attribute_groups_for_node(node, source);

    result.symbols.push(SymbolInfo {
        name,
        fqn,
        kind: PhpSymbolKind::EnumCase,
        uri: uri.to_string(),
        range: node_range(node),
        selection_range: node_range(name_node),
        visibility: Visibility::Public,
        modifiers: SymbolModifiers::default(),
        attributes,
        doc_comment,
        signature: None,
        parent_fqn: Some(parent_fqn.to_string()),
        extends: vec![],
        implements: vec![],
        traits: vec![],
        templates: vec![],
        template_bindings: vec![],
    });
}

/// Apply PHPDoc information to a signature:
/// - Use `@return` as fallback when PHP return type is absent.
/// - Mark params as optional (set synthetic default) when PHPDoc description
///   contains `[optional]`, which is the convention used by phpstorm-stubs for
///   parameters that are optional in PHP but have no explicit default value.
fn apply_phpdoc_to_signature(signature: &mut Signature, doc_comment: &str) {
    let phpdoc = crate::phpdoc::parse_phpdoc(doc_comment);

    // Fallback: @return type
    if signature.return_type.is_none() {
        if let Some(ret) = phpdoc.return_type {
            signature.return_type = Some(ret);
        }
    }

    // Mark [optional] params with a synthetic default value
    for phpdoc_param in &phpdoc.params {
        if let Some(sig_param) = signature
            .params
            .iter_mut()
            .find(|p| p.name == phpdoc_param.name)
        {
            if let Some(type_info) = phpdoc_param.type_info.as_ref() {
                sig_param.type_info = Some(type_info.clone());
            }
        }

        let is_optional = phpdoc_param
            .description
            .as_deref()
            .is_some_and(|d| d.contains("[optional]"));
        if is_optional {
            if let Some(sig_param) = signature
                .params
                .iter_mut()
                .find(|p| p.name == phpdoc_param.name)
            {
                if sig_param.default_value.is_none() {
                    sig_param.default_value = Some("null".to_string());
                }
            }
        }
    }
}

fn phpdoc_templates(doc_comment: Option<&str>) -> Vec<TemplateParam> {
    doc_comment
        .map(crate::phpdoc::parse_phpdoc)
        .map(|doc| doc.templates)
        .unwrap_or_default()
}

fn phpdoc_template_bindings(
    doc_comment: Option<&str>,
    file_symbols: &FileSymbols,
) -> Vec<TemplateBinding> {
    doc_comment
        .map(crate::phpdoc::parse_phpdoc)
        .map(|doc| {
            let template_names: std::collections::HashSet<String> = doc
                .templates
                .into_iter()
                .map(|template| template.name)
                .collect();
            doc.template_bindings
                .into_iter()
                .map(|mut binding| {
                    binding.target = resolve_class_name_in_file(&binding.target, file_symbols);
                    binding.args = binding
                        .args
                        .into_iter()
                        .map(|arg| {
                            resolve_template_type_info_in_file(arg, file_symbols, &template_names)
                        })
                        .collect();
                    binding
                })
                .collect()
        })
        .unwrap_or_default()
}

fn resolve_template_type_info_in_file(
    type_info: TypeInfo,
    file_symbols: &FileSymbols,
    template_names: &std::collections::HashSet<String>,
) -> TypeInfo {
    match type_info {
        TypeInfo::Simple(name)
            if template_names.contains(&name) || is_phpdoc_builtin_type(&name) =>
        {
            TypeInfo::Simple(name)
        }
        TypeInfo::Simple(name) => TypeInfo::Simple(resolve_class_name_in_file(&name, file_symbols)),
        TypeInfo::Generic { base, args } => {
            let base = if template_names.contains(&base) || is_phpdoc_builtin_type(&base) {
                base
            } else {
                resolve_class_name_in_file(&base, file_symbols)
            };
            TypeInfo::Generic {
                base,
                args: args
                    .into_iter()
                    .map(|arg| {
                        resolve_template_type_info_in_file(arg, file_symbols, template_names)
                    })
                    .collect(),
            }
        }
        TypeInfo::ArrayShape(items) => TypeInfo::ArrayShape(resolve_shape_items_in_file(
            items,
            file_symbols,
            template_names,
        )),
        TypeInfo::ObjectShape(items) => TypeInfo::ObjectShape(resolve_shape_items_in_file(
            items,
            file_symbols,
            template_names,
        )),
        TypeInfo::Callable {
            params,
            return_type,
        } => TypeInfo::Callable {
            params: params
                .into_iter()
                .map(|param| {
                    resolve_template_type_info_in_file(param, file_symbols, template_names)
                })
                .collect(),
            return_type: return_type.map(|return_type| {
                Box::new(resolve_template_type_info_in_file(
                    *return_type,
                    file_symbols,
                    template_names,
                ))
            }),
        },
        TypeInfo::ClassString(Some(inner)) => TypeInfo::ClassString(Some(Box::new(
            resolve_template_type_info_in_file(*inner, file_symbols, template_names),
        ))),
        TypeInfo::Conditional {
            subject,
            target,
            if_type,
            else_type,
        } => TypeInfo::Conditional {
            subject,
            target: Box::new(resolve_template_type_info_in_file(
                *target,
                file_symbols,
                template_names,
            )),
            if_type: Box::new(resolve_template_type_info_in_file(
                *if_type,
                file_symbols,
                template_names,
            )),
            else_type: Box::new(resolve_template_type_info_in_file(
                *else_type,
                file_symbols,
                template_names,
            )),
        },
        TypeInfo::Union(types) => TypeInfo::Union(
            types
                .into_iter()
                .map(|type_info| {
                    resolve_template_type_info_in_file(type_info, file_symbols, template_names)
                })
                .collect(),
        ),
        TypeInfo::Intersection(types) => TypeInfo::Intersection(
            types
                .into_iter()
                .map(|type_info| {
                    resolve_template_type_info_in_file(type_info, file_symbols, template_names)
                })
                .collect(),
        ),
        TypeInfo::Nullable(inner) => TypeInfo::Nullable(Box::new(
            resolve_template_type_info_in_file(*inner, file_symbols, template_names),
        )),
        TypeInfo::ClassString(None)
        | TypeInfo::LiteralString(_)
        | TypeInfo::LiteralInt(_)
        | TypeInfo::LiteralFloat(_)
        | TypeInfo::LiteralBool(_)
        | TypeInfo::LiteralNull
        | TypeInfo::Void
        | TypeInfo::Never
        | TypeInfo::Mixed
        | TypeInfo::Self_
        | TypeInfo::Static_
        | TypeInfo::Parent_ => type_info,
    }
}

fn resolve_shape_items_in_file(
    items: Vec<ArrayShapeItem>,
    file_symbols: &FileSymbols,
    template_names: &HashSet<String>,
) -> Vec<ArrayShapeItem> {
    items
        .into_iter()
        .map(|item| ArrayShapeItem {
            key: item.key,
            optional: item.optional,
            value: resolve_template_type_info_in_file(item.value, file_symbols, template_names),
        })
        .collect()
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

fn phpdoc_var_type_for_property(doc_comment: &str, raw_property_name: &str) -> Option<TypeInfo> {
    let phpdoc = crate::phpdoc::parse_phpdoc(doc_comment);
    let var_type = phpdoc.var_type?;
    let tagged_var = phpdoc_tagged_var_name(doc_comment);

    if let Some(tagged_var) = tagged_var {
        if tagged_var != raw_property_name {
            return None;
        }
    }

    Some(var_type)
}

fn phpdoc_tagged_var_name(doc_comment: &str) -> Option<String> {
    for raw_line in doc_comment.lines() {
        let mut line = raw_line.trim();
        if let Some(rest) = line.strip_prefix("/**") {
            line = rest.trim_start();
        }
        if let Some(rest) = line.strip_prefix('*') {
            line = rest.trim_start();
        }
        if line.starts_with("*/") || line.is_empty() {
            continue;
        }
        if let Some(rest) = crate::phpdoc::strip_exact_tag(line, "@var") {
            for token in rest.split_whitespace() {
                if let Some(name) = phpdoc_var_token(token) {
                    return Some(name);
                }
            }
        }
    }
    None
}

fn phpdoc_var_token(token: &str) -> Option<String> {
    let trimmed = token.trim_matches(|c: char| c == ',' || c == ';' || c == ')' || c == '(');
    if !trimmed.starts_with('$') {
        return None;
    }

    let ident: String = trimmed
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '$')
        .collect();
    if ident.len() > 1 {
        Some(ident)
    } else {
        None
    }
}

/// Extract function/method signature (parameters + return type).
fn extract_signature(
    node: Node,
    source: &str,
    php_version: Option<PhpSymbolExtractionVersion>,
) -> Signature {
    let mut params = Vec::new();

    if let Some(param_list) = node.child_by_field_name("parameters") {
        let mut cursor = param_list.walk();
        for child in param_list.children(&mut cursor) {
            if child.kind() == "simple_parameter"
                || child.kind() == "variadic_parameter"
                || child.kind() == "property_promotion_parameter"
            {
                if !node_is_available_for_php_version(child, source, php_version) {
                    continue;
                }
                let param = extract_param(child, source);
                params.push(param);
            }
        }
    }

    params = normalize_signature_params(params);

    let return_type = node
        .child_by_field_name("return_type")
        .map(|t| parse_type_node(t, source));

    Signature {
        params,
        return_type,
    }
}

/// Normalize signature parameters to handle version-gated stub overloads.
///
/// phpstorm-stubs may contain duplicated parameter names in one declaration
/// with attributes like PhpStormStubsElementAvailable. We collapse duplicates
/// by name and merge flags, preferring a richer/variadic variant.
fn normalize_signature_params(params: Vec<ParamInfo>) -> Vec<ParamInfo> {
    let mut out: Vec<ParamInfo> = Vec::new();
    let mut index_by_name: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    for p in params {
        if let Some(&idx) = index_by_name.get(&p.name) {
            let existing = &mut out[idx];

            // Prefer concrete type info when existing param is untyped.
            if existing.type_info.is_none() && p.type_info.is_some() {
                existing.type_info = p.type_info.clone();
            }
            // Keep any explicit default value.
            if existing.default_value.is_none() && p.default_value.is_some() {
                existing.default_value = p.default_value.clone();
            }
            // Merge flags; variadic=true is critical for arity checks.
            existing.is_variadic = existing.is_variadic || p.is_variadic;
            existing.is_by_ref = existing.is_by_ref || p.is_by_ref;
            existing.is_promoted = existing.is_promoted || p.is_promoted;
        } else {
            let idx = out.len();
            index_by_name.insert(p.name.clone(), idx);
            out.push(p);
        }
    }

    out
}

fn extract_param(node: Node, source: &str) -> ParamInfo {
    let name_node = node.child_by_field_name("name");
    let raw_name = name_node
        .map(|n| node_text(n, source))
        .unwrap_or("$unknown");
    let name = raw_name.strip_prefix('$').unwrap_or(raw_name).to_string();

    let type_info = node
        .child_by_field_name("type")
        .map(|t| parse_type_node(t, source));

    let default_value = node
        .child_by_field_name("default_value")
        .map(|n| node_text(n, source).to_string());

    let is_variadic = node.kind() == "variadic_parameter";
    let is_by_ref = has_child_kind(node, "reference_modifier");
    let is_promoted = node.kind() == "property_promotion_parameter";

    ParamInfo {
        name,
        type_info,
        default_value,
        is_variadic,
        is_by_ref,
        is_promoted,
    }
}

/// Parse a type node into TypeInfo.
fn parse_type_node(node: Node, source: &str) -> TypeInfo {
    match node.kind() {
        "union_type" => {
            let mut types = Vec::new();
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() != "|" {
                    types.push(parse_type_node(child, source));
                }
            }
            TypeInfo::Union(types)
        }
        "intersection_type" => {
            let mut types = Vec::new();
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() != "&" {
                    types.push(parse_type_node(child, source));
                }
            }
            TypeInfo::Intersection(types)
        }
        "optional_type" => {
            if let Some(inner) = node.named_child(0) {
                TypeInfo::Nullable(Box::new(parse_type_node(inner, source)))
            } else {
                TypeInfo::Mixed
            }
        }
        _ => {
            let text = node_text(node, source);
            match text.to_lowercase().as_str() {
                "void" => TypeInfo::Void,
                "never" => TypeInfo::Never,
                "mixed" => TypeInfo::Mixed,
                "self" => TypeInfo::Self_,
                "static" => TypeInfo::Static_,
                "parent" => TypeInfo::Parent_,
                _ => TypeInfo::Simple(text.to_string()),
            }
        }
    }
}

// --- Helper functions ---

fn node_text<'a>(node: Node, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

fn node_range(node: Node) -> (u32, u32, u32, u32) {
    let start = node.start_position();
    let end = node.end_position();
    (
        start.row as u32,
        start.column as u32,
        end.row as u32,
        end.column as u32,
    )
}

fn node_is_available_for_php_version(
    node: Node,
    source: &str,
    php_version: Option<PhpSymbolExtractionVersion>,
) -> bool {
    let Some(php_version) = php_version else {
        return true;
    };

    let node_text = leading_attribute_prefix_from_text(node_text(node, source));
    if let Some(constraint) = availability_constraint_from_text(&node_text) {
        return constraint.matches(php_version);
    }

    let prefix = immediate_attribute_prefix(source, node.start_byte());
    availability_constraint_from_text(&prefix)
        .map(|constraint| constraint.matches(php_version))
        .unwrap_or(true)
}

#[derive(Debug, Clone, Copy, Default)]
struct AvailabilityConstraint {
    from: Option<PhpSymbolExtractionVersion>,
    to: Option<PhpSymbolExtractionVersion>,
}

impl AvailabilityConstraint {
    fn matches(self, php_version: PhpSymbolExtractionVersion) -> bool {
        self.from.is_none_or(|from| php_version >= from)
            && self.to.is_none_or(|to| php_version <= to)
    }
}

fn availability_constraint_from_text(text: &str) -> Option<AvailabilityConstraint> {
    let marker = "PhpStormStubsElementAvailable";
    let marker_start = text.find(marker)?;
    let attr = text.get(marker_start..attribute_text_end(text, marker_start))?;

    let from = named_version_argument(attr, "from");
    let to = named_version_argument(attr, "to");
    let from = from.or_else(|| {
        if attr.contains("from:") || attr.contains("to:") {
            None
        } else {
            first_quoted_version(attr)
        }
    });

    Some(AvailabilityConstraint { from, to })
}

fn attribute_text_end(text: &str, marker_start: usize) -> usize {
    text[marker_start..]
        .find(']')
        .map(|offset| marker_start + offset + 1)
        .unwrap_or(text.len())
}

fn named_version_argument(attr: &str, name: &str) -> Option<PhpSymbolExtractionVersion> {
    let needle = format!("{}:", name);
    let start = attr.find(&needle)?;
    first_quoted_version(attr.get(start + needle.len()..)?)
}

fn first_quoted_version(text: &str) -> Option<PhpSymbolExtractionVersion> {
    for quote in ['\'', '"'] {
        let Some(start) = text.find(quote) else {
            continue;
        };
        let rest = text.get(start + quote.len_utf8()..)?;
        let Some(end) = rest.find(quote) else {
            continue;
        };
        if let Some(version) = parse_php_version_literal(&rest[..end]) {
            return Some(version);
        }
    }
    None
}

fn parse_php_version_literal(raw: &str) -> Option<PhpSymbolExtractionVersion> {
    let version = raw.trim();
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    Some(PhpSymbolExtractionVersion { major, minor })
}

fn immediate_attribute_prefix(source: &str, start_byte: usize) -> String {
    let mut end = start_byte.min(source.len());
    let mut result = String::new();

    loop {
        end = trim_end_ascii_whitespace(source, end);
        if end == 0 || !source[..end].ends_with(']') {
            break;
        }

        let Some(start) = find_attribute_group_start(source, end) else {
            break;
        };
        if !result.is_empty() {
            result.insert(0, ' ');
        }
        result.insert_str(0, &source[start..end]);
        end = start;
    }

    result
}

fn leading_attribute_prefix_from_text(text: &str) -> String {
    let mut remaining = text.trim_start();
    let mut result = String::new();

    while remaining.starts_with("#[") {
        let Some(end) = matching_attribute_group_end(remaining) else {
            break;
        };
        if !result.is_empty() {
            result.push(' ');
        }
        result.push_str(&remaining[..end]);
        remaining = remaining[end..].trim_start();
    }

    result
}

fn matching_attribute_group_end(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut quote = None;
    let mut escape = false;

    for (idx, ch) in text.char_indices() {
        if let Some(active_quote) = quote {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == active_quote {
                quote = None;
            }
            continue;
        }

        match ch {
            '\'' | '"' if depth > 0 => quote = Some(ch),
            '[' => depth = depth.saturating_add(1),
            ']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(idx + ch.len_utf8());
                }
            }
            _ => {}
        }
    }
    None
}

fn trim_end_ascii_whitespace(source: &str, mut end: usize) -> usize {
    while end > 0 && source.as_bytes()[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    end
}

fn find_attribute_group_start(source: &str, end: usize) -> Option<usize> {
    let end = end.min(source.len());
    let mut search_end = end;

    while let Some(start) = source[..search_end].rfind("#[") {
        if matching_attribute_group_end(&source[start..end]) == Some(end - start) {
            return Some(start);
        }

        if start == 0 {
            break;
        }
        search_end = start;
    }

    None
}

fn make_fqn(namespace: &Option<String>, name: &str) -> String {
    match namespace {
        Some(ns) if !ns.is_empty() => format!("{}\\{}", ns, name),
        _ => name.to_string(),
    }
}

fn extract_visibility(node: Node, source: &str) -> Visibility {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            return match node_text(child, source) {
                "public" => Visibility::Public,
                "protected" => Visibility::Protected,
                "private" => Visibility::Private,
                _ => Visibility::Public,
            };
        }
    }
    Visibility::Public
}

fn extract_modifiers(node: Node, _source: &str) -> SymbolModifiers {
    let mut mods = SymbolModifiers::default();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "static_modifier" => mods.is_static = true,
            "abstract_modifier" => mods.is_abstract = true,
            "final_modifier" => mods.is_final = true,
            "readonly_modifier" => mods.is_readonly = true,
            _ => {}
        }
    }
    mods
}

fn has_child_kind(node: Node, kind: &str) -> bool {
    let mut cursor = node.walk();
    let result = node.children(&mut cursor).any(|c| c.kind() == kind);
    result
}

/// Find the doc comment (PHPDoc) immediately preceding a node.
fn find_doc_comment(node: Node, source: &str) -> Option<String> {
    find_doc_comment_node(node, source).map(|comment| node_text(comment, source).to_string())
}

fn find_doc_comment_node<'a>(node: Node<'a>, source: &str) -> Option<Node<'a>> {
    // Look for a comment node as a previous sibling
    let mut prev = node.prev_sibling();
    while let Some(p) = prev {
        if p.kind() == "comment" {
            let text = node_text(p, source);
            if text.starts_with("/**") {
                return Some(p);
            }
            // Non-PHPDoc comment — stop looking
            return None;
        }
        // Skip only structural/unnamed trivia between comment and declaration.
        // A named sibling means another declaration or statement owns any
        // earlier PHPDoc and this node is undocumented.
        if p.is_named() {
            return None;
        }
        prev = p.prev_sibling();
    }
    None
}

#[cfg(test)]
#[path = "symbols_tests.rs"]
mod tests;
