//! Rename LSP handlers extracted from `server.rs`.

use super::super::*;

impl PhpLspBackend {
    pub(crate) async fn lsp_rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri_str = params
            .text_document_position
            .text_document
            .uri
            .as_str()
            .to_string();
        let pos = params.text_document_position.position;
        let new_name = &params.new_name;

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

        let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
            self.resolve_member_type(class_fqn, member_name)
        };
        let callable_param_resolver = |ctx: CallableParameterContext<'_>| {
            resolve_callable_parameter_type_from_index(&self.index, &file_symbols, ctx)
        };
        let sym = match symbol_at_position_with_resolvers(
            tree,
            &source,
            pos.line,
            byte_col,
            &file_symbols,
            Some(&resolver),
            Some(&callable_param_resolver),
        ) {
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
                tower_lsp::jsonrpc::Error::invalid_params(invalid_rename_name_message(
                    RenameNameKind::Variable,
                ))
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

        let resolved_for_rename = self.resolve_fqn_with_fallback(&sym.fqn, sym.ref_kind);
        let phpdoc_virtual_member = phpdoc_virtual_member_for_symbol(&self.index, &sym);
        if phpdoc_virtual_member.as_ref().is_some_and(|member| {
            should_reject_phpdoc_virtual_member_rename(&resolved_for_rename, member)
        }) {
            return Err(tower_lsp::jsonrpc::Error::invalid_params(
                "Cannot rename PHPDoc virtual members",
            ));
        }
        if resolved_for_rename.is_none()
            && framework_virtual_member_for_symbol(
                &self.index,
                &sym,
                Some(&uri_str),
                Some(&file_symbols),
                Some(&source),
            )
            .is_some()
        {
            return Err(tower_lsp::jsonrpc::Error::invalid_params(
                "Cannot rename framework virtual members",
            ));
        }

        // Resolve symbol under cursor
        let (target_fqn, target_kind, _old_name) = {
            if let Some(resolved) = resolved_for_rename {
                (resolved.fqn.clone(), resolved.kind, sym.name.clone())
            } else {
                let kind = match rename_target_kind_from_ref_kind(sym.ref_kind) {
                    Some(kind) => kind,
                    None => return Ok(None),
                };
                (sym.fqn.clone(), kind, sym.name.clone())
            }
        };

        if !is_supported_rename_kind(target_kind) {
            return Ok(None);
        }

        if is_member_rename_kind(target_kind) && !target_fqn.contains("::") {
            return Err(tower_lsp::jsonrpc::Error::invalid_params(
                "Cannot safely rename member without a resolved receiver type",
            ));
        }

        let normalized_new_name =
            normalize_symbol_new_name(target_kind, new_name).ok_or_else(|| {
                tower_lsp::jsonrpc::Error::invalid_params(invalid_rename_name_message(
                    RenameNameKind::Symbol(target_kind),
                ))
            })?;
        let property_new_name = if target_kind == php_lsp_types::PhpSymbolKind::Property {
            Some(normalized_new_name.as_str())
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
        let indexed_files: Vec<_> = self
            .index
            .file_references
            .iter()
            .map(|entry| entry.key().clone())
            .collect();

        for (scanned_files, file_uri) in indexed_files.into_iter().enumerate() {
            cooperative_heavy_request_yield(scanned_files).await;
            let refs = self.references_for_file(&file_uri, &target_fqn, target_kind, true);

            if !refs.is_empty() {
                if let Ok(uri) = file_uri.parse::<Uri>() {
                    let edits: Vec<TextEdit> = refs
                        .into_iter()
                        .map(|r| TextEdit {
                            range: range_from_lsp_tuple(r.range),
                            new_text: if target_kind == php_lsp_types::PhpSymbolKind::Property
                                && r.starts_with_dollar
                            {
                                format!(
                                    "${}",
                                    property_new_name.unwrap_or(new_name.trim_start_matches('$'))
                                )
                            } else {
                                property_new_name
                                    .unwrap_or(normalized_new_name.as_str())
                                    .to_string()
                            },
                        })
                        .collect();
                    changes.entry(uri).or_default().extend(edits);
                }
            }
        }

        if changes.is_empty() && is_member_rename_kind(target_kind) {
            Err(tower_lsp::jsonrpc::Error::invalid_params(
                "Cannot safely rename member because no exact references were found",
            ))
        } else if changes.is_empty() {
            Ok(None)
        } else {
            Ok(Some(WorkspaceEdit {
                changes: Some(changes),
                document_changes: None,
                change_annotations: None,
            }))
        }
    }

    pub(crate) async fn lsp_prepare_rename(
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

        let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
            self.resolve_member_type(class_fqn, member_name)
        };
        let callable_param_resolver = |ctx: CallableParameterContext<'_>| {
            resolve_callable_parameter_type_from_index(&self.index, &file_symbols, ctx)
        };

        match symbol_at_position_with_resolvers(
            tree,
            &source,
            pos.line,
            byte_col,
            &file_symbols,
            Some(&resolver),
            Some(&callable_param_resolver),
        ) {
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
                if is_member_ref_kind(sym.ref_kind) && !sym.fqn.contains("::") {
                    return Ok(None);
                }

                // Don't rename built-in or PHPDoc virtual symbols
                let resolved = self.resolve_fqn_with_fallback(&sym.fqn, sym.ref_kind);
                let phpdoc_virtual_member = phpdoc_virtual_member_for_symbol(&self.index, &sym);
                if phpdoc_virtual_member.as_ref().is_some_and(|member| {
                    should_reject_phpdoc_virtual_member_rename(&resolved, member)
                }) {
                    return Ok(None);
                }
                if resolved.is_none()
                    && framework_virtual_member_for_symbol(
                        &self.index,
                        &sym,
                        Some(&uri_str),
                        Some(&file_symbols),
                        Some(&source),
                    )
                    .is_some()
                {
                    return Ok(None);
                }
                let target_kind = if let Some(resolved) = resolved {
                    if resolved.modifiers.is_builtin {
                        return Ok(None);
                    }
                    resolved.kind
                } else {
                    match rename_target_kind_from_ref_kind(sym.ref_kind) {
                        Some(kind) => kind,
                        None => return Ok(None),
                    }
                };

                if !is_supported_rename_kind(target_kind) {
                    return Ok(None);
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
}

#[derive(Debug, Clone, Copy)]
enum RenameNameKind {
    Symbol(php_lsp_types::PhpSymbolKind),
    Variable,
}

pub(in crate::server) fn normalize_variable_new_name(new_name: &str) -> Option<String> {
    let raw_name = exact_trimmed_new_name(new_name)?;
    let raw = raw_name.strip_prefix('$').unwrap_or(raw_name);
    if raw.is_empty() {
        return None;
    }
    if !is_php_identifier(raw) {
        return None;
    }

    let normalized = format!("${}", raw);
    is_renameable_variable(&normalized).then_some(normalized)
}

pub(in crate::server) fn normalize_property_new_name(new_name: &str) -> Option<String> {
    let var = normalize_variable_new_name(new_name)?;
    Some(var.trim_start_matches('$').to_string())
}

fn normalize_symbol_new_name(kind: php_lsp_types::PhpSymbolKind, new_name: &str) -> Option<String> {
    if kind == php_lsp_types::PhpSymbolKind::Property {
        return normalize_property_new_name(new_name);
    }

    let name = exact_trimmed_new_name(new_name)?;
    if name.starts_with('$') || !is_php_identifier(name) || is_php_reserved_identifier(name) {
        return None;
    }

    Some(name.to_string())
}

fn exact_trimmed_new_name(new_name: &str) -> Option<&str> {
    let trimmed = new_name.trim();
    if trimmed.is_empty() || trimmed.len() != new_name.len() {
        return None;
    }
    Some(trimmed)
}

fn is_php_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    is_php_identifier_start(first) && chars.all(is_php_identifier_continue)
}

fn is_php_identifier_start(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphabetic() || !ch.is_ascii()
}

fn is_php_identifier_continue(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric() || !ch.is_ascii()
}

fn is_php_reserved_identifier(name: &str) -> bool {
    PHP_RESERVED_IDENTIFIERS
        .iter()
        .any(|keyword| keyword.eq_ignore_ascii_case(name))
}

const PHP_RESERVED_IDENTIFIERS: &[&str] = &[
    "__halt_compiler",
    "abstract",
    "and",
    "array",
    "as",
    "break",
    "bool",
    "callable",
    "case",
    "catch",
    "class",
    "clone",
    "const",
    "continue",
    "declare",
    "default",
    "die",
    "do",
    "echo",
    "else",
    "elseif",
    "empty",
    "enddeclare",
    "endfor",
    "endforeach",
    "endif",
    "endswitch",
    "endwhile",
    "enum",
    "eval",
    "exit",
    "extends",
    "false",
    "final",
    "finally",
    "fn",
    "for",
    "foreach",
    "float",
    "from",
    "function",
    "global",
    "goto",
    "if",
    "implements",
    "include",
    "include_once",
    "instanceof",
    "insteadof",
    "int",
    "interface",
    "isset",
    "iterable",
    "list",
    "match",
    "mixed",
    "namespace",
    "never",
    "new",
    "null",
    "object",
    "or",
    "parent",
    "print",
    "private",
    "protected",
    "public",
    "readonly",
    "require",
    "require_once",
    "return",
    "self",
    "static",
    "string",
    "switch",
    "throw",
    "trait",
    "true",
    "try",
    "unset",
    "use",
    "var",
    "void",
    "while",
    "xor",
    "yield",
];

pub(in crate::server) fn is_renameable_variable(var_name: &str) -> bool {
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

fn rename_target_kind_from_ref_kind(kind: RefKind) -> Option<php_lsp_types::PhpSymbolKind> {
    match kind {
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

fn is_supported_rename_kind(kind: php_lsp_types::PhpSymbolKind) -> bool {
    matches!(
        kind,
        php_lsp_types::PhpSymbolKind::Class
            | php_lsp_types::PhpSymbolKind::Interface
            | php_lsp_types::PhpSymbolKind::Trait
            | php_lsp_types::PhpSymbolKind::Enum
            | php_lsp_types::PhpSymbolKind::Function
            | php_lsp_types::PhpSymbolKind::Method
            | php_lsp_types::PhpSymbolKind::Property
            | php_lsp_types::PhpSymbolKind::ClassConstant
            | php_lsp_types::PhpSymbolKind::GlobalConstant
            | php_lsp_types::PhpSymbolKind::EnumCase
    )
}

fn invalid_rename_name_message(kind: RenameNameKind) -> &'static str {
    match kind {
        RenameNameKind::Variable => "Invalid variable name",
        RenameNameKind::Symbol(php_lsp_types::PhpSymbolKind::Class) => "Invalid class name",
        RenameNameKind::Symbol(php_lsp_types::PhpSymbolKind::Interface) => "Invalid interface name",
        RenameNameKind::Symbol(php_lsp_types::PhpSymbolKind::Trait) => "Invalid trait name",
        RenameNameKind::Symbol(php_lsp_types::PhpSymbolKind::Enum) => "Invalid enum name",
        RenameNameKind::Symbol(php_lsp_types::PhpSymbolKind::Function) => "Invalid function name",
        RenameNameKind::Symbol(php_lsp_types::PhpSymbolKind::Method) => "Invalid method name",
        RenameNameKind::Symbol(php_lsp_types::PhpSymbolKind::Property) => "Invalid property name",
        RenameNameKind::Symbol(php_lsp_types::PhpSymbolKind::ClassConstant)
        | RenameNameKind::Symbol(php_lsp_types::PhpSymbolKind::GlobalConstant) => {
            "Invalid constant name"
        }
        RenameNameKind::Symbol(php_lsp_types::PhpSymbolKind::EnumCase) => "Invalid enum case name",
        RenameNameKind::Symbol(php_lsp_types::PhpSymbolKind::Namespace) => "Invalid namespace name",
    }
}

fn is_member_rename_kind(kind: php_lsp_types::PhpSymbolKind) -> bool {
    matches!(
        kind,
        php_lsp_types::PhpSymbolKind::Method
            | php_lsp_types::PhpSymbolKind::Property
            | php_lsp_types::PhpSymbolKind::ClassConstant
            | php_lsp_types::PhpSymbolKind::EnumCase
    )
}

fn is_member_ref_kind(kind: RefKind) -> bool {
    matches!(
        kind,
        RefKind::MethodCall
            | RefKind::PropertyAccess
            | RefKind::StaticPropertyAccess
            | RefKind::ClassConstant
    )
}

fn should_reject_phpdoc_virtual_member_rename(
    resolved: &Option<Arc<php_lsp_types::SymbolInfo>>,
    member: &PhpDocVirtualMember,
) -> bool {
    match resolved.as_deref() {
        Some(symbol) => resolved_symbol_is_phpdoc_virtual_member(symbol, member),
        None => true,
    }
}

fn resolved_symbol_is_phpdoc_virtual_member(
    symbol: &php_lsp_types::SymbolInfo,
    member: &PhpDocVirtualMember,
) -> bool {
    let expected_kind = match member.kind {
        PhpDocVirtualMemberKind::Property => php_lsp_types::PhpSymbolKind::Property,
        PhpDocVirtualMemberKind::Method => php_lsp_types::PhpSymbolKind::Method,
    };
    if symbol.kind != expected_kind
        || symbol.parent_fqn.as_deref() != Some(member.owner.fqn.as_str())
    {
        return false;
    }

    let symbol_name = symbol.name.trim_start_matches('$');
    symbol_name == member.name.trim_start_matches('$')
        && symbol.doc_comment.as_deref() == member.owner.doc_comment.as_deref()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_symbol_rename_name_validation_by_kind() {
        let cases = [
            (
                php_lsp_types::PhpSymbolKind::Class,
                "NewClass",
                Some("NewClass"),
            ),
            (
                php_lsp_types::PhpSymbolKind::Interface,
                "_Contract2",
                Some("_Contract2"),
            ),
            (
                php_lsp_types::PhpSymbolKind::Trait,
                "TraitName",
                Some("TraitName"),
            ),
            (php_lsp_types::PhpSymbolKind::Enum, "Status", Some("Status")),
            (
                php_lsp_types::PhpSymbolKind::Function,
                "calculate_2",
                Some("calculate_2"),
            ),
            (
                php_lsp_types::PhpSymbolKind::Method,
                "__invoke",
                Some("__invoke"),
            ),
            (
                php_lsp_types::PhpSymbolKind::ClassConstant,
                "MAX_SIZE",
                Some("MAX_SIZE"),
            ),
            (
                php_lsp_types::PhpSymbolKind::GlobalConstant,
                "_GLOBAL_LIMIT",
                Some("_GLOBAL_LIMIT"),
            ),
            (
                php_lsp_types::PhpSymbolKind::EnumCase,
                "Ready2",
                Some("Ready2"),
            ),
            (
                php_lsp_types::PhpSymbolKind::Property,
                "$displayName",
                Some("displayName"),
            ),
            (
                php_lsp_types::PhpSymbolKind::Property,
                "displayName",
                Some("displayName"),
            ),
        ];

        for (kind, new_name, expected) in cases {
            assert_eq!(
                normalize_symbol_new_name(kind, new_name).as_deref(),
                expected
            );
        }
    }

    #[test]
    fn test_symbol_rename_rejects_invalid_names_by_kind() {
        let cases = [
            (php_lsp_types::PhpSymbolKind::Class, "123"),
            (php_lsp_types::PhpSymbolKind::Interface, "$Contract"),
            (php_lsp_types::PhpSymbolKind::Trait, "foo-bar"),
            (php_lsp_types::PhpSymbolKind::Enum, "enum"),
            (php_lsp_types::PhpSymbolKind::Function, "return"),
            (php_lsp_types::PhpSymbolKind::Method, "foo bar"),
            (php_lsp_types::PhpSymbolKind::ClassConstant, "MAX-VALUE"),
            (php_lsp_types::PhpSymbolKind::GlobalConstant, "$LIMIT"),
            (php_lsp_types::PhpSymbolKind::EnumCase, "case"),
            (php_lsp_types::PhpSymbolKind::Property, "123"),
        ];

        for (kind, new_name) in cases {
            assert!(
                normalize_symbol_new_name(kind, new_name).is_none(),
                "{kind:?} should reject {new_name:?}"
            );
        }
    }

    #[test]
    fn test_variable_rename_name_validation_and_normalization() {
        assert_eq!(
            normalize_variable_new_name("localName").as_deref(),
            Some("$localName")
        );
        assert_eq!(
            normalize_variable_new_name("$localName").as_deref(),
            Some("$localName")
        );

        for new_name in ["", " local", "123", "$123", "local-name", "$this"] {
            assert!(
                normalize_variable_new_name(new_name).is_none(),
                "variable rename should reject {new_name:?}"
            );
        }
    }
}
