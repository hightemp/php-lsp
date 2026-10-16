//! Synchronous, operation-owned PHP function-name facts. No CST/AST is retained.
use php_lsp_types::{FileSymbols, PhpSymbolKind, UseKind};
use std::{cell::RefCell, collections::HashSet, marker::PhantomData, rc::Rc};
use tree_sitter::Node;

struct Facts {
    key: (usize, usize, usize),
    names: FileSymbols,
    declared: HashSet<String>,
    builtins: HashSet<String>,
    external: bool,
}
thread_local! {
    static ACTIVE: RefCell<Option<Rc<Facts>>> = const { RefCell::new(None) };
    #[cfg(test)]
    static BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Keeps name facts within one synchronous operation, restoring a nested
/// source on exit/unwind. Rc deliberately prevents carrying it across tasks.
pub struct FunctionNameScope<'a> {
    previous: Option<Rc<Facts>>,
    owner: bool,
    borrowed_source: PhantomData<(&'a str, Node<'a>)>,
}
impl<'a> FunctionNameScope<'a> {
    /// Start persistent source-only analysis without inheriting external proofs.
    pub(crate) fn isolate() -> Self {
        Self {
            previous: ACTIVE.with(|active| active.borrow_mut().take()),
            owner: true,
            borrowed_source: PhantomData,
        }
    }

    pub fn enter(
        node: Node<'a>,
        source: &'a str,
        symbols: Option<&FileSymbols>,
        has_function: Option<&dyn Fn(&str) -> bool>,
    ) -> Self {
        let mut root = node;
        while let Some(parent) = root.parent() {
            root = parent;
        }
        let key = (root.id(), source.as_ptr() as usize, source.len());
        let previous = ACTIVE.with(|active| active.borrow().clone());
        if previous
            .as_ref()
            .is_some_and(|facts| facts.key == key && (has_function.is_none() || facts.external))
        {
            return Self {
                previous: None,
                owner: false,
                borrowed_source: PhantomData,
            };
        }
        #[cfg(test)]
        BUILDS.with(|builds| builds.set(builds.get() + 1));
        let extracted;
        let symbols = if let Some(symbols) = symbols {
            symbols
        } else {
            extracted = crate::symbols::extract_root_symbols(root, source, "", None);
            &extracted
        };
        let names = FileSymbols {
            namespace: symbols.namespace.clone(),
            namespace_scopes: symbols.namespace_scopes.clone(),
            use_statements: symbols.use_statements.clone(),
            ..Default::default()
        };
        let mut declared: HashSet<String> = symbols
            .symbols
            .iter()
            .filter(|s| s.kind == PhpSymbolKind::Function)
            .map(|s| s.fqn.trim_start_matches('\\').to_ascii_lowercase())
            .collect();
        let mut builtins = HashSet::new();
        if let Some(has_function) = has_function {
            for builtin in ["array_keys", "array_values", "preg_match", "preg_match_all"] {
                if has_function(builtin) {
                    builtins.insert(builtin.to_string());
                }
            }
            for namespace in names
                .namespace_scopes
                .iter()
                .filter_map(|scope| scope.namespace.as_deref())
                .chain(names.namespace.as_deref())
            {
                for builtin in ["array_keys", "array_values", "preg_match", "preg_match_all"] {
                    let fqn = format!("{namespace}\\{builtin}");
                    if has_function(&fqn) {
                        declared.insert(fqn.to_ascii_lowercase());
                    }
                }
            }
        }
        ACTIVE.with(|active| {
            *active.borrow_mut() = Some(Rc::new(Facts {
                key,
                names,
                declared,
                builtins,
                external: has_function.is_some(),
            }))
        });
        Self {
            previous,
            owner: true,
            borrowed_source: PhantomData,
        }
    }
}

impl Drop for FunctionNameScope<'_> {
    fn drop(&mut self) {
        if self.owner {
            ACTIVE.with(|active| *active.borrow_mut() = self.previous.take());
        }
    }
}

/// A builtin's full global identity, or None for a dynamic/foreign/shadowed
/// function. Unqualified calls retain PHP's namespace-to-global fallback.
pub(crate) fn global_builtin_name(call: Node, source: &str) -> Option<String> {
    if call.kind() != "function_call_expression" {
        return None;
    }
    let function = call.child_by_field_name("function")?;
    if !matches!(function.kind(), "name" | "qualified_name" | "relative_name") {
        return None;
    }
    let raw = source.get(function.byte_range())?.trim();
    let _scope = FunctionNameScope::enter(call, source, None, None);
    let facts = ACTIVE.with(|active| active.borrow().clone())?;
    let start = call.start_position();
    let scope = facts
        .names
        .namespace_scope_at_byte_position(start.row as u32, start.column as u32);
    let namespace = scope
        .and_then(|scope| scope.namespace.as_deref())
        .or_else(|| {
            facts
                .names
                .namespace_scopes
                .is_empty()
                .then_some(facts.names.namespace.as_deref())
                .flatten()
        });
    if let Some(absolute) = raw.strip_prefix('\\') {
        return (!absolute.contains('\\')).then(|| absolute.to_ascii_lowercase());
    }
    if let Some((prefix, name)) = raw.split_once('\\') {
        // Qualified aliases always retain a nonempty suffix; only an explicit
        // namespace-relative call in the global namespace can name a builtin.
        return (prefix.eq_ignore_ascii_case("namespace")
            && namespace.is_none()
            && !name.contains('\\'))
        .then(|| name.to_ascii_lowercase());
    }
    for import in &facts.names.use_statements {
        if import.kind != UseKind::Function {
            continue;
        }
        let in_section = scope.map_or(import.namespace.as_deref() == namespace, |scope| {
            (import.range.0, import.range.1) >= (scope.range.0, scope.range.1)
                && (import.range.0, import.range.1) < (scope.range.2, scope.range.3)
        });
        let alias = import
            .alias
            .as_deref()
            .unwrap_or_else(|| import.fqn.rsplit('\\').next().unwrap_or(&import.fqn));
        if in_section && alias.eq_ignore_ascii_case(raw) {
            let target = import.fqn.trim_start_matches('\\');
            return (!target.contains('\\')).then(|| target.to_ascii_lowercase());
        }
    }
    if let Some(namespace) = namespace {
        if !facts.builtins.contains(&raw.to_ascii_lowercase())
            || facts
                .declared
                .contains(&format!("{namespace}\\{raw}").to_ascii_lowercase())
        {
            return None;
        }
    }
    Some(raw.to_ascii_lowercase())
}

#[cfg(test)]
#[path = "function_names_tests.rs"]
mod tests;
