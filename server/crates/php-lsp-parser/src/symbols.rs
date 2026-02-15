//! Extract PHP symbols from tree-sitter CST.
//!
//! Walks the CST and produces `FileSymbols` containing all classes, interfaces,
//! traits, enums, functions, methods, properties, constants, namespace and use statements.

use php_lsp_types::*;
use tree_sitter::{Node, Tree};

/// Extract all symbols from a parsed PHP file.
pub fn extract_file_symbols(tree: &Tree, source: &str, uri: &str) -> FileSymbols {
    let mut result = FileSymbols::default();
    let root = tree.root_node();

    // Walk top-level children of program node.
    // Handle namespace-without-braces by tracking current namespace.
    let mut current_ns: Option<String> = None;
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        match child.kind() {
            "namespace_definition" => {
                // Extract namespace name from the namespace_name child
                let ns_name = find_namespace_name(child, source);
                if let Some(ns) = &ns_name {
                    result.namespace = Some(ns.clone());
                }
                current_ns = ns_name.clone();

                // If namespace has braces, recurse into body
                if let Some(body) = child.child_by_field_name("body") {
                    extract_children(body, source, uri, &mut result, &ns_name);
                }
                // If no body — namespace applies to rest of file (current_ns is set)
            }
            _ => {
                extract_from_node(child, source, uri, &mut result, &current_ns);
            }
        }
    }

    result
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
) {
    match node.kind() {
        "namespace_use_declaration" => {
            extract_use_statements(node, source, result);
        }
        "class_declaration" => {
            extract_class_like(node, source, uri, result, current_ns, PhpSymbolKind::Class);
        }
        "interface_declaration" => {
            extract_class_like(
                node,
                source,
                uri,
                result,
                current_ns,
                PhpSymbolKind::Interface,
            );
        }
        "trait_declaration" => {
            extract_class_like(node, source, uri, result, current_ns, PhpSymbolKind::Trait);
        }
        "enum_declaration" => {
            extract_class_like(node, source, uri, result, current_ns, PhpSymbolKind::Enum);
        }
        "function_definition" => {
            extract_function(node, source, uri, result, current_ns);
        }
        "const_declaration" => {
            extract_global_constants(node, source, uri, result, current_ns);
        }
        _ => {
            // Recurse into children
            extract_children(node, source, uri, result, current_ns);
        }
    }
}

fn extract_children(
    node: Node,
    source: &str,
    uri: &str,
    result: &mut FileSymbols,
    current_ns: &Option<String>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        extract_from_node(child, source, uri, result, current_ns);
    }
}

/// Extract use statements from a `namespace_use_declaration`.
fn extract_use_statements(node: Node, source: &str, result: &mut FileSymbols) {
    // Determine use kind (function/const/normal)
    let kind = determine_use_kind(node, source);

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "namespace_use_clause" {
            extract_single_use_clause(child, source, result, kind);
        } else if child.kind() == "namespace_use_group" {
            extract_use_group(child, node, source, result, kind);
        }
    }
}

/// Extract a single use clause. The CST structure is:
/// namespace_use_clause -> qualified_name, [as, name(alias)]
fn extract_single_use_clause(clause: Node, source: &str, result: &mut FileSymbols, kind: UseKind) {
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
            range,
        });
    }
}

fn extract_use_group(
    group: Node,
    parent: Node,
    source: &str,
    result: &mut FileSymbols,
    kind: UseKind,
) {
    // Get prefix from parent
    let prefix = parent
        .child_by_field_name("prefix")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    let mut cursor = group.walk();
    for child in group.children(&mut cursor) {
        if child.kind() == "namespace_use_clause" {
            if let Some(name_node) = child.child_by_field_name("name") {
                let name = node_text(name_node, source);
                let fqn = if prefix.is_empty() {
                    name.to_string()
                } else {
                    format!("{}\\{}", prefix, name)
                };
                let alias = child
                    .child_by_field_name("alias")
                    .map(|n| node_text(n, source).to_string());

                let sp = child.start_position();
                let ep = child.end_position();
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
                    range,
                });
            }
        }
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

/// Extract a class-like declaration (class, interface, trait, enum).
fn extract_class_like(
    node: Node,
    source: &str,
    uri: &str,
    result: &mut FileSymbols,
    current_ns: &Option<String>,
    kind: PhpSymbolKind,
) {
    let name_node = match node.child_by_field_name("name") {
        Some(n) => n,
        None => return,
    };
    let name = node_text(name_node, source).to_string();
    let fqn = make_fqn(current_ns, &name);

    let modifiers = extract_modifiers(node, source);
    let doc_comment = find_doc_comment(node, source);

    let sym = SymbolInfo {
        name: name.clone(),
        fqn: fqn.clone(),
        kind,
        uri: uri.to_string(),
        range: node_range(node),
        selection_range: node_range(name_node),
        visibility: Visibility::Public,
        modifiers,
        doc_comment,
        signature: None,
        parent_fqn: None,
    };
    result.symbols.push(sym);

    // Extract members from body (declaration_list)
    let body_node = node.child_by_field_name("body");
    if let Some(body) = body_node {
        extract_class_body(body, source, uri, result, &fqn);
    } else {
        // Fallback: iterate children by index
        let count = node.child_count();
        for i in 0..count {
            if let Some(child) = node.child(i) {
                let kind = child.kind();
                if kind == "declaration_list"
                    || kind == "enum_declaration_list"
                    || kind == "class_body"
                {
                    extract_class_body(child, source, uri, result, &fqn);
                    break;
                }
            }
        }
    }
}

/// Extract members from a class/interface/trait/enum body.
fn extract_class_body(
    body: Node,
    source: &str,
    uri: &str,
    result: &mut FileSymbols,
    parent_fqn: &str,
) {
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        match child.kind() {
            "method_declaration" => {
                extract_method(child, source, uri, result, parent_fqn);
            }
            "property_declaration" => {
                extract_properties(child, source, uri, result, parent_fqn);
            }
            "class_const_declaration" | "const_declaration" => {
                extract_class_constants(child, source, uri, result, parent_fqn);
            }
            "enum_case" => {
                extract_enum_case(child, source, uri, result, parent_fqn);
            }
            "use_declaration" => {
                // Trait use — ignore for now (could track trait usage)
            }
            _ => {}
        }
    }
}

fn extract_method(node: Node, source: &str, uri: &str, result: &mut FileSymbols, parent_fqn: &str) {
    let name_node = match node.child_by_field_name("name") {
        Some(n) => n,
        None => return,
    };
    let name = node_text(name_node, source).to_string();
    let fqn = format!("{}::{}", parent_fqn, name);

    let visibility = extract_visibility(node, source);
    let modifiers = extract_modifiers(node, source);
    let doc_comment = find_doc_comment(node, source);
    let signature = extract_signature(node, source);

    result.symbols.push(SymbolInfo {
        name,
        fqn,
        kind: PhpSymbolKind::Method,
        uri: uri.to_string(),
        range: node_range(node),
        selection_range: node_range(name_node),
        visibility,
        modifiers,
        doc_comment,
        signature: Some(signature),
        parent_fqn: Some(parent_fqn.to_string()),
    });
}

fn extract_function(
    node: Node,
    source: &str,
    uri: &str,
    result: &mut FileSymbols,
    current_ns: &Option<String>,
) {
    let name_node = match node.child_by_field_name("name") {
        Some(n) => n,
        None => return,
    };
    let name = node_text(name_node, source).to_string();
    let fqn = make_fqn(current_ns, &name);
    let doc_comment = find_doc_comment(node, source);
    let signature = extract_signature(node, source);

    result.symbols.push(SymbolInfo {
        name,
        fqn,
        kind: PhpSymbolKind::Function,
        uri: uri.to_string(),
        range: node_range(node),
        selection_range: node_range(name_node),
        visibility: Visibility::Public,
        modifiers: SymbolModifiers::default(),
        doc_comment,
        signature: Some(signature),
        parent_fqn: None,
    });
}

fn extract_properties(
    node: Node,
    source: &str,
    uri: &str,
    result: &mut FileSymbols,
    parent_fqn: &str,
) {
    let visibility = extract_visibility(node, source);
    let modifiers = extract_modifiers(node, source);
    let doc_comment = find_doc_comment(node, source);

    // Extract type annotation if present
    let type_info = node
        .child_by_field_name("type")
        .map(|t| parse_type_node(t, source));

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "property_element" {
            if let Some(name_node) = child.child_by_field_name("name") {
                let raw_name = node_text(name_node, source);
                // Remove leading $ from property name
                let name = raw_name.strip_prefix('$').unwrap_or(raw_name).to_string();
                let fqn = format!("{}::${}", parent_fqn, name);

                result.symbols.push(SymbolInfo {
                    name,
                    fqn,
                    kind: PhpSymbolKind::Property,
                    uri: uri.to_string(),
                    range: node_range(node),
                    selection_range: node_range(name_node),
                    visibility,
                    modifiers,
                    doc_comment: doc_comment.clone(),
                    signature: type_info.as_ref().map(|t| Signature {
                        params: vec![],
                        return_type: Some(t.clone()),
                    }),
                    parent_fqn: Some(parent_fqn.to_string()),
                });
            }
        }
    }
}

fn extract_class_constants(
    node: Node,
    source: &str,
    uri: &str,
    result: &mut FileSymbols,
    parent_fqn: &str,
) {
    let visibility = extract_visibility(node, source);
    let modifiers = extract_modifiers(node, source);
    let doc_comment = find_doc_comment(node, source);

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
                    doc_comment: doc_comment.clone(),
                    signature: None,
                    parent_fqn: Some(parent_fqn.to_string()),
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
) {
    let doc_comment = find_doc_comment(node, source);

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "const_element" {
            if let Some(name_node) = child.child_by_field_name("name") {
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
                    doc_comment: doc_comment.clone(),
                    signature: None,
                    parent_fqn: None,
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
) {
    let name_node = match node.child_by_field_name("name") {
        Some(n) => n,
        None => return,
    };
    let name = node_text(name_node, source).to_string();
    let fqn = format!("{}::{}", parent_fqn, name);
    let doc_comment = find_doc_comment(node, source);

    result.symbols.push(SymbolInfo {
        name,
        fqn,
        kind: PhpSymbolKind::EnumCase,
        uri: uri.to_string(),
        range: node_range(node),
        selection_range: node_range(name_node),
        visibility: Visibility::Public,
        modifiers: SymbolModifiers::default(),
        doc_comment,
        signature: None,
        parent_fqn: Some(parent_fqn.to_string()),
    });
}

/// Extract function/method signature (parameters + return type).
fn extract_signature(node: Node, source: &str) -> Signature {
    let mut params = Vec::new();

    if let Some(param_list) = node.child_by_field_name("parameters") {
        let mut cursor = param_list.walk();
        for child in param_list.children(&mut cursor) {
            if child.kind() == "simple_parameter"
                || child.kind() == "variadic_parameter"
                || child.kind() == "property_promotion_parameter"
            {
                let param = extract_param(child, source);
                params.push(param);
            }
        }
    }

    let return_type = node
        .child_by_field_name("return_type")
        .map(|t| parse_type_node(t, source));

    Signature {
        params,
        return_type,
    }
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

fn extract_modifiers(node: Node, source: &str) -> SymbolModifiers {
    let mut mods = SymbolModifiers::default();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "static_modifier" => mods.is_static = true,
            "abstract_modifier" => mods.is_abstract = true,
            "final_modifier" => mods.is_final = true,
            "readonly_modifier" => mods.is_readonly = true,
            _ => {
                if node_text(child, source) == "static" {
                    mods.is_static = true;
                }
            }
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
    // Look for a comment node as a previous sibling
    let mut prev = node.prev_sibling();
    while let Some(p) = prev {
        if p.kind() == "comment" {
            let text = node_text(p, source);
            if text.starts_with("/**") {
                return Some(text.to_string());
            }
            // Non-PHPDoc comment — stop looking
            return None;
        }
        // Skip whitespace/empty lines between comment and declaration
        prev = p.prev_sibling();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::FileParser;

    fn parse_and_extract(code: &str) -> FileSymbols {
        let mut parser = FileParser::new();
        parser.parse_full(code);
        let tree = parser.tree().unwrap();
        extract_file_symbols(tree, code, "file:///test.php")
    }

    #[test]
    fn test_extract_class() {
        let syms = parse_and_extract("<?php\nnamespace App;\nclass Foo {\n}\n");
        assert_eq!(syms.namespace, Some("App".to_string()));
        assert_eq!(syms.symbols.len(), 1);
        assert_eq!(syms.symbols[0].name, "Foo");
        assert_eq!(syms.symbols[0].fqn, "App\\Foo");
        assert_eq!(syms.symbols[0].kind, PhpSymbolKind::Class);
    }

    #[test]
    fn test_extract_interface() {
        let syms = parse_and_extract(
            "<?php\ninterface Loggable {\n    public function log(string $msg): void;\n}\n",
        );
        assert_eq!(syms.symbols.len(), 2); // interface + method
        assert_eq!(syms.symbols[0].kind, PhpSymbolKind::Interface);
        assert_eq!(syms.symbols[0].name, "Loggable");
        assert_eq!(syms.symbols[1].kind, PhpSymbolKind::Method);
        assert_eq!(syms.symbols[1].name, "log");
    }

    #[test]
    fn test_extract_trait() {
        let syms = parse_and_extract(
            "<?php\ntrait HasName {\n    private string $name;\n    public function getName(): string { return $this->name; }\n}\n",
        );
        assert_eq!(syms.symbols[0].kind, PhpSymbolKind::Trait);
        assert!(syms
            .symbols
            .iter()
            .any(|s| s.kind == PhpSymbolKind::Property));
        assert!(syms.symbols.iter().any(|s| s.kind == PhpSymbolKind::Method));
    }

    #[test]
    fn test_extract_enum() {
        let syms = parse_and_extract(
            "<?php\nenum Color {\n    case Red;\n    case Green;\n    case Blue;\n}\n",
        );
        assert_eq!(syms.symbols[0].kind, PhpSymbolKind::Enum);
        assert_eq!(syms.symbols[0].name, "Color");
        let cases: Vec<&SymbolInfo> = syms
            .symbols
            .iter()
            .filter(|s| s.kind == PhpSymbolKind::EnumCase)
            .collect();
        assert_eq!(cases.len(), 3);
        assert_eq!(cases[0].name, "Red");
        assert_eq!(cases[0].fqn, "Color::Red");
    }

    #[test]
    fn test_extract_function() {
        let syms = parse_and_extract(
            "<?php\nnamespace Utils;\nfunction helper(int $x, string $y = 'default'): bool { return true; }\n",
        );
        let func = syms
            .symbols
            .iter()
            .find(|s| s.kind == PhpSymbolKind::Function)
            .unwrap();
        assert_eq!(func.name, "helper");
        assert_eq!(func.fqn, "Utils\\helper");
        let sig = func.signature.as_ref().unwrap();
        assert_eq!(sig.params.len(), 2);
        assert_eq!(sig.params[0].name, "x");
        assert_eq!(sig.params[1].name, "y");
        assert_eq!(sig.params[1].default_value.as_deref(), Some("'default'"));
        assert_eq!(sig.return_type.as_ref().unwrap().to_string(), "bool");
    }

    #[test]
    fn test_extract_method_with_visibility() {
        let syms = parse_and_extract(
            "<?php\nclass Foo {\n    private static function secret(): void {}\n    protected function internal(): int { return 0; }\n    public function api(): string { return ''; }\n}\n",
        );
        let methods: Vec<&SymbolInfo> = syms
            .symbols
            .iter()
            .filter(|s| s.kind == PhpSymbolKind::Method)
            .collect();
        assert_eq!(methods.len(), 3);

        let secret = methods.iter().find(|m| m.name == "secret").unwrap();
        assert_eq!(secret.visibility, Visibility::Private);
        assert!(secret.modifiers.is_static);

        let internal = methods.iter().find(|m| m.name == "internal").unwrap();
        assert_eq!(internal.visibility, Visibility::Protected);

        let api = methods.iter().find(|m| m.name == "api").unwrap();
        assert_eq!(api.visibility, Visibility::Public);
    }

    #[test]
    fn test_extract_properties() {
        let syms = parse_and_extract(
            "<?php\nclass Foo {\n    public string $name;\n    private int $count = 0;\n    protected readonly float $ratio;\n}\n",
        );
        let props: Vec<&SymbolInfo> = syms
            .symbols
            .iter()
            .filter(|s| s.kind == PhpSymbolKind::Property)
            .collect();
        assert_eq!(props.len(), 3);
        assert_eq!(props[0].name, "name");
        assert_eq!(props[1].name, "count");
        assert_eq!(props[2].name, "ratio");
    }

    #[test]
    fn test_extract_class_constants() {
        let syms = parse_and_extract(
            "<?php\nclass Foo {\n    const VERSION = '1.0';\n    public const MAX = 100;\n}\n",
        );
        let consts: Vec<&SymbolInfo> = syms
            .symbols
            .iter()
            .filter(|s| s.kind == PhpSymbolKind::ClassConstant)
            .collect();
        assert_eq!(consts.len(), 2);
        assert_eq!(consts[0].name, "VERSION");
        assert_eq!(consts[0].fqn, "Foo::VERSION");
    }

    #[test]
    fn test_extract_use_statements() {
        let syms = parse_and_extract(
            "<?php\nuse App\\Service\\Foo;\nuse App\\Entity\\Bar as B;\nuse function App\\helper;\n",
        );
        assert_eq!(syms.use_statements.len(), 3);
        assert_eq!(syms.use_statements[0].fqn, "App\\Service\\Foo");
        assert_eq!(syms.use_statements[0].alias, None);
        assert_eq!(syms.use_statements[0].kind, UseKind::Class);

        assert_eq!(syms.use_statements[1].fqn, "App\\Entity\\Bar");
        assert_eq!(syms.use_statements[1].alias, Some("B".to_string()));

        assert_eq!(syms.use_statements[2].fqn, "App\\helper");
        assert_eq!(syms.use_statements[2].kind, UseKind::Function);
    }

    #[test]
    fn test_extract_union_type() {
        let syms =
            parse_and_extract("<?php\nfunction foo(string|int $val): string|null { return ''; }\n");
        let func = &syms.symbols[0];
        let sig = func.signature.as_ref().unwrap();
        assert!(matches!(&sig.params[0].type_info, Some(TypeInfo::Union(_))));
        assert!(matches!(&sig.return_type, Some(TypeInfo::Union(_))));
    }

    #[test]
    fn test_extract_doc_comment() {
        let syms = parse_and_extract("<?php\n/** This is Foo. */\nclass Foo {}\n");
        assert_eq!(
            syms.symbols[0].doc_comment.as_deref(),
            Some("/** This is Foo. */")
        );
    }

    #[test]
    fn test_constructor_promotion() {
        let syms = parse_and_extract(
            "<?php\nclass Foo {\n    public function __construct(\n        private string $name,\n        public int $age = 0,\n    ) {}\n}\n",
        );
        let constructor = syms
            .symbols
            .iter()
            .find(|s| s.name == "__construct")
            .unwrap();
        let sig = constructor.signature.as_ref().unwrap();
        assert_eq!(sig.params.len(), 2);
        assert!(sig.params[0].is_promoted);
        assert!(sig.params[1].is_promoted);
    }

    #[test]
    fn test_extract_no_namespace() {
        let syms =
            parse_and_extract("<?php\nclass GlobalClass {}\nfunction globalFunc(): void {}\n");
        assert_eq!(syms.namespace, None);
        let cls = syms
            .symbols
            .iter()
            .find(|s| s.kind == PhpSymbolKind::Class)
            .unwrap();
        assert_eq!(cls.fqn, "GlobalClass");
        let func = syms
            .symbols
            .iter()
            .find(|s| s.kind == PhpSymbolKind::Function)
            .unwrap();
        assert_eq!(func.fqn, "globalFunc");
    }
}
