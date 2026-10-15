mod support;
use support::*;

async fn send(service: &mut LspService<PhpLspBackend>, request: Request) -> serde_json::Value {
    match service.ready().await.unwrap().call(request).await.unwrap() {
        Some(response) => {
            assert!(response.error().is_none(), "{response:?}");
            extract_result(Some(response))
        }
        None => serde_json::Value::Null,
    }
}
async fn service() -> LspService<PhpLspBackend> {
    service_version("8.2").await
}
async fn service_version(version: &str) -> LspService<PhpLspBackend> {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move { while socket.next().await.is_some() {} });
    send(&mut service,initialize_request_with_options(1,None,Some(json!({"stubExtensions":[],"indexVendor":false,"diagnosticsMode":"off","phpVersion":version})))).await;
    service
}

#[tokio::test(flavor = "current_thread")]
async fn p240_implement_keeps_native_object_and_checks_target_php_version() {
    let mut service = service().await;
    let uri = "file:///p240/Object.php";
    let source="<?php namespace Lib {interface Contract {function build(object $value): object;}} namespace App {class Child implements \\Lib\\Contract {}}";
    send(&mut service, did_open_notification(uri, source)).await;
    let action = implement(&mut service, uri, source).await;
    let result = send(&mut service, code_action_resolve_request(11, action)).await;
    assert!(
        inserted(&result, uri).contains("build(object $value): object"),
        "{result}"
    );
    send(&mut service, shutdown_request(99)).await;
    for (version, ty) in [
        ("7.4", "mixed"),
        ("8.0", "never"),
        ("8.1", "true"),
        ("8.1", "false"),
        ("8.1", "null"),
    ] {
        let mut service = service_version(version).await;
        let source=format!("<?php interface Contract {{function run(): {ty};}} class Child implements Contract {{}}");
        send(&mut service, did_open_notification(uri, &source)).await;
        let (line, col) = utf16_position_after(&source, "class Chi");
        let actions = send(
            &mut service,
            code_action_request(2, uri, line, col, line, col, json!([])),
        )
        .await;
        assert!(
            actions.as_array().unwrap().iter().all(|a| a["title"]
                .as_str()
                .is_none_or(|t| !t.starts_with("Implement "))),
            "unsupported {ty} for {version}: {actions}"
        );
        send(&mut service, shutdown_request(99)).await;
    }
}
async fn implement(
    service: &mut LspService<PhpLspBackend>,
    uri: &str,
    source: &str,
) -> serde_json::Value {
    let (line, col) = utf16_position_after(source, "class Chi");
    let actions = send(
        service,
        code_action_request(10, uri, line, col, line, col, json!([])),
    )
    .await;
    actions
        .as_array()
        .unwrap()
        .iter()
        .find(|a| {
            a["title"]
                .as_str()
                .is_some_and(|t| t.starts_with("Implement "))
        })
        .cloned()
        .unwrap_or_else(|| panic!("missing action: {actions}"))
}
fn inserted(action: &serde_json::Value, uri: &str) -> String {
    action["edit"]["changes"][uri]
        .as_array()
        .unwrap_or_else(|| panic!("missing edit: {action}"))
        .iter()
        .map(|edit| edit["newText"].as_str().unwrap())
        .collect()
}
#[tokio::test(flavor = "current_thread")]
async fn p240_implement_preserves_foreign_types_constants_attributes_and_reference_return() {
    let mut service = service().await;
    let parent="<?php namespace Vendor {class Thing {public const FLAGS=1;} #[\\Attribute] class Meta {function __construct($value){}}} namespace Library {use Vendor\\Thing as Input;use Vendor\\Meta as Marker; abstract class Base {\n/** @param Input $value\n * @return Input\n */\n#[Marker('public function')]\nabstract public function &build(#[Marker('parameter')] Input $value, int $flags=Input::FLAGS): Input;}}";
    let uri = "file:///p240/Child.php";
    let child="<?php namespace App; use Decoy\\Thing as Input; use Decoy\\Meta as Marker; class Child extends \\Library\\Base {}";
    send(
        &mut service,
        did_open_notification("file:///p240/Base.php", parent),
    )
    .await;
    send(&mut service, did_open_notification(uri, child)).await;
    let action = implement(&mut service, uri, child).await;
    let resolved = send(&mut service, code_action_resolve_request(11, action)).await;
    let text = inserted(&resolved, uri);
    for required in [
        "function &build(",
        "\\Vendor\\Thing $value",
        "\\Vendor\\Thing::FLAGS",
        ": \\Vendor\\Thing",
        "#[\\Vendor\\Meta('public function')]",
        "#[\\Vendor\\Meta('parameter')]",
        "@param \\Vendor\\Thing $value",
        "@return \\Vendor\\Thing",
    ] {
        assert!(text.contains(required), "lost {required}: {text}");
    }
    let edited = child.replacen("{}", &format!("{{{text}}}"), 1);
    let mut parser = php_lsp_parser::parser::FileParser::new();
    parser.parse_full(&edited);
    assert!(
        !parser.tree().unwrap().root_node().has_error(),
        "invalid generated PHP: {edited}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn p240_implement_rejects_actions_from_a_previous_document_lifetime() {
    let mut service = service().await;
    let uri = "file:///p240/Reopen.php";
    let source =
        "<?php interface Contract {function run(): void;} class Child implements Contract {}";
    send(&mut service, did_open_notification(uri, source)).await;
    let action = implement(&mut service, uri, source).await;
    send(&mut service, did_close_notification(uri)).await;
    send(&mut service, did_open_notification(uri, source)).await;
    let result = send(&mut service, code_action_resolve_request(12, action)).await;
    assert!(
        result["edit"]["changes"].as_object().unwrap().is_empty(),
        "old lifetime edit: {result}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn p240_implement_nested_defaults_and_attribute_arguments_keep_constant_identity() {
    let mut service = service().await;
    let uri = "file:///p240/NestedDefaults.php";
    let source="<?php namespace Lib {const LIMIT=3; #[\\Attribute] class Meta {function __construct($x){}} interface Contract {#[Meta([LIMIT])] public function build(array $args=[LIMIT]): void;}} namespace App {const LIMIT=99; class Child implements \\Lib\\Contract {}}";
    send(&mut service, did_open_notification(uri, source)).await;
    let action = implement(&mut service, uri, source).await;
    let result = send(&mut service, code_action_resolve_request(11, action)).await;
    let text = inserted(&result, uri);
    assert_eq!(
        text.matches("\\Lib\\LIMIT").count(),
        2,
        "wrong constant scope: {text}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn p240_implement_does_not_choose_the_first_incompatible_interface_contract() {
    let mut service = service().await;
    let uri = "file:///p240/Conflicting.php";
    let source="<?php interface A {function run(int $x): int;} interface B {function run(string $x): string;} class Child implements A,B {}";
    send(&mut service, did_open_notification(uri, source)).await;
    let (line, col) = utf16_position_after(source, "class Chi");
    let actions = send(
        &mut service,
        code_action_request(2, uri, line, col, line, col, json!([])),
    )
    .await;
    for action in actions.as_array().unwrap().iter().filter(|a| {
        a["title"]
            .as_str()
            .is_some_and(|t| t.starts_with("Implement "))
    }) {
        let result = send(&mut service, code_action_resolve_request(3, action.clone())).await;
        assert!(
            result["edit"]["changes"].as_object().unwrap().is_empty(),
            "violates second contract: {result}"
        );
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn p240_relocated_phpdoc_preserves_composite_grouping_and_template_bounds() {
    let mut service = service().await;
    let uri = "file:///p240/Doc.php";
    let source="<?php namespace Lib {class A{} class B{} class C{} interface Contract {\n/** @template T of A\n * @param array{n:(A|B)&C} $row\n * @return T\n */\npublic function run(array $row);}} namespace App {class Child implements \\Lib\\Contract {}}";
    send(&mut service, did_open_notification(uri, source)).await;
    let action = implement(&mut service, uri, source).await;
    let result = send(&mut service, code_action_resolve_request(11, action)).await;
    let text = inserted(&result, uri);
    let a = text.find("/**").unwrap();
    let b = text[a..].find("*/").unwrap() + a + 2;
    let actual = php_lsp_parser::phpdoc::parse_phpdoc(&text[a..b]);
    let expected=php_lsp_parser::phpdoc::parse_phpdoc("/** @template T of \\Lib\\A\n * @param array{n:(\\Lib\\A|\\Lib\\B)&\\Lib\\C} $row\n * @return T\n */");
    assert_eq!(actual.params, expected.params, "grouping: {text}");
    assert_eq!(actual.templates, expected.templates, "bounds: {text}");
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn p240_conflicting_phpdoc_contracts_are_not_collapsed_to_the_first() {
    let mut service = service().await;
    let uri = "file:///p240/DocConflict.php";
    let source="<?php class X{} class Y{} interface A {/** @param X $value */ public function run($value):void;} interface B {/** @param Y $value */ public function run($value):void;} class Child implements A,B {}";
    send(&mut service, did_open_notification(uri, source)).await;
    let (line, col) = utf16_position_after(source, "class Chi");
    let actions = send(
        &mut service,
        code_action_request(2, uri, line, col, line, col, json!([])),
    )
    .await;
    assert!(
        actions.as_array().unwrap().iter().all(|a| a["title"]
            .as_str()
            .is_none_or(|t| !t.starts_with("Implement "))),
        "ambiguous PHPDoc: {actions}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn p240_phpdoc_relative_leaves_and_unbounded_method_templates_keep_their_roles() {
    for (source,expected) in [
        ("<?php namespace Lib {class Top {} abstract class Base extends Top {/** @return array{self:self,parent:parent} */ abstract public function run():array;}} namespace App {class Child extends \\Lib\\Base {}}", "array{self: \\Lib\\Base, parent: \\Lib\\Top}"),
        ("<?php namespace Lib {interface Contract {/** @template T\n * @param T $value\n * @return T\n */ public function run($value);}} namespace App {class Child implements \\Lib\\Contract {}}", "@template T"),
    ] {
        let mut service=service().await;
        let uri="file:///p240/RelativeDoc.php";
        send(&mut service,did_open_notification(uri,source)).await;
        let action=implement(&mut service,uri,source).await;
        let result=send(&mut service,code_action_resolve_request(11,action)).await;
        assert!(inserted(&result,uri).contains(expected),"{result}");
        send(&mut service,shutdown_request(99)).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn p240_prefixed_template_bounds_and_contracts_keep_source_identity() {
    for tag in ["@phpstan-template", "@psalm-template-covariant"] {
        let mut service = service().await;
        let uri = "file:///p240/PrefixedDoc.php";
        let source=format!("<?php namespace Lib {{use Vendor\\Original as Bound; interface Contract {{/** {tag} T of Bound\n * @return T\n */ public function run();}}}} namespace App {{use Decoy\\Wrong as Bound; class Child implements \\Lib\\Contract {{}}}}");
        send(&mut service, did_open_notification(uri, &source)).await;
        let action = implement(&mut service, uri, &source).await;
        let result = send(&mut service, code_action_resolve_request(11, action)).await;
        assert!(
            inserted(&result, uri).contains(&format!("{tag} T of \\Vendor\\Original")),
            "{result}"
        );
        send(&mut service, shutdown_request(99)).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn p240_prefixed_return_contract_conflicts_are_not_discarded() {
    let mut service = service().await;
    let uri = "file:///p240/PrefixedConflict.php";
    let source="<?php class X{} class Y{} interface A {/** @phpstan-return X */ public function run();} interface B {/** @phpstan-return Y */ public function run();} class Child implements A,B {}";
    send(&mut service, did_open_notification(uri, source)).await;
    let (line, col) = utf16_position_after(source, "class Chi");
    let actions = send(
        &mut service,
        code_action_request(2, uri, line, col, line, col, json!([])),
    )
    .await;
    assert!(
        actions.as_array().unwrap().iter().all(|a| a["title"]
            .as_str()
            .is_none_or(|t| !t.starts_with("Implement "))),
        "conflicting prefixed contracts: {actions}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn p240_unsupported_trait_adaptations_do_not_produce_an_incomplete_implementation() {
    let mut service = service().await;
    let uri = "file:///p240/TraitAlias.php";
    let source="<?php trait Required {abstract public function run(): void;} abstract class Base {use Required {run as aliasRun;}} class Child extends Base {}";
    send(&mut service, did_open_notification(uri, source)).await;
    let (line, col) = utf16_position_after(source, "class Chi");
    let actions = send(
        &mut service,
        code_action_request(2, uri, line, col, line, col, json!([])),
    )
    .await;
    assert!(
        actions.as_array().unwrap().iter().all(|a| a["title"]
            .as_str()
            .is_none_or(|t| !t.starts_with("Implement "))),
        "unrepresented alias contract: {actions}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn p240_clients_supporting_document_changes_receive_versioned_edits() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move { while socket.next().await.is_some() {} });
    send(&mut service,Request::build("initialize").id(1).params(json!({"capabilities":{"workspace":{"workspaceEdit":{"documentChanges":true}}},"initializationOptions":{"stubExtensions":[],"indexVendor":false,"diagnosticsMode":"off"}})).finish()).await;
    let uri = "file:///p240/Versioned.php";
    let source =
        "<?php interface Contract {function run():void;} class Child implements Contract {}";
    send(&mut service, did_open_notification(uri, source)).await;
    let action = implement(&mut service, uri, source).await;
    let result = send(
        &mut service,
        code_action_resolve_request(11, action.clone()),
    )
    .await;
    assert_eq!(
        result["edit"]["documentChanges"][0]["textDocument"],
        json!({"uri":uri,"version":1}),
        "{result}"
    );
    send(&mut service, did_change_full_notification(uri, 2, source)).await;
    let stale = send(&mut service, code_action_resolve_request(12, action)).await;
    assert!(stale["edit"]["changes"].as_object().unwrap().is_empty());
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn p240_imports_after_a_declaration_do_not_rebind_its_generated_contract() {
    let mut service = service().await;
    let uri = "file:///p240/LateImports.php";
    let source="<?php namespace Lib {interface Contract {public function run(T $value): T;} use Vendor\\T;} namespace App {class Child implements \\Lib\\Contract {} use Lib\\T;}";
    send(&mut service, did_open_notification(uri, source)).await;
    let action = implement(&mut service, uri, source).await;
    let result = send(&mut service, code_action_resolve_request(11, action)).await;
    assert!(
        inserted(&result, uri).contains("run(\\Lib\\T $value): \\Lib\\T"),
        "late imports changed identity: {result}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn p240_cached_hierarchy_cannot_override_the_cst_declaration_scope() {
    let mut service = service().await;
    let uri = "file:///p240/LateParent.php";
    let source="<?php namespace Lib {interface Wrong {public function wrong();}} namespace App {interface Right {public function right();} class Child implements Right {} use Lib\\Wrong as Right;}";
    send(&mut service, did_open_notification(uri, source)).await;
    let (line, col) = utf16_position_after(source, "class Chi");
    let actions = send(
        &mut service,
        code_action_request(2, uri, line, col, line, col, json!([])),
    )
    .await;
    for action in actions.as_array().unwrap().iter().filter(|a| {
        a["title"]
            .as_str()
            .is_some_and(|t| t.starts_with("Implement "))
    }) {
        let result = send(&mut service, code_action_resolve_request(3, action.clone())).await;
        assert!(
            !serde_json::to_string(&result["edit"])
                .unwrap()
                .contains("function wrong"),
            "unrelated parent: {result}"
        );
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn p240_trait_consumer_dnf_and_multiline_defaults_keep_their_contract() {
    let mut service = service().await;
    let uri = "file:///p240/Trait.php";
    let source = "<?php namespace Lib {class Top{} interface A{} interface B{} class C{} #[\\Attribute] class Mark{} trait Needs {#[Mark, Mark]\nabstract public static function run(self $self, parent $parent, (A&B)|C $value, string $text = 'first\n  😀 last'): static;} abstract class Base extends Top {use Needs;}} namespace App {class Child extends \\Lib\\Base {}}";
    send(&mut service, did_open_notification(uri, source)).await;
    let action = implement(&mut service, uri, source).await;
    let result = send(&mut service, code_action_resolve_request(11, action)).await;
    let text = inserted(&result, uri);
    for required in [
        "#[\\Lib\\Mark, \\Lib\\Mark]",
        "public static function run(\\Lib\\Base $self, \\Lib\\Top $parent",
        "(\\Lib\\A&\\Lib\\B)|\\Lib\\C $value",
        "'first\n  😀 last'",
        "): static",
    ] {
        assert!(text.contains(required), "lost {required}: {text}");
    }
    let edited = source.replacen(
        "class Child extends \\Lib\\Base {}",
        &format!("class Child extends \\Lib\\Base {{{text}}}"),
        1,
    );
    let mut parser = php_lsp_parser::parser::FileParser::new();
    parser.parse_full(&edited);
    assert!(!parser.tree().unwrap().root_node().has_error(), "{edited}");
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn p240_nonportable_defaults_aliases_and_unsupported_syntax_are_declined() {
    let mut unsafe_actions = Vec::new();
    for (version, parent) in [
        ("8.2", "class Value{} abstract class Base {abstract public function run($x = new Value());}"),
        ("8.2", "abstract class Base {private const HIDDEN=1; abstract public function run($x = self::HIDDEN);}"),
        ("8.2", "abstract class Base {abstract public function run($x = __DIR__);}"),
        ("8.2", "/** @phpstan-type Row array{value:int} */ abstract class Base {/** @return Row */ abstract public function run();}"),
        ("7.4", "abstract class Base {abstract public function run(int $x,);}"),
        ("8.0", "enum Status {case Ready;} abstract class Base {abstract public function run($x = Status::Ready);}"),
    ] {
        let mut service = service_version(version).await;
        let uri = "file:///p240/UnsafeContract.php";
        let source = format!("<?php namespace Lib {{{parent}}} namespace App {{class Child extends \\Lib\\Base {{}}}}");
        send(&mut service, did_open_notification(uri, &source)).await;
        let (line, col) = utf16_position_after(&source, "class Chi");
        let actions = send(&mut service, code_action_request(2, uri, line, col, line, col, json!([]))).await;
        if actions.as_array().unwrap().iter().any(|a| a["title"].as_str().is_some_and(|t| t.starts_with("Implement "))) {
            unsafe_actions.push(format!("unsafe {version} {parent}: {actions}"));
        }
        send(&mut service, shutdown_request(99)).await;
    }
    assert!(unsafe_actions.is_empty(), "{}", unsafe_actions.join("\n"));
}

#[tokio::test(flavor = "current_thread")]
async fn p240_multiline_phpdoc_types_are_qualified_as_whole_tag_blocks() {
    let mut service = service().await;
    let uri = "file:///p240/MultilineDoc.php";
    let source = "<?php namespace Lib {class A{} class B{} interface Contract {/**\n * @template T of A\n * | B\n * @return A\n * | B\n */ public function run();}} namespace App {class B{} class Child implements \\Lib\\Contract {}}";
    send(&mut service, did_open_notification(uri, source)).await;
    let action = implement(&mut service, uri, source).await;
    let result = send(&mut service, code_action_resolve_request(11, action)).await;
    let text = inserted(&result, uri);
    let doc = &text[text.find("/**").unwrap()..text.find("*/").unwrap() + 2];
    let actual = php_lsp_parser::phpdoc::parse_phpdoc(doc);
    let expected = php_lsp_parser::phpdoc::parse_phpdoc(
        "/** @template T of \\Lib\\A|\\Lib\\B\n * @return \\Lib\\A|\\Lib\\B\n */",
    );
    assert_eq!(actual.return_type, expected.return_type, "{text}");
    assert_eq!(actual.templates, expected.templates, "{text}");
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn p240_phpdoc_indentation_and_prose_do_not_change_contract_qualification() {
    for (summary, indent, tag) in [
        ("", "  ", "@return"),
        ("", "\t", "@phpstan-return"),
        (
            "Returns the current item.\n * Keeps its original type.\n * ",
            "",
            "@return",
        ),
        ("", "", "@psalm-var"),
    ] {
        let mut service = service().await;
        let uri = "file:///p240/IndentedDoc.php";
        let source = format!("<?php namespace Lib {{use Vendor\\Value as Alias; interface Contract {{/**\n * {summary}{indent}{tag} Alias\n */ public function run();}}}} namespace App {{class Child implements \\Lib\\Contract {{}}}}");
        send(&mut service, did_open_notification(uri, &source)).await;
        let action = implement(&mut service, uri, &source).await;
        let result = send(&mut service, code_action_resolve_request(11, action)).await;
        let text = inserted(&result, uri);
        assert!(text.contains(&format!("{tag} \\Vendor\\Value")), "{text}");
        if !summary.is_empty() {
            assert!(text.contains("Returns the current item."));
        }
        send(&mut service, shutdown_request(99)).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn p240_unhandled_type_bearing_phpdoc_tags_are_not_copied_raw() {
    let mut service = service().await;
    let uri = "file:///p240/SelfOut.php";
    let source = "<?php namespace Lib {class Alias{} interface Contract {/** @phpstan-self-out Alias */ public function run();}} namespace App {class Child implements \\Lib\\Contract {}}";
    send(&mut service, did_open_notification(uri, source)).await;
    let (line, col) = utf16_position_after(source, "class Chi");
    let actions = send(
        &mut service,
        code_action_request(2, uri, line, col, line, col, json!([])),
    )
    .await;
    assert!(
        actions.as_array().unwrap().iter().all(|a| a["title"]
            .as_str()
            .is_none_or(|t| !t.starts_with("Implement "))),
        "{actions}"
    );
    send(&mut service, shutdown_request(99)).await;
}
