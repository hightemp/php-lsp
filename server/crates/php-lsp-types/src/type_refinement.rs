//! Conservative PHPDoc subtyping, independent of parser/index storage.
use crate::{TemplateParam, TypeInfo};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeRefinement {
    Compatible,
    Incompatible,
    Unknown,
}

impl TypeRefinement {
    fn all(values: impl IntoIterator<Item = Self>) -> Self {
        let mut unknown = false;
        let mut seen = false;
        for value in values {
            seen = true;
            match value {
                Self::Incompatible => return Self::Incompatible,
                Self::Unknown => unknown = true,
                Self::Compatible => {}
            }
        }
        if !seen || unknown {
            Self::Unknown
        } else {
            Self::Compatible
        }
    }
    fn any(values: impl IntoIterator<Item = Self>) -> Self {
        let mut unknown = false;
        let mut seen = false;
        for value in values {
            seen = true;
            match value {
                Self::Compatible => return Self::Compatible,
                Self::Unknown => unknown = true,
                Self::Incompatible => {}
            }
        }
        if !seen || unknown {
            Self::Unknown
        } else {
            Self::Incompatible
        }
    }
}

pub fn phpdoc_refines_native(doc: &TypeInfo, native: &TypeInfo) -> TypeRefinement {
    phpdoc_refines_native_with(doc, native, &|_, _| None)
}

/// `class_relation` proves or disproves class/interface inheritance; None means
/// absent metadata, not an incompatible declaration.
pub fn phpdoc_refines_native_with(
    doc: &TypeInfo,
    native: &TypeInfo,
    class_relation: &impl Fn(&str, &str) -> Option<bool>,
) -> TypeRefinement {
    phpdoc_refines_native_with_templates(doc, native, class_relation, &[])
}

pub fn phpdoc_refines_native_with_templates(
    doc: &TypeInfo,
    native: &TypeInfo,
    class_relation: &impl Fn(&str, &str) -> Option<bool>,
    templates: &[TemplateParam],
) -> TypeRefinement {
    refine(doc, native, class_relation, templates, 0)
}

pub fn preferred_type(
    native: Option<&TypeInfo>,
    doc: Option<&TypeInfo>,
    fallback: Option<&TypeInfo>,
    relation: &impl Fn(&str, &str) -> Option<bool>,
) -> Option<TypeInfo> {
    let Some(native) = native else {
        return doc.or(fallback).cloned();
    };
    if let Some(doc) = doc {
        if phpdoc_refines_native_with(doc, native, relation) == TypeRefinement::Compatible {
            return Some(doc.clone());
        }
    }
    if let Some(fallback) = fallback {
        if phpdoc_refines_native_with(fallback, native, relation) == TypeRefinement::Compatible {
            return Some(fallback.clone());
        }
    }
    Some(native.clone())
}

fn refine(
    doc: &TypeInfo,
    native: &TypeInfo,
    relation: &impl Fn(&str, &str) -> Option<bool>,
    templates: &[TemplateParam],
    depth: usize,
) -> TypeRefinement {
    use TypeInfo::*;
    use TypeRefinement::*;
    if depth >= 64 {
        return Unknown;
    }
    if let Simple(name) = doc {
        if let Some(template) = templates.iter().find(|template| template.name == *name) {
            return refine(
                template.bound.as_ref().unwrap_or(&Mixed),
                native,
                relation,
                templates,
                depth + 1,
            );
        }
    }
    if doc == native {
        return Compatible;
    }
    if let Simple(name) = doc {
        if let Some(normalized) = normalized_leaf(name) {
            return refine(&normalized, native, relation, templates, depth + 1);
        }
    }
    if let Simple(name) = native {
        if let Some(normalized) = normalized_leaf(name) {
            return refine(doc, &normalized, relation, templates, depth + 1);
        }
    }
    let next = |d, n| refine(d, n, relation, templates, depth + 1);
    match (doc, native) {
        (_, Mixed) | (Never, _) => Compatible,
        (Mixed, _) => Incompatible,
        (Nullable(inner), _) => {
            TypeRefinement::all([next(inner, native), next(&LiteralNull, native)])
        }
        (Union(parts), _) => TypeRefinement::all(parts.iter().map(|part| next(part, native))),
        (_, Nullable(inner)) => TypeRefinement::any([next(doc, inner), next(doc, &LiteralNull)]),
        (_, Union(parts)) => TypeRefinement::any(parts.iter().map(|part| next(doc, part))),
        (_, Intersection(parts)) => TypeRefinement::all(parts.iter().map(|part| next(doc, part))),
        (Intersection(parts), _) => {
            TypeRefinement::any(parts.iter().map(|part| next(part, native)))
        }
        (
            Conditional {
                if_type, else_type, ..
            },
            _,
        ) => TypeRefinement::all([next(if_type, native), next(else_type, native)]),
        (_, Conditional { .. }) => Unknown,
        (Static_, Self_) => Compatible,
        (Generic { base, .. }, Static_) if base.eq_ignore_ascii_case("static") => Compatible,
        (_, Static_) => Incompatible,
        (Simple(d), Simple(n)) => names_refine(d, n, relation),
        (Generic { base, .. }, Simple(native)) => {
            if base == "key-of" && matches!(family(native), "string" | "int" | "array-key") {
                return Unknown;
            }
            if base.contains('-') && !is_primitive(base) {
                return Unknown;
            }
            if (family(base) == "array" && matches!(family(native), "array" | "iterable"))
                || (family(base) == "iterable" && family(native) == "iterable")
            {
                Compatible
            } else {
                names_refine(base, native, relation)
            }
        }
        (ArrayShape(_), Simple(name)) if matches!(family(name), "array" | "iterable") => Compatible,
        (ObjectShape(_), Simple(name)) if family(name) == "object" => Compatible,
        (Callable { .. }, Simple(name)) if family(name) == "callable" => Compatible,
        (ClassString(_), Simple(name)) if family(name) == "string" => Compatible,
        (LiteralString(_), Simple(name)) if family(name) == "string" => Compatible,
        (LiteralInt(_), Simple(name)) if family(name) == "int" => Compatible,
        (LiteralFloat(_), Simple(name)) if family(name) == "float" => Compatible,
        (LiteralBool(_), Simple(name)) if family(name) == "bool" => Compatible,
        (Generic { base: d, args: da }, Generic { base: n, args: na })
            if d.eq_ignore_ascii_case(n) && da.len() == na.len() =>
        {
            TypeRefinement::all(da.iter().zip(na).map(|(d, n)| next(d, n)))
        }
        (Generic { .. }, Generic { .. }) => Unknown,
        (Simple(d), _) if !is_primitive(d) => {
            if primitive_type(native) {
                Incompatible
            } else {
                Unknown
            }
        }
        (_, Simple(n)) if !is_primitive(n) => {
            if primitive_type(doc) {
                Incompatible
            } else {
                let name = match doc {
                    Self_ => "self",
                    Static_ => "static",
                    Parent_ => "parent",
                    _ => return Unknown,
                };
                relation(name, n)
                    .map(|yes| if yes { Compatible } else { Incompatible })
                    .unwrap_or(Unknown)
            }
        }
        (_, Self_ | Parent_) => {
            let name = match native {
                Self_ => "self",
                Static_ => "static",
                _ => "parent",
            };
            let doc_name = match doc {
                Simple(name) => name.as_str(),
                Self_ => "self",
                Static_ => "static",
                Parent_ => "parent",
                _ => {
                    return if primitive_type(doc) {
                        Incompatible
                    } else {
                        Unknown
                    }
                }
            };
            relation(doc_name, name)
                .map(|yes| if yes { Compatible } else { Incompatible })
                .unwrap_or(Unknown)
        }
        _ => Incompatible,
    }
}

fn names_refine(
    doc: &str,
    native: &str,
    relation: &impl Fn(&str, &str) -> Option<bool>,
) -> TypeRefinement {
    use TypeRefinement::*;
    if doc
        .trim_start_matches('\\')
        .eq_ignore_ascii_case(native.trim_start_matches('\\'))
    {
        return Compatible;
    }
    let d = family(doc);
    let n = family(native);
    if n == "mixed" {
        return Compatible;
    }
    if d == "mixed" {
        return Incompatible;
    }
    if d == n && n == native && is_primitive(native) {
        return Compatible;
    }
    if d == "array" && n == "iterable" {
        return Compatible;
    }
    if !is_primitive(doc) {
        if n == "object" {
            return Compatible;
        }
        if n == "iterable" {
            return relation(doc, "Traversable")
                .map(|b| if b { Compatible } else { Incompatible })
                .unwrap_or(Unknown);
        }
        if n == "callable" {
            // Inheritance alone cannot disprove __invoke-based callability.
            return relation(doc, "Closure")
                .map(|b| if b { Compatible } else { Unknown })
                .unwrap_or(Unknown);
        }
        if !is_primitive(native) {
            return relation(doc, native)
                .map(|b| if b { Compatible } else { Incompatible })
                .unwrap_or(Unknown);
        }
    }
    Incompatible
}

fn normalized_leaf(name: &str) -> Option<TypeInfo> {
    match name.to_ascii_lowercase().as_str() {
        "mixed" => Some(TypeInfo::Mixed),
        "never" | "never-return" | "no-return" => Some(TypeInfo::Never),
        "void" => Some(TypeInfo::Void),
        "null" => Some(TypeInfo::LiteralNull),
        "true" => Some(TypeInfo::LiteralBool(true)),
        "false" => Some(TypeInfo::LiteralBool(false)),
        "self" => Some(TypeInfo::Self_),
        "static" => Some(TypeInfo::Static_),
        "parent" => Some(TypeInfo::Parent_),
        "integer" => Some(TypeInfo::Simple("int".into())),
        "boolean" => Some(TypeInfo::Simple("bool".into())),
        "double" => Some(TypeInfo::Simple("float".into())),
        _ => None,
    }
}

fn family(name: &str) -> &str {
    match name {
        "positive-int" | "negative-int" | "non-negative-int" | "non-positive-int"
        | "non-zero-int" | "int-mask" | "int-mask-of" => "int",
        "non-empty-string" | "numeric-string" | "literal-string" | "lowercase-string"
        | "uppercase-string" | "class-string" => "string",
        "non-empty-array" | "list" | "non-empty-list" => "array",
        "key-of" => "array-key",
        "properties-of"
        | "public-properties-of"
        | "protected-properties-of"
        | "private-properties-of" => "array",
        other => other,
    }
}

pub fn is_primitive(name: &str) -> bool {
    matches!(
        family(name),
        "int"
            | "float"
            | "string"
            | "bool"
            | "array"
            | "iterable"
            | "object"
            | "callable"
            | "resource"
            | "scalar"
            | "array-key"
            | "numeric"
            | "mixed"
            | "void"
            | "never"
            | "null"
            | "true"
            | "false"
    )
}

fn primitive_type(ty: &TypeInfo) -> bool {
    match ty {
        TypeInfo::Simple(name) => is_primitive(name) && family(name) != "object",
        TypeInfo::LiteralString(_)
        | TypeInfo::LiteralInt(_)
        | TypeInfo::LiteralFloat(_)
        | TypeInfo::LiteralBool(_)
        | TypeInfo::LiteralNull
        | TypeInfo::Void
        | TypeInfo::Never
        | TypeInfo::Mixed
        | TypeInfo::ArrayShape(_)
        | TypeInfo::ClassString(_) => true,
        TypeInfo::Generic { base, .. } => is_primitive(base),
        _ => false,
    }
}

#[cfg(test)]
#[path = "type_refinement_tests.rs"]
mod tests;
