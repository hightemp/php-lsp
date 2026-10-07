mod support;
use php_lsp_types::uri::path_to_uri;
use std::collections::BTreeSet;
use support::*;

const TRAIT_SOURCE: &str = "<?php\nnamespace Scope;\ntrait Hidden {\n    private function traitSecret(): void {}\n    protected function traitGuarded(): void {}\n}\n";
const SOURCE: &str = r#"<?php
namespace Scope;
use Scope\Subject as Alias;
class Subject {
    use Hidden;
    private function secret(): void {}
    protected function guarded(): void {}
    private static function staticSecret(): void {}
    protected const GUARDED = 1;
    public function compare(Subject $other, Subject|Child $union): void {
        $other->ordinary;
        $union->composite;
        Alias::named;
        $this->traitSecret();
    }
}
class Child extends Subject {
    public function compareChild(Subject $other): void { $other->childAccess; }
}
class Stranger {
    use Hidden;
    public function compareStranger(Subject $other, Subject|Child $union): void {
        $other->foreignOrdinary;
        $union->foreignComposite;
    }
}
class LookupBase {
    protected int $hidden;
    private function localSecret(): void {}
    public function inspectLookup(LookupBase|LookupChild $union): void { $union->localSecret(); }
}
class LookupChild extends LookupBase { protected int $hidden; public function localSecret(): void {} }
class LookupSibling extends LookupBase {
    public function inspectSibling(LookupChild $child, LookupBase|LookupChild $union): void {
        $child->ordinaryRedeclaration;
        $union->compositeRedeclaration;
    }
}
trait Suppressed { private function choice(): int {} }
class Override {
    use Suppressed;
    public function choice(): string {}
    public function inspectOverride(Override|OverrideChild $union): void { $union->overrideAccess; }
}
class OverrideChild extends Override {}
"#;

async fn send(service: &mut LspService<PhpLspBackend>, request: Request) -> serde_json::Value {
    let response = service.ready().await.unwrap().call(request).await.unwrap();
    if let Some(response) = &response {
        assert!(response.error().is_none(), "{response:?}");
    }
    response
        .map(|response| extract_result(Some(response)))
        .unwrap_or(serde_json::Value::Null)
}

struct Fixture {
    service: LspService<PhpLspBackend>,
    uri: String,
    trait_uri: String,
    source: String,
}

impl Fixture {
    async fn new() -> Self {
        Self::with_source(SOURCE).await
    }

    async fn with_source(source: &str) -> Self {
        let (mut service, socket) = LspService::new(PhpLspBackend::new);
        tokio::spawn(async move {
            socket.collect::<Vec<_>>().await;
        });
        send(
            &mut service,
            initialize_request_with_options(
                1,
                None,
                Some(json!({"stubExtensions":[], "indexVendor":false, "diagnosticsMode":"off"})),
            ),
        )
        .await;
        let root = std::env::temp_dir().join("php-lsp-visibility");
        let uri = path_to_uri(&root.join("Use.php")).unwrap();
        let trait_uri = path_to_uri(&root.join("Hidden.php")).unwrap();
        send(
            &mut service,
            did_open_notification(&trait_uri, TRAIT_SOURCE),
        )
        .await;
        send(&mut service, did_open_notification(&uri, source)).await;
        Self {
            service,
            uri,
            trait_uri,
            source: source.into(),
        }
    }

    async fn completion(&mut self, marker: &str) -> serde_json::Value {
        let before = &self.source[..self.source.find(marker).unwrap()];
        let line = before.bytes().filter(|byte| *byte == b'\n').count() as u32;
        let column = before.rsplit('\n').next().unwrap().encode_utf16().count() as u32;
        send(
            &mut self.service,
            completion_request(2, &self.uri, line, column),
        )
        .await
    }

    async fn labels(&mut self, marker: &str) -> BTreeSet<String> {
        let result = self.completion(marker).await;
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
async fn ordinary_and_composite_completion_share_class_and_trait_visibility() {
    let mut fixture = Fixture::new().await;
    for marker in ["ordinary", "composite"] {
        let labels = fixture.labels(marker).await;
        for expected in ["secret", "guarded", "traitSecret", "traitGuarded"] {
            assert!(
                labels.contains(expected),
                "{marker}: missing {expected}: {labels:?}"
            );
        }
    }
    let labels = fixture.labels("named").await;
    assert!(
        labels.contains("staticSecret") && labels.contains("GUARDED"),
        "{labels:?}"
    );
    let before = &SOURCE[..SOURCE.find("traitSecret();").unwrap()];
    let line = before.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let column = before.rsplit('\n').next().unwrap().encode_utf16().count() as u32 + 2;
    let definition = send(
        &mut fixture.service,
        definition_request(3, &fixture.uri, line, column),
    )
    .await;
    let location = definition
        .as_array()
        .and_then(|items| items.first())
        .unwrap_or(&definition);
    assert_eq!(
        location
            .get("targetUri")
            .or_else(|| location.get("uri"))
            .and_then(|value| value.as_str()),
        Some(fixture.trait_uri.as_str()),
        "{definition}"
    );
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn unrelated_trait_consumers_and_inherited_private_members_remain_hidden() {
    let mut fixture = Fixture::new().await;
    for marker in ["foreignOrdinary", "foreignComposite"] {
        let labels = fixture.labels(marker).await;
        for forbidden in ["secret", "guarded", "traitSecret", "traitGuarded"] {
            assert!(
                !labels.contains(forbidden),
                "{marker}: leaked {forbidden}: {labels:?}"
            );
        }
    }
    let labels = fixture.labels("childAccess").await;
    assert!(
        labels.contains("guarded") && labels.contains("traitGuarded"),
        "{labels:?}"
    );
    assert!(
        !labels.contains("secret") && !labels.contains("traitSecret"),
        "{labels:?}"
    );
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn completion_visibility_refreshes_after_unsaved_trait_change() {
    let mut fixture = Fixture::new().await;
    assert!(!fixture.labels("childAccess").await.contains("traitSecret"));
    let updated = TRAIT_SOURCE.replace(
        "private function traitSecret",
        "protected function traitSecret",
    );
    send(
        &mut fixture.service,
        did_change_full_notification(&fixture.trait_uri, 2, &updated),
    )
    .await;
    assert!(fixture.labels("childAccess").await.contains("traitSecret"));
    send(
        &mut fixture.service,
        did_change_full_notification(&fixture.trait_uri, 3, TRAIT_SOURCE),
    )
    .await;
    assert!(!fixture.labels("childAccess").await.contains("traitSecret"));
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn composite_lookup_preserves_private_binding_and_blocks_hidden_property_fallback() {
    let mut fixture = Fixture::new().await;
    for marker in ["ordinaryRedeclaration", "compositeRedeclaration"] {
        assert!(!fixture.labels(marker).await.contains("hidden"), "{marker}");
    }
    assert!(fixture
        .labels("localSecret();")
        .await
        .contains("localSecret"));
    let before = &SOURCE[..SOURCE.find("localSecret();").unwrap()];
    let line = before.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let col = before.rsplit('\n').next().unwrap().encode_utf16().count() as u32 + 2;
    let definition = send(
        &mut fixture.service,
        definition_request(3, &fixture.uri, line, col),
    )
    .await;
    let expected_line = SOURCE[..SOURCE.find("private function localSecret").unwrap()]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count() as u64;
    let locations = definition
        .as_array()
        .expect("composite definition locations");
    assert_eq!(locations.len(), 1, "{definition}");
    let range = locations[0]
        .get("targetSelectionRange")
        .or_else(|| locations[0].get("range"))
        .unwrap();
    assert_eq!(
        range["start"]["line"].as_u64(),
        Some(expected_line),
        "{definition}"
    );
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn composite_completion_uses_class_override_instead_of_suppressed_private_trait() {
    let mut fixture = Fixture::new().await;
    let before = &SOURCE[..SOURCE.find("overrideAccess").unwrap()];
    let line = before.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let column = before.rsplit('\n').next().unwrap().encode_utf16().count() as u32;
    let result = send(
        &mut fixture.service,
        completion_request(2, &fixture.uri, line, column),
    )
    .await;
    let items = completion_items_from_result(&result);
    let item = items.iter().find(|item| item["label"] == "choice").unwrap();
    assert!(
        item["detail"].as_str().unwrap().contains("string"),
        "{item}"
    );
    fixture.finish().await;
}

const OBJECT_STATIC_SOURCE: &str = r#"<?php
namespace Scope;
/** @method static string virtualChoice() */
class Subject {
    public static function visible(): string {}
    protected static function guarded(): string {}
    private static function secret(): string {}
    public static int $counter;
    public const VALUE = 1;
    public function inspect(Subject $other, Subject|Child $union): void {
        /* 😀 */ $other->inside;
        $union->unionInside;
    }
}
class Child extends Subject {}
class Stranger { public function inspect(Subject $other): void { $other->foreign; } }
function inspectOutside(Subject $other, Subject|Child $union): void {
    $other?->outside;
    $union?->unionOutside;
}
class StaticBase {
    private static function choice(): int {}
    public function inspect(StaticChild $other, StaticBase|StaticChild $union, PrivateChild $private): void {
        $other->ordinaryBinding;
        $union->compositeBinding;
        StaticChild::namedBinding;
        $private->privateObject;
        PrivateChild::privateClass;
    }
}
class StaticChild extends StaticBase { public static function choice(): string {} }
class PrivateChild extends StaticBase { private function choice(): string {} }
"#;

#[tokio::test(flavor = "current_thread")]
async fn object_static_method_visibility_agrees_for_ordinary_union_and_nullsafe_calls() {
    for source in [
        OBJECT_STATIC_SOURCE.to_string(),
        OBJECT_STATIC_SOURCE.replace('\n', "\r\n"),
    ] {
        let mut fixture = Fixture::with_source(&source).await;
        for marker in ["inside", "unionInside"] {
            let labels = fixture.labels(marker).await;
            for allowed in ["visible", "guarded", "secret"] {
                assert!(
                    labels.contains(allowed),
                    "{marker}: missing {allowed}: {labels:?}"
                );
            }
            for forbidden in ["counter", "$counter", "VALUE", "virtualChoice"] {
                assert!(
                    !labels.contains(forbidden),
                    "{marker}: leaked {forbidden}: {labels:?}"
                );
            }
        }
        for marker in ["foreign", "outside", "unionOutside"] {
            let labels = fixture.labels(marker).await;
            assert!(labels.contains("visible"), "{marker}: {labels:?}");
            assert!(!labels.contains("guarded") && !labels.contains("secret"));
        }
        fixture.finish().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn object_and_class_calls_preserve_distinct_private_static_declaration_identity() {
    let mut fixture = Fixture::with_source(OBJECT_STATIC_SOURCE).await;
    for marker in ["ordinaryBinding", "compositeBinding", "privateObject"] {
        let result = fixture.completion(marker).await;
        let items = completion_items_from_result(&result);
        let choice = items
            .iter()
            .find(|item| item["label"] == "choice")
            .expect("private static object call");
        assert!(
            choice["detail"].as_str().unwrap().contains("int"),
            "{marker}: {choice}"
        );
        if marker != "compositeBinding" {
            assert_eq!(
                choice["data"]["fqn"], "Scope\\StaticBase::choice",
                "{choice}"
            );
            let resolved = send(
                &mut fixture.service,
                completion_resolve_request(3, choice.clone()),
            )
            .await;
            assert!(
                resolved["detail"].as_str().unwrap().contains("int"),
                "{resolved}"
            );
        }
    }
    let result = fixture.completion("namedBinding").await;
    let items = completion_items_from_result(&result);
    let choice = items.iter().find(|item| item["label"] == "choice").unwrap();
    assert_eq!(choice["data"]["fqn"], "Scope\\StaticChild::choice");
    assert!(choice["detail"].as_str().unwrap().contains("string"));
    assert!(!fixture.labels("privateClass").await.contains("choice"));
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn unsaved_static_method_visibility_updates_object_completion_without_stale_access() {
    let mut fixture = Fixture::with_source(OBJECT_STATIC_SOURCE).await;
    assert!(fixture.labels("outside").await.contains("visible"));
    fixture.source = fixture.source.replace(
        "public static function visible",
        "private static function visible",
    );
    send(
        &mut fixture.service,
        did_change_full_notification(&fixture.uri, 2, &fixture.source),
    )
    .await;
    assert!(!fixture.labels("outside").await.contains("visible"));
    assert!(fixture.labels("inside").await.contains("visible"));
    fixture.source = OBJECT_STATIC_SOURCE.into();
    send(
        &mut fixture.service,
        did_change_full_notification(&fixture.uri, 3, &fixture.source),
    )
    .await;
    assert!(fixture.labels("outside").await.contains("visible"));
    fixture.finish().await;
}
