mod support;
use php_lsp_types::uri::path_to_uri;
use support::*;

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
                Some(json!({"stubExtensions":[],"diagnosticsMode":"off","indexVendor":false})),
            ),
        )
        .await;
        Self {
            service,
            uri: path_to_uri(&std::env::temp_dir().join("php-lsp-call-hierarchy/Calls.php"))
                .unwrap(),
        }
    }
    async fn open(&mut self, source: &str) {
        send(&mut self.service, did_open_notification(&self.uri, source)).await;
    }
    async fn change(&mut self, source: &str, version: i32) {
        send(
            &mut self.service,
            did_change_full_notification(&self.uri, version, source),
        )
        .await;
    }
    async fn prepare(&mut self, source: &str, needle: &str) -> serde_json::Value {
        let (line, col) = utf16_position_at(source, needle);
        let result = send(
            &mut self.service,
            prepare_call_hierarchy_request(2, &self.uri, line, col + 1),
        )
        .await;
        result.as_array().expect("hierarchy item")[0].clone()
    }
    async fn incoming(&mut self, item: serde_json::Value) -> serde_json::Value {
        send(&mut self.service, incoming_calls_request(3, item)).await
    }
    async fn outgoing(&mut self, item: serde_json::Value) -> serde_json::Value {
        send(&mut self.service, outgoing_calls_request(4, item)).await
    }
    async fn finish(mut self) {
        send(&mut self.service, shutdown_request(99)).await;
    }
}

fn edge_fqns(value: &serde_json::Value, direction: &str) -> Vec<String> {
    let mut result: Vec<_> = value
        .as_array()
        .into_iter()
        .flatten()
        .map(|edge| edge[direction]["data"]["fqn"].as_str().unwrap().to_string())
        .collect();
    result.sort();
    result
}
fn assert_call_range(source: &str, edge: &serde_json::Value, needle: &str) {
    let (line, col) = utf16_position_at(source, needle);
    assert_eq!(
        edge["fromRanges"][0]["start"],
        json!({"line":line,"character":col})
    );
}

#[tokio::test(flavor = "current_thread")]
async fn incoming_calls_exclude_unrelated_and_unknown_instance_receivers_and_scoped_calls() {
    let source="<?php namespace App;\nclass Target { public function run(): void {} }\nclass Unrelated { public function run(): void {} public function wrong(): void { $this->run(); self::run(); static::run(); } }\nfunction good(Target $target): void { $target->run(); }\nfunction bad(Unrelated $other): void { $other->run(); }\nfunction unknown($anything): void { $anything->run(); }\n";
    let mut fixture = Fixture::new().await;
    fixture.open(source).await;
    let target = fixture.prepare(source, "run(): void {}").await;
    let result = fixture.incoming(target).await;
    assert_eq!(edge_fqns(&result, "from"), vec!["App\\good"]);
    assert_eq!(result[0]["fromRanges"].as_array().unwrap().len(), 1);
    assert_call_range(source, &result[0], "run(); }\nfunction bad");
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn incoming_calls_bind_inherited_methods_and_exclude_overriding_declarations() {
    let source="<?php namespace App;\nclass ParentService { public function run(): void {} }\nclass Inherited extends ParentService { public function inside(): void { self::run(); parent::run(); static::run(); } }\nclass OverrideService extends ParentService { public function run(): void {} }\nfunction inherited(Inherited $service): void { $service->run(); }\nfunction overridden(OverrideService $service): void { $service->run(); }\n";
    let mut fixture = Fixture::new().await;
    fixture.open(source).await;
    let target = fixture.prepare(source, "run(): void {}").await;
    let result = fixture.incoming(target).await;
    assert_eq!(
        edge_fqns(&result, "from"),
        vec!["App\\Inherited::inside", "App\\inherited"]
    );
    let inside = result
        .as_array()
        .unwrap()
        .iter()
        .find(|edge| edge["from"]["name"] == "inside")
        .unwrap();
    assert_eq!(inside["fromRanges"].as_array().unwrap().len(), 3);
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn call_hierarchy_includes_nullsafe_edges_and_preserves_utf16_crlf_ranges() {
    let source="<?php namespace App;\nclass Target { public function run(): void {} }\nfunction caller(?Target $service): void { /* 😀 Ж */ $service?->run(); }\n";
    for source in [source.to_string(), source.replace('\n', "\r\n")] {
        let mut fixture = Fixture::new().await;
        fixture.open(&source).await;
        let target = fixture.prepare(&source, "run(): void {}").await;
        let incoming = fixture.incoming(target).await;
        assert_eq!(edge_fqns(&incoming, "from"), vec!["App\\caller"]);
        assert_call_range(&source, &incoming[0], "run(); }");
        let caller = fixture.prepare(&source, "caller(?Target").await;
        let outgoing = fixture.outgoing(caller).await;
        assert_eq!(edge_fqns(&outgoing, "to"), vec!["App\\Target::run"]);
        assert_call_range(&source, &outgoing[0], "run(); }");
        fixture.finish().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn call_hierarchy_excludes_first_class_callables_imports_and_anonymous_bodies() {
    let source="<?php namespace App;\nfunction target(): void {}\nfunction real(): void { target(); }\nfunction factory(): void { $first = target(...); $closure = function() { target(); }; $arrow = fn() => target(); }\nfunction nestedOwner(): void { function nested(): void { target(); } }\n";
    let mut fixture = Fixture::new().await;
    fixture.open(source).await;
    let target = fixture.prepare(source, "target(): void {}").await;
    let incoming = fixture.incoming(target).await;
    assert_eq!(
        edge_fqns(&incoming, "from"),
        // Unrepresented nested callables are omitted, never assigned to their parent.
        vec!["App\\real"]
    );
    let factory = fixture.prepare(source, "factory(): void").await;
    let outgoing = fixture.outgoing(factory).await;
    assert!(outgoing.is_null(), "{outgoing}");
    let nested_owner = fixture.prepare(source, "nestedOwner(): void").await;
    assert!(fixture.outgoing(nested_owner).await.is_null());
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn incoming_calls_keep_recursion_and_group_all_ranges_by_caller_identity() {
    let source = "<?php namespace App;\nfunction recursive(): void { recursive(); recursive(); }\n";
    let mut fixture = Fixture::new().await;
    fixture.open(source).await;
    let target = fixture.prepare(source, "recursive(): void").await;
    let incoming = fixture.incoming(target.clone()).await;
    assert_eq!(edge_fqns(&incoming, "from"), vec!["App\\recursive"]);
    assert_eq!(incoming[0]["fromRanges"].as_array().unwrap().len(), 2);
    let outgoing = fixture.outgoing(target).await;
    assert_eq!(edge_fqns(&outgoing, "to"), vec!["App\\recursive"]);
    assert_eq!(outgoing[0]["fromRanges"].as_array().unwrap().len(), 2);
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn incoming_calls_apply_namespace_aliases_casing_and_unsaved_receiver_changes() {
    let source="<?php namespace Model { class Target { public function Run(): void {} } class Other { public function Run(): void {} } }\nnamespace Usage { use Model\\Target as Alias; function caller(Alias $object): void { $object->rUN(); } }\n";
    let mut fixture = Fixture::new().await;
    fixture.open(source).await;
    let target = fixture.prepare(source, "Run(): void {}").await;
    assert_eq!(
        edge_fqns(&fixture.incoming(target.clone()).await, "from"),
        vec!["Usage\\caller"]
    );
    let changed = source.replace("use Model\\Target as Alias", "use Model\\Other as Alias");
    fixture.change(&changed, 2).await;
    assert!(fixture.incoming(target.clone()).await.is_null());
    fixture.change(source, 3).await;
    assert_eq!(
        edge_fqns(&fixture.incoming(target.clone()).await, "from"),
        vec!["Usage\\caller"]
    );
    let removed = source.replace("$object->rUN();", "");
    fixture.change(&removed, 4).await;
    assert!(fixture.incoming(target).await.is_null());
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn incoming_calls_resolve_open_cross_file_member_chains_with_index_type_information() {
    let mut fixture = Fixture::new().await;
    let library="<?php namespace Lib; class Target { public function run(): void {} } class Factory { public function create(): Target {} }";
    fixture.open(library).await;
    let target = fixture.prepare(library, "run(): void {}").await;
    let caller_uri =
        path_to_uri(&std::env::temp_dir().join("php-lsp-call-hierarchy/Caller.php")).unwrap();
    let caller="<?php namespace UseSite; function caller(?\\Lib\\Factory $factory): void { $factory?->create()?->run(); }";
    send(
        &mut fixture.service,
        did_open_notification(&caller_uri, caller),
    )
    .await;
    let result = fixture.incoming(target.clone()).await;
    assert_eq!(edge_fqns(&result, "from"), vec!["UseSite\\caller"]);
    assert_eq!(result[0]["from"]["uri"], caller_uri);
    send(
        &mut fixture.service,
        did_change_full_notification(
            &caller_uri,
            2,
            "<?php namespace UseSite; function caller($unknown): void { $unknown->run(); }",
        ),
    )
    .await;
    assert!(fixture.incoming(target).await.is_null());
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn call_hierarchy_object_calls_keep_lexical_private_binding_for_child_receivers() {
    let source="<?php class Base { private function run(): void {} public function caller(Child $other): void { $other->run(); } }\nclass Child extends Base { public function run(): void {} public function child(Child $other): void { $other->run(); } }";
    let mut fixture = Fixture::new().await;
    fixture.open(source).await;
    let base = fixture.prepare(source, "run(): void {}").await;
    assert_eq!(
        edge_fqns(&fixture.incoming(base).await, "from"),
        vec!["Base::caller"]
    );
    let caller = fixture.prepare(source, "caller(Child").await;
    assert_eq!(
        edge_fqns(&fixture.outgoing(caller).await, "to"),
        vec!["Base::run"]
    );
    let child = fixture
        .prepare(source, "run(): void {} public function child")
        .await;
    assert_eq!(
        edge_fqns(&fixture.incoming(child).await, "from"),
        vec!["Child::child"]
    );
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn call_hierarchy_private_trait_method_binding_uses_its_lexical_consuming_scope() {
    let source="<?php trait Shared { private function run(): void {} public function caller(Child $other): void { $other->run(); } }\nclass Base { use Shared; } class Child extends Base { public function run(): void {} }";
    let mut fixture = Fixture::new().await;
    fixture.open(source).await;
    let target = fixture.prepare(source, "run(): void {}").await;
    assert_eq!(
        edge_fqns(&fixture.incoming(target).await, "from"),
        vec!["Shared::caller"]
    );
    let caller = fixture.prepare(source, "caller(Child").await;
    assert_eq!(
        edge_fqns(&fixture.outgoing(caller).await, "to"),
        vec!["Shared::run"]
    );
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn call_hierarchy_omits_inaccessible_private_and_protected_method_edges() {
    for visibility in ["private", "protected"] {
        let source=format!("<?php class Base {{ {visibility} function run(): void {{}} }} class Child extends Base {{ function caller(Child $other) {{ $other->run(); }} }} function external(Base $other) {{ $other->run(); }}");
        let mut fixture = Fixture::new().await;
        fixture.open(&source).await;
        let target = fixture.prepare(&source, "run(): void {}").await;
        let incoming = fixture.incoming(target).await;
        if visibility == "private" {
            assert!(incoming.is_null(), "{incoming}");
        } else {
            assert_eq!(edge_fqns(&incoming, "from"), vec!["Child::caller"]);
        }
        let external = fixture.prepare(&source, "external(Base").await;
        assert!(fixture.outgoing(external).await.is_null());
        fixture.finish().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn call_hierarchy_class_override_suppresses_private_trait_binding() {
    let source="<?php trait Shared { private function run(): void {} public function caller(Base $other): void { $other->run(); } } class Base { use Shared; public function run(): void {} }";
    let mut fixture = Fixture::new().await;
    fixture.open(source).await;
    let trait_method = fixture.prepare(source, "run(): void {}").await;
    assert!(fixture.incoming(trait_method).await.is_null());
    let caller = fixture.prepare(source, "caller(Base").await;
    assert_eq!(
        edge_fqns(&fixture.outgoing(caller).await, "to"),
        vec!["Base::run"]
    );
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn call_hierarchy_omits_ambiguous_private_dispatch_for_reused_trait_copies() {
    let source="<?php trait Shared { private function run(): void {} public function caller(Child $other): void { $other->run(); } } class Base { use Shared; } class Child extends Base { use Shared; public function run(): void {} }";
    let mut fixture = Fixture::new().await;
    fixture.open(source).await;
    let caller = fixture.prepare(source, "caller(Child").await;
    assert!(fixture.outgoing(caller).await.is_null());
    let target = fixture.prepare(source, "run(): void {}").await;
    assert!(fixture.incoming(target).await.is_null());
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn call_hierarchy_protected_trait_access_uses_the_proven_consuming_class() {
    let source="<?php trait Shared { protected function run(): void {} public function caller(Child $other): void { $other->run(); } } class Base { use Shared; } class Child extends Base {}";
    let mut fixture = Fixture::new().await;
    fixture.open(source).await;
    let target = fixture.prepare(source, "run(): void {}").await;
    assert_eq!(
        edge_fqns(&fixture.incoming(target).await, "from"),
        vec!["Shared::caller"]
    );
    let caller = fixture.prepare(source, "caller(Child").await;
    assert_eq!(
        edge_fqns(&fixture.outgoing(caller).await, "to"),
        vec!["Shared::run"]
    );
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn incoming_constructor_calls_match_the_constructed_class_only() {
    let source="<?php namespace App;\nclass Target { public function __construct() {} }\nclass Other { public function __construct() {} }\nfunction good() { new Target(); }\nfunction wrong() { new Other(); }\n";
    let mut fixture = Fixture::new().await;
    fixture.open(source).await;
    let target = fixture.prepare(source, "__construct() {}").await;
    assert_eq!(
        edge_fqns(&fixture.incoming(target).await, "from"),
        vec!["App\\good"]
    );
    fixture.finish().await;
}
