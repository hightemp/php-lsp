//! Caller alternatives are accumulated once, without repeatedly parsing a growing union.
use super::*;
use std::collections::BTreeSet;

#[derive(Default)]
pub(super) struct TwigCallerTypes {
    alternatives: BTreeSet<String>,
    nullable: bool,
    mixed: bool,
}

impl TwigCallerTypes {
    pub(super) fn insert(&mut self, text: &str) {
        if self.mixed {
            return;
        }
        let mut pending = vec![
            twig_context_type_text_to_type_info(text).unwrap_or(php_lsp_types::TypeInfo::Mixed)
        ];
        while let Some(ty) = pending.pop() {
            use php_lsp_types::TypeInfo;
            match ty {
                TypeInfo::Union(types) => pending.extend(types),
                TypeInfo::Nullable(inner) => {
                    self.nullable = true;
                    pending.push(*inner);
                }
                TypeInfo::LiteralNull => self.nullable = true,
                TypeInfo::Mixed => {
                    self.mixed = true;
                    self.alternatives.clear();
                    return;
                }
                other => {
                    // Callable return unions and intersections need grouping when
                    // embedded in a larger caller union. Never split nested text.
                    let plain = other.to_string();
                    let text =
                        if twig_context_type_text_to_type_info(&plain).as_ref() == Some(&other) {
                            plain
                        } else {
                            php_lsp_parser::resolve::receiver_type_text(&other)
                        };
                    self.alternatives.insert(
                        if matches!(other, TypeInfo::Callable { .. } | TypeInfo::Intersection(_)) {
                            format!("({text})")
                        } else {
                            text
                        },
                    );
                }
            }
        }
    }
    pub(super) fn finish(self) -> String {
        if self.mixed {
            return "mixed".into();
        }
        if self.alternatives.is_empty() {
            return if self.nullable { "null" } else { "mixed" }.into();
        }
        if self.nullable && self.alternatives.len() == 1 {
            return format!("?{}", self.alternatives.first().unwrap());
        }
        let mut parts = self.alternatives.into_iter().collect::<Vec<_>>();
        if self.nullable {
            parts.push("null".into());
        }
        parts.join("|")
    }
}

#[cfg(test)]
thread_local! {
    pub(super) static PARSED_TYPE_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
