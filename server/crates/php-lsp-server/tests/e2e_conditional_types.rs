mod support;
use support::*;

async fn send(service: &mut LspService<PhpLspBackend>, request: Request) -> serde_json::Value {
    let response = service.ready().await.unwrap().call(request).await.unwrap();
    response
        .map(|r| {
            assert!(r.error().is_none(), "{r:?}");
            extract_result(Some(r))
        })
        .unwrap_or(serde_json::Value::Null)
}
async fn service() -> LspService<PhpLspBackend> {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move { while socket.next().await.is_some() {} });
    send(
        &mut service,
        initialize_request_with_options(
            1,
            None,
            Some(json!({"stubExtensions":[],"indexVendor":false,"diagnosticsMode":"off"})),
        ),
    )
    .await;
    service
}
fn source(condition: &str, argument: &str) -> String {
    format!("<?php\r\nclass Hit {{function hit(){{}}}} class Miss {{function miss(){{}}}}\r\nclass Choice {{/** @return {condition} */ function choose($value,$flag=true){{}}}}\r\n/* 😀 */ $result=(new Choice())->choose({argument}); echo /*USE*/$result; $result->/*CALL*/hit();")
}
async fn hover(service: &mut LspService<PhpLspBackend>, uri: &str, source: &str) -> String {
    let (line, col) = utf16_position_after(source, "/*USE*/");
    let result = send(service, hover_request(20, uri, line, col + 1)).await;
    hover_markdown_value(&result)
}
async fn expect_type(
    service: &mut LspService<PhpLspBackend>,
    uri: &str,
    source: &str,
    expected: &str,
    absent: &str,
) {
    let text = hover(service, uri, source).await;
    assert!(
        text.contains(expected) && !text.contains(absent),
        "expected {expected}, absent {absent}: {text}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn literal_method_arguments_choose_the_correct_hover_completion_definition_and_inlay() {
    let mut service = service().await;
    let uri = "file:///test/conditional-literals.php";
    for (target, argument) in [
        ("string", "'value'"),
        ("int", "42"),
        ("bool", "true"),
        ("float", "1.25"),
    ] {
        let source = source(&format!("($value is {target} ? Hit : Miss)"), argument);
        send(&mut service, did_open_notification(uri, &source)).await;
        expect_type(&mut service, uri, &source, "Hit $result", "Miss $result").await;
        let (line, col) = utf16_position_after(&source, "$result->");
        let completion = send(&mut service, completion_request(21, uri, line, col)).await;
        let items = completion
            .as_array()
            .or_else(|| completion["items"].as_array())
            .unwrap();
        assert!(
            items.iter().any(|i| i["label"] == "hit"),
            "wrong conditional completion: {completion}"
        );
        assert!(
            !items.iter().any(|i| i["label"] == "miss"),
            "else members leaked: {completion}"
        );
        let (line, col) = utf16_position_after(&source, "/*CALL*/");
        let definition = send(&mut service, definition_request(22, uri, line, col + 1)).await;
        let location = definition
            .as_array()
            .and_then(|v| v.first())
            .unwrap_or(&definition);
        let (line, col) = utf16_position_after(&source, "class Hit {function ");
        assert_eq!(location["uri"], uri, "{definition}");
        assert_eq!(
            location["range"]["start"],
            json!({"line":line,"character":col}),
            "{definition}"
        );
        let (line, col) = utf16_position_for_offset(&source, source.len());
        let hints = send(&mut service, inlay_hint_request(23, uri, 0, 0, line, col)).await;
        assert!(
            hints
                .as_array()
                .unwrap()
                .iter()
                .any(|h| inlay_hint_label_text(h).as_deref() == Some(": Hit")),
            "missing specialized inlay: {hints}"
        );
        send(&mut service, did_close_notification(uri)).await;
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn disjoint_argument_selects_else_without_template_guessing() {
    let mut service = service().await;
    let uri = "file:///test/conditional-else.php";
    let source = source("($value is string ? Hit : Miss)", "42");
    send(&mut service, did_open_notification(uri, &source)).await;
    expect_type(&mut service, uri, &source, "Miss $result", "Hit $result").await;
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_argument_preserves_both_possible_return_branches() {
    let mut service = service().await;
    let uri = "file:///test/conditional-unknown.php";
    let source = source("($value is string ? Hit : Miss)", "$unknown");
    send(&mut service, did_open_notification(uri, &source)).await;
    let text = hover(&mut service, uri, &source).await;
    assert!(
        text.contains("Hit") && text.contains("Miss"),
        "unknown subject guessed one branch: {text}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn negated_and_nested_conditionals_specialize_their_semantic_branches() {
    let mut service = service().await;
    let uri = "file:///test/conditional-nested.php";
    for (condition, argument, expected, absent) in [
        (
            "($value is not null ? Hit : Miss)",
            "'value'",
            "Hit $result",
            "Miss $result",
        ),
        (
            "($value is not null ? Hit : Miss)",
            "null",
            "Miss $result",
            "Hit $result",
        ),
        (
            "($value is string ? ($flag is bool ? Hit : Miss) : Miss)",
            "'value', true",
            "Hit $result",
            "Miss $result",
        ),
    ] {
        let source = source(condition, argument);
        send(&mut service, did_open_notification(uri, &source)).await;
        expect_type(&mut service, uri, &source, expected, absent).await;
        send(&mut service, did_close_notification(uri)).await;
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn declared_template_subject_is_bound_from_its_parameter() {
    let mut service = service().await;
    let uri = "file:///test/conditional-template-subject.php";
    let source="<?php class Hit {} class Miss {} /**\n * @template T\n * @param T $value\n * @return (T is int ? Hit : Miss)\n */ function choose($value){} $result=choose(42); echo /*USE*/$result;";
    send(&mut service, did_open_notification(uri, source)).await;
    expect_type(&mut service, uri, source, "Hit $result", "Miss $result").await;
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn conditional_types_follow_versioned_unsaved_arguments_and_close_reopen() {
    let mut service = service().await;
    let uri = "file:///test/conditional-lifecycle.php";
    let original = source("($value is string ? Hit : Miss)", "'value'");
    send(&mut service, did_open_notification(uri, &original)).await;
    expect_type(&mut service, uri, &original, "Hit $result", "Miss $result").await;
    let changed = original.replace("choose('value')", "choose(42)");
    send(&mut service, did_change_full_notification(uri, 2, &changed)).await;
    expect_type(&mut service, uri, &changed, "Miss $result", "Hit $result").await;
    send(&mut service, did_close_notification(uri)).await;
    send(&mut service, did_open_notification(uri, &original)).await;
    expect_type(&mut service, uri, &original, "Hit $result", "Miss $result").await;
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn conditional_phpdoc_cannot_override_a_conflicting_native_return() {
    let mut service = service().await;
    let uri = "file:///test/conditional-native.php";
    let source="<?php class Wrong {} /** @return ($value is string ? Wrong : Wrong) */ function choose($value):int {return 1;} $result=choose('value'); echo /*USE*/$result;";
    send(&mut service, did_open_notification(uri, source)).await;
    expect_type(&mut service, uri, source, "int $result", "Wrong $result").await;
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn named_defaults_and_unknown_supplied_values_have_distinct_conditional_results() {
    let mut service = service().await;
    let uri = "file:///test/conditional-defaults.php";
    let base="<?php class Hit {} class Miss {} class Choice {/** @return ($value is 'ready' ? Hit : Miss) */ function choose($prefix='', $value='ready'){}} $result=(new Choice())->CALL; echo /*USE*/$result;";
    for (call, known) in [
        ("choose()", Some("Hit")),
        ("choose(value:'ready',prefix:'noise')", Some("Hit")),
        ("choose('noise','ready')", Some("Hit")),
        ("choose(value:'other')", Some("Miss")),
        ("choose(value:$unknown)", None),
        ("choose(...$unknown)", None),
    ] {
        let source = base.replace("CALL", call);
        send(&mut service, did_open_notification(uri, &source)).await;
        let text = hover(&mut service, uri, &source).await;
        if let Some(known) = known {
            assert!(text.contains(&format!("{known} $result")), "{call}: {text}");
        } else {
            assert!(
                text.contains("Hit") && text.contains("Miss"),
                "{call}: {text}"
            );
        }
        send(&mut service, did_close_notification(uri)).await;
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_unbound_declared_template_does_not_prove_the_true_branch() {
    let mut service = service().await;
    let uri = "file:///test/conditional-unbound-template.php";
    let source="<?php class Hit {} class Miss {} /**\n * @template T\n * @return ($value is T ? Hit : Miss)\n */ function choose($value){} $result=choose('value'); echo /*USE*/$result;";
    send(&mut service, did_open_notification(uri, source)).await;
    let text = hover(&mut service, uri, source).await;
    assert!(
        text.contains("Hit") && text.contains("Miss"),
        "unbound target inferred itself: {text}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn caller_templates_are_unknown_types_rather_than_nominal_class_names() {
    let mut service = service().await;
    let uri = "file:///test/conditional-caller-template.php";
    let source="<?php class Hit {} class Miss {} class Choice {/** @return ($value is int ? Hit : Miss) */ function choose($value){}} /**\n * @template U\n * @param U $input\n */ function caller($input) {$result=(new Choice())->choose($input); echo /*USE*/$result;}";
    send(&mut service, did_open_notification(uri, source)).await;
    let text = hover(&mut service, uri, source).await;
    assert!(
        text.contains("Hit") && text.contains("Miss"),
        "caller U treated as a disjoint class: {text}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn subtype_conditions_use_the_declaring_namespace_and_indexed_inheritance() {
    let mut service = service().await;
    send(
        &mut service,
        did_open_notification(
            "file:///test/conditional-base.php",
            "<?php namespace Domain; class Base {} class Child extends Base {}",
        ),
    )
    .await;
    send(&mut service,did_open_notification("file:///test/conditional-factory.php","<?php namespace Predicates; use Domain\\Base as Bound; class Hit {} class Miss {} class Choice {/** @return ($value is Bound ? Hit : Miss) */ function choose($value){}}")).await;
    let uri = "file:///test/conditional-subtype-caller.php";
    let source="<?php namespace Caller; class Bound {} $result=(new \\Predicates\\Choice())->choose(new \\Domain\\Child()); echo /*USE*/$result;";
    send(&mut service, did_open_notification(uri, source)).await;
    expect_type(&mut service, uri, source, "Hit $result", "Miss $result").await;
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn receiver_templates_are_independent_bindings_for_conditional_subjects() {
    let mut service = service().await;
    let uri = "file:///test/conditional-receiver-template.php";
    let source="<?php class Hit {} class Miss {} /** @template T */ class Holder {/** @return (T is int ? Hit : Miss) */ function choose(){}} /** @var Holder<int> $holder */ $holder=new Holder; $result=$holder->choose(); echo /*USE*/$result;";
    send(&mut service, did_open_notification(uri, source)).await;
    expect_type(&mut service, uri, source, "Hit $result", "Miss $result").await;
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_caller_template_does_not_become_an_equal_named_declared_class() {
    let mut service = service().await;
    let uri = "file:///test/conditional-template-class-collision.php";
    let source="<?php class U {} class Hit {} class Miss {} class Choice {/** @return ($value is U ? Hit : Miss) */ function choose($value){}} /**\n * @template U\n * @param U $input\n */ function caller($input) {$result=(new Choice())->choose($input); echo /*USE*/$result;}";
    send(&mut service, did_open_notification(uri, source)).await;
    let text = hover(&mut service, uri, source).await;
    assert!(
        text.contains("Hit") && text.contains("Miss"),
        "caller U was confused with class U: {text}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_unbound_result_template_does_not_navigate_to_an_unrelated_class() {
    let mut service = service().await;
    let uri = "file:///test/conditional-unbound-result.php";
    let source="<?php class T {function unrelated(){}} /**\n * @template T\n * @return ($value is string ? T : mixed)\n */ function choose($value){} $result=choose('value'); $result->/*CALL*/unrelated(); echo /*USE*/$result;";
    send(&mut service, did_open_notification(uri, source)).await;
    let (line, col) = utf16_position_after(source, "/*CALL*/");
    let definition = send(&mut service, definition_request(20, uri, line, col + 1)).await;
    assert!(
        definition.is_null() || definition.as_array().is_some_and(|v| v.is_empty()),
        "unbound template became class T: {definition}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_conditional_template_bound_keeps_the_native_return_guarantee() {
    let mut service = service().await;
    let uri = "file:///test/conditional-native-bound.php";
    let source="<?php class Foo {} class T {} /**\n * @template T of Foo\n * @return ($value is string ? T : Foo)\n */ function choose($value):Foo {return new Foo;} $result=choose('value'); echo /*USE*/$result;";
    send(&mut service, did_open_notification(uri, source)).await;
    expect_type(&mut service, uri, source, "Foo $result", "T $result").await;
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn namespaced_templates_bind_without_becoming_same_named_classes() {
    let mut service = service().await;
    let uri = "file:///test/conditional-namespaced-template.php";
    let source="<?php namespace Ns; class Foo {} class T {function unrelated(){}} /**\n * @template T\n * @param class-string<T> $value\n * @return ($value is class-string<T> ? T : mixed)\n */ function choose($value){} $result=choose(Foo::class); echo /*USE*/$result;";
    send(&mut service, did_open_notification(uri, source)).await;
    expect_type(&mut service, uri, source, "Foo $result", "T $result").await;
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn false_conditional_branch_definitions_do_not_pick_a_same_named_true_branch_method() {
    let mut service = service().await;
    let uri = "file:///test/conditional-definition-identity.php";
    let source="<?php class Hit {function common(){}} class Miss {function /*DECL*/common(){}} class Choice {/** @return ($value is string ? Hit : Miss) */ function choose($value){}} $result=(new Choice())->choose(42); $result->/*CALL*/common();";
    send(&mut service, did_open_notification(uri, source)).await;
    let (line, col) = utf16_position_after(source, "/*CALL*/");
    let result = send(&mut service, definition_request(20, uri, line, col + 1)).await;
    let location = result.as_array().and_then(|v| v.first()).unwrap_or(&result);
    let (line, col) = utf16_position_after(source, "/*DECL*/");
    assert_eq!(
        location["range"]["start"],
        json!({"line":line,"character":col}),
        "definition selected the true branch: {result}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn nested_conditional_method_returns_specialize_foreach_members() {
    let mut service = service().await;
    let uri = "file:///test/conditional-foreach.php";
    let source="<?php class Hit {function hit(){}} class Miss {function /*DECL*/miss(){}} class Choice {/** @return array<int, ($value is string ? Hit : Miss)> */ function choose($value){}} foreach((new Choice())->choose(42) as $result) {echo /*USE*/$result; $result->/*CALL*/miss();}";
    send(&mut service, did_open_notification(uri, source)).await;
    expect_type(&mut service, uri, source, "Miss $result", "Hit $result").await;
    let (line, col) = utf16_position_after(source, "$result->");
    let completion = send(&mut service, completion_request(21, uri, line, col)).await;
    let items = completion
        .as_array()
        .or_else(|| completion["items"].as_array())
        .unwrap();
    assert!(
        items.iter().any(|item| item["label"] == "miss"),
        "{completion}"
    );
    assert!(
        !items.iter().any(|item| item["label"] == "hit"),
        "{completion}"
    );
    let (line, col) = utf16_position_after(source, "/*CALL*/");
    let result = send(&mut service, definition_request(22, uri, line, col + 1)).await;
    let location = result
        .as_array()
        .and_then(|items| items.first())
        .unwrap_or(&result);
    let (line, col) = utf16_position_after(source, "/*DECL*/");
    assert_eq!(
        location["range"]["start"],
        json!({"line":line,"character":col}),
        "{result}"
    );
    send(&mut service, shutdown_request(99)).await;
}
