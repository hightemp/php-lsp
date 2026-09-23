mod support;
use php_lsp_types::uri::path_to_uri;
use std::collections::BTreeSet;
use support::*;

const TYPES: &str = r#"<?php
namespace Domain;
interface LeftResult { public function sharedLeaf(): void; public function leftLeaf(): void; }
interface RightResult { public function sharedLeaf(): void; public function rightLeaf(): void; }
interface Left { public function leftOnly(): void; public function common(): LeftResult; }
interface Right { public function rightOnly(): void; public function common(): RightResult; }
interface Third { public function common(): LeftResult; }
/** @template T */
interface Box { /** @return T */ public function get(); }
class AccessBase { protected function guarded(): void {} }
class AccessChild extends AccessBase {}
interface NestedB extends RightResult { public function leftLeaf(): void; }
interface ThirdResult { public function sharedLeaf(): void; }
interface NestedLeft { public function next(): LeftResult|NestedB; }
interface NestedRight { public function next(): LeftResult|ThirdResult; }
/** @method LeftResult virtualCall() */
class VirtualLeft { public function __call(string $name, array $args) {} }
/** @method RightResult virtualCall() */
class VirtualRight { public function __call(string $name, array $args) {} }
class Base { public function late(): static {} public function declared(): self {} }
class Child extends Base { public function childOnly(): void {} }
interface Tag {}
class Factory { public function next(): Left|Right {} }
interface Untyped { public function common(); }
interface MixedResult { public function common(): mixed; }
use Domain\LeftResult as ResultAlias;
interface ShapeLeft { /** @return array{item: ResultAlias} */ public function box(); }
interface ShapeRight { /** @return array{item: RightResult} */ public function box(); }
class Upper { public int $X; public LeftResult $shared; }
class Lower { public int $x; public RightResult $shared; }
interface InheritedLeft extends Left {}
interface InheritedRight extends Left {}
/**
 * @property-read int $ro
 * @property-write int $wo
 */
class TaggedLeft {}
/**
 * @property-read int $ro
 * @property-write int $wo
 */
class TaggedRight {}
"#;

struct Fixture {
    service: LspService<PhpLspBackend>,
    messages: UnboundedReceiver<Request>,
    source: String,
    uri: String,
    types_uri: String,
}

async fn send(service: &mut LspService<PhpLspBackend>, request: Request) -> serde_json::Value {
    let response = service.ready().await.unwrap().call(request).await.unwrap();
    if let Some(response) = &response {
        assert!(response.error().is_none(), "{response:?}");
    }
    response
        .map(|response| extract_result(Some(response)))
        .unwrap_or(serde_json::Value::Null)
}

impl Fixture {
    async fn new(type_text: &str, native: bool, expression: &str) -> Self {
        let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
        let (sender, messages) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(message) = socket.next().await {
                let _ = sender.send(message);
            }
        });
        send(&mut service, initialize_request_with_options(1, None, Some(json!({"stubExtensions":[], "indexVendor":false, "diagnosticsMode":"basic-semantic"})))).await;
        let base = std::env::temp_dir().join("php-lsp-composite-receivers");
        let uri = path_to_uri(&base.join("Use.php")).unwrap();
        let types_uri = path_to_uri(&base.join("Types.php")).unwrap();
        send(&mut service, did_open_notification(&types_uri, TYPES)).await;
        let declaration = if native {
            format!("function test({type_text} $value) {{")
        } else {
            format!("function test($value) {{\n/** @var {type_text} $value */")
        };
        let source = format!("<?php\nnamespace Client;\nuse Domain\\Left as L;\nuse Domain\\Right as R;\nuse Domain\\Third as T;\n{declaration}\n    {expression};\n}}\n");
        send(&mut service, did_open_notification(&uri, &source)).await;
        Self {
            service,
            messages,
            source,
            uri,
            types_uri,
        }
    }

    fn position(&self, needle: &str, offset: usize) -> (u32, u32) {
        let byte = self.source.rfind(needle).unwrap() + offset;
        let before = &self.source[..byte];
        let line = before.bytes().filter(|byte| *byte == b'\n').count() as u32;
        let column = before.rsplit('\n').next().unwrap().encode_utf16().count() as u32;
        (line, column)
    }

    async fn labels(&mut self, expression: &str) -> BTreeSet<String> {
        let (line, column) = self.position(expression, expression.len());
        let result = send(
            &mut self.service,
            completion_request(2, &self.uri, line, column),
        )
        .await;
        completion_items_from_result(&result)
            .iter()
            .filter_map(|item| item["label"].as_str().map(str::to_string))
            .collect()
    }

    async fn finish(mut self) {
        send(&mut self.service, shutdown_request(99)).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn intersection_completion_keeps_both_constituents_in_any_order() {
    for native in [false, true] {
        for receiver in ["L&R", "R&L"] {
            let mut fixture = Fixture::new(receiver, native, "$value->").await;
            let labels = fixture.labels("$value->").await;
            assert!(
                labels.contains("common")
                    && labels.contains("leftOnly")
                    && labels.contains("rightOnly"),
                "{receiver} native={native}: {labels:?}"
            );
            fixture.finish().await;
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn union_completion_exposes_only_common_members_in_any_order() {
    for native in [false, true] {
        for receiver in ["L|R", "R|L"] {
            let mut fixture = Fixture::new(receiver, native, "$value->").await;
            let labels = fixture.labels("$value->").await;
            assert!(labels.contains("common"), "{receiver}: {labels:?}");
            assert!(
                !labels.contains("leftOnly") && !labels.contains("rightOnly"),
                "unsafe union completion {receiver} native={native}: {labels:?}"
            );
            fixture.finish().await;
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn union_member_chain_keeps_all_return_alternatives() {
    for receiver in ["L|R", "R|L"] {
        let mut fixture = Fixture::new(receiver, true, "$value->common()->").await;
        let labels = fixture.labels("$value->common()->").await;
        assert!(labels.contains("sharedLeaf"), "{labels:?}");
        assert!(
            !labels.contains("leftLeaf") && !labels.contains("rightLeaf"),
            "first return alternative leaked: {labels:?}"
        );
        fixture.finish().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn dnf_receiver_applies_intersection_then_union_member_rules() {
    let mut fixture = Fixture::new("(L&R)|T", true, "$value->").await;
    let labels = fixture.labels("$value->").await;
    assert!(labels.contains("common"), "{labels:?}");
    assert!(
        !labels.contains("leftOnly") && !labels.contains("rightOnly"),
        "{labels:?}"
    );
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn scalar_union_does_not_invent_a_guaranteed_object_receiver() {
    for receiver in ["L|int", "int|L"] {
        let mut fixture = Fixture::new(receiver, true, "$value->").await;
        let labels = fixture.labels("$value->").await;
        assert!(
            !labels.contains("leftOnly") && !labels.contains("common"),
            "{receiver}: {labels:?}"
        );
        fixture.finish().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn nullable_object_nullsafe_completion_remains_available() {
    let mut fixture = Fixture::new("?L", true, "$value?->").await;
    assert!(fixture.labels("$value?->").await.contains("leftOnly"));
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn intersection_hover_definition_and_diagnostics_resolve_second_constituent() {
    let mut fixture = Fixture::new("L&R", false, "$value->rightOnly()").await;
    let (line, column) = fixture.position("rightOnly", 2);
    let hover = send(
        &mut fixture.service,
        hover_request(2, &fixture.uri, line, column),
    )
    .await;
    assert!(
        hover_markdown_value(&hover).contains("rightOnly"),
        "{hover}"
    );
    let definition = send(
        &mut fixture.service,
        definition_request(3, &fixture.uri, line, column),
    )
    .await;
    let location = definition
        .as_array()
        .and_then(|values| values.first())
        .unwrap_or(&definition);
    assert_eq!(
        location
            .get("uri")
            .or_else(|| location.get("targetUri"))
            .and_then(|uri| uri.as_str()),
        Some(fixture.types_uri.as_str()),
        "{definition}"
    );
    let diagnostics =
        next_publish_diagnostics(&mut fixture.messages, &fixture.uri, Duration::from_secs(2)).await;
    assert!(
        published_diagnostic_messages(&diagnostics)
            .iter()
            .all(|message| !message.contains("Unknown method")),
        "{diagnostics}"
    );
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn union_exclusive_member_is_not_an_exact_definition_or_hover() {
    let mut fixture = Fixture::new("L|R", true, "$value->leftOnly()").await;
    let (line, column) = fixture.position("leftOnly", 2);
    let hover = send(
        &mut fixture.service,
        hover_request(2, &fixture.uri, line, column),
    )
    .await;
    let definition = send(
        &mut fixture.service,
        definition_request(3, &fixture.uri, line, column),
    )
    .await;
    assert!(hover.is_null(), "unsafe union hover: {hover}");
    assert!(
        definition.is_null() || definition.as_array().is_some_and(Vec::is_empty),
        "unsafe union definition: {definition}"
    );
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn assigned_composite_call_result_keeps_all_branches() {
    let mut fixture = Fixture::new("L|R", true, "$result = $value->common();\n    $result->").await;
    let labels = fixture.labels("$result->").await;
    assert!(labels.contains("sharedLeaf"), "{labels:?}");
    assert!(
        !labels.contains("leftLeaf") && !labels.contains("rightLeaf"),
        "{labels:?}"
    );
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn common_member_definition_keeps_every_real_target_and_hover_return() {
    let mut fixture = Fixture::new("L|R", true, "$value->common()").await;
    let (line, col) = fixture.position("common", 2);
    let definition = send(
        &mut fixture.service,
        definition_request(2, &fixture.uri, line, col),
    )
    .await;
    assert_eq!(definition.as_array().map(Vec::len), Some(2), "{definition}");
    let hover = send(
        &mut fixture.service,
        hover_request(3, &fixture.uri, line, col),
    )
    .await;
    let text = hover_markdown_value(&hover);
    assert!(
        text.contains("LeftResult") && text.contains("RightResult") && text.contains('|'),
        "{text}"
    );
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn composite_completion_resolve_preserves_merged_return_detail() {
    let mut fixture = Fixture::new("L|R", true, "$value->").await;
    let (line, col) = fixture.position("$value->", "$value->".len());
    let result = send(
        &mut fixture.service,
        completion_request(2, &fixture.uri, line, col),
    )
    .await;
    let item = completion_items_from_result(&result)
        .into_iter()
        .find(|item| item["label"] == "common")
        .unwrap();
    let resolved = send(
        &mut fixture.service,
        Request::build("completionItem/resolve")
            .params(item.clone())
            .id(3)
            .finish(),
    )
    .await;
    assert_eq!(resolved["detail"], item["detail"]);
    let detail = resolved["detail"].as_str().unwrap();
    assert!(
        detail.contains("LeftResult") && detail.contains("RightResult"),
        "{detail}"
    );
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_union_branch_cannot_be_silently_dropped() {
    let mut fixture = Fixture::new("L|Missing", false, "$value->").await;
    assert!(fixture.labels("$value->").await.is_empty());
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn generic_receiver_arguments_survive_common_member_returns() {
    let mut fixture = Fixture::new(
        "\\Domain\\Box<L>|\\Domain\\Box<R>",
        false,
        "$value->get()->",
    )
    .await;
    let labels = fixture.labels("$value->get()->").await;
    assert!(
        labels.contains("common") && !labels.contains("leftOnly") && !labels.contains("rightOnly"),
        "{labels:?}"
    );
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_null_union_and_nullable_syntax_agree_for_nullsafe_access() {
    for receiver in ["L|null", "null|L"] {
        let mut fixture = Fixture::new(receiver, true, "$value?->leftOnly()").await;
        let (line, col) = fixture.position("leftOnly", 2);
        let hover = send(
            &mut fixture.service,
            hover_request(2, &fixture.uri, line, col),
        )
        .await;
        assert!(
            hover_markdown_value(&hover).contains("leftOnly"),
            "{receiver}: {hover}"
        );
        fixture.finish().await;
        let mut fixture = Fixture::new(receiver, true, "$value?->").await;
        assert!(fixture.labels("$value?->").await.contains("leftOnly"));
        fixture.finish().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn iterable_unions_preserve_subscript_and_foreach_element_alternatives() {
    for expression in ["$value[0]->", "foreach ($value as $element) { $element-> }"] {
        let mut fixture = Fixture::new("array<int,L>|array<int,R>", false, expression).await;
        let receiver = if expression.starts_with("foreach") {
            "$element->"
        } else {
            expression
        };
        let labels = fixture.labels(receiver).await;
        assert!(
            labels.contains("common")
                && !labels.contains("leftOnly")
                && !labels.contains("rightOnly"),
            "{expression}: {labels:?}"
        );
        fixture.finish().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn nested_composite_returns_are_identical_directly_and_after_assignment() {
    for expression in [
        "$value->next()->",
        "$result = $value->next();\n    $result->",
    ] {
        let mut fixture = Fixture::new(
            "\\Domain\\NestedLeft&\\Domain\\NestedRight",
            true,
            expression,
        )
        .await;
        let receiver = if expression.starts_with("$result") {
            "$result->"
        } else {
            expression
        };
        let labels = fixture.labels(receiver).await;
        assert!(
            labels.contains("leftLeaf") && labels.contains("sharedLeaf"),
            "{expression}: {labels:?}"
        );
        fixture.finish().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn crlf_and_unicode_do_not_change_composite_chain_completion() {
    let mut fixture = Fixture::new("L|R", true, "$value->common()->").await;
    fixture.source = fixture
        .source
        .replace("    $value", "    /* Я */ $value")
        .replace('\n', "\r\n");
    send(
        &mut fixture.service,
        did_change_full_notification(&fixture.uri, 2, &fixture.source),
    )
    .await;
    assert!(fixture
        .labels("$value->common()->")
        .await
        .contains("sharedLeaf"));
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn protected_common_member_is_visible_from_subclass_scope() {
    let mut fixture = Fixture::new(
        "\\Domain\\AccessBase|\\Domain\\AccessChild",
        true,
        "$value->",
    )
    .await;
    fixture.source = fixture.source.replace(
        "function test",
        "class Owner extends \\Domain\\AccessBase { function test",
    );
    fixture.source.push_str("}\n");
    send(
        &mut fixture.service,
        did_change_full_notification(&fixture.uri, 2, &fixture.source),
    )
    .await;
    assert!(fixture.labels("$value->").await.contains("guarded"));
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn virtual_phpdoc_members_participate_in_composite_selection() {
    let mut fixture = Fixture::new(
        "\\Domain\\VirtualLeft|\\Domain\\VirtualRight",
        false,
        "$value->virtualCall()->",
    )
    .await;
    assert!(fixture
        .labels("$value->virtualCall()->")
        .await
        .contains("sharedLeaf"));
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn composite_requests_load_closed_vendor_branches_with_diagnostics_off() {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "php-lsp-composite-vendor-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("vendor/composer")).unwrap();
    fs::create_dir_all(root.join("vendor/acme/contracts/src")).unwrap();
    fs::write(
        root.join("composer.json"),
        r#"{"autoload":{"psr-4":{"Client\\":"src/"}}}"#,
    )
    .unwrap();
    fs::write(root.join("vendor/composer/installed.json"), r#"{"packages":[{"name":"acme/contracts","install-path":"../acme/contracts","autoload":{"psr-4":{"Vendor\\":"src/"}}}]}"#).unwrap();
    fs::write(
        root.join("vendor/acme/contracts/src/Left.php"),
        "<?php namespace Vendor; interface Left { public function leftOnly(): void; }",
    )
    .unwrap();
    fs::write(
        root.join("vendor/acme/contracts/src/Right.php"),
        "<?php namespace Vendor; interface Right { public function rightOnly(): void; }",
    )
    .unwrap();
    let source = "<?php\nnamespace Client;\nfunction test(\\Vendor\\Left&\\Vendor\\Right $value) {\n    $value->rightOnly();\n}";
    let uri = path_to_uri(&root.join("src/Use.php")).unwrap();
    let root_uri = path_to_uri(&root).unwrap();
    for feature in ["completion", "hover", "definition"] {
        let (mut service, socket) = LspService::new(PhpLspBackend::new);
        tokio::spawn(async move {
            socket.collect::<Vec<_>>().await;
        });
        send(
            &mut service,
            initialize_request_with_options(
                1,
                Some(&root_uri),
                Some(json!({"stubExtensions":[], "indexVendor":true, "diagnosticsMode":"off"})),
            ),
        )
        .await;
        send(&mut service, did_open_notification(&uri, source)).await;
        match feature {
            "completion" => {
                let result = send(&mut service, completion_request(2, &uri, 3, 12)).await;
                let items = completion_items_from_result(&result);
                assert!(
                    items.iter().any(|item| item["label"] == "leftOnly")
                        && items.iter().any(|item| item["label"] == "rightOnly"),
                    "cold vendor completion: {result}"
                );
            }
            "hover" => {
                let result = send(&mut service, hover_request(2, &uri, 3, 15)).await;
                assert!(
                    hover_markdown_value(&result).contains("rightOnly"),
                    "cold vendor hover: {result}"
                );
            }
            _ => {
                let result = send(&mut service, definition_request(2, &uri, 3, 15)).await;
                assert!(
                    !result.is_null() && !result.as_array().is_some_and(Vec::is_empty),
                    "cold vendor definition: {result}"
                );
            }
        }
        send(&mut service, shutdown_request(99)).await;
    }
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn inherited_self_and_static_keep_declaring_and_receiver_scopes_distinct() {
    for method in ["late", "declared"] {
        let expression = format!("$value->{method}()->");
        let mut fixture = Fixture::new("\\Domain\\Child&\\Domain\\Tag", true, &expression).await;
        let labels = fixture.labels(&expression).await;
        assert_eq!(
            labels.contains("childOnly"),
            method == "late",
            "{method}: {labels:?}"
        );
        fixture.finish().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn composite_variable_type_definition_lists_all_constituents() {
    let mut fixture = Fixture::new("L|R", true, "$value->common()").await;
    let (line, col) = fixture.position("$value->", 2);
    let result = send(
        &mut fixture.service,
        type_definition_request(2, &fixture.uri, line, col),
    )
    .await;
    assert_eq!(result.as_array().map(Vec::len), Some(2), "{result}");
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn direct_and_assigned_new_preserve_qualified_receiver_provenance() {
    for expression in [
        "(new \\Domain\\Factory())->next()->",
        "$factory = new \\Domain\\Factory();\n    $factory->next()->",
    ] {
        let mut fixture = Fixture::new("L", true, expression).await;
        let receiver = if expression.starts_with("$factory") {
            "$factory->next()->"
        } else {
            expression
        };
        let labels = fixture.labels(receiver).await;
        assert!(
            labels.contains("common")
                && !labels.contains("leftOnly")
                && !labels.contains("rightOnly"),
            "{expression}: {labels:?}"
        );
        fixture.finish().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn one_letter_receiver_uses_the_whole_variable_node() {
    let mut fixture = Fixture::new("L&R", true, "$value->").await;
    fixture.source = fixture.source.replace("$value", "$x");
    send(
        &mut fixture.service,
        did_change_full_notification(&fixture.uri, 2, &fixture.source),
    )
    .await;
    let labels = fixture.labels("$x->").await;
    assert!(
        labels.contains("leftOnly") && labels.contains("rightOnly"),
        "{labels:?}"
    );
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn global_new_type_is_not_requalified_into_the_client_namespace() {
    for expression in [
        "(new \\Factory())->next()->",
        "$factory = new \\Factory();\n    $factory->next()->",
    ] {
        let mut fixture = Fixture::new("L", true, expression).await;
        let uri = path_to_uri(&std::env::temp_dir().join("php-lsp-composite-receivers/Global.php"))
            .unwrap();
        send(
            &mut fixture.service,
            did_open_notification(
                &uri,
                "<?php class Factory { public function next(): \\Domain\\Left|\\Domain\\Right {} }",
            ),
        )
        .await;
        let receiver = if expression.starts_with("$factory") {
            "$factory->next()->"
        } else {
            expression
        };
        let labels = fixture.labels(receiver).await;
        assert!(
            labels.contains("common")
                && !labels.contains("leftOnly")
                && !labels.contains("rightOnly"),
            "{expression}: {labels:?}"
        );
        fixture.finish().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn result_shapes_keep_declaring_namespace_and_imports_across_subscripts() {
    for expression in [
        "$value->box()['item']->",
        "$box = $value->box();\n    $box['item']->",
    ] {
        let mut fixture =
            Fixture::new("\\Domain\\ShapeLeft|\\Domain\\ShapeRight", true, expression).await;
        let receiver = if expression.starts_with("$box") {
            "$box['item']->"
        } else {
            expression
        };
        let labels = fixture.labels(receiver).await;
        assert!(
            labels.contains("sharedLeaf")
                && !labels.contains("leftLeaf")
                && !labels.contains("rightLeaf"),
            "{expression}: {labels:?}"
        );
        fixture.finish().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn intersection_keeps_known_return_constraint_while_union_stays_uncertain() {
    for unknown in ["\\Domain\\Untyped", "\\Domain\\MixedResult"] {
        for receiver in [
            format!("L&{unknown}"),
            format!("{unknown}&L"),
            format!("L|{unknown}"),
        ] {
            let mut fixture = Fixture::new(&receiver, true, "$value->common()->").await;
            let labels = fixture.labels("$value->common()->").await;
            assert_eq!(
                labels.contains("leftLeaf"),
                receiver.contains('&'),
                "{receiver}: {labels:?}"
            );
            fixture.finish().await;
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn property_names_remain_case_sensitive_and_property_results_merge() {
    let mut fixture = Fixture::new("\\Domain\\Upper|\\Domain\\Lower", true, "$value->").await;
    let labels = fixture.labels("$value->").await;
    assert!(
        !labels.contains("X") && !labels.contains("x") && labels.contains("shared"),
        "{labels:?}"
    );
    fixture.finish().await;
    let mut fixture =
        Fixture::new("\\Domain\\Upper|\\Domain\\Lower", true, "$value->shared->").await;
    let labels = fixture.labels("$value->shared->").await;
    assert!(
        labels.contains("sharedLeaf")
            && !labels.contains("leftLeaf")
            && !labels.contains("rightLeaf"),
        "{labels:?}"
    );
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn inherited_shared_definition_is_deduplicated() {
    let mut fixture = Fixture::new(
        "\\Domain\\InheritedLeft|\\Domain\\InheritedRight",
        true,
        "$value->common()",
    )
    .await;
    let (line, col) = fixture.position("common", 2);
    let result = send(
        &mut fixture.service,
        definition_request(2, &fixture.uri, line, col),
    )
    .await;
    assert_eq!(result.as_array().map(Vec::len), Some(1), "{result}");
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn dynamic_member_names_do_not_produce_false_composite_diagnostics() {
    let mut fixture = Fixture::new("L|R", true, "$method = 'common'; $value->$method()").await;
    let diagnostics =
        next_publish_diagnostics(&mut fixture.messages, &fixture.uri, Duration::from_secs(2)).await;
    assert!(
        published_diagnostic_messages(&diagnostics)
            .iter()
            .all(|message| !message.contains("$method") && !message.contains("Unknown method")),
        "{diagnostics}"
    );
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn composite_virtual_properties_preserve_read_and_write_access_modes() {
    for write in [false, true] {
        let mut fixture = Fixture::new(
            "\\Domain\\TaggedLeft|\\Domain\\TaggedRight",
            true,
            if write { "$value-> = 1" } else { "$value->" },
        )
        .await;
        let labels = fixture.labels("$value->").await;
        assert_eq!(labels.contains("ro"), !write, "write={write}: {labels:?}");
        assert_eq!(labels.contains("wo"), write, "write={write}: {labels:?}");
        fixture.finish().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn composite_virtual_property_definitions_keep_distinct_identical_docblocks() {
    for (property, expression) in [("ro", "$value->ro"), ("wo", "$value->wo = 1")] {
        let mut fixture = Fixture::new(
            "\\Domain\\TaggedLeft|\\Domain\\TaggedRight",
            true,
            expression,
        )
        .await;
        let (line, col) = fixture.position(&format!("->{property}"), 3);
        let result = send(
            &mut fixture.service,
            definition_request(2, &fixture.uri, line, col),
        )
        .await;
        assert_eq!(
            result.as_array().map(Vec::len),
            Some(2),
            "{property}: {result}"
        );
        let hover = send(
            &mut fixture.service,
            hover_request(3, &fixture.uri, line, col),
        )
        .await;
        assert!(
            hover_markdown_value(&hover).contains(&format!("${property}")),
            "{hover}"
        );
        fixture.finish().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn assigned_nullsafe_call_retains_nullable_result() {
    for receiver in ["L|null", "null|L"] {
        let mut fixture = Fixture::new(
            receiver,
            true,
            "$result = $value?->common();\n    $result?->leftLeaf()",
        )
        .await;
        let (line, col) = fixture.position("$result?->", 3);
        let hover = send(
            &mut fixture.service,
            hover_request(2, &fixture.uri, line, col),
        )
        .await;
        let text = hover_markdown_value(&hover);
        assert!(
            text.contains("LeftResult") && (text.contains('?') || text.contains("null")),
            "{receiver}: {text}"
        );
        assert!(fixture.labels("$result?->").await.contains("leftLeaf"));
        fixture.finish().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn object_shape_composites_keep_shared_properties_and_intersection_members() {
    for operator in ['|', '&'] {
        let ty =
            format!("object{{left:int,shared:string}}{operator}object{{right:int,shared:string}}");
        let mut fixture = Fixture::new(&ty, false, "$value->shared").await;
        let labels = fixture.labels("$value->").await;
        assert!(labels.contains("shared"), "{ty}: {labels:?}");
        assert_eq!(
            labels.contains("left") && labels.contains("right"),
            operator == '&',
            "{ty}: {labels:?}"
        );
        let (line, col) = fixture.position("shared", 2);
        let hover = send(
            &mut fixture.service,
            hover_request(2, &fixture.uri, line, col),
        )
        .await;
        assert!(hover_markdown_value(&hover).contains("string"), "{hover}");
        let diagnostics =
            next_publish_diagnostics(&mut fixture.messages, &fixture.uri, Duration::from_secs(2))
                .await;
        assert!(
            published_diagnostic_messages(&diagnostics)
                .iter()
                .all(|message| !message.contains("shared")),
            "{diagnostics}"
        );
        fixture.finish().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn scalar_exit_guards_narrow_only_when_control_flow_and_binding_are_safe() {
    for (body, narrowed) in [
        ("if (false === $value) { return; }", true),
        ("if ($value === false) { return; }", true),
        ("if ($value === false) { $message = 'return'; }", false),
        (
            "if ($value === false) { return; } $value = unknown();",
            false,
        ),
        (
            "$alias =& $value; if ($value === false) { return; } $alias = false;",
            false,
        ),
        (
            "if ($value === false) { return; } extract(['value' => false]);",
            false,
        ),
        (
            "remember($value); if ($value === false) { return; } changeRemembered();",
            false,
        ),
    ] {
        let expression = format!("{body}\n    $value->");
        let mut fixture = Fixture::new("L|false", true, &expression).await;
        assert_eq!(
            fixture.labels("$value->").await.contains("leftOnly"),
            narrowed,
            "{body}"
        );
        fixture.finish().await;
    }
}
