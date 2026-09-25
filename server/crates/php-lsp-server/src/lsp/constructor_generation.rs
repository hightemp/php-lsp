//! Conservative parent-constructor planning shared by proposal and lazy resolve.

use super::*;
use php_lsp_types::{FileSymbols, PhpSymbolKind, SymbolInfo, Visibility};

pub(crate) struct ParentConstructorPlan {
    pub params: Vec<ParentParameter>,
    pub visibility: Visibility,
}

pub(crate) struct ParentParameter {
    pub name: String,
    pub declaration: String,
    pub optional: bool,
    pub variadic: bool,
}

pub(crate) struct ConstructorGenerationPlan {
    pub parent: Option<ParentConstructorPlan>,
}

impl PhpLspBackend {
    pub(super) async fn constructor_generation_plan(
        &self,
        request: &WorkspaceRequestContext,
        source: &str,
        file_symbols: &FileSymbols,
        class: &SymbolInfo,
    ) -> Option<ConstructorGenerationPlan> {
        let properties = constructor_generation_properties(source, file_symbols, &class.fqn);
        if properties.is_empty()
            || direct_method_name_exists(file_symbols, &class.fqn, "__construct")
        {
            return None;
        }

        let index = request.index(&self.index);
        let revision = index.revision_snapshot();
        let mut current = class.clone();
        let mut current_file = file_symbols.clone();
        let mut visited = HashSet::new();
        let mut inherited = None;
        loop {
            if current.kind != PhpSymbolKind::Class
                || !visited.insert(current.fqn.to_ascii_lowercase())
                // Trait adaptations and interface constructor contracts require a
                // separate proof. An incomplete hierarchy must never mean "no ctor".
                || !current.traits.is_empty()
                || !current.implements.is_empty()
                || current.modifiers.is_builtin
            {
                return None;
            }
            if !current.fqn.eq_ignore_ascii_case(&class.fqn)
                && direct_property_symbols_from_file(&current_file, &current.fqn)
                    .iter()
                    .any(|ancestor| {
                        ancestor.visibility != Visibility::Private
                            && properties.iter().any(|property| {
                                property.symbol.name == ancestor.name
                                    && (property.symbol.modifiers.is_readonly
                                        || class.modifiers.is_readonly
                                        || ancestor.modifiers.is_readonly
                                        || current.modifiers.is_readonly)
                            })
                    })
            {
                // Redeclarations of non-private properties share storage. The
                // parent call may already initialize that readonly slot.
                return None;
            }
            let methods = direct_method_symbols_from_file(&current_file, &current.fqn);
            // An abstract ancestor's constructor contract still constrains the
            // child after an intermediate class provides a concrete override.
            if methods.iter().any(|method| {
                method.name.eq_ignore_ascii_case("__construct") && method.modifiers.is_abstract
            }) {
                return None;
            }
            if !request.runtime_config().php_version.at_least(8, 0)
                && !current.fqn.contains('\\')
                && methods
                    .iter()
                    .any(|method| method.name.eq_ignore_ascii_case(&current.name))
            {
                return None;
            }
            if inherited.is_none() && !current.fqn.eq_ignore_ascii_case(&class.fqn) {
                let constructors = methods
                    .into_iter()
                    .filter(|method| method.name.eq_ignore_ascii_case("__construct"))
                    .collect::<Vec<_>>();
                if constructors.len() > 1 {
                    return None;
                }
                if let Some(method) = constructors.first() {
                    if method.modifiers.is_final
                        || method.modifiers.is_abstract
                        || method.modifiers.is_static
                        || method.visibility == Visibility::Private
                    {
                        return None;
                    }
                    inherited = Some((current.clone(), (*method).clone()));
                }
            }
            match current.extends.as_slice() {
                [] => break,
                [parent] => {
                    current = file_symbols
                        .symbols
                        .iter()
                        .find(|symbol| {
                            symbol.kind == PhpSymbolKind::Class
                                && symbol.fqn.eq_ignore_ascii_case(parent)
                        })
                        .cloned()
                        .or_else(|| index.get_type(parent).map(|symbol| (*symbol).clone()))?;
                    if current.modifiers.is_final {
                        return None;
                    }
                    current_file = if current.uri == class.uri {
                        file_symbols.clone()
                    } else {
                        index
                            .read()
                            .file_symbols()
                            .get(&current.uri)?
                            .value()
                            .as_ref()
                            .clone()
                    };
                }
                _ => return None,
            }
        }

        let parent = if let Some((owner, method)) = inherited {
            let parent_source = if method.uri == class.uri {
                source.to_string()
            } else {
                self.source_for_uri_in_request(
                    request,
                    &method.uri,
                    "parent constructor source read",
                )
                .await?
            };
            let php_version = request.runtime_config().php_version;
            Some(
                tokio::task::spawn_blocking(move || {
                    parent_constructor_plan(&parent_source, &owner, &method, php_version)
                })
                .await
                .ok()??,
            )
        } else {
            None
        };
        if index.revision_snapshot() != revision {
            return None;
        }
        if let Some(parent) = &parent {
            // Sharing a parameter with a property silently conflates two inputs.
            if parent.params.iter().any(|param| {
                properties
                    .iter()
                    .any(|property| property.symbol.name == param.name)
            }) {
                return None;
            }
        }
        Some(ConstructorGenerationPlan { parent })
    }
}

fn parent_constructor_plan(
    source: &str,
    owner: &SymbolInfo,
    method: &SymbolInfo,
    php_version: PhpVersion,
) -> Option<ParentConstructorPlan> {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let tree = parser.tree()?;
    if tree.root_node().has_error() {
        return None;
    }
    let symbols = extract_file_symbols(tree, source, &method.uri);
    let actual_owner = symbols
        .symbols
        .iter()
        .find(|symbol| symbol.fqn == owner.fqn)?;
    let actual_method = constructor_symbol(&symbols, &owner.fqn)?;
    if actual_owner.extends != owner.extends
        || actual_owner.traits != owner.traits
        || actual_owner.implements != owner.implements
        || actual_owner.modifiers.is_readonly != owner.modifiers.is_readonly
        || actual_method.range != method.range
        || actual_method.modifiers.is_final
        || actual_method.modifiers.is_abstract
        || actual_method.modifiers.is_static
        || actual_method.visibility != method.visibility
    {
        return None;
    }
    let (start, end) = byte_offsets_for_range(source, method.range)?;
    let node = node_for_exact_byte_span(tree.root_node(), start, end, "method_declaration")?;
    if callable_returns_by_reference(node) || node.child_by_field_name("return_type").is_some() {
        return None;
    }
    let scope = symbols.scoped_at_byte_position(method.range.0, method.range.1);
    if constructor_observes_arguments(node.child_by_field_name("body")?, source, &scope) {
        return None;
    }
    let parameters = node.child_by_field_name("parameters")?;
    let mut cursor = parameters.walk();
    let mut params = Vec::new();
    let mut names = HashSet::new();
    let mut saw_optional = false;
    let mut saw_variadic = false;
    for parameter in parameters
        .named_children(&mut cursor)
        .filter(|node| !node.is_extra())
    {
        if !matches!(
            parameter.kind(),
            "simple_parameter" | "variadic_parameter" | "property_promotion_parameter"
        ) || parameter.child_by_field_name("attributes").is_some()
            || saw_variadic
        {
            return None;
        }
        let mut name_node = parameter.child_by_field_name("name")?;
        let by_ref = name_node.kind() == "by_ref"
            || parameter
                .child_by_field_name("reference_modifier")
                .is_some();
        if name_node.kind() == "by_ref" {
            name_node = name_node.named_child(0)?;
        }
        let name = source.get(name_node.byte_range())?.strip_prefix('$')?;
        if !php_identifier_part_is_valid(name) || !names.insert(name.to_string()) || name == "this"
        {
            return None;
        }
        let variadic = parameter.kind() == "variadic_parameter";
        let default = parameter.child_by_field_name("default_value");
        if default.is_some_and(|value| !constructor_default_is_portable(value))
            || (saw_optional && default.is_none() && !variadic)
        {
            return None;
        }
        let mut declaration = String::new();
        if let Some(type_node) = parameter.child_by_field_name("type") {
            declaration.push_str(&constructor_parameter_type(
                type_node,
                source,
                &scope,
                owner,
                php_version,
            )?);
            declaration.push(' ');
        }
        if by_ref {
            declaration.push('&');
        }
        if variadic {
            declaration.push_str("...");
        }
        declaration.push('$');
        declaration.push_str(name);
        if let Some(value) = default {
            declaration.push_str(" = ");
            declaration.push_str(source.get(value.byte_range())?);
        }
        saw_optional |= default.is_some();
        saw_variadic |= variadic;
        params.push(ParentParameter {
            name: name.to_string(),
            declaration,
            optional: default.is_some(),
            variadic,
        });
    }
    Some(ParentConstructorPlan {
        params,
        visibility: method.visibility,
    })
}

fn constructor_observes_arguments(
    body: tree_sitter::Node,
    source: &str,
    symbols: &FileSymbols,
) -> bool {
    // Explicit forwarding supplies optional defaults, changing the observable
    // argument count. Indirect calls and evaluated/included code cannot be
    // proven free of these observers either.
    let observes = |name: &str| {
        matches!(
            name,
            "func_num_args"
                | "func_get_args"
                | "func_get_arg"
                | "debug_backtrace"
                | "debug_print_backtrace"
                | "call_user_func"
                | "call_user_func_array"
                | "eval"
        )
    };
    let aliases = symbols
        .use_statements
        .iter()
        .filter(|statement| statement.kind == php_lsp_types::UseKind::Function)
        .filter(|statement| observes(&short_name(&statement.fqn).to_ascii_lowercase()))
        .map(|statement| existing_use_alias(statement).to_ascii_lowercase())
        .collect::<HashSet<_>>();
    let mut pending = vec![body];
    while let Some(node) = pending.pop() {
        if matches!(
            node.kind(),
            "include_expression"
                | "include_once_expression"
                | "require_expression"
                | "require_once_expression"
        ) {
            return true;
        }
        if node.kind() == "function_call_expression" {
            let Some(function) = node.child_by_field_name("function") else {
                return true;
            };
            if !matches!(function.kind(), "name" | "qualified_name") {
                return true;
            }
            let name = source
                .get(function.byte_range())
                .unwrap_or("")
                .trim_start_matches('\\')
                .to_ascii_lowercase();
            if observes(short_name(&name)) || aliases.contains(&name) {
                return true;
            }
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
}

fn constructor_default_is_portable(node: tree_sitter::Node) -> bool {
    // Names, class constants, magic constants and `new` depend on the declaring
    // scope. Only copy literal defaults whose meaning cannot change in a child.
    match node.kind() {
        "integer" | "float" | "boolean" | "null" => true,
        "string" | "encapsed_string" => is_static_string_literal_node(node),
        "unary_op_expression" => node
            .child_by_field_name("argument")
            .is_some_and(|child| matches!(child.kind(), "integer" | "float")),
        "array_creation_expression" | "array_element_initializer" => {
            let mut cursor = node.walk();
            let portable = node
                .named_children(&mut cursor)
                .filter(|child| !child.is_extra())
                .all(constructor_default_is_portable);
            portable
        }
        _ => false,
    }
}

fn constructor_parameter_type(
    node: tree_sitter::Node,
    source: &str,
    symbols: &FileSymbols,
    owner: &SymbolInfo,
    php_version: PhpVersion,
) -> Option<String> {
    match node.kind() {
        "primitive_type" | "named_type" => {
            let raw = source.get(node.byte_range())?.trim();
            let lower = raw.to_ascii_lowercase();
            match lower.as_str() {
                "self" => Some(format!("\\{}", owner.fqn)),
                "parent" => Some(format!("\\{}", owner.extends.first()?)),
                "int" | "float" | "bool" | "string" | "array" | "object" | "callable"
                | "iterable" => Some(lower),
                "mixed" if php_version.at_least(8, 0) => Some(lower),
                "true" | "false" | "null" if php_version.at_least(8, 2) => Some(lower),
                "void" | "never" | "static" | "mixed" | "true" | "false" | "null" => None,
                _ => {
                    simple_native_type_hint_text(raw)?;
                    Some(format!(
                        "\\{}",
                        resolve_class_name_pub(raw, symbols).trim_start_matches('\\')
                    ))
                }
            }
        }
        "optional_type" => Some(format!(
            "?{}",
            constructor_parameter_type(
                first_non_extra_named_child(node)?,
                source,
                symbols,
                owner,
                php_version
            )?
        )),
        "union_type" if php_version.at_least(8, 0) => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .filter(|child| !child.is_extra())
                .map(|child| constructor_parameter_type(child, source, symbols, owner, php_version))
                .collect::<Option<Vec<_>>>()
                .map(|parts| parts.join("|"))
        }
        "intersection_type" if php_version.at_least(8, 1) => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .filter(|child| !child.is_extra())
                .map(|child| constructor_parameter_type(child, source, symbols, owner, php_version))
                .collect::<Option<Vec<_>>>()
                .map(|parts| parts.join("&"))
        }
        _ => None,
    }
}
