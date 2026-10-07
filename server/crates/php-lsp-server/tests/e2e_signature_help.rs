mod support;
use php_lsp_types::uri::path_to_uri;
use support::*;

const DECLARATIONS: &str = r#"<?php
namespace Scope;
function foo($first, $second, $third = null) {}
function outer($first, $second) {}
function inner($first, $second) {}
class Service {
    public function run($first, $second, $third = null) {}
    public static function make($first, $second) {}
    public function __construct($first, $second) {}
}
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
}
impl Fixture {
    async fn new() -> Self {
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
        let uri =
            path_to_uri(&std::env::temp_dir().join("php-lsp-signature-help/Use.php")).unwrap();
        Self { service, uri }
    }

    async fn request(&mut self, marked: &str, version: i32) -> serde_json::Value {
        let before = &marked[..marked.find("<cursor>").unwrap()];
        let line = before.bytes().filter(|byte| *byte == b'\n').count() as u32;
        let col = before.rsplit('\n').next().unwrap().encode_utf16().count() as u32;
        let source = marked.replacen("<cursor>", "", 1);
        if version == 1 {
            send(&mut self.service, did_open_notification(&self.uri, &source)).await;
        } else {
            send(
                &mut self.service,
                did_change_full_notification(&self.uri, version, &source),
            )
            .await;
        }
        send(
            &mut self.service,
            signature_help_request(2, &self.uri, line, col),
        )
        .await
    }

    async fn expect(&mut self, body: &str, fqn: &str, active: u64, version: i32) {
        let result = self
            .request(&format!("{DECLARATIONS}{body}"), version)
            .await;
        assert!(
            result["signatures"][0]["label"]
                .as_str()
                .is_some_and(|label| label.starts_with(fqn)),
            "{body}: {result}"
        );
        assert_eq!(
            result["activeParameter"].as_u64(),
            Some(active),
            "{body}: {result}"
        );
        assert_eq!(
            result["signatures"][0]["activeParameter"].as_u64(),
            Some(active)
        );
    }

    async fn finish(mut self) {
        send(&mut self.service, shutdown_request(99)).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn nullsafe_signature_help_resolves_method_with_comments_and_utf16_cursor() {
    let mut fixture = Fixture::new().await;
    for (index, body) in [
        "function demo(?Service $service) { /* 😀 */ $service?->run(1 /* , , */, <cursor>2); }"
            .to_string(),
        "function demo(?Service $service) { /* 😀 */ $service?->run(1 /* , , */, <cursor>2); }"
            .replace('\n', "\r\n"),
    ]
    .iter()
    .enumerate()
    {
        let source = format!("{DECLARATIONS}{body}");
        let source = if index == 1 {
            source.replace('\n', "\r\n")
        } else {
            source
        };
        let result = fixture.request(&source, index as i32 + 1).await;
        assert!(
            result["signatures"][0]["label"]
                .as_str()
                .is_some_and(|label| label.starts_with("Scope\\Service::run(")),
            "{result}"
        );
        assert_eq!(result["activeParameter"].as_u64(), Some(1), "{result}");
    }
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn signature_help_ignores_comment_and_string_punctuation_in_argument_positions() {
    let mut fixture = Fixture::new().await;
    for (index, body) in [
        "foo(1 /* , , */, <cursor>2);",
        "foo(1 /* ' ( */, <cursor>2);",
        "foo(1, // , , (\n<cursor>2);",
        "foo(1, # , , )\n<cursor>2);",
        "foo(<<<TEXT\na, b, (\nTEXT\n, <cursor>2);",
        "foo(<<<'TEXT'\na, b, [\nTEXT\n, <cursor>2);",
        "foo(`printf a,b`, <cursor>2);",
    ]
    .iter()
    .enumerate()
    {
        fixture
            .expect(body, "Scope\\foo(", 1, index as i32 + 1)
            .await;
    }
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn signature_help_handles_incomplete_function_method_static_and_constructor_calls() {
    let mut fixture = Fixture::new().await;
    for (index, (body, fqn, active)) in [
        ("foo(<cursor>", "Scope\\foo(", 0),
        ("foo(1, /* , ( */ <cursor>", "Scope\\foo(", 1),
        ("outer(inner(1, <cursor>", "Scope\\inner(", 1),
        ("new Service(1, <cursor>", "Scope\\Service::__construct(", 1),
        ("Service::make(1, <cursor>", "Scope\\Service::make(", 1),
        (
            "$service = new Service(1, 2); $service?->run(1, <cursor>",
            "Scope\\Service::run(",
            1,
        ),
        (
            "function demo(?Service $service) { $service?->run(1, <cursor>\n}",
            "Scope\\Service::run(",
            1,
        ),
    ]
    .iter()
    .enumerate()
    {
        fixture.expect(body, fqn, *active, index as i32 + 1).await;
    }
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn signature_help_tracks_nested_call_boundaries_and_rejects_nonargument_positions() {
    let mut fixture = Fixture::new().await;
    for (index, (body, fqn, active)) in [
        ("outer(inner(1, <cursor>2), 3);", "Scope\\inner(", 1),
        ("outer(inner(1, 2)<cursor>, 3);", "Scope\\outer(", 0),
        ("outer(inner(1, 2), <cursor>3);", "Scope\\outer(", 1),
    ]
    .iter()
    .enumerate()
    {
        fixture.expect(body, fqn, *active, index as i32 + 1).await;
    }
    for (index, body) in [
        "foo<cursor>(1, 2);",
        "foo(1, 2)<cursor>;",
        "$items = [1, <cursor>2];",
    ]
    .iter()
    .enumerate()
    {
        let result = fixture
            .request(&format!("{DECLARATIONS}{body}"), index as i32 + 4)
            .await;
        assert!(result.is_null(), "{body}: {result}");
    }
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn signature_help_resolves_complete_and_incomplete_nullsafe_chains_across_files() {
    let mut fixture = Fixture::new().await;
    let factory_uri =
        path_to_uri(&std::env::temp_dir().join("php-lsp-signature-help/Factory.php")).unwrap();
    send(&mut fixture.service, did_open_notification(&factory_uri, "<?php namespace Vendor; class Factory { public function service(): ?\\Scope\\Service {} }")).await;
    for (index, body) in [
        "function demo(?\\Vendor\\Factory $factory) { $factory?->service()?->run(1, <cursor>2); }",
        "function demo(?\\Vendor\\Factory $factory) { $factory?->service()?->run(1, <cursor>\n}",
    ]
    .iter()
    .enumerate()
    {
        fixture
            .expect(body, "Scope\\Service::run(", 1, index as i32 + 1)
            .await;
    }
    fixture.finish().await;
}
