use super::*;
use crate::parser::FileParser;

fn parse_and_extract(code: &str) -> FileSymbols {
    let mut parser = FileParser::new();
    parser.parse_full(code);
    let tree = parser.tree().unwrap();
    extract_file_symbols(tree, code, "file:///test.php")
}

fn parse_and_extract_for_version(code: &str, major: u16, minor: u16) -> FileSymbols {
    let mut parser = FileParser::new();
    parser.parse_full(code);
    let tree = parser.tree().unwrap();
    extract_file_symbols_for_php_version(
        tree,
        code,
        "phpstub://Core/test.php",
        PhpSymbolExtractionVersion { major, minor },
    )
}

#[test]
fn test_extract_class() {
    let syms = parse_and_extract("<?php\nnamespace App;\nclass Foo {\n}\n");
    assert_eq!(syms.namespace, Some("App".to_string()));
    assert_eq!(syms.symbols.len(), 1);
    assert_eq!(syms.symbols[0].name, "Foo");
    assert_eq!(syms.symbols[0].fqn, "App\\Foo");
    assert_eq!(syms.symbols[0].kind, PhpSymbolKind::Class);
}

#[test]
fn test_extract_interface() {
    let syms = parse_and_extract(
        "<?php\ninterface Loggable {\n    public function log(string $msg): void;\n}\n",
    );
    assert_eq!(syms.symbols.len(), 2); // interface + method
    assert_eq!(syms.symbols[0].kind, PhpSymbolKind::Interface);
    assert_eq!(syms.symbols[0].name, "Loggable");
    assert_eq!(syms.symbols[1].kind, PhpSymbolKind::Method);
    assert_eq!(syms.symbols[1].name, "log");
}

#[test]
fn test_extract_trait() {
    let syms = parse_and_extract(
            "<?php\ntrait HasName {\n    private string $name;\n    public function getName(): string { return $this->name; }\n}\n",
        );
    assert_eq!(syms.symbols[0].kind, PhpSymbolKind::Trait);
    assert!(syms
        .symbols
        .iter()
        .any(|s| s.kind == PhpSymbolKind::Property));
    assert!(syms.symbols.iter().any(|s| s.kind == PhpSymbolKind::Method));
}

#[test]
fn test_extract_enum() {
    let syms = parse_and_extract(
        "<?php\nenum Color {\n    case Red;\n    case Green;\n    case Blue;\n}\n",
    );
    assert_eq!(syms.symbols[0].kind, PhpSymbolKind::Enum);
    assert_eq!(syms.symbols[0].name, "Color");
    let cases: Vec<&SymbolInfo> = syms
        .symbols
        .iter()
        .filter(|s| s.kind == PhpSymbolKind::EnumCase)
        .collect();
    assert_eq!(cases.len(), 3);
    assert_eq!(cases[0].name, "Red");
    assert_eq!(cases[0].fqn, "Color::Red");
}

#[test]
fn test_extract_enum_builtin_properties() {
    let syms = parse_and_extract(
            "<?php\nnamespace App;\ninterface HasCode {}\nenum Level: int implements HasCode { case Info = 200; }\n",
        );
    let name = syms
        .symbols
        .iter()
        .find(|s| s.kind == PhpSymbolKind::Property && s.fqn == "App\\Level::$name")
        .expect("enum name property should be extracted");
    assert!(name.modifiers.is_readonly);
    assert!(matches!(
        name.signature
            .as_ref()
            .and_then(|sig| sig.return_type.as_ref()),
        Some(TypeInfo::Simple(value)) if value == "string"
    ));

    let value = syms
        .symbols
        .iter()
        .find(|s| s.kind == PhpSymbolKind::Property && s.fqn == "App\\Level::$value")
        .expect("backed enum value property should be extracted");
    assert!(value.modifiers.is_readonly);
    assert!(matches!(
        value
            .signature
            .as_ref()
            .and_then(|sig| sig.return_type.as_ref()),
        Some(TypeInfo::Simple(value)) if value == "int"
    ));
}

#[test]
fn test_extract_function() {
    let syms = parse_and_extract(
            "<?php\nnamespace Utils;\nfunction helper(int $x, string $y = 'default'): bool { return true; }\n",
        );
    let func = syms
        .symbols
        .iter()
        .find(|s| s.kind == PhpSymbolKind::Function)
        .unwrap();
    assert_eq!(func.name, "helper");
    assert_eq!(func.fqn, "Utils\\helper");
    let sig = func.signature.as_ref().unwrap();
    assert_eq!(sig.params.len(), 2);
    assert_eq!(sig.params[0].name, "x");
    assert_eq!(sig.params[1].name, "y");
    assert_eq!(sig.params[1].default_value.as_deref(), Some("'default'"));
    assert_eq!(sig.return_type.as_ref().unwrap().to_string(), "bool");
}

#[test]
fn test_extract_method_with_visibility() {
    let syms = parse_and_extract(
            "<?php\nclass Foo {\n    private static function secret(): void {}\n    protected function internal(): int { return 0; }\n    public function api(): string { return ''; }\n}\n",
        );
    let methods: Vec<&SymbolInfo> = syms
        .symbols
        .iter()
        .filter(|s| s.kind == PhpSymbolKind::Method)
        .collect();
    assert_eq!(methods.len(), 3);

    let secret = methods.iter().find(|m| m.name == "secret").unwrap();
    assert_eq!(secret.visibility, Visibility::Private);
    assert!(secret.modifiers.is_static);

    let internal = methods.iter().find(|m| m.name == "internal").unwrap();
    assert_eq!(internal.visibility, Visibility::Protected);

    let api = methods.iter().find(|m| m.name == "api").unwrap();
    assert_eq!(api.visibility, Visibility::Public);
}

#[test]
fn test_static_return_type_is_not_static_modifier() {
    let syms = parse_and_extract(
            "<?php\nclass Foo {\n    public function fluent(): static { return $this; }\n    public static function make(): static { return new static(); }\n}\n",
        );
    let methods: Vec<&SymbolInfo> = syms
        .symbols
        .iter()
        .filter(|s| s.kind == PhpSymbolKind::Method)
        .collect();
    assert_eq!(methods.len(), 2);

    let fluent = methods.iter().find(|m| m.name == "fluent").unwrap();
    assert!(!fluent.modifiers.is_static);
    assert_eq!(
        fluent
            .signature
            .as_ref()
            .and_then(|sig| sig.return_type.as_ref())
            .map(ToString::to_string)
            .as_deref(),
        Some("static")
    );

    let make = methods.iter().find(|m| m.name == "make").unwrap();
    assert!(make.modifiers.is_static);
}

#[test]
fn test_extract_properties() {
    let syms = parse_and_extract(
            "<?php\nclass Foo {\n    public string $name;\n    private int $count = 0;\n    protected readonly float $ratio;\n}\n",
        );
    let props: Vec<&SymbolInfo> = syms
        .symbols
        .iter()
        .filter(|s| s.kind == PhpSymbolKind::Property)
        .collect();
    assert_eq!(props.len(), 3);
    assert_eq!(props[0].name, "name");
    assert_eq!(props[1].name, "count");
    assert_eq!(props[2].name, "ratio");
}

#[test]
fn test_extract_class_constants() {
    let syms = parse_and_extract(
        "<?php\nclass Foo {\n    const VERSION = '1.0';\n    public const MAX = 100;\n}\n",
    );
    let consts: Vec<&SymbolInfo> = syms
        .symbols
        .iter()
        .filter(|s| s.kind == PhpSymbolKind::ClassConstant)
        .collect();
    assert_eq!(consts.len(), 2);
    assert_eq!(consts[0].name, "VERSION");
    assert_eq!(consts[0].fqn, "Foo::VERSION");
}

#[test]
fn test_extract_use_statements() {
    let syms = parse_and_extract(
        "<?php\nuse App\\Service\\Foo;\nuse App\\Entity\\Bar as B;\nuse function App\\helper;\n",
    );
    assert_eq!(syms.use_statements.len(), 3);
    assert_eq!(syms.use_statements[0].fqn, "App\\Service\\Foo");
    assert_eq!(syms.use_statements[0].alias, None);
    assert_eq!(syms.use_statements[0].kind, UseKind::Class);
    assert_eq!(syms.use_statements[0].namespace, None);

    assert_eq!(syms.use_statements[1].fqn, "App\\Entity\\Bar");
    assert_eq!(syms.use_statements[1].alias, Some("B".to_string()));

    assert_eq!(syms.use_statements[2].fqn, "App\\helper");
    assert_eq!(syms.use_statements[2].kind, UseKind::Function);
    assert_eq!(syms.use_statements[2].namespace, None);
}

#[test]
fn test_extract_use_statement_namespace_scopes() {
    let syms = parse_and_extract(
        r#"<?php
namespace App\Controller {
use Symfony\Component\Routing\Attribute\Route;
}
namespace App\Api {
use App\Attribute\Route as LocalRoute;
}
"#,
    );

    assert_eq!(syms.use_statements.len(), 2);
    assert_eq!(
        syms.use_statements[0].namespace.as_deref(),
        Some("App\\Controller")
    );
    assert_eq!(
        syms.use_statements[1].namespace.as_deref(),
        Some("App\\Api")
    );
}

#[test]
fn test_extract_mixed_group_use_clause_kinds_prefixes_and_aliases() {
    let syms = parse_and_extract(
        r#"<?php
use Vendor\Package\{
    Thing as Alias,
    function helper as DoWork,
    const FLAG as LocalFlag
};
use function Vendor\Functions\{first, second as Other};
"#,
    );

    let imports: Vec<(&str, Option<&str>, UseKind)> = syms
        .use_statements
        .iter()
        .map(|statement| {
            (
                statement.fqn.as_str(),
                statement.alias.as_deref(),
                statement.kind,
            )
        })
        .collect();
    assert_eq!(
        imports,
        vec![
            ("Vendor\\Package\\Thing", Some("Alias"), UseKind::Class),
            ("Vendor\\Package\\helper", Some("DoWork"), UseKind::Function,),
            (
                "Vendor\\Package\\FLAG",
                Some("LocalFlag"),
                UseKind::Constant,
            ),
            ("Vendor\\Functions\\first", None, UseKind::Function),
            (
                "Vendor\\Functions\\second",
                Some("Other"),
                UseKind::Function,
            ),
        ]
    );
}

#[test]
fn test_unbracketed_namespace_scopes_limit_repeated_aliases() {
    let syms = parse_and_extract(
        r#"<?php
namespace First;
use Vendor\First\Service as Shared;
new Shared();

namespace Second;
use Vendor\Second\Service as Shared;
new Shared();
"#,
    );

    assert_eq!(syms.namespace_scopes.len(), 2);
    let first = syms.scoped_at_byte_position(3, 4);
    assert_eq!(first.namespace.as_deref(), Some("First"));
    assert_eq!(first.use_statements.len(), 1);
    assert_eq!(first.use_statements[0].fqn, "Vendor\\First\\Service");

    let second = syms.scoped_at_byte_position(7, 4);
    assert_eq!(second.namespace.as_deref(), Some("Second"));
    assert_eq!(second.use_statements.len(), 1);
    assert_eq!(second.use_statements[0].fqn, "Vendor\\Second\\Service");
}

#[test]
fn test_repeated_same_namespace_blocks_isolate_aliases_during_extraction() {
    let syms = parse_and_extract(
        r#"<?php
namespace App {
    use Vendor\First\BaseType as SharedBase;
    class FirstChild extends SharedBase {}
}
namespace App {
    use Vendor\Second\BaseType as SharedBase;
    class SecondChild extends SharedBase {}
}
"#,
    );

    let first = syms
        .symbols
        .iter()
        .find(|symbol| symbol.fqn == r"App\FirstChild")
        .expect("first class should be extracted");
    assert_eq!(first.extends, vec![r"Vendor\First\BaseType"]);

    let second = syms
        .symbols
        .iter()
        .find(|symbol| symbol.fqn == r"App\SecondChild")
        .expect("second class should be extracted");
    assert_eq!(second.extends, vec![r"Vendor\Second\BaseType"]);
}

#[test]
fn test_extract_union_type() {
    let syms =
        parse_and_extract("<?php\nfunction foo(string|int $val): string|null { return ''; }\n");
    let func = &syms.symbols[0];
    let sig = func.signature.as_ref().unwrap();
    assert!(matches!(&sig.params[0].type_info, Some(TypeInfo::Union(_))));
    assert!(matches!(&sig.return_type, Some(TypeInfo::Union(_))));
}

#[test]
fn test_extract_doc_comment() {
    let syms = parse_and_extract("<?php\n/** This is Foo. */\nclass Foo {}\n");
    assert_eq!(
        syms.symbols[0].doc_comment.as_deref(),
        Some("/** This is Foo. */")
    );
}

#[test]
fn test_method_does_not_inherit_previous_method_doc_comment() {
    let syms = parse_and_extract(
        r#"<?php
class Foo {
    /**
     * @return array<string, int>
     */
    public function documented(): array { return []; }

    public function plain(): Bar { return new Bar(); }
}

class Bar {}
"#,
    );

    let documented = syms
        .symbols
        .iter()
        .find(|symbol| symbol.fqn == "Foo::documented")
        .unwrap();
    assert!(documented.doc_comment.is_some());

    let plain = syms
        .symbols
        .iter()
        .find(|symbol| symbol.fqn == "Foo::plain")
        .unwrap();
    assert!(plain.doc_comment.is_none());
    assert_eq!(
        plain
            .signature
            .as_ref()
            .and_then(|signature| signature.return_type.as_ref())
            .map(ToString::to_string),
        Some("Bar".to_string())
    );
}

#[test]
fn test_extract_file_level_type_alias_metadata() {
    let code = r#"<?php
/**
 * @phpstan-type UserShape array{id: int}
 * @phpstan-import-type ExternalShape from Types as LocalShape
 */
namespace App;

function getShape() {}
"#;
    let syms = parse_and_extract(code);

    assert_eq!(syms.type_aliases.len(), 1);
    assert_eq!(syms.type_aliases[0].name, "UserShape");
    assert!(matches!(
        syms.type_aliases[0].type_info,
        TypeInfo::ArrayShape(_)
    ));
    assert_eq!(syms.type_alias_imports.len(), 1);
    assert_eq!(syms.type_alias_imports[0].name, "LocalShape");
    assert_eq!(syms.type_alias_imports[0].source_alias, "ExternalShape");
    assert_eq!(syms.type_alias_imports[0].source_type, "Types");
}

#[test]
fn test_class_type_alias_docblock_is_not_file_level_alias() {
    let syms = parse_and_extract(
        "<?php\n/**\n * @phpstan-type UserShape array{id: int}\n */\nclass Foo {}\n",
    );
    assert!(syms.type_aliases.is_empty());
    assert_eq!(
        syms.symbols[0].doc_comment.as_deref(),
        Some("/**\n * @phpstan-type UserShape array{id: int}\n */")
    );
}

#[test]
fn test_constructor_promotion() {
    let syms = parse_and_extract(
            "<?php\nclass Foo {\n    public function __construct(\n        private string $name,\n        public int $age = 0,\n    ) {}\n}\n",
        );
    let constructor = syms
        .symbols
        .iter()
        .find(|s| s.name == "__construct")
        .unwrap();
    let sig = constructor.signature.as_ref().unwrap();
    assert_eq!(sig.params.len(), 2);
    assert!(sig.params[0].is_promoted);
    assert!(sig.params[1].is_promoted);
}

#[test]
fn test_extract_no_namespace() {
    let syms = parse_and_extract("<?php\nclass GlobalClass {}\nfunction globalFunc(): void {}\n");
    assert_eq!(syms.namespace, None);
    let cls = syms
        .symbols
        .iter()
        .find(|s| s.kind == PhpSymbolKind::Class)
        .unwrap();
    assert_eq!(cls.fqn, "GlobalClass");
    let func = syms
        .symbols
        .iter()
        .find(|s| s.kind == PhpSymbolKind::Function)
        .unwrap();
    assert_eq!(func.fqn, "globalFunc");
}

#[test]
fn test_extract_signature_dedupes_version_gated_duplicate_params() {
    let syms = parse_and_extract(
            "<?php\nfunction array_map(?callable $callback, array $array, $arrays, array ...$arrays): array {}\n",
        );
    let func = syms
        .symbols
        .iter()
        .find(|s| s.kind == PhpSymbolKind::Function)
        .unwrap();
    let sig = func.signature.as_ref().unwrap();

    // callback, array, arrays(variadic)
    assert_eq!(sig.params.len(), 3);
    assert_eq!(sig.params[2].name, "arrays");
    assert!(sig.params[2].is_variadic);
}

#[test]
fn test_phpstorm_stubs_element_available_filters_symbols_by_version() {
    let code = r#"<?php
#[PhpStormStubsElementAvailable('8.1')]
function only_81(): void {}

#[PhpStormStubsElementAvailable(to: '7.4')]
function old_only(): void {}

#[PhpStormStubsElementAvailable(from: '8.0')]
function since_80(): void {}
"#;

    let php80 = parse_and_extract_for_version(code, 8, 0);
    assert!(php80.symbols.iter().any(|symbol| symbol.name == "since_80"));
    assert!(!php80.symbols.iter().any(|symbol| symbol.name == "only_81"));
    assert!(!php80.symbols.iter().any(|symbol| symbol.name == "old_only"));

    let php81 = parse_and_extract_for_version(code, 8, 1);
    assert!(php81.symbols.iter().any(|symbol| symbol.name == "only_81"));
    assert!(php81.symbols.iter().any(|symbol| symbol.name == "since_80"));
    assert!(!php81.symbols.iter().any(|symbol| symbol.name == "old_only"));
}

#[test]
fn test_phpstorm_stubs_element_available_filters_params_by_version() {
    let code = r#"<?php
function demo(
    #[PhpStormStubsElementAvailable(from: '5.3', to: '7.4')] $value = null,
    #[PhpStormStubsElementAvailable(from: '8.0')] string $value,
    #[PhpStormStubsElementAvailable('8.1')] int $mode = 0
): void {}
"#;

    let php74 = parse_and_extract_for_version(code, 7, 4);
    let php74_sig = php74.symbols[0].signature.as_ref().unwrap();
    assert_eq!(php74_sig.params.len(), 1);
    assert_eq!(php74_sig.params[0].name, "value");
    assert!(php74_sig.params[0].default_value.is_some());

    let php80 = parse_and_extract_for_version(code, 8, 0);
    let php80_sig = php80.symbols[0].signature.as_ref().unwrap();
    assert_eq!(php80_sig.params.len(), 1);
    assert_eq!(php80_sig.params[0].name, "value");
    assert!(php80_sig.params[0].type_info.is_some());
    assert!(php80_sig.params[0].default_value.is_none());

    let php81 = parse_and_extract_for_version(code, 8, 1);
    let php81_sig = php81.symbols[0].signature.as_ref().unwrap();
    assert_eq!(php81_sig.params.len(), 2);
    assert_eq!(php81_sig.params[1].name, "mode");
}

#[test]
fn test_extract_namespaced_global_constant() {
    let syms = parse_and_extract("<?php\nnamespace App;\n\nconst BUILD = 'dev';\n");
    let c = syms
        .symbols
        .iter()
        .find(|s| s.kind == PhpSymbolKind::GlobalConstant)
        .expect("global constant should be extracted");
    assert_eq!(c.name, "BUILD");
    assert_eq!(c.fqn, "App\\BUILD");
}

#[test]
fn test_extract_class_extends() {
    let syms = parse_and_extract(
        "<?php\nnamespace App;\n\nuse App\\Base\\BaseClass;\n\nclass Foo extends BaseClass {}\n",
    );
    let cls = syms
        .symbols
        .iter()
        .find(|s| s.kind == PhpSymbolKind::Class)
        .unwrap();
    assert_eq!(cls.name, "Foo");
    assert_eq!(cls.fqn, "App\\Foo");
    assert_eq!(cls.extends, vec!["App\\Base\\BaseClass".to_string()]);
    assert!(cls.implements.is_empty());
}

#[test]
fn test_extract_class_template_metadata() {
    let syms = parse_and_extract(
        r#"<?php
namespace App;

use Vendor\Repository\BaseRepository;
use Vendor\Entity\User;

/**
 * @template TEntity of object
 * @extends BaseRepository<TEntity>
 * @mixin \Vendor\Builder<User>
 */
class UserRepository extends BaseRepository {}
"#,
    );
    let cls = syms
        .symbols
        .iter()
        .find(|s| s.kind == PhpSymbolKind::Class)
        .unwrap();

    assert_eq!(cls.templates.len(), 1);
    assert_eq!(cls.templates[0].name, "TEntity");
    assert_eq!(
        cls.templates[0].bound,
        Some(TypeInfo::Simple("object".to_string()))
    );
    assert_eq!(cls.template_bindings.len(), 2);
    assert_eq!(cls.template_bindings[0].kind, TemplateBindingKind::Extends);
    assert_eq!(
        cls.template_bindings[0].target,
        "Vendor\\Repository\\BaseRepository"
    );
    assert_eq!(
        cls.template_bindings[0].args,
        vec![TypeInfo::Simple("TEntity".to_string())]
    );
    assert_eq!(cls.template_bindings[1].kind, TemplateBindingKind::Mixin);
    assert_eq!(cls.template_bindings[1].target, "Vendor\\Builder");
    assert_eq!(
        cls.template_bindings[1].args,
        vec![TypeInfo::Simple("Vendor\\Entity\\User".to_string())]
    );
}

#[test]
fn test_extract_doctrine_repository_class_attribute_metadata() {
    let syms = parse_and_extract(
        r#"<?php
namespace App\Entity;

use App\Repository\OrderRepository;
use Doctrine\ORM\Mapping as ORM;

#[ORM\Entity(repositoryClass: OrderRepository::class)]
class Order {}
"#,
    );
    let cls = syms
        .symbols
        .iter()
        .find(|s| s.kind == PhpSymbolKind::Class && s.name == "Order")
        .unwrap();

    assert_eq!(cls.template_bindings.len(), 1);
    assert_eq!(
        cls.template_bindings[0].kind,
        TemplateBindingKind::RepositoryClass
    );
    assert_eq!(
        cls.template_bindings[0].target,
        "App\\Repository\\OrderRepository"
    );
    assert!(cls.template_bindings[0].args.is_empty());
    assert_eq!(cls.attributes.len(), 1);
    assert_eq!(
        cls.attributes[0].text,
        r#"#[ORM\Entity(repositoryClass: OrderRepository::class)]"#
    );
    assert_eq!(cls.attributes[0].range, (6, 0, 6, 54));
}

#[test]
fn test_extract_symbol_attribute_metadata_for_members() {
    let syms = parse_and_extract(
        r#"<?php
namespace App\Controller;

use Doctrine\ORM\Mapping as ORM;
use Symfony\Component\Routing\Attribute\Route;

class DashboardController {
    #[Route('/dashboard', name: 'app_dashboard')]
    public function dashboard(): void {}

    #[ORM\ManyToOne(targetEntity: User::class)]
    private User $owner;
}
"#,
    );
    let method = syms
        .symbols
        .iter()
        .find(|s| s.kind == PhpSymbolKind::Method && s.name == "dashboard")
        .unwrap();
    let property = syms
        .symbols
        .iter()
        .find(|s| s.kind == PhpSymbolKind::Property && s.name == "owner")
        .unwrap();

    assert_eq!(method.attributes.len(), 1);
    assert_eq!(
        method.attributes[0].text,
        r#"#[Route('/dashboard', name: 'app_dashboard')]"#
    );
    assert_eq!(method.attributes[0].range.0, 7);
    assert_eq!(method.attributes[0].range.1, 4);

    assert_eq!(property.attributes.len(), 1);
    assert_eq!(
        property.attributes[0].text,
        r#"#[ORM\ManyToOne(targetEntity: User::class)]"#
    );
    assert_eq!(property.attributes[0].range.0, 10);
    assert_eq!(property.attributes[0].range.1, 4);
}

#[test]
fn test_extract_symbol_attribute_metadata_ignores_brackets_inside_strings() {
    let syms = parse_and_extract(
        r#"<?php
namespace App\Controller;

use Symfony\Component\Routing\Attribute\Route;
use Symfony\Component\Validator\Constraints as Assert;

class FileController {
    private string $open = '[';

    #[Route('/file[0-9].csv', name: 'file_[id]')]
    #[Assert\Regex(pattern: '/^[a-z]+$/')]
    public function download(): void {}
}
"#,
    );
    let method = syms
        .symbols
        .iter()
        .find(|s| s.kind == PhpSymbolKind::Method && s.name == "download")
        .unwrap();

    assert_eq!(
        method
            .attributes
            .iter()
            .map(|attribute| attribute.text.as_str())
            .collect::<Vec<_>>(),
        vec![
            "#[Route('/file[0-9].csv', name: 'file_[id]')]",
            "#[Assert\\Regex(pattern: '/^[a-z]+$/')]",
        ]
    );
}

#[test]
fn test_extract_function_and_method_templates() {
    let syms = parse_and_extract(
        r#"<?php
/**
 * @template TResult
 * @param class-string<TResult> $class
 * @return TResult
 */
function make(string $class) {}

class Factory {
    /**
     * @template TItem
     * @return TItem
     */
    public function item() {}
}
"#,
    );

    let func = syms
        .symbols
        .iter()
        .find(|s| s.kind == PhpSymbolKind::Function)
        .unwrap();
    assert_eq!(func.templates.len(), 1);
    assert_eq!(func.templates[0].name, "TResult");
    assert_eq!(
        func.signature.as_ref().unwrap().return_type,
        Some(TypeInfo::Simple("TResult".to_string()))
    );
    assert_eq!(
        func.signature.as_ref().unwrap().params[0].type_info,
        Some(TypeInfo::ClassString(Some(Box::new(TypeInfo::Simple(
            "TResult".to_string()
        )))))
    );

    let method = syms
        .symbols
        .iter()
        .find(|s| s.kind == PhpSymbolKind::Method)
        .unwrap();
    assert_eq!(method.templates.len(), 1);
    assert_eq!(method.templates[0].name, "TItem");
    assert_eq!(
        method.signature.as_ref().unwrap().return_type,
        Some(TypeInfo::Simple("TItem".to_string()))
    );
}

#[test]
fn test_extract_class_implements() {
    let syms = parse_and_extract(
        "<?php\nnamespace App;\n\nclass Foo implements \\Countable, \\Serializable {}\n",
    );
    let cls = syms
        .symbols
        .iter()
        .find(|s| s.kind == PhpSymbolKind::Class)
        .unwrap();
    assert_eq!(
        cls.implements,
        vec!["Countable".to_string(), "Serializable".to_string()]
    );
}

#[test]
fn test_extract_trait_uses() {
    let syms = parse_and_extract(
            "<?php\nnamespace App;\n\nuse Vendor\\Shared\\Auditable;\n\nclass Foo {\n    use Auditable;\n    use LocalTrait;\n}\n",
        );
    let cls = syms
        .symbols
        .iter()
        .find(|s| s.kind == PhpSymbolKind::Class)
        .unwrap();
    assert_eq!(
        cls.traits,
        vec![
            "Vendor\\Shared\\Auditable".to_string(),
            "App\\LocalTrait".to_string()
        ]
    );
}

#[test]
fn test_extract_class_extends_and_implements() {
    let syms = parse_and_extract("<?php\nclass Child extends Parent_ implements Foo, Bar {}\n");
    let cls = syms
        .symbols
        .iter()
        .find(|s| s.kind == PhpSymbolKind::Class)
        .unwrap();
    assert_eq!(cls.extends, vec!["Parent_".to_string()]);
    assert_eq!(cls.implements, vec!["Foo".to_string(), "Bar".to_string()]);
}

#[test]
fn test_extract_interface_extends() {
    let syms = parse_and_extract("<?php\ninterface Foo extends Bar, Baz {}\n");
    let iface = syms
        .symbols
        .iter()
        .find(|s| s.kind == PhpSymbolKind::Interface)
        .unwrap();
    assert_eq!(iface.extends, vec!["Bar".to_string(), "Baz".to_string()]);
    assert!(iface.implements.is_empty());
}

#[test]
fn test_phpdoc_optional_sets_default_value() {
    // Simulates mb_strtolower stub: $encoding has no default but PHPDoc says [optional]
    let syms = parse_and_extract(
        r#"<?php
/**
 * @param string $string The string
 * @param string|null $encoding [optional]
 * @return string
 */
function mb_strtolower(string $string, ?string $encoding): string {}
"#,
    );
    let func = syms
        .symbols
        .iter()
        .find(|s| s.kind == PhpSymbolKind::Function && s.name == "mb_strtolower")
        .unwrap();
    let sig = func.signature.as_ref().unwrap();
    assert_eq!(sig.params.len(), 2);
    // $string has no default
    assert!(sig.params[0].default_value.is_none());
    // $encoding should now have a synthetic default from [optional]
    assert!(
        sig.params[1].default_value.is_some(),
        "$encoding should have a synthetic default_value from PHPDoc [optional]"
    );
}

#[test]
fn test_phpdoc_optional_on_byref_param() {
    // Simulates str_replace stub: &$count has no default but PHPDoc says [optional]
    let syms = parse_and_extract(
        r#"<?php
/**
 * @param array|string $search
 * @param array|string $replace
 * @param array|string $subject
 * @param int &$count [optional] How many replacements were done
 * @return array|string
 */
function str_replace(array|string $search, array|string $replace, array|string $subject, &$count): array|string {}
"#,
    );
    let func = syms
        .symbols
        .iter()
        .find(|s| s.kind == PhpSymbolKind::Function && s.name == "str_replace")
        .unwrap();
    let sig = func.signature.as_ref().unwrap();
    assert_eq!(sig.params.len(), 4);
    // First 3 have no default
    assert!(sig.params[0].default_value.is_none());
    assert!(sig.params[1].default_value.is_none());
    assert!(sig.params[2].default_value.is_none());
    // &$count should have a synthetic default from [optional]
    assert!(
        sig.params[3].default_value.is_some(),
        "&$count should have a synthetic default_value from PHPDoc [optional]"
    );
}

#[test]
fn test_promoted_constructor_params_emit_property_symbols() {
    let syms = parse_and_extract(
        r#"<?php
namespace App;

class MyService {
    public function __construct(
        protected readonly LoggerInterface $logger,
        private string $name,
        int $notPromoted = 0,
    ) {}
}
"#,
    );

    // Should have Property symbols for promoted params
    let logger_prop = syms
        .symbols
        .iter()
        .find(|s| s.kind == PhpSymbolKind::Property && s.fqn == "App\\MyService::$logger");
    assert!(
        logger_prop.is_some(),
        "Expected Property symbol for promoted $logger, symbols: {:?}",
        syms.symbols
            .iter()
            .map(|s| (&s.fqn, &s.kind))
            .collect::<Vec<_>>()
    );
    let logger = logger_prop.unwrap();
    assert_eq!(logger.visibility, Visibility::Protected);
    assert!(logger.modifiers.is_readonly);
    // Type should be LoggerInterface
    let ret_type = logger
        .signature
        .as_ref()
        .and_then(|s| s.return_type.as_ref());
    assert!(
        matches!(ret_type, Some(TypeInfo::Simple(t)) if t == "LoggerInterface"),
        "Expected LoggerInterface type, got: {:?}",
        ret_type
    );

    let name_prop = syms
        .symbols
        .iter()
        .find(|s| s.kind == PhpSymbolKind::Property && s.fqn == "App\\MyService::$name");
    assert!(
        name_prop.is_some(),
        "Expected Property symbol for promoted $name"
    );
    let name = name_prop.unwrap();
    assert_eq!(name.visibility, Visibility::Private);

    // $notPromoted is a regular parameter — should NOT be a Property
    let not_promoted = syms
        .symbols
        .iter()
        .find(|s| s.kind == PhpSymbolKind::Property && s.name == "notPromoted");
    assert!(
        not_promoted.is_none(),
        "Regular param $notPromoted should NOT become a Property symbol"
    );
}

#[test]
fn test_property_phpdoc_var_sets_property_type_when_native_type_is_missing() {
    let syms = parse_and_extract(
        r#"<?php
namespace App;

use App\Entity\User;

class Holder {
    /** @var User $user */
    private $user;

    /** @var User $native */
    private Account $native;
}
"#,
    );

    let user_prop = syms
        .symbols
        .iter()
        .find(|s| s.kind == PhpSymbolKind::Property && s.fqn == "App\\Holder::$user")
        .expect("property should be extracted");
    let user_type = user_prop
        .signature
        .as_ref()
        .and_then(|sig| sig.return_type.as_ref());
    assert!(matches!(user_type, Some(TypeInfo::Simple(name)) if name == "User"));

    let native_prop = syms
        .symbols
        .iter()
        .find(|s| s.kind == PhpSymbolKind::Property && s.fqn == "App\\Holder::$native")
        .expect("native property should be extracted");
    let native_type = native_prop
        .signature
        .as_ref()
        .and_then(|sig| sig.return_type.as_ref());
    assert!(matches!(native_type, Some(TypeInfo::Simple(name)) if name == "Account"));
}

#[test]
fn test_phpdoc_method_tags_emit_virtual_method_symbols() {
    let syms = parse_and_extract(
        r#"<?php
namespace App;

/**
 * @method void refresh(string &$token, int ...$ids, [bool $force])
 * @method static User make()
 */
interface Helper {}
"#,
    );

    let refresh = syms
        .symbols
        .iter()
        .find(|s| s.kind == PhpSymbolKind::Method && s.fqn == "App\\Helper::refresh")
        .expect("@method refresh should be emitted as a method symbol");
    assert_eq!(refresh.parent_fqn.as_deref(), Some("App\\Helper"));
    assert!(!refresh.modifiers.is_static);
    assert!(matches!(
        refresh
            .signature
            .as_ref()
            .and_then(|sig| sig.return_type.as_ref()),
        Some(TypeInfo::Void)
    ));
    let refresh_params = &refresh.signature.as_ref().unwrap().params;
    assert_eq!(refresh_params.len(), 3);
    assert_eq!(refresh_params[0].name, "token");
    assert!(refresh_params[0].is_by_ref);
    assert_eq!(
        refresh_params[0].type_info,
        Some(TypeInfo::Simple("string".to_string()))
    );
    assert_eq!(refresh_params[1].name, "ids");
    assert!(refresh_params[1].is_variadic);
    assert_eq!(
        refresh_params[1].type_info,
        Some(TypeInfo::Simple("int".to_string()))
    );
    assert_eq!(refresh_params[2].name, "force");
    assert_eq!(refresh_params[2].default_value.as_deref(), Some("null"));

    let make = syms
        .symbols
        .iter()
        .find(|s| s.kind == PhpSymbolKind::Method && s.fqn == "App\\Helper::make")
        .expect("@method static make should be emitted as a method symbol");
    assert!(make.modifiers.is_static);
}

#[test]
fn test_phpdoc_property_tags_emit_virtual_property_symbols() {
    let syms = parse_and_extract(
        r#"<?php
namespace App;

/**
 * @property int $current_logid
 * @property-read string $id
 * @property-write string $secret
 */
interface Loggable {}
"#,
    );

    let current_logid = syms
        .symbols
        .iter()
        .find(|s| s.kind == PhpSymbolKind::Property && s.fqn == "App\\Loggable::$current_logid")
        .expect("@property current_logid should be emitted as a property symbol");
    assert!(matches!(
        current_logid
            .signature
            .as_ref()
            .and_then(|sig| sig.return_type.as_ref()),
        Some(TypeInfo::Simple(value)) if value == "int"
    ));

    assert!(syms
        .symbols
        .iter()
        .any(|s| s.kind == PhpSymbolKind::Property && s.fqn == "App\\Loggable::$id"));
    assert!(!syms
        .symbols
        .iter()
        .any(|s| s.kind == PhpSymbolKind::Property && s.fqn == "App\\Loggable::$secret"));
}
