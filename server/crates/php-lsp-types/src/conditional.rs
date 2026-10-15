//! Conservative conditional type specialization shared by parser and server.
use crate::type_refinement::{family, normalized_leaf, phpdoc_refines_native_with, TypeRefinement};
use crate::{ArrayShapeItem, TypeInfo};
use std::collections::{HashMap, HashSet};

const MAX_DEPTH: usize = 64;
pub const MAX_MATCH_STEPS: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternMatch {
    Yes,
    No,
    Unknown,
}

/// An exhausted inspection must still enter conservative specialization.
pub fn may_contain_conditional(ty: &TypeInfo) -> bool {
    fn visit(ty: &TypeInfo, depth: usize, remaining: &mut usize) -> bool {
        if depth >= MAX_DEPTH || *remaining == 0 {
            return true;
        }
        *remaining -= 1;
        match ty {
            TypeInfo::Conditional { .. } => true,
            TypeInfo::Generic { args, .. }
            | TypeInfo::Union(args)
            | TypeInfo::Intersection(args) => args.iter().any(|ty| visit(ty, depth + 1, remaining)),
            TypeInfo::ArrayShape(items) | TypeInfo::ObjectShape(items) => items
                .iter()
                .any(|item| visit(&item.value, depth + 1, remaining)),
            TypeInfo::Nullable(inner) | TypeInfo::ClassString(Some(inner)) => {
                visit(inner, depth + 1, remaining)
            }
            TypeInfo::Callable {
                params,
                return_type,
            } => {
                params.iter().any(|ty| visit(ty, depth + 1, remaining))
                    || return_type
                        .as_ref()
                        .is_some_and(|ty| visit(ty, depth + 1, remaining))
            }
            _ => false,
        }
    }
    let mut remaining = MAX_MATCH_STEPS;
    visit(ty, 0, &mut remaining)
}

/// A condition tests independently established bindings; it never infers them.
pub fn match_type_pattern(
    pattern: &TypeInfo,
    actual: &TypeInfo,
    templates: &HashSet<String>,
    bindings: &HashMap<String, TypeInfo>,
    relation: &impl Fn(&str, &str) -> Option<bool>,
) -> PatternMatch {
    Matcher {
        templates,
        bindings,
        relation,
        remaining: MAX_MATCH_STEPS,
    }
    .check(pattern, actual, 0)
}

struct Matcher<'a, R> {
    templates: &'a HashSet<String>,
    bindings: &'a HashMap<String, TypeInfo>,
    relation: &'a R,
    remaining: usize,
}
impl<R: Fn(&str, &str) -> Option<bool>> Matcher<'_, R> {
    fn check(&mut self, pattern: &TypeInfo, actual: &TypeInfo, depth: usize) -> PatternMatch {
        use PatternMatch::*;
        use TypeInfo::*;
        if depth >= MAX_DEPTH || self.remaining == 0 {
            return Unknown;
        }
        self.remaining -= 1;
        if let Simple(name) = pattern {
            if self.templates.contains(name) {
                return self
                    .bindings
                    .get(name)
                    .filter(|bound| *bound != pattern)
                    .map(|bound| self.check(bound, actual, depth + 1))
                    .unwrap_or(Unknown);
            }
            if let Some(normalized) = normalized_leaf(name) {
                return self.check(&normalized, actual, depth + 1);
            }
        }
        if let Simple(name) = actual {
            if self.templates.contains(name) {
                return Unknown;
            }
            if let Some(normalized) = normalized_leaf(name) {
                return self.check(pattern, &normalized, depth + 1);
            }
        }
        match self.equivalent(pattern, actual, depth) {
            Some(true) => return Yes,
            None => return Unknown,
            Some(false) => {}
        }
        if matches!(pattern, Mixed) || matches!(actual, Never) {
            return Yes;
        }
        if matches!(actual, Mixed) {
            return Unknown;
        }
        if let Nullable(inner) = actual {
            let inner = self.check(pattern, inner, depth + 1);
            let null = self.check(pattern, &LiteralNull, depth + 1);
            return if inner == Yes && null == Yes {
                Yes
            } else if inner == No && null == No {
                No
            } else {
                Unknown
            };
        }
        if let Nullable(inner) = pattern {
            let inner = self.check(inner, actual, depth + 1);
            if inner == Yes {
                return Yes;
            }
            let null = self.check(&LiteralNull, actual, depth + 1);
            return if null == Yes {
                Yes
            } else if inner == No && null == No {
                No
            } else {
                Unknown
            };
        }
        if let Union(parts) = actual {
            let mut yes = false;
            let mut no = false;
            for part in parts {
                match self.check(pattern, part, depth + 1) {
                    Yes => yes = true,
                    No => no = true,
                    Unknown => return Unknown,
                }
                if yes && no {
                    return Unknown;
                }
            }
            return if yes {
                Yes
            } else if no {
                No
            } else {
                Unknown
            };
        }
        if let Union(parts) = pattern {
            let mut unknown = parts.is_empty();
            for part in parts {
                match self.check(part, actual, depth + 1) {
                    Yes => return Yes,
                    Unknown => unknown = true,
                    No => {}
                }
                if self.remaining == 0 {
                    return Unknown;
                }
            }
            return if unknown { Unknown } else { No };
        }
        if let Intersection(parts) = pattern {
            let mut unknown = parts.is_empty();
            for part in parts {
                match self.check(part, actual, depth + 1) {
                    No => return No,
                    Unknown => unknown = true,
                    Yes => {}
                }
                if self.remaining == 0 {
                    return Unknown;
                }
            }
            return if unknown { Unknown } else { Yes };
        }
        if let Intersection(parts) = actual {
            let mut no = false;
            for part in parts {
                match self.check(pattern, part, depth + 1) {
                    Yes => return Yes,
                    No => no = true,
                    Unknown => {}
                }
                if self.remaining == 0 {
                    return Unknown;
                }
            }
            return if no { No } else { Unknown };
        }
        match (pattern, actual) {
            (ClassString(None), ClassString(_)) => return Yes,
            (ClassString(Some(p)), ClassString(Some(a))) => return self.check(p, a, depth + 1),
            (Generic { base: p, args: pa }, Generic { base: a, args: aa })
                if same_name(p, a) && pa.len() == aa.len() =>
            {
                // Builtin array/iterable value positions are covariant. User-class
                // variance requires metadata that this structural matcher lacks.
                if !matches!(
                    p.to_ascii_lowercase().as_str(),
                    "array" | "list" | "non-empty-array" | "non-empty-list" | "iterable"
                ) {
                    return Unknown;
                }
                let mut unknown = false;
                for (p, a) in pa.iter().zip(aa) {
                    match self.check(p, a, depth + 1) {
                        No => return No,
                        Unknown => unknown = true,
                        Yes => {}
                    }
                    if self.remaining == 0 {
                        return Unknown;
                    }
                }
                return if unknown { Unknown } else { Yes };
            }
            (LiteralBool(_), LiteralBool(_)) => return No,
            (LiteralString(p), LiteralString(a)) => {
                return match (plain_string(p), plain_string(a)) {
                    (Some(p), Some(a)) => {
                        if p == a {
                            Yes
                        } else {
                            No
                        }
                    }
                    _ => Unknown,
                }
            }
            (LiteralInt(p), LiteralInt(a)) => {
                return match (integer(p), integer(a)) {
                    (Some(p), Some(a)) => {
                        if p == a {
                            Yes
                        } else {
                            No
                        }
                    }
                    _ => Unknown,
                }
            }
            (LiteralFloat(p), LiteralFloat(a)) => {
                return match (
                    p.replace('_', "").parse::<f64>(),
                    a.replace('_', "").parse::<f64>(),
                ) {
                    (Ok(p), Ok(a)) if p.is_finite() && a.is_finite() => {
                        if p == a {
                            Yes
                        } else {
                            No
                        }
                    }
                    _ => Unknown,
                }
            }
            (Simple(p), Simple(a)) if same_name(p, a) => return Yes,
            _ => {}
        }
        let safe_pattern = match pattern {
            Simple(name) => !name.contains('-'),
            _ => true,
        };
        if safe_pattern
            && phpdoc_refines_native_with(actual, pattern, self.relation)
                == TypeRefinement::Compatible
        {
            return Yes;
        }
        if let (Some(p), Some(a)) = (families(pattern), families(actual)) {
            if !p.iter().any(|p| a.contains(p)) {
                return No;
            }
            if let Simple(name) = pattern {
                if matches!(name.as_str(), "scalar" | "array-key")
                    && a.iter().all(|a| p.contains(a))
                {
                    return Yes;
                }
            }
        }
        Unknown
    }

    fn equivalent(&mut self, p: &TypeInfo, a: &TypeInfo, depth: usize) -> Option<bool> {
        use TypeInfo::*;
        if depth >= MAX_DEPTH || self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        match (p, a) {
            (Simple(p), Simple(a)) => {
                if self.templates.contains(p) || self.templates.contains(a) {
                    return None;
                }
                Some(same_name(p, a))
            }
            (Generic { base: p, args: ps }, Generic { base: a, args: as_ }) => {
                if !same_name(p, a) || ps.len() != as_.len() {
                    return Some(false);
                }
                for (p, a) in ps.iter().zip(as_) {
                    if !self.equivalent(p, a, depth + 1)? {
                        return Some(false);
                    }
                }
                Some(true)
            }
            (Union(ps), Union(as_)) | (Intersection(ps), Intersection(as_)) => {
                if ps.len() != as_.len() {
                    return Some(false);
                }
                for (p, a) in ps.iter().zip(as_) {
                    if !self.equivalent(p, a, depth + 1)? {
                        return Some(false);
                    }
                }
                Some(true)
            }
            (Nullable(p), Nullable(a)) | (ClassString(Some(p)), ClassString(Some(a))) => {
                self.equivalent(p, a, depth + 1)
            }
            (ArrayShape(ps), ArrayShape(as_)) | (ObjectShape(ps), ObjectShape(as_)) => {
                if ps.len() != as_.len() {
                    return Some(false);
                }
                for (p, a) in ps.iter().zip(as_) {
                    if p.key != a.key
                        || p.optional != a.optional
                        || !self.equivalent(&p.value, &a.value, depth + 1)?
                    {
                        return Some(false);
                    }
                }
                Some(true)
            }
            (
                Callable {
                    params: ps,
                    return_type: pr,
                },
                Callable {
                    params: as_,
                    return_type: ar,
                },
            ) => {
                if ps.len() != as_.len() {
                    return Some(false);
                }
                for (p, a) in ps.iter().zip(as_) {
                    if !self.equivalent(p, a, depth + 1)? {
                        return Some(false);
                    }
                }
                match (pr, ar) {
                    (Some(p), Some(a)) => self.equivalent(p, a, depth + 1),
                    (None, None) => Some(true),
                    _ => Some(false),
                }
            }
            (Conditional { .. }, _) | (_, Conditional { .. }) => Some(false),
            (LiteralString(p), LiteralString(a))
            | (LiteralInt(p), LiteralInt(a))
            | (LiteralFloat(p), LiteralFloat(a)) => Some(p == a),
            (LiteralBool(p), LiteralBool(a)) => Some(p == a),
            (ClassString(None), ClassString(None))
            | (LiteralNull, LiteralNull)
            | (Mixed, Mixed)
            | (Never, Never)
            | (Void, Void)
            | (Self_, Self_)
            | (Static_, Static_)
            | (Parent_, Parent_) => Some(true),
            _ => Some(false),
        }
    }
}

fn same_name(a: &str, b: &str) -> bool {
    a.trim_start_matches('\\')
        .eq_ignore_ascii_case(b.trim_start_matches('\\'))
}
fn plain_string(raw: &str) -> Option<&str> {
    let quote = raw.chars().next()?;
    if !matches!(quote, '\'' | '"') || !raw.ends_with(quote) || raw.len() < 2 {
        return None;
    }
    let content = &raw[1..raw.len() - 1];
    (!(content.contains('\\') || quote == '"' && content.contains('$'))).then_some(content)
}
fn integer(raw: &str) -> Option<i128> {
    let raw = raw.replace('_', "");
    let (negative, text) = if let Some(s) = raw.strip_prefix('-') {
        (true, s)
    } else {
        (false, raw.strip_prefix('+').unwrap_or(&raw))
    };
    let lower = text.to_ascii_lowercase();
    let (radix, digits) = if let Some(s) = lower.strip_prefix("0x") {
        (16, s)
    } else if let Some(s) = lower.strip_prefix("0b") {
        (2, s)
    } else if let Some(s) = lower.strip_prefix("0o") {
        (8, s)
    } else if lower.len() > 1 && lower.starts_with('0') {
        (8, &lower[1..])
    } else {
        (10, lower.as_str())
    };
    let value = i128::from_str_radix(digits, radix).ok()?;
    if negative {
        value.checked_neg()
    } else {
        Some(value)
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Family {
    Int,
    Float,
    String,
    Bool,
    Null,
    Array,
    Object,
    Resource,
    Void,
}
fn families(ty: &TypeInfo) -> Option<&'static [Family]> {
    use Family as F;
    use TypeInfo::*;
    match ty {
        LiteralString(_) | ClassString(_) => Some(&[F::String]),
        LiteralInt(_) => Some(&[F::Int]),
        LiteralFloat(_) => Some(&[F::Float]),
        LiteralBool(_) => Some(&[F::Bool]),
        LiteralNull => Some(&[F::Null]),
        ArrayShape(_) => Some(&[F::Array]),
        ObjectShape(_) => Some(&[F::Object]),
        Void => Some(&[F::Void]),
        Callable { .. } => Some(&[F::String, F::Array, F::Object]),
        Simple(name) | Generic { base: name, .. } => match family(&name.to_ascii_lowercase()) {
            "int" | "integer" => Some(&[F::Int]),
            "float" | "double" => Some(&[F::Float]),
            "string" => Some(&[F::String]),
            "bool" | "boolean" => Some(&[F::Bool]),
            "null" => Some(&[F::Null]),
            "array" => Some(&[F::Array]),
            "iterable" => Some(&[F::Array, F::Object]),
            "object" => Some(&[F::Object]),
            "resource" => Some(&[F::Resource]),
            "void" => Some(&[F::Void]),
            "scalar" => Some(&[F::Int, F::Float, F::String, F::Bool]),
            "array-key" => Some(&[F::Int, F::String]),
            "numeric" | "number" => Some(&[F::Int, F::Float, F::String]),
            "callable" => Some(&[F::String, F::Array, F::Object]),
            "mixed" | "never" => None,
            _ if name.starts_with('\\')
                && name
                    .chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, '_' | '\\')) =>
            {
                Some(&[F::Object])
            }
            _ => None,
        },
        _ => None,
    }
}

/// Specialize every nested conditional, keeping both branches when unproven.
pub fn resolve_conditionals(
    ty: &TypeInfo,
    arguments: &HashMap<String, TypeInfo>,
    templates: &HashSet<String>,
    bindings: &HashMap<String, TypeInfo>,
    relation: &impl Fn(&str, &str) -> Option<bool>,
) -> TypeInfo {
    resolve_inner(ty, arguments, templates, bindings, relation, 0)
}

fn resolve_inner(
    ty: &TypeInfo,
    arguments: &HashMap<String, TypeInfo>,
    templates: &HashSet<String>,
    bindings: &HashMap<String, TypeInfo>,
    relation: &impl Fn(&str, &str) -> Option<bool>,
    depth: usize,
) -> TypeInfo {
    if depth >= MAX_DEPTH {
        return TypeInfo::Mixed;
    }
    if let TypeInfo::Conditional {
        subject,
        target,
        if_type,
        else_type,
    } = ty
    {
        let key = subject.trim().trim_start_matches('$');
        let actual = if subject.trim().starts_with('$') {
            arguments.get(key)
        } else if templates.contains(key) {
            bindings.get(key)
        } else {
            None
        };
        let target = try_substitute_types(target, bindings);
        let result = actual
            .zip(target.as_ref())
            .map(|(actual, target)| {
                match_type_pattern(target, actual, templates, bindings, relation)
            })
            .unwrap_or(PatternMatch::Unknown);
        return match result {
            PatternMatch::Yes => {
                resolve_inner(if_type, arguments, templates, bindings, relation, depth + 1)
            }
            PatternMatch::No => resolve_inner(
                else_type,
                arguments,
                templates,
                bindings,
                relation,
                depth + 1,
            ),
            PatternMatch::Unknown => union(vec![
                resolve_inner(if_type, arguments, templates, bindings, relation, depth + 1),
                resolve_inner(
                    else_type,
                    arguments,
                    templates,
                    bindings,
                    relation,
                    depth + 1,
                ),
            ]),
        };
    }
    if let TypeInfo::Simple(name) = ty {
        if let Some(bound) = bindings.get(name) {
            return try_substitute_types(bound, &HashMap::new()).unwrap_or(TypeInfo::Mixed);
        }
        return if templates.contains(name) {
            TypeInfo::Mixed
        } else {
            ty.clone()
        };
    }
    map_children(ty, &mut |child| {
        resolve_inner(child, arguments, templates, bindings, relation, depth + 1)
    })
}

pub fn substitute_types(ty: &TypeInfo, bindings: &HashMap<String, TypeInfo>) -> TypeInfo {
    try_substitute_types(ty, bindings).unwrap_or(TypeInfo::Mixed)
}

/// Budget exhaustion is not a successful binding to mixed.
pub fn try_substitute_types(
    ty: &TypeInfo,
    bindings: &HashMap<String, TypeInfo>,
) -> Option<TypeInfo> {
    let mut remaining = MAX_MATCH_STEPS;
    let mut failed = false;
    let result = substitute_inner(ty, bindings, 0, &mut remaining, &mut failed, true);
    (!failed).then_some(result)
}

pub fn try_substitute_type_list<'a>(
    types: impl IntoIterator<Item = &'a TypeInfo>,
    bindings: &HashMap<String, TypeInfo>,
) -> Option<Vec<TypeInfo>> {
    let mut remaining = MAX_MATCH_STEPS;
    let mut failed = false;
    let mut result = Vec::new();
    for ty in types {
        result.push(substitute_inner(
            ty,
            bindings,
            0,
            &mut remaining,
            &mut failed,
            true,
        ));
        if failed {
            return None;
        }
    }
    Some(result)
}

fn substitute_inner(
    ty: &TypeInfo,
    bindings: &HashMap<String, TypeInfo>,
    depth: usize,
    remaining: &mut usize,
    failed: &mut bool,
    replace: bool,
) -> TypeInfo {
    if depth >= MAX_DEPTH || *remaining == 0 || child_count(ty) > *remaining {
        *failed = true;
        return TypeInfo::Mixed;
    }
    *remaining -= 1;
    if replace {
        if let TypeInfo::Simple(name) = ty {
            if let Some(bound) = bindings.get(name) {
                return substitute_inner(bound, bindings, depth + 1, remaining, failed, false);
            }
        }
    }
    map_children(ty, &mut |child| {
        substitute_inner(child, bindings, depth + 1, remaining, failed, replace)
    })
}

fn child_count(ty: &TypeInfo) -> usize {
    match ty {
        TypeInfo::Generic { args, .. } | TypeInfo::Union(args) | TypeInfo::Intersection(args) => {
            args.len()
        }
        TypeInfo::ArrayShape(items) | TypeInfo::ObjectShape(items) => items.len(),
        TypeInfo::Callable {
            params,
            return_type,
        } => params.len() + usize::from(return_type.is_some()),
        TypeInfo::Conditional { .. } => 3,
        TypeInfo::Nullable(_) | TypeInfo::ClassString(Some(_)) => 1,
        _ => 0,
    }
}

/// Infer only self-contained literal defaults; unresolved constants stay unknown.
pub fn literal_default_type(raw: &str) -> Option<TypeInfo> {
    let text = raw.trim();
    match text.to_ascii_lowercase().as_str() {
        "null" => return Some(TypeInfo::LiteralNull),
        "true" => return Some(TypeInfo::LiteralBool(true)),
        "false" => return Some(TypeInfo::LiteralBool(false)),
        "[]" | "array()" => return Some(TypeInfo::ArrayShape(Vec::new())),
        _ => {}
    }
    if text.len() >= 2
        && matches!(text.as_bytes()[0], b'\'' | b'"')
        && text.as_bytes().first() == text.as_bytes().last()
    {
        if text.starts_with('"') && text.contains('$') {
            return None;
        }
        return Some(TypeInfo::LiteralString(text.into()));
    }
    if integer(text).is_some() {
        return Some(TypeInfo::LiteralInt(text.into()));
    }
    if text.chars().any(|c| c.is_ascii_digit())
        && text
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-' | '_'))
        && text.replace('_', "").parse::<f64>().is_ok()
    {
        return Some(TypeInfo::LiteralFloat(text.into()));
    }
    None
}

fn union(types: Vec<TypeInfo>) -> TypeInfo {
    let mut parts = Vec::new();
    for ty in types {
        for part in match ty {
            TypeInfo::Union(parts) => parts,
            other => vec![other],
        } {
            if part == TypeInfo::Mixed {
                return TypeInfo::Mixed;
            }
            if !parts.contains(&part) {
                parts.push(part);
            }
        }
    }
    if parts.len() == 1 {
        parts.pop().unwrap()
    } else {
        TypeInfo::Union(parts)
    }
}

/// Transform children before their parent with substitution's depth/work limits.
/// Replacements are not traversed again. Failure discards the whole result.
pub fn try_transform_type(
    ty: &TypeInfo,
    transform: &mut impl FnMut(TypeInfo) -> Option<TypeInfo>,
) -> Option<TypeInfo> {
    fn visit(
        ty: &TypeInfo,
        transform: &mut impl FnMut(TypeInfo) -> Option<TypeInfo>,
        depth: usize,
        remaining: &mut usize,
    ) -> Option<TypeInfo> {
        if depth >= MAX_DEPTH || *remaining == 0 || child_count(ty) > *remaining {
            return None;
        }
        *remaining -= 1;
        let mut failed = false;
        let mapped = map_children(ty, &mut |child| {
            visit(child, transform, depth + 1, remaining).unwrap_or_else(|| {
                failed = true;
                TypeInfo::Mixed
            })
        });
        if failed {
            None
        } else {
            transform(mapped)
        }
    }
    let mut remaining = MAX_MATCH_STEPS;
    visit(ty, transform, 0, &mut remaining)
}

fn map_children(ty: &TypeInfo, map: &mut impl FnMut(&TypeInfo) -> TypeInfo) -> TypeInfo {
    use TypeInfo::*;
    match ty {
        Generic { base, args } => Generic {
            base: base.clone(),
            args: args.iter().map(&mut *map).collect(),
        },
        Union(parts) => Union(parts.iter().map(&mut *map).collect()),
        Intersection(parts) => Intersection(parts.iter().map(&mut *map).collect()),
        Nullable(inner) => Nullable(Box::new(map(inner))),
        ClassString(Some(inner)) => ClassString(Some(Box::new(map(inner)))),
        ArrayShape(items) | ObjectShape(items) => {
            let items = items
                .iter()
                .map(|i| ArrayShapeItem {
                    key: i.key.clone(),
                    optional: i.optional,
                    value: map(&i.value),
                })
                .collect();
            if matches!(ty, ArrayShape(_)) {
                ArrayShape(items)
            } else {
                ObjectShape(items)
            }
        }
        Callable {
            params,
            return_type,
        } => Callable {
            params: params.iter().map(&mut *map).collect(),
            return_type: return_type.as_ref().map(|t| Box::new(map(t))),
        },
        Conditional {
            subject,
            target,
            if_type,
            else_type,
        } => Conditional {
            subject: subject.clone(),
            target: Box::new(map(target)),
            if_type: Box::new(map(if_type)),
            else_type: Box::new(map(else_type)),
        },
        _ => ty.clone(),
    }
}

#[cfg(test)]
#[path = "conditional_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "type_transform_tests.rs"]
mod transform_tests;
