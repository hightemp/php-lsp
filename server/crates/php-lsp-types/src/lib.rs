//! Shared types for php-lsp.
//!
//! Contains symbol definitions, type information, and common data structures
//! used across parser, index, and completion crates.

use serde::{Deserialize, Serialize};

/// Kind of a PHP symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PhpSymbolKind {
    Class,
    Interface,
    Trait,
    Enum,
    Function,
    Method,
    Property,
    ClassConstant,
    GlobalConstant,
    EnumCase,
    Namespace,
}

impl PhpSymbolKind {
    /// Convert to LSP SymbolKind.
    pub fn to_lsp_symbol_kind(self) -> lsp_types::SymbolKind {
        match self {
            PhpSymbolKind::Class => lsp_types::SymbolKind::CLASS,
            PhpSymbolKind::Interface => lsp_types::SymbolKind::INTERFACE,
            PhpSymbolKind::Trait => lsp_types::SymbolKind::INTERFACE,
            PhpSymbolKind::Enum => lsp_types::SymbolKind::ENUM,
            PhpSymbolKind::Function => lsp_types::SymbolKind::FUNCTION,
            PhpSymbolKind::Method => lsp_types::SymbolKind::METHOD,
            PhpSymbolKind::Property => lsp_types::SymbolKind::PROPERTY,
            PhpSymbolKind::ClassConstant => lsp_types::SymbolKind::CONSTANT,
            PhpSymbolKind::GlobalConstant => lsp_types::SymbolKind::CONSTANT,
            PhpSymbolKind::EnumCase => lsp_types::SymbolKind::ENUM_MEMBER,
            PhpSymbolKind::Namespace => lsp_types::SymbolKind::NAMESPACE,
        }
    }
}

/// Visibility modifier for class members.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum Visibility {
    #[default]
    Public,
    Protected,
    Private,
}

/// Modifiers on a symbol (bitflags-style).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct SymbolModifiers {
    pub is_static: bool,
    pub is_abstract: bool,
    pub is_final: bool,
    pub is_readonly: bool,
    pub is_deprecated: bool,
    pub is_builtin: bool,
}

/// Represents a PHP type.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TypeInfo {
    Simple(String),
    Union(Vec<TypeInfo>),
    Intersection(Vec<TypeInfo>),
    Nullable(Box<TypeInfo>),
    Void,
    Never,
    Mixed,
    Self_,
    Static_,
    Parent_,
}

impl std::fmt::Display for TypeInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeInfo::Simple(name) => write!(f, "{}", name),
            TypeInfo::Union(types) => {
                let parts: Vec<String> = types.iter().map(|t| t.to_string()).collect();
                write!(f, "{}", parts.join("|"))
            }
            TypeInfo::Intersection(types) => {
                let parts: Vec<String> = types.iter().map(|t| t.to_string()).collect();
                write!(f, "{}", parts.join("&"))
            }
            TypeInfo::Nullable(inner) => write!(f, "?{}", inner),
            TypeInfo::Void => write!(f, "void"),
            TypeInfo::Never => write!(f, "never"),
            TypeInfo::Mixed => write!(f, "mixed"),
            TypeInfo::Self_ => write!(f, "self"),
            TypeInfo::Static_ => write!(f, "static"),
            TypeInfo::Parent_ => write!(f, "parent"),
        }
    }
}

/// Parameter information for a function/method.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParamInfo {
    pub name: String,
    pub type_info: Option<TypeInfo>,
    pub default_value: Option<String>,
    pub is_variadic: bool,
    pub is_by_ref: bool,
    pub is_promoted: bool,
}

/// Function/method signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    pub params: Vec<ParamInfo>,
    pub return_type: Option<TypeInfo>,
}

/// Parsed PHPDoc information.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhpDoc {
    pub summary: Option<String>,
    pub params: Vec<PhpDocParam>,
    pub return_type: Option<TypeInfo>,
    pub var_type: Option<TypeInfo>,
    pub throws: Vec<TypeInfo>,
    pub deprecated: Option<String>,
    pub properties: Vec<PhpDocProperty>,
    pub methods: Vec<PhpDocMethod>,
}

/// A @param tag from PHPDoc.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhpDocParam {
    pub name: String,
    pub type_info: Option<TypeInfo>,
    pub description: Option<String>,
}

/// A @property tag from PHPDoc.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhpDocProperty {
    pub name: String,
    pub type_info: Option<TypeInfo>,
    pub description: Option<String>,
}

/// A @method tag from PHPDoc.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhpDocMethod {
    pub name: String,
    pub return_type: Option<TypeInfo>,
    pub params: Vec<ParamInfo>,
    pub is_static: bool,
    pub description: Option<String>,
}

/// Full information about a symbol in the index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolInfo {
    /// Short name (e.g. "Foo", "bar", "BAZ")
    pub name: String,
    /// Fully Qualified Name (e.g. "App\\Service\\Foo")
    pub fqn: String,
    /// Kind of symbol
    pub kind: PhpSymbolKind,
    /// File URI
    pub uri: String,
    /// Range in the file (start line, start col, end line, end col)
    pub range: (u32, u32, u32, u32),
    /// Selection range (the name part)
    pub selection_range: (u32, u32, u32, u32),
    /// Visibility
    pub visibility: Visibility,
    /// Modifiers
    pub modifiers: SymbolModifiers,
    /// Raw doc comment
    pub doc_comment: Option<String>,
    /// Parsed signature (for functions/methods)
    pub signature: Option<Signature>,
    /// Parent FQN (for methods/properties → class FQN)
    pub parent_fqn: Option<String>,
}

/// A use statement in a PHP file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UseStatement {
    pub fqn: String,
    pub alias: Option<String>,
    pub kind: UseKind,
}

/// Kind of use statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UseKind {
    Class,
    Function,
    Constant,
}

/// Symbols extracted from a single file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileSymbols {
    pub namespace: Option<String>,
    pub use_statements: Vec<UseStatement>,
    pub symbols: Vec<SymbolInfo>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_type_info_display() {
        assert_eq!(TypeInfo::Simple("string".into()).to_string(), "string");
        assert_eq!(TypeInfo::Void.to_string(), "void");
        assert_eq!(
            TypeInfo::Union(vec![
                TypeInfo::Simple("string".into()),
                TypeInfo::Simple("int".into()),
            ])
            .to_string(),
            "string|int"
        );
        assert_eq!(
            TypeInfo::Nullable(Box::new(TypeInfo::Simple("Foo".into()))).to_string(),
            "?Foo"
        );
    }

    #[test]
    fn test_symbol_kind_to_lsp() {
        assert_eq!(
            PhpSymbolKind::Class.to_lsp_symbol_kind(),
            lsp_types::SymbolKind::CLASS
        );
        assert_eq!(
            PhpSymbolKind::Function.to_lsp_symbol_kind(),
            lsp_types::SymbolKind::FUNCTION
        );
    }
}
