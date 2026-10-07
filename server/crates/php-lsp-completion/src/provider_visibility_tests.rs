use super::*;
use php_lsp_parser::{parser::FileParser, symbols::extract_file_symbols};
use std::collections::BTreeSet;

struct Fixture {
    source: String,
    symbols: FileSymbols,
    index: WorkspaceIndex,
}

impl Fixture {
    fn new(source: &str) -> Self {
        let mut parser = FileParser::new();
        parser.parse_full(source);
        let symbols =
            extract_file_symbols(parser.tree().unwrap(), source, "file:///visibility.php");
        let index = WorkspaceIndex::new();
        index.update_file("file:///visibility.php", symbols.clone());
        Self {
            source: source.into(),
            symbols,
            index,
        }
    }

    fn items(&self, marker: &str, context: CompletionContext) -> Vec<CompletionItem> {
        let before = &self.source[..self.source.find(marker).unwrap()];
        let line = before.bytes().filter(|byte| *byte == b'\n').count() as u32;
        let col = before.rsplit('\n').next().unwrap().len() as u32;
        provide_completions_at_range(&context, &self.index, &self.symbols, (line, col, line, col))
    }

    fn instance(&self, marker: &str, receiver: &str, expression: &str) -> BTreeSet<String> {
        self.items(
            marker,
            CompletionContext::MemberAccess {
                object_expr: expression.into(),
                class_fqn: Some(receiver.into()),
                member_prefix: String::new(),
                access_mode: MemberAccessMode::Read,
            },
        )
        .into_iter()
        .map(|item| item.label)
        .collect()
    }

    fn static_labels(&self, marker: &str, receiver: &str, expression: &str) -> BTreeSet<String> {
        self.items(
            marker,
            CompletionContext::StaticAccess {
                class_expr: expression.into(),
                class_fqn: receiver.into(),
                member_prefix: String::new(),
            },
        )
        .into_iter()
        .map(|item| item.label)
        .collect()
    }
}

#[test]
fn other_instance_has_same_class_private_and_protected_access() {
    let fixture = Fixture::new(
        r#"<?php
class Subject {
    public function visible() {}
    protected function guarded() {}
    private function secret() {}
    protected int $guardedValue;
    private int $secretValue;
    public function inspect(Subject $other) { /*inside*/ }
}
class Stranger { public function inspect(Subject $other) { /*foreign*/ } }
/*outside*/
"#,
    );
    for expression in ["$this", "$other", "$factory->get()"] {
        let labels = fixture.instance("/*inside*/", "Subject", expression);
        for expected in ["guarded", "secret", "guardedValue", "secretValue"] {
            assert!(
                labels.contains(expected),
                "{expression}: missing {expected}: {labels:?}"
            );
        }
    }
    for marker in ["/*foreign*/", "/*outside*/"] {
        let labels = fixture.instance(marker, "Subject", "$other");
        assert!(labels.contains("visible"));
        for forbidden in ["guarded", "secret", "guardedValue", "secretValue"] {
            assert!(
                !labels.contains(forbidden),
                "{marker}: leaked {forbidden}: {labels:?}"
            );
        }
    }
}

#[test]
fn inheritance_visibility_depends_on_declaring_scope_not_receiver_spelling() {
    let fixture = Fixture::new(
        r#"<?php
class Base {
    protected function baseGuarded() {}
    private function baseSecret() {}
    public function inspect() { /*base*/ }
}
class Child extends Base {
    protected function childGuarded() {}
    private function childSecret() {}
    public function inspectChild() { /*child*/ }
}
class Sibling extends Base { public function inspectSibling() { /*sibling*/ } }
"#,
    );
    let from_base = fixture.instance("/*base*/", "Child", "$other");
    assert!(
        from_base.contains("baseGuarded") && from_base.contains("childGuarded"),
        "{from_base:?}"
    );
    assert!(from_base.contains("baseSecret"));
    assert!(!from_base.contains("childSecret"));
    let from_child = fixture.instance("/*child*/", "Base", "$other");
    assert!(from_child.contains("baseGuarded"), "{from_child:?}");
    assert!(!from_child.contains("baseSecret"));
    let from_sibling = fixture.instance("/*sibling*/", "Child", "$other");
    assert!(from_sibling.contains("baseGuarded"), "{from_sibling:?}");
    assert!(!from_sibling.contains("childGuarded") && !from_sibling.contains("childSecret"));
}

#[test]
fn named_static_access_has_class_scope_visibility_for_methods_properties_and_constants() {
    let fixture = Fixture::new(
        r#"<?php
namespace App;
class Subject {
    protected static function guarded() {}
    private static function secret() {}
    protected static int $guardedValue;
    private static int $secretValue;
    protected const GUARDED = 1;
    private const SECRET = 1;
    public function inspect() { /*inside*/ }
}
class Stranger { public function inspect() { /*foreign*/ } }
/*outside*/
"#,
    );
    for expression in ["self", "static", "Subject", "Alias", "\\APP\\subject"] {
        let labels = fixture.static_labels("/*inside*/", "\\app\\SUBJECT", expression);
        for expected in [
            "guarded",
            "secret",
            "$guardedValue",
            "$secretValue",
            "GUARDED",
            "SECRET",
        ] {
            assert!(
                labels.contains(expected),
                "{expression}: missing {expected}: {labels:?}"
            );
        }
    }
    for marker in ["/*foreign*/", "/*outside*/"] {
        let labels = fixture.static_labels(marker, "App\\Subject", "Subject");
        assert_eq!(labels, BTreeSet::from(["class".into()]));
    }
}

const TRAITS: &str = r#"<?php
trait Secrets {
    private function secret() {}
    protected function guarded() {}
    private int $secretValue;
    private static function staticSecret() {}
    private const SECRET = 1;
}
trait Wrapper { use Secrets; public function inspectTrait() { /*trait*/ } }
class Owner { use Wrapper; public function inspect() { /*owner*/ } }
class Child extends Owner { public function inspectChild() { /*child*/ } }
class Stranger { use Wrapper; public function inspectStranger() { /*stranger*/ } }
"#;

#[test]
fn nested_trait_private_members_belong_to_consuming_class_and_keep_source_identity() {
    let fixture = Fixture::new(TRAITS);
    for receiver in ["Owner", "Child", "\\oWnEr"] {
        let labels = fixture.instance("/*owner*/", receiver, "$other");
        assert!(
            labels.contains("secret") && labels.contains("secretValue"),
            "{receiver}: {labels:?}"
        );
    }
    let items = fixture.items(
        "/*owner*/",
        CompletionContext::MemberAccess {
            object_expr: "$this".into(),
            class_fqn: Some("Owner".into()),
            member_prefix: String::new(),
            access_mode: MemberAccessMode::Read,
        },
    );
    let secret = items
        .iter()
        .find(|item| item.label == "secret")
        .expect("private trait method");
    assert_eq!(secret.data, Some(json!("Secrets::secret")));
    let declaration = fixture.index.resolve_member("Owner::secret").unwrap();
    assert_eq!(declaration.parent_fqn.as_deref(), Some("Secrets"));
    assert_eq!(declaration.uri, "file:///visibility.php");
    let labels = fixture.static_labels("/*owner*/", "Owner", "Owner");
    assert!(
        labels.contains("staticSecret") && labels.contains("SECRET"),
        "{labels:?}"
    );
}

#[test]
fn trait_private_members_do_not_leak_to_subclasses_or_unrelated_consumers() {
    let fixture = Fixture::new(TRAITS);
    for (marker, receiver) in [
        ("/*child*/", "Child"),
        ("/*owner*/", "Stranger"),
        ("/*stranger*/", "Owner"),
    ] {
        let labels = fixture.instance(marker, receiver, "$other");
        assert!(
            !labels.contains("secret") && !labels.contains("secretValue"),
            "{marker} on {receiver}: {labels:?}"
        );
        let labels = fixture.static_labels(marker, receiver, "self");
        assert!(
            !labels.contains("staticSecret") && !labels.contains("SECRET"),
            "{marker} on {receiver}: {labels:?}"
        );
    }
    let own = fixture.instance("/*stranger*/", "Stranger", "$this");
    assert!(own.contains("secret"), "{own:?}");
}

#[test]
fn protected_trait_visibility_uses_receiver_consuming_hierarchy() {
    let fixture = Fixture::new(TRAITS);
    let inherited = fixture.instance("/*child*/", "Owner", "$other");
    assert!(inherited.contains("guarded"), "{inherited:?}");
    for (marker, receiver) in [("/*owner*/", "Stranger"), ("/*stranger*/", "Child")] {
        // Even a malformed/incomplete context spelling `$this` must not bypass the relationship check.
        let labels = fixture.instance(marker, receiver, "$this");
        assert!(
            !labels.contains("guarded"),
            "{marker} on {receiver}: {labels:?}"
        );
    }
}

#[test]
fn nested_trait_scope_and_cycles_are_safe() {
    let mut fixture = Fixture::new(TRAITS);
    let labels = fixture.instance("/*trait*/", "Wrapper", "$this");
    assert!(labels.contains("secret"), "{labels:?}");
    fixture
        .symbols
        .symbols
        .iter_mut()
        .find(|symbol| symbol.fqn == "Secrets")
        .unwrap()
        .traits
        .push("Wrapper".into());
    fixture
        .index
        .update_file("file:///visibility.php", fixture.symbols.clone());
    let cyclic = fixture.instance("/*trait*/", "Wrapper", "$this");
    assert!(cyclic.contains("secret"), "{cyclic:?}");
}

#[test]
fn open_buffer_trait_relationship_replaces_stale_index_access_scope() {
    let mut fixture = Fixture::new(TRAITS);
    fixture
        .symbols
        .symbols
        .iter_mut()
        .find(|symbol| symbol.fqn == "Owner")
        .unwrap()
        .traits
        .clear();
    let labels = fixture.instance("/*owner*/", "Owner", "$this");
    assert!(
        !labels.contains("secret") && !labels.contains("guarded"),
        "removed trait leaked: {labels:?}"
    );
}

#[test]
fn protected_overrides_keep_derived_completion_identity_for_sibling_callers() {
    let fixture = Fixture::new(
        r#"<?php
class Base { protected function common(): Base {} }
class Child extends Base { protected function COMMON(): Child {} }
class Sibling extends Base { public function inspect() { /*cursor*/ } }
"#,
    );
    let items = fixture.items(
        "/*cursor*/",
        CompletionContext::MemberAccess {
            object_expr: "$other".into(),
            class_fqn: Some("Child".into()),
            member_prefix: String::new(),
            access_mode: MemberAccessMode::Read,
        },
    );
    let item = items
        .iter()
        .find(|item| item.label.eq_ignore_ascii_case("common"))
        .expect("accessible override");
    assert_eq!(item.data, Some(json!("Child::COMMON")));
    assert!(item.detail.as_deref().unwrap().contains("Child"));
}

#[test]
fn phpdoc_mixin_does_not_grant_private_or_protected_class_scope() {
    let fixture = Fixture::new(
        r#"<?php
class Foreign { private function secret() {} protected function guarded() {} public function visible() {} }
/** @mixin Foreign */
class Subject { public function inspect() { /*cursor*/ } }
"#,
    );
    let labels = fixture.instance("/*cursor*/", "Subject", "$this");
    assert!(labels.contains("visible"), "{labels:?}");
    assert!(
        !labels.contains("guarded") && !labels.contains("secret"),
        "{labels:?}"
    );
}

#[test]
fn repeated_trait_use_in_parent_and_child_preserves_each_private_scope() {
    let fixture = Fixture::new(
        r#"<?php
trait Feature { private function secret() {} private static function staticSecret() {} private const SECRET = 1; }
class Base { use Feature; public function inspect() { /*base*/ } }
class Child extends Base { use Feature; public function inspectChild() { /*child*/ } }
class Other { use Feature; public function inspectOther() { /*other*/ } }
"#,
    );
    for marker in ["/*base*/", "/*child*/"] {
        assert!(fixture
            .instance(marker, "Child", "$other")
            .contains("secret"));
    }
    assert!(!fixture
        .instance("/*other*/", "Child", "$other")
        .contains("secret"));
    let inherited = fixture.static_labels("/*base*/", "Child", "Child");
    assert!(
        !inherited.contains("staticSecret") && !inherited.contains("SECRET"),
        "{inherited:?}"
    );
    let own = fixture.static_labels("/*child*/", "Child", "Child");
    assert!(own.contains("staticSecret") && own.contains("SECRET"));
}

#[test]
fn protected_property_and_constant_redeclarations_do_not_share_method_prototype_access() {
    let fixture = Fixture::new(
        r#"<?php
class Base { protected int $value; protected static int $counter; protected const VALUE = 1; protected static function common() {} }
class Child extends Base { protected int $value; protected static int $counter; protected const VALUE = 2; protected static function common() {} }
class Sibling extends Base { public function inspect() { /*cursor*/ } }
"#,
    );
    let instance = fixture.instance("/*cursor*/", "Child", "$other");
    assert!(
        !instance.contains("value"),
        "hidden child property fell back to parent: {instance:?}"
    );
    let scoped = fixture.static_labels("/*cursor*/", "Child", "Child");
    assert!(scoped.contains("common"));
    assert!(
        !scoped.contains("VALUE") && !scoped.contains("$counter"),
        "{scoped:?}"
    );
}

#[test]
fn private_instance_binding_prefers_cursor_scope_while_static_binding_uses_receiver() {
    let fixture = Fixture::new(
        r#"<?php
class Base {
    private function secret() {} private int $value;
    private static function staticSecret() {} private static int $counter; private const SECRET = 1;
    public function inspect() { /*cursor*/ }
}

class Child extends Base {
    public function secret() {} public int $value;
    public static function staticSecret() {} public static int $counter; public const SECRET = 2;
}
class PlainChild extends Base {}
"#,
    );
    let items = fixture.items(
        "/*cursor*/",
        CompletionContext::MemberAccess {
            object_expr: "$other".into(),
            class_fqn: Some("Child".into()),
            member_prefix: String::new(),
            access_mode: MemberAccessMode::Read,
        },
    );
    for (name, expected) in [("secret", "Base::secret"), ("value", "Base::$value")] {
        let item = items.iter().find(|item| item.label == name).unwrap();
        assert_eq!(item.data, Some(json!(expected)), "{name}");
    }
    let items = fixture.items(
        "/*cursor*/",
        CompletionContext::StaticAccess {
            class_expr: "Child".into(),
            class_fqn: "Child".into(),
            member_prefix: String::new(),
        },
    );
    for (name, expected) in [
        ("staticSecret", "Child::staticSecret"),
        ("$counter", "Child::$counter"),
        ("SECRET", "Child::SECRET"),
    ] {
        let item = items.iter().find(|item| item.label == name).unwrap();
        assert_eq!(item.data, Some(json!(expected)), "{name}");
    }
    let inherited = fixture.static_labels("/*cursor*/", "PlainChild", "PlainChild");
    assert!(inherited.contains("staticSecret") && inherited.contains("$counter"));
    assert!(
        !inherited.contains("SECRET"),
        "private constants are not inherited: {inherited:?}"
    );
}

#[test]
fn consuming_class_or_outer_trait_declarations_suppress_imported_private_trait_method() {
    for (declaration, owner) in [
        (
            "use Feature; public function choice(): string {}",
            "Subject",
        ),
        ("use Wrapper;", "Wrapper"),
    ] {
        let source = format!(
            r#"<?php
trait Feature {{ private function choice(): int {{}} }}
trait Wrapper {{ use Feature; public function choice(): string {{}} }}
class Subject {{ {declaration} public function inspect() {{ /*cursor*/ }} }}
"#
        );
        let fixture = Fixture::new(&source);
        let items = fixture.items(
            "/*cursor*/",
            CompletionContext::MemberAccess {
                object_expr: "$this".into(),
                class_fqn: Some("Subject".into()),
                member_prefix: String::new(),
                access_mode: MemberAccessMode::Read,
            },
        );
        let item = items.iter().find(|item| item.label == "choice").unwrap();
        assert_eq!(item.data, Some(json!(format!("{owner}::choice"))));
        assert!(item.detail.as_deref().unwrap().contains("string"));
    }
    let fixture = Fixture::new(
        r#"<?php
trait Feature { private function choice(): int {} }
class Base { use Feature; public function inspect() { /*cursor*/ } }
class Child extends Base { public function choice(): string {} }
"#,
    );
    let items = fixture.items(
        "/*cursor*/",
        CompletionContext::MemberAccess {
            object_expr: "$other".into(),
            class_fqn: Some("Child".into()),
            member_prefix: String::new(),
            access_mode: MemberAccessMode::Read,
        },
    );
    assert_eq!(
        items
            .iter()
            .find(|item| item.label == "choice")
            .unwrap()
            .data,
        Some(json!("Feature::choice"))
    );
}

#[test]
fn object_calls_include_native_static_methods_with_class_scope_visibility() {
    let fixture = Fixture::new(
        r#"<?php
class Subject {
    public static function visible(): string {}
    protected static function guarded(): string {}
    private static function secret(): string {}
    public static int $counter;
    public const VALUE = 1;
    public function inspect() { /*inside*/ }
}
class Stranger { public function inspect() { /*foreign*/ } }
/*outside*/
"#,
    );
    for expression in ["$this", "$other", "$factory->get()"] {
        let labels = fixture.instance("/*inside*/", "Subject", expression);
        for name in ["visible", "guarded", "secret"] {
            assert!(labels.contains(name), "{expression}: {labels:?}");
        }
    }
    for marker in ["/*foreign*/", "/*outside*/"] {
        let labels = fixture.instance(marker, "Subject", "$other");
        assert!(labels.contains("visible"), "{marker}: {labels:?}");
        assert!(!labels.contains("guarded") && !labels.contains("secret"));
    }
}

#[test]
fn object_access_excludes_class_constants_enum_cases_and_static_properties() {
    let fixture = Fixture::new(
        r#"<?php
class Subject {
    public int $value;
    public static int $counter;
    public const VALUE = 1;
    protected const GUARDED = 2;
    public function inspect() { /*inside*/ }
}
enum Status { case READY; public const VALUE = 1; public function inspect() { /*enum*/ } }
"#,
    );
    let labels = fixture.instance("/*inside*/", "Subject", "$this");
    assert!(labels.contains("value"));
    for forbidden in ["counter", "$counter", "VALUE", "GUARDED", "class"] {
        assert!(!labels.contains(forbidden), "{labels:?}");
    }
    let labels = fixture.instance("/*enum*/", "Status", "$this");
    assert!(labels.contains("name"));
    assert!(
        !labels.contains("READY") && !labels.contains("VALUE"),
        "{labels:?}"
    );
}

#[test]
fn private_static_method_binding_depends_on_object_or_class_call_syntax() {
    let fixture = Fixture::new(
        r#"<?php
class Base { private static function choice(): int {} public function inspect() { /*base*/ } }
class Child extends Base { public static function choice(): string {} }
class PrivateChild extends Base { private static function choice(): string {} }
"#,
    );
    for receiver in ["Child", "PrivateChild"] {
        let items = fixture.items(
            "/*base*/",
            CompletionContext::MemberAccess {
                object_expr: "$other".into(),
                class_fqn: Some(receiver.into()),
                member_prefix: String::new(),
                access_mode: MemberAccessMode::Read,
            },
        );
        let item = items
            .iter()
            .find(|item| item.label == "choice")
            .expect("scope-bound private static method");
        assert_eq!(item.data, Some(json!("Base::choice")));
        assert!(item.detail.as_deref().unwrap().contains("int"));
    }
    let items = fixture.items(
        "/*base*/",
        CompletionContext::StaticAccess {
            class_expr: "Child".into(),
            class_fqn: "Child".into(),
            member_prefix: String::new(),
        },
    );
    assert_eq!(
        items
            .iter()
            .find(|item| item.label == "choice")
            .unwrap()
            .data,
        Some(json!("Child::choice"))
    );
    assert!(!fixture
        .static_labels("/*base*/", "PrivateChild", "PrivateChild")
        .contains("choice"));
}

#[test]
fn object_call_keeps_trait_private_static_binding_separate_from_class_lookup() {
    let fixture = Fixture::new(
        r#"<?php
trait Feature { private static function choice(): int {} }
class Base { use Feature; public function inspect() { /*base*/ } }
class Child extends Base { public static function choice(): string {} }
class Reused extends Base { use Feature; public function inspectReused() { /*reused*/ } }
class Stranger { use Feature; public function inspectOther() { /*other*/ } }
"#,
    );
    for receiver in ["Child", "Reused"] {
        let items = fixture.items(
            "/*base*/",
            CompletionContext::MemberAccess {
                object_expr: "$other".into(),
                class_fqn: Some(receiver.into()),
                member_prefix: String::new(),
                access_mode: MemberAccessMode::Read,
            },
        );
        assert_eq!(
            items
                .iter()
                .find(|item| item.label == "choice")
                .expect("trait private static method")
                .data,
            Some(json!("Feature::choice"))
        );
    }
    assert!(!fixture
        .instance("/*other*/", "Reused", "$other")
        .contains("choice"));
    assert!(fixture
        .instance("/*reused*/", "Reused", "$other")
        .contains("choice"));
    assert!(!fixture
        .static_labels("/*base*/", "Reused", "Reused")
        .contains("choice"));
}

#[test]
fn inaccessible_nonstatic_redeclaration_blocks_static_ancestor_fallback() {
    let fixture = Fixture::new(
        r#"<?php
class Base { private static function choice(): int {} public function inspect() { /*base*/ } }
class Child extends Base { private function choice(): string {} }
"#,
    );
    assert!(!fixture
        .static_labels("/*base*/", "Child", "Child")
        .contains("choice"));
    let items = fixture.items(
        "/*base*/",
        CompletionContext::MemberAccess {
            object_expr: "$other".into(),
            class_fqn: Some("Child".into()),
            member_prefix: String::new(),
            access_mode: MemberAccessMode::Read,
        },
    );
    assert_eq!(
        items
            .iter()
            .find(|item| item.label == "choice")
            .unwrap()
            .data,
        Some(json!("Base::choice"))
    );
}

#[test]
fn static_phpdoc_virtual_methods_stay_class_only_while_native_methods_allow_object_calls() {
    let fixture = Fixture::new(
        r#"<?php
/** @method static string virtualChoice() */
class Subject {
    /** @method static string nativeChoice() */
    public static function nativeChoice(): string {}
    public function inspect() { /*inside*/ }
}
"#,
    );
    let object = fixture.instance("/*inside*/", "Subject", "$other");
    assert!(object.contains("nativeChoice"), "{object:?}");
    assert!(!object.contains("virtualChoice"), "{object:?}");
    let class = fixture.static_labels("/*inside*/", "Subject", "Subject");
    assert!(class.contains("nativeChoice") && class.contains("virtualChoice"));
}
