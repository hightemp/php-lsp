//! Shared types for php-lsp.
//!
//! Contains symbol definitions, type information, and common data structures
//! used across parser, index, and completion crates.

use serde::{Deserialize, Serialize};

pub mod uri;

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
///
/// `Display` output is user-visible in hover, completion details, inlay hints,
/// code actions, and tests. Keep formatting stable unless all callers and
/// regressions are updated deliberately.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TypeInfo {
    Simple(String),
    Generic {
        base: String,
        args: Vec<TypeInfo>,
    },
    ArrayShape(Vec<ArrayShapeItem>),
    ObjectShape(Vec<ArrayShapeItem>),
    Callable {
        params: Vec<TypeInfo>,
        return_type: Option<Box<TypeInfo>>,
    },
    ClassString(Option<Box<TypeInfo>>),
    Conditional {
        subject: String,
        target: Box<TypeInfo>,
        if_type: Box<TypeInfo>,
        else_type: Box<TypeInfo>,
    },
    LiteralString(String),
    LiteralInt(String),
    LiteralFloat(String),
    LiteralBool(bool),
    LiteralNull,
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
            TypeInfo::Generic { base, args } => {
                let parts: Vec<String> = args.iter().map(|t| t.to_string()).collect();
                write!(f, "{}<{}>", base, parts.join(", "))
            }
            TypeInfo::ArrayShape(items) => {
                let parts: Vec<String> = items.iter().map(|item| item.to_string()).collect();
                write!(f, "array{{{}}}", parts.join(", "))
            }
            TypeInfo::ObjectShape(items) => {
                let parts: Vec<String> = items.iter().map(|item| item.to_string()).collect();
                write!(f, "object{{{}}}", parts.join(", "))
            }
            TypeInfo::Callable {
                params,
                return_type,
            } => {
                let parts: Vec<String> = params.iter().map(|t| t.to_string()).collect();
                write!(f, "callable({})", parts.join(", "))?;
                if let Some(return_type) = return_type {
                    write!(f, ": {}", return_type)?;
                }
                Ok(())
            }
            TypeInfo::ClassString(Some(inner)) => write!(f, "class-string<{}>", inner),
            TypeInfo::ClassString(None) => write!(f, "class-string"),
            TypeInfo::Conditional {
                subject,
                target,
                if_type,
                else_type,
            } => write!(
                f,
                "({} is {} ? {} : {})",
                subject, target, if_type, else_type
            ),
            TypeInfo::LiteralString(value) => write!(f, "{}", value),
            TypeInfo::LiteralInt(value) => write!(f, "{}", value),
            TypeInfo::LiteralFloat(value) => write!(f, "{}", value),
            TypeInfo::LiteralBool(value) => write!(f, "{}", value),
            TypeInfo::LiteralNull => write!(f, "null"),
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

/// One item inside an array shape PHPDoc type.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArrayShapeItem {
    pub key: Option<String>,
    pub optional: bool,
    pub value: TypeInfo,
}

impl std::fmt::Display for ArrayShapeItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(ref key) = self.key {
            if self.optional {
                write!(f, "{}?: {}", key, self.value)
            } else {
                write!(f, "{}: {}", key, self.value)
            }
        } else {
            write!(f, "{}", self.value)
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

/// Variance declared for a PHPDoc template parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum TemplateVariance {
    #[default]
    Invariant,
    Covariant,
    Contravariant,
}

/// A `@template` declaration from PHPDoc.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TemplateParam {
    pub name: String,
    pub bound: Option<TypeInfo>,
    pub variance: TemplateVariance,
}

/// The PHPDoc tag that binds template arguments to another type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TemplateBindingKind {
    Extends,
    Implements,
    Use,
    Mixin,
}

/// A generic relation declared by `@extends`, `@implements`, `@use`, or `@mixin`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TemplateBinding {
    pub kind: TemplateBindingKind,
    pub target: String,
    pub args: Vec<TypeInfo>,
}

/// A PHPStan/Psalm local type alias declared by `@phpstan-type` or `@psalm-type`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PhpDocTypeAlias {
    pub name: String,
    pub type_info: TypeInfo,
}

/// A PHPStan/Psalm imported type alias declared by `@phpstan-import-type` or
/// `@psalm-import-type`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PhpDocTypeAliasImport {
    /// Local alias name visible in this scope.
    pub name: String,
    /// Alias name declared on the source type.
    pub source_alias: String,
    /// Source class/interface/trait/enum name as written in PHPDoc.
    pub source_type: String,
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
    pub templates: Vec<TemplateParam>,
    pub template_bindings: Vec<TemplateBinding>,
    pub type_aliases: Vec<PhpDocTypeAlias>,
    pub type_alias_imports: Vec<PhpDocTypeAliasImport>,
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
    pub access: PhpDocPropertyAccess,
    pub description: Option<String>,
}

/// Access mode declared by @property, @property-read or @property-write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PhpDocPropertyAccess {
    ReadWrite,
    ReadOnly,
    WriteOnly,
}

impl PhpDocPropertyAccess {
    pub fn is_readable(self) -> bool {
        matches!(
            self,
            PhpDocPropertyAccess::ReadWrite | PhpDocPropertyAccess::ReadOnly
        )
    }

    pub fn is_writable(self) -> bool {
        matches!(
            self,
            PhpDocPropertyAccess::ReadWrite | PhpDocPropertyAccess::WriteOnly
        )
    }
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
///
/// `range` and `selection_range` use tree-sitter byte columns, not LSP UTF-16
/// columns. Convert them before returning locations/ranges to an LSP client.
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
    /// Byte-column range in the file (start line, start col, end line, end col).
    pub range: (u32, u32, u32, u32),
    /// Byte-column selection range for the name part.
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
    /// Extended class/interface FQNs (for class-like symbols)
    #[serde(default)]
    pub extends: Vec<String>,
    /// Implemented interface FQNs (for classes/enums)
    #[serde(default)]
    pub implements: Vec<String>,
    /// Used trait FQNs (`use SomeTrait;` inside class/trait bodies)
    #[serde(default)]
    pub traits: Vec<String>,
    /// Template parameters declared on this class/function/method.
    #[serde(default)]
    pub templates: Vec<TemplateParam>,
    /// PHPDoc generic bindings declared on this class-like symbol.
    #[serde(default)]
    pub template_bindings: Vec<TemplateBinding>,
}

/// A use statement in a PHP file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UseStatement {
    pub fqn: String,
    pub alias: Option<String>,
    pub kind: UseKind,
    /// Source byte-column range (start line, start col, end line, end col).
    pub range: (u32, u32, u32, u32),
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
    #[serde(default)]
    pub type_aliases: Vec<PhpDocTypeAlias>,
    #[serde(default)]
    pub type_alias_imports: Vec<PhpDocTypeAliasImport>,
}

/// A precomputed symbol occurrence used by references/rename/code lens.
///
/// Unlike `SymbolInfo` ranges, `range` is already an LSP UTF-16 range because
/// references are emitted directly as LSP locations and workspace edits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolReference {
    pub target_fqn: String,
    pub target_kind: PhpSymbolKind,
    /// LSP UTF-16 range: start line/character, end line/character.
    pub range: (u32, u32, u32, u32),
    pub is_declaration: bool,
    /// True when the edited text itself starts with `$` (`$prop`, `Class::$prop`).
    pub starts_with_dollar: bool,
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
        assert_eq!(
            TypeInfo::Generic {
                base: "array".into(),
                args: vec![
                    TypeInfo::Simple("int".into()),
                    TypeInfo::Simple("User".into())
                ],
            }
            .to_string(),
            "array<int, User>"
        );
        assert_eq!(
            TypeInfo::ClassString(Some(Box::new(TypeInfo::Simple("User".into())))).to_string(),
            "class-string<User>"
        );
        assert_eq!(
            TypeInfo::Conditional {
                subject: "$class".into(),
                target: Box::new(TypeInfo::ClassString(Some(Box::new(TypeInfo::Simple(
                    "T".into()
                ))))),
                if_type: Box::new(TypeInfo::Simple("T".into())),
                else_type: Box::new(TypeInfo::Simple("object".into())),
            }
            .to_string(),
            "($class is class-string<T> ? T : object)"
        );
        assert_eq!(
            TypeInfo::Callable {
                params: vec![TypeInfo::Simple("A".into())],
                return_type: Some(Box::new(TypeInfo::Simple("B".into()))),
            }
            .to_string(),
            "callable(A): B"
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
