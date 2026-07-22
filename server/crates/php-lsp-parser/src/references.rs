//! Find references to a symbol within a single file's CST.
//!
//! Given a target FQN and the file's CST + symbols, returns all locations
//! in the file that reference the target.

use crate::cst::{ancestor_field_contains, is_foreach_header_declared_variable};
use crate::resolve::{
    namespace_relative_function_call, resolve_class_name_pub, resolve_constant_name_pub,
    resolve_function_name_pub, resolve_scope_class_name_pub, symbol_at_position_with_resolvers,
    unqualified_name_allows_global_fallback, CallableParamTypeResolver, MemberTypeResolver,
    RefKind,
};
use crate::utf16::range_byte_to_utf16;
use php_lsp_types::{
    symbol_fqn_eq, FileSymbols, PhpSymbolKind, SymbolReference, SymbolReferenceReceiver, UseKind,
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
                target_kind,
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
            allows_global_fallback: false,
            rename_range: None,
            preserve_spelling_on_rename: false,
            is_import_target: false,
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
            .then_with(|| {
                left.allows_global_fallback
                    .cmp(&right.allows_global_fallback)
            })
            .then_with(|| left.rename_range.cmp(&right.rename_range))
            .then_with(|| {
                left.preserve_spelling_on_rename
                    .cmp(&right.preserve_spelling_on_rename)
            })
            .then_with(|| left.is_import_target.cmp(&right.is_import_target))
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
        && left.allows_global_fallback == right.allows_global_fallback
        && left.rename_range == right.rename_range
        && left.preserve_spelling_on_rename == right.preserve_spelling_on_rename
        && left.is_import_target == right.is_import_target
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
    let start = node.start_position();
    let scoped_file_symbols =
        file_symbols.scoped_at_byte_position(start.row as u32, start.column as u32);
    let file_symbols = scoped_file_symbols.as_ref();

    if node.kind() == "namespace_definition" {
        if let Some((function_name, selection)) = namespace_relative_function_call(node, source) {
            push_symbol_reference(
                references,
                resolve_function_name_to_fqn(&function_name, file_symbols),
                PhpSymbolKind::Function,
                reference_range(source, selection),
                CollectedReferenceOptions::default(),
            );
        }
    }

    match node.kind() {
        "namespace_use_clause" => {
            push_import_target_reference(node, source, file_symbols, references);
        }
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
                        CollectedReferenceOptions {
                            receiver: SymbolReferenceReceiver::StaticClass {
                                class_fqn: scope_fqn,
                            },
                            ..Default::default()
                        },
                    );
                } else {
                    push_symbol_reference(
                        references,
                        format!("::{}", member_name),
                        PhpSymbolKind::Method,
                        reference_range(source, name_node),
                        CollectedReferenceOptions {
                            receiver: SymbolReferenceReceiver::Unresolved,
                            ..Default::default()
                        },
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
                        CollectedReferenceOptions {
                            starts_with_dollar: raw_name.starts_with('$'),
                            receiver: SymbolReferenceReceiver::StaticClass {
                                class_fqn: scope_fqn,
                            },
                            ..Default::default()
                        },
                    );
                } else {
                    push_symbol_reference(
                        references,
                        format!("::{}", member),
                        kind,
                        reference_range(source, name_node),
                        CollectedReferenceOptions {
                            starts_with_dollar: raw_name.starts_with('$'),
                            receiver: SymbolReferenceReceiver::Unresolved,
                            ..Default::default()
                        },
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
                    CollectedReferenceOptions {
                        receiver,
                        ..Default::default()
                    },
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
                    CollectedReferenceOptions {
                        receiver,
                        ..Default::default()
                    },
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
                    CollectedReferenceOptions {
                        receiver,
                        ..Default::default()
                    },
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
                    CollectedReferenceOptions {
                        allows_global_fallback: unqualified_name_allows_global_fallback(
                            text,
                            UseKind::Function,
                            file_symbols,
                        ),
                        rename_range: Some(terminal_identifier_range(source, func_node)),
                        preserve_spelling_on_rename: explicit_import_alias_covers_entire_name(
                            text,
                            UseKind::Function,
                            file_symbols,
                        ),
                        ..Default::default()
                    },
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
        CollectedReferenceOptions {
            rename_range: Some(terminal_identifier_range(source, node)),
            preserve_spelling_on_rename: explicit_import_alias_covers_entire_name(
                text,
                UseKind::Class,
                file_symbols,
            ),
            ..Default::default()
        },
    );
}

fn push_import_target_reference(
    clause: Node,
    source: &str,
    file_symbols: &FileSymbols,
    references: &mut Vec<SymbolReference>,
) {
    let clause_range = node_range(clause);
    let Some(use_statement) = file_symbols
        .use_statements
        .iter()
        .find(|statement| statement.range == clause_range)
    else {
        return;
    };

    let alias_node = clause.child_by_field_name("alias");
    let mut cursor = clause.walk();
    let Some(target_node) = clause.named_children(&mut cursor).find(|child| {
        Some(child.id()) != alias_node.map(|alias| alias.id())
            && matches!(child.kind(), "name" | "qualified_name" | "namespace_name")
    }) else {
        return;
    };

    let target_kind = match use_statement.kind {
        UseKind::Class => PhpSymbolKind::Class,
        UseKind::Function => PhpSymbolKind::Function,
        UseKind::Constant => PhpSymbolKind::GlobalConstant,
    };
    push_symbol_reference(
        references,
        use_statement.fqn.trim_start_matches('\\').to_string(),
        target_kind,
        reference_range(source, target_node),
        CollectedReferenceOptions {
            rename_range: Some(terminal_identifier_range(source, target_node)),
            is_import_target: true,
            ..Default::default()
        },
    );
}

fn explicit_import_alias_covers_entire_name(
    raw_name: &str,
    use_kind: UseKind,
    file_symbols: &FileSymbols,
) -> bool {
    let raw_name = raw_name.trim();
    if raw_name.starts_with('\\') {
        return false;
    }
    let (first_segment, suffix) = raw_name
        .split_once('\\')
        .map_or((raw_name, None), |(first, rest)| (first, Some(rest)));
    if suffix.is_some() {
        return false;
    }

    file_symbols.use_statements.iter().any(|statement| {
        if statement.kind != use_kind || statement.alias.is_none() {
            return false;
        }
        let alias = statement.alias.as_deref().unwrap_or_default();
        match use_kind {
            UseKind::Class | UseKind::Function => alias.eq_ignore_ascii_case(first_segment),
            UseKind::Constant => alias == first_segment,
        }
    })
}

fn terminal_identifier_range(source: &str, node: Node) -> (u32, u32, u32, u32) {
    reference_range(source, terminal_identifier_node(node).unwrap_or(node))
}

fn terminal_identifier_node<'tree>(node: Node<'tree>) -> Option<Node<'tree>> {
    if node.kind() == "name" {
        return Some(node);
    }

    for index in (0..node.named_child_count()).rev() {
        if let Some(found) = node.named_child(index).and_then(terminal_identifier_node) {
            return Some(found);
        }
    }
    None
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
    if namespace_relative_function_call(node, source).is_some() {
        return;
    }
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
            | "qualified_name"
            | "namespace_name"
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
        CollectedReferenceOptions {
            allows_global_fallback: unqualified_name_allows_global_fallback(
                text,
                UseKind::Constant,
                file_symbols,
            ),
            rename_range: Some(terminal_identifier_range(source, node)),
            preserve_spelling_on_rename: explicit_import_alias_covers_entire_name(
                text,
                UseKind::Constant,
                file_symbols,
            ),
            ..Default::default()
        },
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

struct CollectedReferenceOptions {
    starts_with_dollar: bool,
    allows_global_fallback: bool,
    rename_range: Option<(u32, u32, u32, u32)>,
    preserve_spelling_on_rename: bool,
    is_import_target: bool,
    receiver: SymbolReferenceReceiver,
}

impl Default for CollectedReferenceOptions {
    fn default() -> Self {
        Self {
            starts_with_dollar: false,
            allows_global_fallback: false,
            rename_range: None,
            preserve_spelling_on_rename: false,
            is_import_target: false,
            receiver: SymbolReferenceReceiver::None,
        }
    }
}

fn push_symbol_reference(
    references: &mut Vec<SymbolReference>,
    target_fqn: String,
    target_kind: PhpSymbolKind,
    range: (u32, u32, u32, u32),
    options: CollectedReferenceOptions,
) {
    references.push(SymbolReference {
        target_fqn,
        target_kind,
        range,
        is_declaration: false,
        starts_with_dollar: options.starts_with_dollar,
        allows_global_fallback: options.allows_global_fallback,
        rename_range: options.rename_range,
        preserve_spelling_on_rename: options.preserve_spelling_on_rename,
        is_import_target: options.is_import_target,
        receiver: options.receiver,
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
            if sym.fqn.eq_ignore_ascii_case(target_fqn)
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
    let start = node.start_position();
    let scoped_file_symbols =
        file_symbols.scoped_at_byte_position(start.row as u32, start.column as u32);
    let file_symbols = scoped_file_symbols.as_ref();
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

    if resolved.eq_ignore_ascii_case(target_fqn) {
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
    match name {
        "self" | "static" | "parent" | "$this" | "string" | "int" | "float" | "bool" | "array"
        | "callable" | "iterable" | "object" | "mixed" | "void" | "never" | "null" | "false"
        | "true" => name.to_string(),
        _ => resolve_class_name_pub(name, file_symbols),
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
            if sym.kind == PhpSymbolKind::Function
                && symbol_fqn_eq(&sym.fqn, target_fqn, PhpSymbolKind::Function)
            {
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
    let start = node.start_position();
    let scoped_file_symbols =
        file_symbols.scoped_at_byte_position(start.row as u32, start.column as u32);
    let file_symbols = scoped_file_symbols.as_ref();
    if node.kind() == "namespace_definition" {
        if let Some((function_name, selection)) = namespace_relative_function_call(node, source) {
            let resolved = resolve_function_name_to_fqn(&function_name, file_symbols);
            if resolved_name_matches_target(&resolved, target_fqn, PhpSymbolKind::Function, false) {
                results.push(ReferenceLocation {
                    range: node_range(selection),
                });
            }
        }
    }
    if node.kind() == "function_call_expression" {
        if let Some(func_node) = node.child_by_field_name("function") {
            let text = &source[func_node.byte_range()];
            let resolved = resolve_function_name_to_fqn(text, file_symbols);
            if resolved_name_matches_target(
                &resolved,
                target_fqn,
                PhpSymbolKind::Function,
                unqualified_name_allows_global_fallback(text, UseKind::Function, file_symbols)
                    && !file_symbols.symbols.iter().any(|symbol| {
                        symbol.kind == PhpSymbolKind::Function
                            && symbol_fqn_eq(&symbol.fqn, &resolved, PhpSymbolKind::Function)
                    }),
            ) {
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
    resolve_function_name_pub(name, file_symbols)
}

fn resolved_name_matches_target(
    resolved_fqn: &str,
    target_fqn: &str,
    target_kind: PhpSymbolKind,
    allows_global_fallback: bool,
) -> bool {
    symbol_fqn_eq(resolved_fqn, target_fqn, target_kind)
        || (allows_global_fallback
            && resolved_fqn
                .rsplit_once('\\')
                .is_some_and(|(_, short_name)| symbol_fqn_eq(short_name, target_fqn, target_kind)))
}

/// Resolve a global constant name to FQN.
fn resolve_constant_name_to_fqn(name: &str, file_symbols: &FileSymbols) -> String {
    resolve_constant_name_pub(name, file_symbols)
}

/// Find all references to a class member (method, property, class constant, enum case).
fn find_member_references(
    root: Node,
    source: &str,
    file_symbols: &FileSymbols,
    target_fqn: &str,
    target_kind: PhpSymbolKind,
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
            if symbol_fqn_eq(&sym.fqn, target_fqn, target_kind) {
                results.push(ReferenceLocation {
                    range: sym.selection_range,
                });
            }
        }
    }

    walk_for_member_refs(
        root,
        source,
        file_symbols,
        target_fqn,
        member_name,
        target_kind,
        results,
    );
}

/// Walk CST for member access references.
fn walk_for_member_refs(
    node: Node,
    source: &str,
    file_symbols: &FileSymbols,
    target_fqn: &str,
    member_name: &str,
    target_kind: PhpSymbolKind,
    results: &mut Vec<ReferenceLocation>,
) {
    let start = node.start_position();
    let scoped_file_symbols =
        file_symbols.scoped_at_byte_position(start.row as u32, start.column as u32);
    let file_symbols = scoped_file_symbols.as_ref();
    let kind = node.kind();

    match kind {
        // $obj->property (and callable-like member access without invocation)
        "member_access_expression" | "nullsafe_member_access_expression" => {
            if target_kind != PhpSymbolKind::Property {
                // Method targets must not match property-access syntax.
            } else if let Some(name_node) = node.child_by_field_name("name") {
                let text = &source[name_node.byte_range()];
                if member_reference_name_matches(text, member_name, target_kind) {
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
        "member_call_expression" | "nullsafe_member_call_expression" => {
            if target_kind != PhpSymbolKind::Method {
                // Property targets should not match method calls with the same short name.
            } else if let Some(name_node) = node.child_by_field_name("name") {
                let text = &source[name_node.byte_range()];
                if member_reference_name_matches(text, member_name, target_kind) {
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
            if target_kind != PhpSymbolKind::Method {
                // Constant/property targets should not match scoped method calls.
            } else if let Some(name_node) = node.child_by_field_name("name") {
                let text = &source[name_node.byte_range()];
                if member_reference_name_matches(text, member_name, target_kind) {
                    // For scoped access, also check that the scope resolves to the right class
                    if let Some(scope_node) = node.child_by_field_name("scope") {
                        let scope_text = &source[scope_node.byte_range()];
                        let scope_fqn = resolve_name_to_fqn(scope_text, file_symbols);
                        let expected_class = &target_fqn[..target_fqn.rfind("::").unwrap_or(0)];

                        if scope_fqn.eq_ignore_ascii_case(expected_class)
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
                let syntax_matches_target = match target_kind {
                    PhpSymbolKind::Property => text.starts_with('$'),
                    PhpSymbolKind::ClassConstant | PhpSymbolKind::EnumCase => {
                        !text.starts_with('$')
                    }
                    _ => false,
                };
                if syntax_matches_target
                    && member_reference_name_matches(text, member_name, target_kind)
                {
                    // For scoped access, also check that the scope resolves to the right class
                    if let Some(scope_node) = node.child_by_field_name("scope") {
                        let scope_text = &source[scope_node.byte_range()];
                        let scope_fqn = resolve_name_to_fqn(scope_text, file_symbols);
                        let expected_class = &target_fqn[..target_fqn.rfind("::").unwrap_or(0)];

                        if scope_fqn.eq_ignore_ascii_case(expected_class)
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
            if !matches!(
                target_kind,
                PhpSymbolKind::ClassConstant | PhpSymbolKind::EnumCase
            ) {
                // Method/property targets should not match class constant access.
            } else if let (Some(scope_node), Some(name_node)) =
                (node.named_child(0), node.named_child(1))
            {
                let text = &source[name_node.byte_range()];
                if member_reference_name_matches(text, member_name, target_kind) {
                    let scope_text = &source[scope_node.byte_range()];
                    let scope_fqn = resolve_name_to_fqn(scope_text, file_symbols);
                    let expected_class = &target_fqn[..target_fqn.rfind("::").unwrap_or(0)];

                    if scope_fqn.eq_ignore_ascii_case(expected_class)
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
            file_symbols,
            target_fqn,
            member_name,
            target_kind,
            results,
        );
    }
}

fn member_reference_name_matches(
    reference_member: &str,
    target_member: &str,
    target_kind: PhpSymbolKind,
) -> bool {
    match target_kind {
        PhpSymbolKind::Method => reference_member.eq_ignore_ascii_case(target_member),
        PhpSymbolKind::Property => {
            reference_member.trim_start_matches('$') == target_member.trim_start_matches('$')
        }
        PhpSymbolKind::ClassConstant | PhpSymbolKind::EnumCase => reference_member == target_member,
        _ => false,
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
            if sym.kind == PhpSymbolKind::GlobalConstant
                && symbol_fqn_eq(&sym.fqn, target_fqn, PhpSymbolKind::GlobalConstant)
            {
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
    let start = node.start_position();
    let scoped_file_symbols =
        file_symbols.scoped_at_byte_position(start.row as u32, start.column as u32);
    let file_symbols = scoped_file_symbols.as_ref();

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
            && parent_kind != "qualified_name"
            && parent_kind != "namespace_name"
            && parent_kind != "use_declaration"
            && parent_kind != "namespace_use_clause"
        {
            let text = &source[node.byte_range()];
            // Try resolving as constant
            let resolved = resolve_constant_name_to_fqn(text, file_symbols);
            if resolved_name_matches_target(
                &resolved,
                target_fqn,
                PhpSymbolKind::GlobalConstant,
                unqualified_name_allows_global_fallback(text, UseKind::Constant, file_symbols)
                    && !file_symbols.symbols.iter().any(|symbol| {
                        symbol.kind == PhpSymbolKind::GlobalConstant
                            && symbol_fqn_eq(&symbol.fqn, &resolved, PhpSymbolKind::GlobalConstant)
                    }),
            ) {
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
    if ancestor_field_contains(node, "foreach_statement", &["key", "value"])
        || is_foreach_header_declared_variable(node, source)
    {
        return true;
    }

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
            allows_global_fallback: false,
            rename_range: None,
            preserve_spelling_on_rename: false,
            is_import_target: false,
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
    fn test_collected_reference_ranges_are_utf16_after_emoji() {
        let code = "<?php\nnamespace App;\nclass Foo {}\n$emoji = \"😀\"; new Foo();\n";
        let refs = collect_refs(code);

        assert!(
            refs.iter().any(|reference| {
                reference.target_fqn == "App\\Foo"
                    && !reference.is_declaration
                    && reference.range == (3, 19, 3, 22)
            }),
            "class reference after emoji should use UTF-16 range, got {refs:?}"
        );
    }

    #[test]
    fn test_variable_reference_ranges_are_utf16_after_emoji() {
        let code = "<?php\n$emoji = \"😀\"; $target = 1; echo $target;\n";
        let target_byte_col = code.lines().nth(1).unwrap().find("$target").unwrap() as u32;
        let refs = find_var_refs_at(code, 1, target_byte_col, true);

        assert!(
            refs.iter()
                .any(|reference| reference.range == (1, 17, 1, 24)),
            "declaration range should use UTF-16 columns, got {refs:?}"
        );
        assert!(
            refs.iter()
                .any(|reference| reference.range == (1, 35, 1, 42)),
            "usage range should use UTF-16 columns, got {refs:?}"
        );
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
    fn test_find_class_and_function_references_case_insensitively() {
        let code = r#"<?php
namespace App;
class MixedCaseClass {}
function MixedCaseFunction() {}
new mixedcaseclass();
MIXEDCASEFUNCTION();
"#;

        let class_refs = find_refs(code, r"app\MIXEDCASECLASS", PhpSymbolKind::Class);
        assert_eq!(class_refs.len(), 2);

        let function_refs = find_refs(code, r"APP\mixedcasefunction", PhpSymbolKind::Function);
        assert_eq!(function_refs.len(), 2);
    }

    #[test]
    fn test_collect_references_uses_each_namespace_block_import_scope() {
        let code = r#"<?php
namespace First {
    use Vendor\First\Service as Shared;
    new Shared();
}
namespace Second {
    use Vendor\Second\Service as Shared;
    new Shared();
}
"#;
        let refs = collect_refs(code);

        assert!(refs.iter().any(|reference| {
            reference.target_fqn == r"Vendor\First\Service" && reference.range.0 == 3
        }));
        assert!(refs.iter().any(|reference| {
            reference.target_fqn == r"Vendor\Second\Service" && reference.range.0 == 7
        }));
    }

    #[test]
    fn test_collect_references_resolves_namespace_relative_and_qualified_functions() {
        let code = r#"<?php
namespace App\Feature;
namespace\helper();
Support\other();
"#;
        let refs = collect_refs(code);

        assert!(refs.iter().any(|reference| {
            reference.target_kind == PhpSymbolKind::Function
                && reference.target_fqn == r"App\Feature\helper"
                && !reference.allows_global_fallback
        }));
        assert!(refs.iter().any(|reference| {
            reference.target_kind == PhpSymbolKind::Function
                && reference.target_fqn == r"App\Feature\Support\other"
                && !reference.allows_global_fallback
        }));
    }

    #[test]
    fn test_global_function_reference_fallback_requires_unqualified_source_name() {
        let code = r#"<?php
namespace App;
strlen('plain');
Support\strlen('qualified');
"#;

        let global_refs = find_refs(code, "strlen", PhpSymbolKind::Function);
        assert_eq!(
            global_refs
                .iter()
                .map(|reference| reference.range.0)
                .collect::<Vec<_>>(),
            vec![2],
            "only the unqualified call may match the canonical global function"
        );

        let collected = collect_refs(code);
        let plain = collected
            .iter()
            .find(|reference| {
                reference.target_kind == PhpSymbolKind::Function && reference.range.0 == 2
            })
            .expect("unqualified function reference should be collected");
        assert_eq!(plain.target_fqn, r"App\strlen");
        assert!(plain.allows_global_fallback);

        let qualified = collected
            .iter()
            .find(|reference| {
                reference.target_kind == PhpSymbolKind::Function && reference.range.0 == 3
            })
            .expect("qualified function reference should be collected");
        assert!(!qualified.allows_global_fallback);

        let explicit = collect_refs(
            r#"<?php
namespace App\Feature;
namespace\strlen('explicit');
Support\other();
"#,
        );
        let explicit = explicit
            .iter()
            .find(|reference| reference.target_kind == PhpSymbolKind::Function)
            .expect("namespace-relative function reference should be collected");
        assert!(!explicit.allows_global_fallback);
    }

    #[test]
    fn test_collect_references_expands_namespace_aliases_for_qualified_symbols() {
        let code = r#"<?php
namespace App;
use Vendor\Package as Lib;
use const Vendor\FLAG as ConstAlias;
lib\helper();
echo LIB\FLAG;
echo ConstAlias\CHILD;
"#;
        let refs = collect_refs(code);

        assert!(refs.iter().any(|reference| {
            reference.target_kind == PhpSymbolKind::Function
                && reference.target_fqn == r"Vendor\Package\helper"
        }));
        assert!(refs.iter().any(|reference| {
            reference.target_kind == PhpSymbolKind::GlobalConstant
                && reference.target_fqn == r"Vendor\Package\FLAG"
        }));
        assert!(refs.iter().any(|reference| {
            reference.target_kind == PhpSymbolKind::GlobalConstant
                && reference.target_fqn == r"App\ConstAlias\CHILD"
        }));
        assert!(!refs.iter().any(|reference| {
            reference.target_kind == PhpSymbolKind::GlobalConstant
                && reference.target_fqn == r"Vendor\FLAG\CHILD"
        }));
    }

    #[test]
    fn test_global_constant_references_ignore_namespace_case_only() {
        let code = r#"<?php
echo \Vendor\Package\FLAG;
"#;

        assert!(!find_refs(code, r"vendor\package\FLAG", PhpSymbolKind::GlobalConstant).is_empty());
        assert!(find_refs(code, r"vendor\package\flag", PhpSymbolKind::GlobalConstant).is_empty());
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
    fn test_find_method_references_case_insensitively() {
        let code = r#"<?php
namespace App;

class Foo {
    public static function propFind() {}

    public function run(): void {
        self::propfind();
        $this->PROPFIND();
        $this->propFind;
    }
}

Foo::PROPFIND();
"#;
        let refs = find_refs(code, "App\\Foo::propFind", PhpSymbolKind::Method);

        assert_eq!(
            refs.len(),
            4,
            "Method references should include declaration and differently-cased calls, but not property access"
        );
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
    fn test_import_references_preserve_explicit_aliases_for_rename() {
        let code = r#"<?php
namespace App;

use Vendor\{
    Service as Alias,
    ImplicitService,
    function helper as call_it,
    function plain_helper,
    const FLAG as LOCAL_FLAG,
    const OTHER
};

new Alias();
new ImplicitService();
call_it();
plain_helper();
echo LOCAL_FLAG;
echo OTHER;
"#;
        let refs = collect_refs(code);

        for (target_fqn, target_kind) in [
            (r"Vendor\Service", PhpSymbolKind::Class),
            (r"Vendor\helper", PhpSymbolKind::Function),
            (r"Vendor\FLAG", PhpSymbolKind::GlobalConstant),
        ] {
            let matching: Vec<_> = refs
                .iter()
                .filter(|reference| {
                    reference.target_fqn == target_fqn && reference.target_kind == target_kind
                })
                .collect();
            assert_eq!(
                matching.len(),
                2,
                "expected import target plus aliased usage for {target_fqn}: {matching:?}"
            );
            assert_eq!(
                matching
                    .iter()
                    .filter(|reference| reference.preserve_spelling_on_rename)
                    .count(),
                1,
                "only the explicit alias usage should preserve spelling: {matching:?}"
            );
            assert_eq!(
                matching
                    .iter()
                    .filter(|reference| reference.is_import_target)
                    .count(),
                1,
                "one reference must identify the import target: {matching:?}"
            );
            assert!(matching
                .iter()
                .all(|reference| reference.rename_range.is_some()));
        }

        for (target_fqn, target_kind) in [
            (r"Vendor\ImplicitService", PhpSymbolKind::Class),
            (r"Vendor\plain_helper", PhpSymbolKind::Function),
            (r"Vendor\OTHER", PhpSymbolKind::GlobalConstant),
        ] {
            let matching: Vec<_> = refs
                .iter()
                .filter(|reference| {
                    reference.target_fqn == target_fqn && reference.target_kind == target_kind
                })
                .collect();
            assert_eq!(
                matching.len(),
                2,
                "expected import target plus implicit usage for {target_fqn}: {matching:?}"
            );
            assert!(
                matching
                    .iter()
                    .all(|reference| !reference.preserve_spelling_on_rename),
                "implicit aliases must be renamed with their target: {matching:?}"
            );
            assert_eq!(
                matching
                    .iter()
                    .filter(|reference| reference.is_import_target)
                    .count(),
                1,
                "one reference must identify the import target: {matching:?}"
            );
            assert!(matching
                .iter()
                .all(|reference| reference.rename_range.is_some()));
        }
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
    fn test_find_variable_references_marks_foreach_wrapped_values_as_declarations() {
        let code = r#"<?php
function run(array $items): void {
    foreach ($items as &$value) {
        echo $value;
    }
}
"#;
        let (line, col) = find_line_col(code, "echo $value;");
        let refs = find_var_refs_at(code, line, col + "echo ".len() as u32 + 1, true);
        assert_eq!(
            refs.len(),
            2,
            "by-reference foreach value should count as declaration + body usage"
        );

        let refs_no_decl = find_var_refs_at(code, line, col + "echo ".len() as u32 + 1, false);
        assert_eq!(
            refs_no_decl.len(),
            1,
            "by-reference foreach declaration should not be counted as a read usage"
        );
    }

    #[test]
    fn test_find_variable_references_marks_foreach_key_and_value_as_declarations() {
        let code = r#"<?php
function run(array $items): void {
    foreach ($items as $key => $value) {
        echo $key;
        echo $value;
    }
}
"#;
        for (needle, var_name) in [("echo $key;", "$key"), ("echo $value;", "$value")] {
            let (line, col) = find_line_col(code, needle);
            let refs = find_var_refs_at(code, line, col + "echo ".len() as u32 + 1, true);
            assert_eq!(
                refs.len(),
                2,
                "foreach {var_name} should count as declaration + body usage"
            );

            let refs_no_decl = find_var_refs_at(code, line, col + "echo ".len() as u32 + 1, false);
            assert_eq!(
                refs_no_decl.len(),
                1,
                "foreach {var_name} declaration should be excluded when requested"
            );
        }
    }

    #[test]
    fn test_find_variable_references_marks_foreach_destructuring_values_as_declarations() {
        let code = r#"<?php
function run(array $rows): void {
    foreach ($rows as ['id' => $id, 'name' => $name]) {
        echo $id;
        echo $name;
    }
}
"#;
        for (needle, var_name) in [("echo $id;", "$id"), ("echo $name;", "$name")] {
            let (line, col) = find_line_col(code, needle);
            let refs = find_var_refs_at(code, line, col + "echo ".len() as u32 + 1, true);
            assert_eq!(
                refs.len(),
                2,
                "foreach destructured {var_name} should count as declaration + body usage"
            );

            let refs_no_decl = find_var_refs_at(code, line, col + "echo ".len() as u32 + 1, false);
            assert_eq!(
                refs_no_decl.len(),
                1,
                "foreach destructured {var_name} declaration should be excluded when requested"
            );
        }
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
