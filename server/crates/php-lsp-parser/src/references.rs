//! Find references to a symbol within a single file's CST.
//!
//! Given a target FQN and the file's CST + symbols, returns all locations
//! in the file that reference the target.

use crate::resolve::{
    resolve_scope_class_name_pub, symbol_at_position_with_resolvers, CallableParamTypeResolver,
    MemberTypeResolver, RefKind,
};
use crate::utf16::range_byte_to_utf16;
use php_lsp_types::{
    FileSymbols, PhpSymbolKind, SymbolReference, SymbolReferenceReceiver, UseKind,
};
use tree_sitter::{Node, Point, Tree};

/// A location within a file where a reference was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceLocation {
    pub range: (u32, u32, u32, u32),
}

/// Find local variable references in the same lexical scope at cursor position.
///
/// Scope is the nearest enclosing function/method/closure/arrow function,
/// or the whole file if cursor is at top level.
pub fn find_variable_references_at_position(
    tree: &Tree,
    source: &str,
    line: u32,
    character: u32,
    include_declaration: bool,
) -> Vec<ReferenceLocation> {
    let root = tree.root_node();
    let point = Point::new(line as usize, character as usize);
    let mut node = match root.descendant_for_point_range(point, point) {
        Some(n) => n,
        None => return vec![],
    };

    while !node.is_named() {
        node = match node.parent() {
            Some(p) => p,
            None => return vec![],
        };
    }
    if node.kind() == "name" {
        if let Some(parent) = node.parent() {
            if parent.kind() == "variable_name" {
                node = parent;
            }
        }
    }

    // Climb to a variable-like node.
    loop {
        let text = &source[node.byte_range()];
        if node.kind() == "variable_name" || text.starts_with('$') {
            break;
        }
        node = match node.parent() {
            Some(p) => p,
            None => return vec![],
        };
    }

    let var_name = normalize_var_name(&source[node.byte_range()]);
    let scope = find_variable_scope(node).unwrap_or(root);

    let mut refs: Vec<ReferenceLocation> = Vec::new();
    let mut declarations: Vec<(u32, u32, u32, u32)> = Vec::new();
    walk_variable_refs(scope, source, &var_name, &mut refs, &mut declarations);

    if include_declaration {
        refs
    } else {
        refs.into_iter()
            .filter(|r| !declarations.contains(&r.range))
            .collect()
    }
}

/// Find all references to the given FQN within a single file.
///
/// `target_fqn` is the fully qualified name to search for.
/// `target_kind` helps narrow the search (class vs function vs member).
/// `include_declaration` if true, also includes the declaration site.
pub fn find_references_in_file(
    tree: &Tree,
    source: &str,
    file_symbols: &FileSymbols,
    target_fqn: &str,
    target_kind: PhpSymbolKind,
    include_declaration: bool,
) -> Vec<ReferenceLocation> {
    let mut results = Vec::new();
    let root = tree.root_node();

    match target_kind {
        PhpSymbolKind::Class
        | PhpSymbolKind::Interface
        | PhpSymbolKind::Trait
        | PhpSymbolKind::Enum => {
            find_class_references(
                root,
                source,
                file_symbols,
                target_fqn,
                include_declaration,
                &mut results,
            );
        }
        PhpSymbolKind::Function => {
            find_function_references(
                root,
                source,
                file_symbols,
                target_fqn,
                include_declaration,
                &mut results,
            );
        }
        PhpSymbolKind::Method
        | PhpSymbolKind::Property
        | PhpSymbolKind::ClassConstant
        | PhpSymbolKind::EnumCase => {
            find_member_references(
                root,
                source,
                file_symbols,
                target_fqn,
                include_declaration,
                &mut results,
            );
        }
        PhpSymbolKind::GlobalConstant => {
            find_constant_references(
                root,
                source,
                file_symbols,
                target_fqn,
                include_declaration,
                &mut results,
            );
        }
        PhpSymbolKind::Namespace => {
            // Namespace references not typically searched
        }
    }

    results
}

/// Collect non-local symbol occurrences in a file for workspace-level references.
///
/// Local variables remain scope-sensitive and are intentionally handled from the
/// current open buffer instead of being stored in the workspace occurrence index.
pub fn collect_symbol_references_in_file(
    tree: &Tree,
    source: &str,
    file_symbols: &FileSymbols,
) -> Vec<SymbolReference> {
    collect_symbol_references_in_file_with_resolvers(tree, source, file_symbols, None, None)
}

/// Collect non-local symbol occurrences, using optional resolvers for receiver
/// type inference when indexing member references.
pub fn collect_symbol_references_in_file_with_resolvers(
    tree: &Tree,
    source: &str,
    file_symbols: &FileSymbols,
    resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
) -> Vec<SymbolReference> {
    let mut references = Vec::new();

    for symbol in &file_symbols.symbols {
        if symbol.kind == PhpSymbolKind::Namespace {
            continue;
        }
        references.push(SymbolReference {
            target_fqn: symbol.fqn.clone(),
            target_kind: symbol.kind,
            range: range_byte_to_utf16(source, symbol.selection_range),
            is_declaration: true,
            starts_with_dollar: symbol.kind == PhpSymbolKind::Property,
            receiver: SymbolReferenceReceiver::None,
        });
    }

    collect_symbol_references_walk(
        tree,
        tree.root_node(),
        source,
        file_symbols,
        &mut references,
        resolver,
        callable_resolver,
    );
    sort_and_dedup_symbol_references(&mut references);
    references
}

fn sort_and_dedup_symbol_references(references: &mut Vec<SymbolReference>) {
    references.sort_by(|left, right| {
        left.target_fqn
            .cmp(&right.target_fqn)
            .then_with(|| {
                symbol_reference_kind_rank(left.target_kind)
                    .cmp(&symbol_reference_kind_rank(right.target_kind))
            })
            .then_with(|| left.range.cmp(&right.range))
            .then_with(|| left.is_declaration.cmp(&right.is_declaration))
            .then_with(|| left.starts_with_dollar.cmp(&right.starts_with_dollar))
            .then_with(|| left.receiver.cmp(&right.receiver))
    });
    references.dedup_by(symbol_references_equal_for_dedup);
}

fn symbol_references_equal_for_dedup(
    left: &mut SymbolReference,
    right: &mut SymbolReference,
) -> bool {
    symbol_references_have_same_dedup_key(left, right)
}

fn symbol_references_have_same_dedup_key(left: &SymbolReference, right: &SymbolReference) -> bool {
    left.target_fqn == right.target_fqn
        && left.target_kind == right.target_kind
        && left.range == right.range
        && left.is_declaration == right.is_declaration
        && left.starts_with_dollar == right.starts_with_dollar
        && left.receiver == right.receiver
}

fn symbol_reference_kind_rank(kind: PhpSymbolKind) -> u8 {
    match kind {
        PhpSymbolKind::Class => 0,
        PhpSymbolKind::Interface => 1,
        PhpSymbolKind::Trait => 2,
        PhpSymbolKind::Enum => 3,
        PhpSymbolKind::Function => 4,
        PhpSymbolKind::Method => 5,
        PhpSymbolKind::Property => 6,
        PhpSymbolKind::ClassConstant => 7,
        PhpSymbolKind::GlobalConstant => 8,
        PhpSymbolKind::EnumCase => 9,
        PhpSymbolKind::Namespace => 10,
    }
}

fn collect_symbol_references_walk(
    tree: &Tree,
    node: Node,
    source: &str,
    file_symbols: &FileSymbols,
    references: &mut Vec<SymbolReference>,
    resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
) {
    match node.kind() {
        "object_creation_expression" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "name" || child.kind() == "qualified_name" {
                    push_class_reference(child, source, file_symbols, references);
                    break;
                }
            }
        }
        "scoped_call_expression" => {
            if let Some(scope_node) = node.child_by_field_name("scope") {
                push_class_reference(scope_node, source, file_symbols, references);
            }
            if let Some(name_node) = node.child_by_field_name("name") {
                let member_name = &source[name_node.byte_range()];
                if let Some(scope_fqn) = scoped_member_reference_class(node, source, file_symbols) {
                    push_symbol_reference(
                        references,
                        format!("{}::{}", scope_fqn, member_name),
                        PhpSymbolKind::Method,
                        reference_range(source, name_node),
                        false,
                        false,
                        SymbolReferenceReceiver::StaticClass {
                            class_fqn: scope_fqn,
                        },
                    );
                } else {
                    push_symbol_reference(
                        references,
                        format!("::{}", member_name),
                        PhpSymbolKind::Method,
                        reference_range(source, name_node),
                        false,
                        false,
                        SymbolReferenceReceiver::Unresolved,
                    );
                }
            }
        }
        "scoped_property_access_expression" => {
            if let Some(scope_node) = node.child_by_field_name("scope") {
                push_class_reference(scope_node, source, file_symbols, references);
            }
            if let Some(name_node) = node.child_by_field_name("name") {
                let raw_name = &source[name_node.byte_range()];
                let bare_name = raw_name.trim_start_matches('$');
                let kind = if raw_name.starts_with('$') {
                    PhpSymbolKind::Property
                } else {
                    PhpSymbolKind::ClassConstant
                };
                let member = if kind == PhpSymbolKind::Property {
                    format!("${}", bare_name)
                } else {
                    bare_name.to_string()
                };
                if let Some(scope_fqn) = scoped_member_reference_class(node, source, file_symbols) {
                    push_symbol_reference(
                        references,
                        format!("{}::{}", scope_fqn, member),
                        kind,
                        reference_range(source, name_node),
                        false,
                        raw_name.starts_with('$'),
                        SymbolReferenceReceiver::StaticClass {
                            class_fqn: scope_fqn,
                        },
                    );
                } else {
                    push_symbol_reference(
                        references,
                        format!("::{}", member),
                        kind,
                        reference_range(source, name_node),
                        false,
                        raw_name.starts_with('$'),
                        SymbolReferenceReceiver::Unresolved,
                    );
                }
            }
        }
        "member_access_expression" | "nullsafe_member_access_expression" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let text = &source[name_node.byte_range()];
                let (target_fqn, receiver) = resolved_instance_member_reference(
                    tree,
                    source,
                    file_symbols,
                    name_node,
                    PhpSymbolKind::Property,
                    resolver,
                    callable_resolver,
                )
                .unwrap_or_else(|| {
                    (
                        format!("::${}", text.trim_start_matches('$')),
                        SymbolReferenceReceiver::Unresolved,
                    )
                });
                push_symbol_reference(
                    references,
                    target_fqn,
                    PhpSymbolKind::Property,
                    reference_range(source, name_node),
                    false,
                    false,
                    receiver,
                );
            }
        }
        "member_call_expression" | "nullsafe_member_call_expression" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let text = &source[name_node.byte_range()];
                let (target_fqn, receiver) = resolved_instance_member_reference(
                    tree,
                    source,
                    file_symbols,
                    name_node,
                    PhpSymbolKind::Method,
                    resolver,
                    callable_resolver,
                )
                .unwrap_or_else(|| (format!("::{}", text), SymbolReferenceReceiver::Unresolved));
                push_symbol_reference(
                    references,
                    target_fqn,
                    PhpSymbolKind::Method,
                    reference_range(source, name_node),
                    false,
                    false,
                    receiver,
                );
            }
        }
        "class_constant_access_expression" => {
            if let (Some(scope_node), Some(name_node)) = (node.named_child(0), node.named_child(1))
            {
                push_class_reference(scope_node, source, file_symbols, references);
                let text = &source[name_node.byte_range()];
                let scope_fqn = scoped_member_reference_class_from_scope(
                    scope_node,
                    node,
                    source,
                    file_symbols,
                );
                let (target, receiver) = if let Some(scope_fqn) = scope_fqn {
                    (
                        format!("{}::{}", scope_fqn, text),
                        SymbolReferenceReceiver::StaticClass {
                            class_fqn: scope_fqn,
                        },
                    )
                } else {
                    (format!("::{}", text), SymbolReferenceReceiver::Unresolved)
                };
                push_symbol_reference(
                    references,
                    target,
                    PhpSymbolKind::ClassConstant,
                    reference_range(source, name_node),
                    false,
                    false,
                    receiver,
                );
            }
        }
        "function_call_expression" => {
            if let Some(func_node) = node.child_by_field_name("function") {
                let text = &source[func_node.byte_range()];
                push_symbol_reference(
                    references,
                    resolve_function_name_to_fqn(text, file_symbols),
                    PhpSymbolKind::Function,
                    reference_range(source, func_node),
                    false,
                    false,
                    SymbolReferenceReceiver::None,
                );
            }
        }
        "named_type" | "base_clause" | "class_interface_clause" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "name" || child.kind() == "qualified_name" {
                    push_class_reference(child, source, file_symbols, references);
                }
            }
            if node.named_child_count() == 0
                && (node.kind() == "name" || node.kind() == "qualified_name")
            {
                push_class_reference(node, source, file_symbols, references);
            }
        }
        "instanceof_expression" => {
            if let Some(right) = node.child_by_field_name("right") {
                push_class_reference(right, source, file_symbols, references);
            }
        }
        "trait_use_clause" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "name" || child.kind() == "qualified_name" {
                    push_class_reference(child, source, file_symbols, references);
                }
            }
        }
        "catch_clause" => {
            if let Some(type_node) = node.child_by_field_name("type") {
                let mut cursor = type_node.walk();
                for child in type_node.named_children(&mut cursor) {
                    if child.kind() == "name" || child.kind() == "qualified_name" {
                        push_class_reference(child, source, file_symbols, references);
                    }
                }
                if type_node.kind() == "name" || type_node.kind() == "qualified_name" {
                    push_class_reference(type_node, source, file_symbols, references);
                }
            }
        }
        "name" | "qualified_name" => {
            push_constant_reference_if_plain_name(node, source, file_symbols, references);
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_symbol_references_walk(
            tree,
            child,
            source,
            file_symbols,
            references,
            resolver,
            callable_resolver,
        );
    }
}

fn push_class_reference(
    node: Node,
    source: &str,
    file_symbols: &FileSymbols,
    references: &mut Vec<SymbolReference>,
) {
    let text = &source[node.byte_range()];
    let resolved = resolve_name_to_fqn(text, file_symbols);
    if is_builtin_or_relative_class_name(&resolved) {
        return;
    }
    push_symbol_reference(
        references,
        resolved,
        PhpSymbolKind::Class,
        reference_range(source, node),
        false,
        false,
        SymbolReferenceReceiver::None,
    );
}

fn scoped_member_reference_class(
    node: Node,
    source: &str,
    file_symbols: &FileSymbols,
) -> Option<String> {
    let scope_node = node.child_by_field_name("scope")?;
    scoped_member_reference_class_from_scope(scope_node, node, source, file_symbols)
}

fn scoped_member_reference_class_from_scope(
    scope_node: Node,
    context_node: Node,
    source: &str,
    file_symbols: &FileSymbols,
) -> Option<String> {
    let scope_text = &source[scope_node.byte_range()];
    let resolved = resolve_scope_class_name_pub(scope_text, context_node, source, file_symbols);
    if is_builtin_or_relative_class_name(&resolved) {
        return None;
    }
    Some(resolved)
}

fn push_constant_reference_if_plain_name(
    node: Node,
    source: &str,
    file_symbols: &FileSymbols,
    references: &mut Vec<SymbolReference>,
) {
    let parent_kind = node.parent().map(|p| p.kind()).unwrap_or("");
    if matches!(
        parent_kind,
        "function_call_expression"
            | "object_creation_expression"
            | "class_declaration"
            | "interface_declaration"
            | "trait_declaration"
            | "enum_declaration"
            | "function_definition"
            | "named_type"
            | "use_declaration"
            | "namespace_use_clause"
            | "scoped_call_expression"
            | "scoped_property_access_expression"
            | "class_constant_access_expression"
            | "member_access_expression"
            | "member_call_expression"
    ) {
        return;
    }

    let text = &source[node.byte_range()];
    if is_builtin_or_relative_class_name(text) {
        return;
    }
    push_symbol_reference(
        references,
        resolve_constant_name_to_fqn(text, file_symbols),
        PhpSymbolKind::GlobalConstant,
        reference_range(source, node),
        false,
        false,
        SymbolReferenceReceiver::None,
    );
}

fn is_builtin_or_relative_class_name(name: &str) -> bool {
    matches!(
        name,
        "self"
            | "static"
            | "parent"
            | "$this"
            | "string"
            | "int"
            | "float"
            | "bool"
            | "array"
            | "callable"
            | "iterable"
            | "object"
            | "mixed"
            | "void"
            | "never"
            | "null"
            | "false"
            | "true"
    )
}

fn resolved_instance_member_reference(
    tree: &Tree,
    source: &str,
    file_symbols: &FileSymbols,
    name_node: Node,
    target_kind: PhpSymbolKind,
    resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
) -> Option<(String, SymbolReferenceReceiver)> {
    let start = name_node.start_position();
    let symbol = symbol_at_position_with_resolvers(
        tree,
        source,
        start.row as u32,
        start.column as u32,
        file_symbols,
        resolver,
        callable_resolver,
    )?;
    if !ref_kind_matches_symbol_kind(symbol.ref_kind, target_kind) {
        return None;
    }

    let receiver_fqn = symbol
        .fqn
        .rsplit_once("::")
        .map(|(receiver, _)| receiver.to_string())?;
    if is_builtin_or_relative_class_name(&receiver_fqn) {
        return None;
    }

    Some((
        symbol.fqn,
        SymbolReferenceReceiver::ResolvedType {
            type_fqn: receiver_fqn,
        },
    ))
}

fn ref_kind_matches_symbol_kind(ref_kind: RefKind, target_kind: PhpSymbolKind) -> bool {
    matches!(
        (ref_kind, target_kind),
        (RefKind::MethodCall, PhpSymbolKind::Method)
            | (RefKind::PropertyAccess, PhpSymbolKind::Property)
            | (RefKind::StaticPropertyAccess, PhpSymbolKind::Property)
            | (RefKind::ClassConstant, PhpSymbolKind::ClassConstant)
            | (RefKind::ClassConstant, PhpSymbolKind::EnumCase)
    )
}

fn push_symbol_reference(
    references: &mut Vec<SymbolReference>,
    target_fqn: String,
    target_kind: PhpSymbolKind,
    range: (u32, u32, u32, u32),
    is_declaration: bool,
    starts_with_dollar: bool,
    receiver: SymbolReferenceReceiver,
) {
    references.push(SymbolReference {
        target_fqn,
        target_kind,
        range,
        is_declaration,
        starts_with_dollar,
        receiver,
    });
}

fn reference_range(source: &str, node: Node) -> (u32, u32, u32, u32) {
    range_byte_to_utf16(source, node_range(node))
}

/// Find all references to a class/interface/trait/enum in a file.
fn find_class_references(
    root: Node,
    source: &str,
    file_symbols: &FileSymbols,
    target_fqn: &str,
    include_declaration: bool,
    results: &mut Vec<ReferenceLocation>,
) {
    // Check declarations in this file
    if include_declaration {
        for sym in &file_symbols.symbols {
            if sym.fqn == target_fqn
                && matches!(
                    sym.kind,
                    PhpSymbolKind::Class
                        | PhpSymbolKind::Interface
                        | PhpSymbolKind::Trait
                        | PhpSymbolKind::Enum
                )
            {
                results.push(ReferenceLocation {
                    range: sym.selection_range,
                });
            }
        }
    }

    // Walk the CST looking for name nodes that resolve to the target FQN
    walk_for_class_refs(root, source, file_symbols, target_fqn, results);
}

/// Recursively walk the CST to find class name references.
fn walk_for_class_refs(
    node: Node,
    source: &str,
    file_symbols: &FileSymbols,
    target_fqn: &str,
    results: &mut Vec<ReferenceLocation>,
) {
    let kind = node.kind();

    // Check nodes that can contain class name references
    match kind {
        // new ClassName()
        "object_creation_expression" => {
            // The class name is a direct child (name or qualified_name node)
            let cursor = &mut node.walk();
            for child in node.named_children(cursor) {
                if child.kind() == "name" || child.kind() == "qualified_name" {
                    check_class_name_ref(child, source, file_symbols, target_fqn, results);
                    break;
                }
            }
        }

        // ClassName::method() or ClassName::$prop or ClassName::CONST
        "scoped_call_expression" | "scoped_property_access_expression" => {
            if let Some(scope_node) = node.child_by_field_name("scope") {
                check_class_name_ref(scope_node, source, file_symbols, target_fqn, results);
            }
        }

        // Type hints: function(ClassName $x): ClassName
        "named_type" => {
            // named_type contains a child name or qualified_name
            let cursor = &mut node.walk();
            for child in node.named_children(cursor) {
                if child.kind() == "name" || child.kind() == "qualified_name" {
                    check_class_name_ref(child, source, file_symbols, target_fqn, results);
                }
            }
            // Also check the node itself if it's a name (fallback)
            if node.named_child_count() == 0 {
                check_class_name_ref(node, source, file_symbols, target_fqn, results);
            }
        }

        // extends/implements
        "base_clause" | "class_interface_clause" => {
            let cursor = &mut node.walk();
            for child in node.named_children(cursor) {
                if child.kind() == "name" || child.kind() == "qualified_name" {
                    check_class_name_ref(child, source, file_symbols, target_fqn, results);
                }
            }
        }

        // instanceof
        "instanceof_expression" => {
            if let Some(right) = node.child_by_field_name("right") {
                check_class_name_ref(right, source, file_symbols, target_fqn, results);
            }
        }

        // catch clause
        "catch_clause" => {
            if let Some(type_node) = node.child_by_field_name("type") {
                let cursor = &mut type_node.walk();
                for child in type_node.named_children(cursor) {
                    if child.kind() == "name" || child.kind() == "qualified_name" {
                        check_class_name_ref(child, source, file_symbols, target_fqn, results);
                    }
                }
                // Also check the type node itself if it's a name
                if type_node.kind() == "name" || type_node.kind() == "qualified_name" {
                    check_class_name_ref(type_node, source, file_symbols, target_fqn, results);
                }
            }
        }

        _ => {}
    }

    // Recurse into children
    let cursor = &mut node.walk();
    for child in node.named_children(cursor) {
        walk_for_class_refs(child, source, file_symbols, target_fqn, results);
    }
}

/// Check if a node is a class name reference to the target FQN.
fn check_class_name_ref(
    node: Node,
    source: &str,
    file_symbols: &FileSymbols,
    target_fqn: &str,
    results: &mut Vec<ReferenceLocation>,
) {
    let text = &source[node.byte_range()];
    let resolved = resolve_name_to_fqn(text, file_symbols);

    if resolved == target_fqn {
        let start = node.start_position();
        let end = node.end_position();
        results.push(ReferenceLocation {
            range: (
                start.row as u32,
                start.column as u32,
                end.row as u32,
                end.column as u32,
            ),
        });
    }
}

/// Resolve a name to FQN using use statements and namespace context.
fn resolve_name_to_fqn(name: &str, file_symbols: &FileSymbols) -> String {
    // Already fully qualified
    if name.starts_with('\\') {
        return name.trim_start_matches('\\').to_string();
    }

    // Special names
    match name {
        "self" | "static" | "parent" | "$this" | "string" | "int" | "float" | "bool" | "array"
        | "callable" | "iterable" | "object" | "mixed" | "void" | "never" | "null" | "false"
        | "true" => return name.to_string(),
        _ => {}
    }

    // Try use statements
    let parts: Vec<&str> = name.split('\\').collect();
    let first_part = parts[0];

    for use_stmt in &file_symbols.use_statements {
        if use_stmt.kind != UseKind::Class {
            continue;
        }

        let alias = use_stmt
            .alias
            .as_deref()
            .unwrap_or_else(|| use_stmt.fqn.rsplit('\\').next().unwrap_or(&use_stmt.fqn));

        if alias == first_part {
            if parts.len() == 1 {
                return use_stmt.fqn.clone();
            } else {
                let rest = parts[1..].join("\\");
                return format!("{}\\{}", use_stmt.fqn, rest);
            }
        }
    }

    // Prepend namespace
    if let Some(ref ns) = file_symbols.namespace {
        format!("{}\\{}", ns, name)
    } else {
        name.to_string()
    }
}

/// Find all references to a function in a file.
fn find_function_references(
    root: Node,
    source: &str,
    file_symbols: &FileSymbols,
    target_fqn: &str,
    include_declaration: bool,
    results: &mut Vec<ReferenceLocation>,
) {
    if include_declaration {
        for sym in &file_symbols.symbols {
            if sym.fqn == target_fqn && sym.kind == PhpSymbolKind::Function {
                results.push(ReferenceLocation {
                    range: sym.selection_range,
                });
            }
        }
    }

    walk_for_function_refs(root, source, file_symbols, target_fqn, results);
}

/// Walk CST looking for function call references.
fn walk_for_function_refs(
    node: Node,
    source: &str,
    file_symbols: &FileSymbols,
    target_fqn: &str,
    results: &mut Vec<ReferenceLocation>,
) {
    if node.kind() == "function_call_expression" {
        if let Some(func_node) = node.child_by_field_name("function") {
            let text = &source[func_node.byte_range()];
            let resolved = resolve_function_name_to_fqn(text, file_symbols);
            if resolved == target_fqn {
                let start = func_node.start_position();
                let end = func_node.end_position();
                results.push(ReferenceLocation {
                    range: (
                        start.row as u32,
                        start.column as u32,
                        end.row as u32,
                        end.column as u32,
                    ),
                });
            }
        }
    }

    let cursor = &mut node.walk();
    for child in node.named_children(cursor) {
        walk_for_function_refs(child, source, file_symbols, target_fqn, results);
    }
}

/// Resolve a function name to FQN.
fn resolve_function_name_to_fqn(name: &str, file_symbols: &FileSymbols) -> String {
    if name.starts_with('\\') {
        return name.trim_start_matches('\\').to_string();
    }

    // Try use statements (function)
    for use_stmt in &file_symbols.use_statements {
        if use_stmt.kind != UseKind::Function {
            continue;
        }

        let alias = use_stmt
            .alias
            .as_deref()
            .unwrap_or_else(|| use_stmt.fqn.rsplit('\\').next().unwrap_or(&use_stmt.fqn));

        if alias == name {
            return use_stmt.fqn.clone();
        }
    }

    // Keep already-qualified names stable.
    if name.contains('\\') {
        return name.to_string();
    }

    // Namespace-qualified for simple names
    if let Some(ref ns) = file_symbols.namespace {
        format!("{}\\{}", ns, name)
    } else {
        name.to_string()
    }
}

/// Resolve a global constant name to FQN.
fn resolve_constant_name_to_fqn(name: &str, file_symbols: &FileSymbols) -> String {
    if name.starts_with('\\') {
        return name.trim_start_matches('\\').to_string();
    }

    for use_stmt in &file_symbols.use_statements {
        if use_stmt.kind != UseKind::Constant {
            continue;
        }

        let alias = use_stmt
            .alias
            .as_deref()
            .unwrap_or_else(|| use_stmt.fqn.rsplit('\\').next().unwrap_or(&use_stmt.fqn));

        if alias == name {
            return use_stmt.fqn.clone();
        }
    }

    if name.contains('\\') {
        return name.to_string();
    }

    if let Some(ref ns) = file_symbols.namespace {
        format!("{}\\{}", ns, name)
    } else {
        name.to_string()
    }
}

/// Find all references to a class member (method, property, class constant, enum case).
fn find_member_references(
    root: Node,
    source: &str,
    file_symbols: &FileSymbols,
    target_fqn: &str,
    include_declaration: bool,
    results: &mut Vec<ReferenceLocation>,
) {
    // Parse the target FQN: "ClassName::memberName"
    let member_name = if let Some(pos) = target_fqn.rfind("::") {
        &target_fqn[pos + 2..]
    } else {
        return;
    };

    if include_declaration {
        for sym in &file_symbols.symbols {
            if sym.fqn == target_fqn {
                results.push(ReferenceLocation {
                    range: sym.selection_range,
                });
            }
        }
    }

    walk_for_member_refs(root, source, file_symbols, target_fqn, member_name, results);
}

/// Walk CST for member access references.
fn walk_for_member_refs(
    node: Node,
    source: &str,
    _file_symbols: &FileSymbols,
    target_fqn: &str,
    member_name: &str,
    results: &mut Vec<ReferenceLocation>,
) {
    let kind = node.kind();
    let is_property_target = member_name.starts_with('$');
    let normalized_member_name = member_name.strip_prefix('$').unwrap_or(member_name);

    match kind {
        // $obj->property (and callable-like member access without invocation)
        "member_access_expression" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let text = &source[name_node.byte_range()];
                if text == normalized_member_name {
                    let start = name_node.start_position();
                    let end = name_node.end_position();
                    results.push(ReferenceLocation {
                        range: (
                            start.row as u32,
                            start.column as u32,
                            end.row as u32,
                            end.column as u32,
                        ),
                    });
                }
            }
        }

        // $obj->method()
        "member_call_expression" => {
            if is_property_target {
                // Property target should not match method calls with the same short name.
            } else if let Some(name_node) = node.child_by_field_name("name") {
                let text = &source[name_node.byte_range()];
                if text == member_name {
                    let start = name_node.start_position();
                    let end = name_node.end_position();
                    results.push(ReferenceLocation {
                        range: (
                            start.row as u32,
                            start.column as u32,
                            end.row as u32,
                            end.column as u32,
                        ),
                    });
                }
            }
        }

        // ClassName::method()
        "scoped_call_expression" => {
            if is_property_target {
                // Property target should not match scoped method calls.
            } else if let Some(name_node) = node.child_by_field_name("name") {
                let text = &source[name_node.byte_range()];
                if text == member_name {
                    // For scoped access, also check that the scope resolves to the right class
                    if let Some(scope_node) = node.child_by_field_name("scope") {
                        let scope_text = &source[scope_node.byte_range()];
                        let scope_fqn = resolve_name_to_fqn(scope_text, _file_symbols);
                        let expected_class = &target_fqn[..target_fqn.rfind("::").unwrap_or(0)];

                        if scope_fqn == expected_class
                            || scope_text == "self"
                            || scope_text == "static"
                            || scope_text == "parent"
                        {
                            let start = name_node.start_position();
                            let end = name_node.end_position();
                            results.push(ReferenceLocation {
                                range: (
                                    start.row as u32,
                                    start.column as u32,
                                    end.row as u32,
                                    end.column as u32,
                                ),
                            });
                        }
                    }
                }
            }
        }

        // ClassName::$prop or ClassName::CONST
        "scoped_property_access_expression" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let text = &source[name_node.byte_range()];
                let matches_member =
                    text == member_name || (!is_property_target && text == normalized_member_name);
                if matches_member {
                    // For scoped access, also check that the scope resolves to the right class
                    if let Some(scope_node) = node.child_by_field_name("scope") {
                        let scope_text = &source[scope_node.byte_range()];
                        let scope_fqn = resolve_name_to_fqn(scope_text, _file_symbols);
                        let expected_class = &target_fqn[..target_fqn.rfind("::").unwrap_or(0)];

                        if scope_fqn == expected_class
                            || scope_text == "self"
                            || scope_text == "static"
                            || scope_text == "parent"
                        {
                            let start = name_node.start_position();
                            let end = name_node.end_position();
                            results.push(ReferenceLocation {
                                range: (
                                    start.row as u32,
                                    start.column as u32,
                                    end.row as u32,
                                    end.column as u32,
                                ),
                            });
                        }
                    }
                }
            }
        }

        // self::CONST / ClassName::CONST
        "class_constant_access_expression" => {
            if is_property_target {
                // Property target should not match class constant access.
            } else if let (Some(scope_node), Some(name_node)) =
                (node.named_child(0), node.named_child(1))
            {
                let text = &source[name_node.byte_range()];
                if text == member_name {
                    let scope_text = &source[scope_node.byte_range()];
                    let scope_fqn = resolve_name_to_fqn(scope_text, _file_symbols);
                    let expected_class = &target_fqn[..target_fqn.rfind("::").unwrap_or(0)];

                    if scope_fqn == expected_class
                        || scope_text == "self"
                        || scope_text == "static"
                        || scope_text == "parent"
                    {
                        let start = name_node.start_position();
                        let end = name_node.end_position();
                        results.push(ReferenceLocation {
                            range: (
                                start.row as u32,
                                start.column as u32,
                                end.row as u32,
                                end.column as u32,
                            ),
                        });
                    }
                }
            }
        }

        _ => {}
    }

    let cursor = &mut node.walk();
    for child in node.named_children(cursor) {
        walk_for_member_refs(
            child,
            source,
            _file_symbols,
            target_fqn,
            member_name,
            results,
        );
    }
}

/// Find references to a global constant.
fn find_constant_references(
    root: Node,
    source: &str,
    file_symbols: &FileSymbols,
    target_fqn: &str,
    include_declaration: bool,
    results: &mut Vec<ReferenceLocation>,
) {
    if include_declaration {
        for sym in &file_symbols.symbols {
            if sym.fqn == target_fqn && sym.kind == PhpSymbolKind::GlobalConstant {
                results.push(ReferenceLocation {
                    range: sym.selection_range,
                });
            }
        }
    }

    // Constants are referenced as plain names — similar to class names
    walk_for_constant_refs(root, source, file_symbols, target_fqn, results);
}

/// Walk CST for constant references.
fn walk_for_constant_refs(
    node: Node,
    source: &str,
    file_symbols: &FileSymbols,
    target_fqn: &str,
    results: &mut Vec<ReferenceLocation>,
) {
    // Constants appear as "name" nodes that are not function calls, class names, etc.
    if node.kind() == "name" || node.kind() == "qualified_name" {
        let parent = node.parent();
        let parent_kind = parent.map(|p| p.kind()).unwrap_or("");

        // Skip nodes that are part of other constructs
        if parent_kind != "function_call_expression"
            && parent_kind != "object_creation_expression"
            && parent_kind != "class_declaration"
            && parent_kind != "interface_declaration"
            && parent_kind != "trait_declaration"
            && parent_kind != "enum_declaration"
            && parent_kind != "function_definition"
            && parent_kind != "named_type"
            && parent_kind != "use_declaration"
            && parent_kind != "namespace_use_clause"
        {
            let text = &source[node.byte_range()];
            // Try resolving as constant
            let resolved = resolve_constant_name_to_fqn(text, file_symbols);
            if resolved == target_fqn {
                let start = node.start_position();
                let end = node.end_position();
                results.push(ReferenceLocation {
                    range: (
                        start.row as u32,
                        start.column as u32,
                        end.row as u32,
                        end.column as u32,
                    ),
                });
            }
        }
    }

    let cursor = &mut node.walk();
    for child in node.named_children(cursor) {
        walk_for_constant_refs(child, source, file_symbols, target_fqn, results);
    }
}

fn walk_variable_refs(
    node: Node,
    source: &str,
    var_name: &str,
    refs: &mut Vec<ReferenceLocation>,
    declarations: &mut Vec<(u32, u32, u32, u32)>,
) {
    if node.kind() == "variable_name" {
        let text = normalize_var_name(&source[node.byte_range()]);
        if text == var_name {
            let range = node_range(node);
            refs.push(ReferenceLocation { range });
            if is_variable_declaration(node, source, var_name) {
                declarations.push(range);
            }
        }
    }

    let cursor = &mut node.walk();
    for child in node.named_children(cursor) {
        walk_variable_refs(child, source, var_name, refs, declarations);
    }
}

fn is_variable_declaration(node: Node, source: &str, var_name: &str) -> bool {
    let parent = match node.parent() {
        Some(p) => p,
        None => return false,
    };

    match parent.kind() {
        "simple_parameter" | "property_promotion_parameter" => parent
            .child_by_field_name("name")
            .map(|n| n.id() == node.id())
            .unwrap_or(false),
        "assignment_expression" => parent
            .child_by_field_name("left")
            .map(|n| normalize_var_name(&source[n.byte_range()]) == var_name)
            .unwrap_or(false),
        "foreach_statement" => ["key", "value"].iter().any(|field| {
            parent
                .child_by_field_name(field)
                .map(|n| n.id() == node.id())
                .unwrap_or(false)
        }),
        "catch_clause" => ["name", "variable"].iter().any(|field| {
            parent
                .child_by_field_name(field)
                .map(|n| n.id() == node.id())
                .unwrap_or(false)
        }),
        "anonymous_function_use_clause" => true,
        _ => false,
    }
}

fn find_variable_scope(node: Node) -> Option<Node> {
    let mut current = node.parent();
    while let Some(n) = current {
        match n.kind() {
            "method_declaration"
            | "function_definition"
            | "arrow_function"
            | "anonymous_function"
            | "anonymous_function_creation_expression" => return Some(n),
            _ => current = n.parent(),
        }
    }
    None
}

fn normalize_var_name(text: &str) -> String {
    if text.starts_with('$') {
        text.to_string()
    } else {
        format!("${}", text)
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::FileParser;
    use crate::symbols::extract_file_symbols;

    fn find_refs(code: &str, target_fqn: &str, kind: PhpSymbolKind) -> Vec<ReferenceLocation> {
        let mut parser = FileParser::new();
        parser.parse_full(code);
        let tree = parser.tree().unwrap();
        let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
        find_references_in_file(tree, code, &file_symbols, target_fqn, kind, true)
    }

    fn collect_refs(code: &str) -> Vec<SymbolReference> {
        let mut parser = FileParser::new();
        parser.parse_full(code);
        let tree = parser.tree().unwrap();
        let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
        collect_symbol_references_in_file(tree, code, &file_symbols)
    }

    fn synthetic_symbol_reference(
        target_kind: PhpSymbolKind,
        starts_with_dollar: bool,
    ) -> SymbolReference {
        SymbolReference {
            target_fqn: "App\\Target::member".to_string(),
            target_kind,
            range: (4, 8, 4, 14),
            is_declaration: false,
            starts_with_dollar,
            receiver: SymbolReferenceReceiver::ResolvedType {
                type_fqn: "App\\Target".to_string(),
            },
        }
    }

    fn find_var_refs_at(
        code: &str,
        line: u32,
        col: u32,
        include_declaration: bool,
    ) -> Vec<ReferenceLocation> {
        let mut parser = FileParser::new();
        parser.parse_full(code);
        let tree = parser.tree().unwrap();
        find_variable_references_at_position(tree, code, line, col, include_declaration)
    }

    fn find_line_col(code: &str, needle: &str) -> (u32, u32) {
        for (line, row) in code.lines().enumerate() {
            if let Some(col) = row.find(needle) {
                return (line as u32, col as u32);
            }
        }
        panic!("needle not found: {}", needle);
    }

    #[test]
    fn test_find_class_references_new() {
        let code = r#"<?php
namespace App;

use App\Service\UserService;

$svc = new UserService();
$svc2 = new UserService();
"#;
        let refs = find_refs(code, "App\\Service\\UserService", PhpSymbolKind::Class);
        assert_eq!(refs.len(), 2, "Should find 2 new-expression references");
    }

    #[test]
    fn test_find_class_references_type_hint() {
        let code = r#"<?php
namespace App;

use App\Model\User;

class Controller {
    public function show(User $user): User {
        return $user;
    }
}
"#;
        let refs = find_refs(code, "App\\Model\\User", PhpSymbolKind::Class);
        // Should find type hint in param + return type = 2
        assert!(
            refs.len() >= 2,
            "Should find at least 2 type hint references, found {}",
            refs.len()
        );
    }

    #[test]
    fn test_find_function_references() {
        let code = r#"<?php
namespace App;

function helper() {}

helper();
helper();
"#;
        let refs = find_refs(code, "App\\helper", PhpSymbolKind::Function);
        // 1 declaration + 2 calls = 3
        assert_eq!(refs.len(), 3, "Should find declaration + 2 calls");
    }

    #[test]
    fn test_find_static_method_references() {
        let code = r#"<?php
namespace App;

class Foo {
    public static function bar() {}
}

Foo::bar();
"#;
        let refs = find_refs(code, "App\\Foo::bar", PhpSymbolKind::Method);
        // declaration + 1 call = 2
        assert!(!refs.is_empty(), "Should find at least 1 reference");
    }

    #[test]
    fn test_find_property_references_not_method_with_same_name() {
        let code = r#"<?php
namespace App;

class Baz {
    public string $test = '';
    public function test(): string { return 'ok'; }
}

function run(Baz $baz): void {
    echo $baz->test;
    $baz->test();
}
"#;

        let refs = find_refs(code, "App\\Baz::$test", PhpSymbolKind::Property);
        // declaration + one property usage
        assert_eq!(
            refs.len(),
            2,
            "Property references should not include method calls with the same name"
        );
    }

    #[test]
    fn test_find_class_constant_references() {
        let code = r#"<?php
namespace App;

class RenameTarget {
    public const STATE_ACTIVE = 'active';
    public function touch(): void {
        echo self::STATE_ACTIVE;
    }
}

echo RenameTarget::STATE_ACTIVE;
"#;
        let refs = find_refs(
            code,
            "App\\RenameTarget::STATE_ACTIVE",
            PhpSymbolKind::ClassConstant,
        );
        // declaration + 2 usages
        assert_eq!(refs.len(), 3, "Should find declaration + 2 constant usages");
    }

    #[test]
    fn test_symbol_reference_sort_key_matches_dedup_key() {
        let duplicate_method = synthetic_symbol_reference(PhpSymbolKind::Method, false);
        let distinct_property_same_range =
            synthetic_symbol_reference(PhpSymbolKind::Property, false);
        let distinct_method_with_dollar = synthetic_symbol_reference(PhpSymbolKind::Method, true);
        let mut refs = vec![
            duplicate_method.clone(),
            distinct_property_same_range,
            duplicate_method,
            distinct_method_with_dollar,
        ];

        sort_and_dedup_symbol_references(&mut refs);

        assert_eq!(
            refs.len(),
            3,
            "same-kind duplicates should collapse, but different kind/dollar state must survive"
        );
        assert_eq!(
            refs.iter()
                .filter(|reference| reference.target_kind == PhpSymbolKind::Method
                    && !reference.starts_with_dollar)
                .count(),
            1
        );
        assert!(refs.iter().any(|reference| {
            reference.target_kind == PhpSymbolKind::Property && !reference.starts_with_dollar
        }));
        assert!(refs.iter().any(|reference| {
            reference.target_kind == PhpSymbolKind::Method && reference.starts_with_dollar
        }));
    }

    #[test]
    fn test_collect_symbol_references_resolves_imported_global_constants() {
        let code = r#"<?php
namespace App;

use const Vendor\FLAGS\ENABLED as IS_ENABLED;

echo IS_ENABLED;
"#;
        let refs = collect_refs(code);

        assert!(refs.iter().any(|reference| {
            reference.target_fqn == "Vendor\\FLAGS\\ENABLED"
                && reference.target_kind == PhpSymbolKind::GlobalConstant
                && !reference.is_declaration
        }));
    }

    #[test]
    fn test_collect_symbol_references_for_workspace_index() {
        let code = r#"<?php
namespace App;

use App\Model\User;

const FLAG = true;
function helper(): void {}

class Service {
    public const STATE = 'ok';
    public string $name = '';

    public function run(User $user): void {
        helper();
        echo self::STATE;
        echo $this->name;
        echo FLAG;
    }
}

$service = new Service();
"#;

        let refs = collect_refs(code);

        assert!(refs.iter().any(|reference| {
            reference.target_fqn == "App\\Service"
                && reference.target_kind == PhpSymbolKind::Class
                && reference.is_declaration
        }));
        assert!(refs.iter().any(|reference| {
            reference.target_fqn == "App\\Service"
                && reference.target_kind == PhpSymbolKind::Class
                && !reference.is_declaration
        }));
        assert!(refs.iter().any(|reference| {
            reference.target_fqn == "App\\Model\\User"
                && reference.target_kind == PhpSymbolKind::Class
                && !reference.is_declaration
        }));
        assert!(refs.iter().any(|reference| {
            reference.target_fqn == "App\\helper"
                && reference.target_kind == PhpSymbolKind::Function
                && !reference.is_declaration
        }));
        assert!(refs.iter().any(|reference| {
            reference.target_fqn == "App\\Service::STATE"
                && reference.target_kind == PhpSymbolKind::ClassConstant
                && !reference.is_declaration
        }));
        assert!(refs.iter().any(|reference| {
            reference.target_fqn == "App\\Service::$name"
                && reference.target_kind == PhpSymbolKind::Property
                && !reference.is_declaration
                && !reference.starts_with_dollar
                && reference.receiver
                    == SymbolReferenceReceiver::ResolvedType {
                        type_fqn: "App\\Service".to_string(),
                    }
        }));
        assert!(refs.iter().any(|reference| {
            reference.target_fqn == "App\\FLAG"
                && reference.target_kind == PhpSymbolKind::GlobalConstant
                && !reference.is_declaration
        }));
    }

    #[test]
    fn test_find_variable_references_in_function_scope() {
        let code = r#"<?php
function run(string $x): void {
    $x = $x . "!";
    echo $x;
}
"#;
        let (line, col) = find_line_col(code, "echo $x;");
        let refs = find_var_refs_at(code, line, col + 6, true);
        // param + assignment left + assignment right + echo usage
        assert_eq!(refs.len(), 4);

        let refs_no_decl = find_var_refs_at(code, line, col + 6, false);
        // assignment right + echo usage
        assert_eq!(refs_no_decl.len(), 2);
    }

    #[test]
    fn test_find_variable_references_do_not_cross_scope() {
        let code = r#"<?php
$x = 1;
function demo(): void {
    $x = 2;
    echo $x;
}
echo $x;
"#;
        let (line, col) = find_line_col(code, "echo $x;");
        let refs = find_var_refs_at(code, line, col + 6, true);
        // only inner assignment + inner usage
        assert_eq!(refs.len(), 2);
    }
}
