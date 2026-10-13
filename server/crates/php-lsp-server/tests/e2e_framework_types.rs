mod support;
use support::*;
use tower_lsp::LanguageServer;

async fn send(service: &mut LspService<PhpLspBackend>, request: Request) -> serde_json::Value {
    let response = service.ready().await.unwrap().call(request).await.unwrap();
    if let Some(response) = response {
        assert!(response.error().is_none(), "{response:?}");
        extract_result(Some(response))
    } else {
        serde_json::Value::Null
    }
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
    // Minimal builtin symbol metadata for namespace fallback; no PHP executes.
    send(&mut service,did_open_notification("file:///test/P232Builtins.php","<?php function array_values(array $array):array{} function array_keys(array $array):array{} function preg_match($pattern,$subject,&$matches=null){} function preg_match_all($pattern,$subject,&$matches=null){}" )).await;
    service
}
async fn hover(
    service: &mut LspService<PhpLspBackend>,
    uri: &str,
    source: &str,
    marker: &str,
) -> String {
    let (line, column) = utf16_position_after(source, marker);
    let result = send(service, hover_request(20, uri, line, column + 1)).await;
    hover_markdown_value(&result)
}

#[tokio::test(flavor = "current_thread")]
async fn eloquent_terminals_preserve_nullable_models_and_safe_finder_unions() {
    let mut service = service().await;
    let uri = "file:///test/framework-terminals.php";
    let source="<?php namespace Illuminate\\Database\\Eloquent {class Model{}} namespace App {class User extends \\Illuminate\\Database\\Eloquent\\Model{} function run(){$first=User::first();/*FIRST*/$first;$required=User::firstOrFail();/*REQUIRED*/$required;$scalar=User::find(1);/*SCALAR*/$scalar;$many=User::find([1,2]);/*MANY*/$many;}}";
    send(&mut service, did_open_notification(uri, source)).await;
    let first = hover(&mut service, uri, source, "/*FIRST*/").await;
    assert!(
        first.contains("User $first") && first.contains('?'),
        "non-null first: {first}"
    );
    let required = hover(&mut service, uri, source, "/*REQUIRED*/").await;
    assert!(
        required.contains("User $required") && !required.contains('?'),
        "wrong firstOrFail: {required}"
    );
    for marker in ["/*SCALAR*/", "/*MANY*/"] {
        let found = hover(&mut service, uri, source, marker).await;
        assert!(
            found.contains("User") && found.contains("Collection") && found.contains('?'),
            "unsafe argument-free finder metadata: {found}"
        );
    }
    let relation=source.replace("$many;}}", "$many;/** @var \\Illuminate\\Database\\Eloquent\\Relations\\HasMany<\\App\\User> $relation */$relation=$unknown;$related=$relation->first();/*RELATED*/$related;}}");
    send(
        &mut service,
        did_change_full_notification(uri, 2, &relation),
    )
    .await;
    let related = hover(&mut service, uri, &relation, "/*RELATED*/").await;
    assert!(
        related.contains("User $related") && related.contains('?'),
        "non-null related model: {related}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn method_hover_links_keep_interface_and_parent_with_different_casing() {
    let mut service = service().await;
    let uri = "file:///test/framework-hover-casing.php";
    let source="<?php\r\n/* 😀 */ interface Contract {function RUN();}\r\nclass ParentType {function Run(){}}\r\nclass Actual extends ParentType implements Contract {function /*METHOD*/run(){}}";
    send(&mut service, did_open_notification(uri, source)).await;
    let result = hover(&mut service, uri, source, "/*METHOD*/").await;
    assert!(
        result.contains("**Implements:**")
            && result.contains("Contract::RUN")
            && result.contains("#L2"),
        "lost interface link: {result}"
    );
    assert!(
        result.contains("**Overrides:**")
            && result.contains("ParentType::Run")
            && result.contains("#L3"),
        "lost parent link: {result}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn cross_file_unknown_function_presence_replaces_builtin_fallback() {
    let mut service = service().await;
    let uri = "file:///test/builtin-caller.php";
    let external = "file:///test/builtin-shadow.php";
    let source="<?php namespace App; class Known{} function run(){/** @var Known[] $items */$items=[];$values=array_values($items);foreach($values as $value){/*USE*/$value;}}";
    send(&mut service, did_open_notification(uri, source)).await;
    assert!(hover(&mut service, uri, source, "/*USE*/")
        .await
        .contains("Known"));
    send(
        &mut service,
        did_open_notification(
            external,
            "<?php namespace App; function array_values($items){}",
        ),
    )
    .await;
    let changed = hover(&mut service, uri, source, "/*USE*/").await;
    assert!(
        !changed.contains("Known"),
        "foreign function inherited builtin: {changed}"
    );
    send(
        &mut service,
        did_change_full_notification(external, 2, "<?php namespace App; function other($items){}"),
    )
    .await;
    assert!(hover(&mut service, uri, source, "/*USE*/")
        .await
        .contains("Known"));
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn foreign_relation_owner_does_not_gain_a_model_type() {
    let mut service = service().await;
    let uri = "file:///test/foreign-relation.php";
    let source="<?php namespace App; class User{} class HasMany{} function run(){/** @var HasMany<User> $relation */$relation=$unknown;$value=$relation->first();/*USE*/$value;}";
    send(&mut service, did_open_notification(uri, source)).await;
    let result = hover(&mut service, uri, source, "/*USE*/").await;
    assert!(
        !result.contains("User") && !result.contains("HasMany"),
        "foreign framework type: {result}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn namespaced_builtin_output_definition_uses_index_evidence() {
    let mut service = service().await;
    let uri = "file:///test/output-definition.php";
    let source="<?php namespace App; function run($text){preg_match('/x/',$text,/*OUTPUT*/$matches);/*USE*/$matches;}";
    send(&mut service, did_open_notification(uri, source)).await;
    let (line, column) = utf16_position_after(source, "/*USE*/");
    let result = send(&mut service, definition_request(50, uri, line, column + 1)).await;
    let location = result
        .as_array()
        .and_then(|items| items.first())
        .unwrap_or(&result);
    let (line, column) = utf16_position_after(source, "/*OUTPUT*/");
    assert_eq!(
        location["range"]["start"],
        json!({"line":line,"character":column}),
        "lost builtin output binding: {result}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_function_shadow_changes_preserve_the_final_fallback_state() {
    let mut service = service().await;
    let uri = "file:///test/builtin-concurrent-caller.php";
    let external = "file:///test/builtin-concurrent-functions.php";
    let caller="<?php namespace App; class Known{} function run(){/** @var Known[] $items */$items=[];$values=array_values($items);foreach($values as $value){/*USE*/$value;}}";
    send(&mut service, did_open_notification(uri, caller)).await;
    send(
        &mut service,
        did_open_notification(external, "<?php namespace App; function other($items){}"),
    )
    .await;
    let service = std::sync::Arc::new(service);
    let (line, column) = utf16_position_after(caller, "/*USE*/");
    tokio::time::timeout(Duration::from_secs(20), async {
        for version in 2..22 {
            let shadow = version % 2 == 0;
            let source = if shadow {
                "<?php namespace App; function array_values($items){}"
            } else {
                "<?php namespace App; function other($items){}"
            };
            let change = did_change_full_notification(external, version, source);
            let params = serde_json::from_value(change.params().unwrap().clone()).unwrap();
            let request = hover_request(90, uri, line, column + 1);
            let query = serde_json::from_value(request.params().unwrap().clone()).unwrap();
            let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
            let writer = service.clone();
            let gate = barrier.clone();
            let write = tokio::spawn(async move {
                gate.wait().await;
                writer.inner().did_change(params).await;
            });
            let reader = service.clone();
            let read = tokio::spawn(async move {
                barrier.wait().await;
                reader.inner().hover(query).await
            });
            let (write, read) = tokio::join!(write, read);
            write.unwrap();
            let overlap =
                hover_markdown_value(&serde_json::to_value(read.unwrap().unwrap()).unwrap());
            assert!(
                overlap.is_empty() || overlap.contains("Known"),
                "foreign/mixed type: {overlap}"
            );
            let request = hover_request(91, uri, line, column + 1);
            let query = serde_json::from_value(request.params().unwrap().clone()).unwrap();
            let current = hover_markdown_value(
                &serde_json::to_value(service.inner().hover(query).await.unwrap()).unwrap(),
            );
            assert_eq!(
                current.contains("Known"),
                !shadow,
                "stale function-presence snapshot at{version}: {current}"
            );
        }
    })
    .await
    .expect("function shadow stress watchdog");
    service.inner().shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn eloquent_terminal_receivers_keep_definition_and_completion_consistent_with_hover() {
    let mut service = service().await;
    let uri = "file:///test/framework-receiver-definition.php";
    let source="<?php namespace App; class User extends \\Illuminate\\Database\\Eloquent\\Model {function /*METHOD*/onlyUser(){}} function run(User $model){$required=$model->firstOrFail();/*HOVER*/$required;$required->/*CALL*/onlyUser();}";
    send(&mut service, did_open_notification(uri, source)).await;
    assert!(hover(&mut service, uri, source, "/*HOVER*/")
        .await
        .contains("User"));
    let (line, column) = utf16_position_after(source, "/*CALL*/");
    let result = send(&mut service, definition_request(70, uri, line, column + 1)).await;
    let location = result
        .as_array()
        .and_then(|items| items.first())
        .unwrap_or(&result);
    let (line, column) = utf16_position_after(source, "/*METHOD*/");
    assert_eq!(location["uri"], uri, "lost terminal receiver: {result}");
    assert_eq!(
        location["range"]["start"],
        json!({"line":line,"character":column}),
        "wrong receiver owner: {result}"
    );
    let (line, column) = utf16_position_after(source, "$required->");
    let result = send(&mut service, completion_request(71, uri, line, column)).await;
    let items = result
        .as_array()
        .or_else(|| result["items"].as_array())
        .expect("terminal receiver completion items");
    assert!(
        items
            .iter()
            .any(|item| item["label"] == "onlyUser" && item["kind"] == 2),
        "lost terminal receiver completion: {result}"
    );
    send(&mut service, shutdown_request(99)).await;
}
