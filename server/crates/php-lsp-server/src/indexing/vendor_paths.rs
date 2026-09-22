//! Logical vendor boundary. Symlink targets deliberately remain unrestricted.

use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub(crate) struct VendorPathPolicy {
    root: PathBuf,
}

impl VendorPathPolicy {
    pub(crate) fn new(root: &Path) -> Option<Self> {
        let absolute = std::path::absolute(root).ok()?;
        let mut root = PathBuf::new();
        for component in absolute.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    if !root.pop() {
                        return None;
                    }
                }
                _ => root.push(component.as_os_str()),
            }
        }
        Some(Self { root })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Interpret metadata separators portably, before any filesystem access.
    pub(crate) fn join(&self, base: &Path, relative: &str) -> Option<PathBuf> {
        if relative.starts_with(['/', '\\'])
            || relative.chars().any(|ch| ch.is_control() || ch == ':')
        {
            return None;
        }
        let base = self.check(base)?;
        self.check(&base.join(relative.replace('\\', "/")))
    }

    /// Check final candidates (including include targets), retaining logical URIs.
    /// `..` is evaluated here, never by the OS after following a symlink.
    pub(crate) fn check(&self, path: &Path) -> Option<PathBuf> {
        if !self.root.is_absolute() {
            return None;
        }
        let relative = path.strip_prefix(&self.root).ok()?;
        let portable;
        let relative = if let Some(text) = relative.to_str() {
            portable = PathBuf::from(text.replace('\\', "/"));
            portable.as_path()
        } else {
            relative
        };
        let mut result = self.root.clone();
        for component in relative.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    if result == self.root || !result.pop() {
                        return None;
                    }
                }
                Component::Normal(part) => {
                    if part.to_str().is_some_and(|text| {
                        text.chars().any(|ch| {
                            ch.is_control() || matches!(ch, ':' | '*' | '?' | '"' | '<' | '>' | '|')
                        }) || text.ends_with(['.', ' '])
                    }) {
                        return None;
                    }
                    result.push(part);
                }
                Component::RootDir | Component::Prefix(_) => return None,
            }
        }
        Some(result)
    }
}

pub(super) fn valid_vendor_class_name(name: &str) -> bool {
    !name.is_empty()
        && name.split('\\').all(|part| {
            let mut chars = part.chars();
            let start = |ch: char| ch == '_' || ch.is_ascii_alphabetic() || ch as u32 >= 0x80;
            chars.next().is_some_and(start) && chars.all(|ch| start(ch) || ch.is_ascii_digit())
        })
}

pub(super) fn valid_vendor_prefix(prefix: &str, psr4: bool) -> bool {
    prefix.is_empty()
        || ((!psr4 || prefix.ends_with('\\'))
            && valid_vendor_class_name(prefix.strip_suffix('\\').unwrap_or(prefix)))
}

pub(super) fn valid_package_name(name: &str) -> bool {
    let parts = name.split('/').collect::<Vec<_>>();
    parts.len() == 2
        && parts.iter().all(|part| {
            part.as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
                && part
                    .bytes()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'_' | b'.' | b'-'))
        })
}
