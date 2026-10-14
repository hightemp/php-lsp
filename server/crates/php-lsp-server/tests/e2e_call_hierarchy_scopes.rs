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
const URI: &str = "file:///test/p236-callable-scopes.php";
async fn service(source: &str) -> LspService<PhpLspBackend> {
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
    send(&mut service, did_open_notification(URI, source)).await;
    service
}
async fn prepare(
    service: &mut LspService<PhpLspBackend>,
    source: &str,
    needle: &str,
) -> serde_json::Value {
    let (line, column) = utf16_position_after(source, needle);
    send(
        service,
        prepare_call_hierarchy_request(2, URI, line, column),
    )
    .await
}
async fn assert_no_outer_fallback(fragment: &str, needle: &str) {
    let source=format!("<?php\r\nfunction target() {{}}\r\nclass Owner {{ function outer($captured) {{ /* 😀 */ {fragment} $after = 1; }} }}");
    let mut service = service(&source).await;
    let value = prepare(&mut service, &source, needle).await;
    assert!(
        value.is_null() || value.as_array().is_some_and(Vec::is_empty),
        "nested callable borrowed outer owner: {value}\n{source}"
    );
    let value = prepare(&mut service, &source, "$aft").await;
    assert_eq!(value[0]["data"]["fqn"], "Owner::outer");
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn p236_prepare_does_not_fall_back_from_closure_body_or_capture_to_outer_method() {
    for fragment in [
        "$f = function() use ($captured) { $inside = 1; target(); };",
        "$f = static function() use (&$captured) { $inside = 1; target(); };",
    ] {
        assert_no_outer_fallback(fragment, "$ins").await;
        assert_no_outer_fallback(fragment, "use (").await;
    }
}
#[tokio::test(flavor = "current_thread")]
async fn p236_prepare_does_not_fall_back_from_arrow_parameters_or_body() {
    for fragment in [
        "$f = fn($inside) => $inside;",
        "$f = static fn($inside) => fn() => $inside;",
    ] {
        assert_no_outer_fallback(fragment, "fn($ins").await;
        assert_no_outer_fallback(fragment, "=> $ins").await;
    }
}
#[tokio::test(flavor = "current_thread")]
async fn p236_prepare_unrepresented_nested_function_never_selects_its_outer_function() {
    assert_no_outer_fallback("function nested($inside) { $inside; target(); }", "$ins").await;
}
#[tokio::test(flavor = "current_thread")]
async fn p236_prepare_anonymous_class_body_and_method_do_not_select_outer_method() {
    assert_no_outer_fallback(
        "$o = new class { public $inside; function method() {} };",
        "$ins",
    )
    .await;
    assert_no_outer_fallback(
        "$o = new class { function method($inside) { $inside; target(); } };",
        "$ins",
    )
    .await;
}
#[tokio::test(flavor = "current_thread")]
async fn p236_prepare_resolves_explicit_callee_inside_closure_without_inventing_a_caller() {
    let source="<?php\r\nfunction target() {}\r\nfunction outer() { $f = function() { /* 😀 */ target(); }; }";
    let mut service = service(source).await;
    let prepared = prepare(&mut service, source, "/* 😀 */ tar").await;
    assert_eq!(prepared[0]["data"]["fqn"], "target");
    let incoming = send(&mut service, incoming_calls_request(3, prepared[0].clone())).await;
    assert!(incoming.is_null(), "{incoming}");
    send(&mut service, shutdown_request(99)).await;
}
#[tokio::test(flavor = "current_thread")]
async fn p236_graph_retains_only_outer_calls_including_nullsafe_after_nested_callables() {
    let source="<?php\r\nfunction target() {}\r\nclass Peer { function ping() {} }\r\nclass Owner { function outer(Peer $peer) { /* 😀 */ target(); $a=function() { target(); }; $b=static fn()=>target(); $c=fn()=>fn()=>target(); function nested(){ target(); } $o=new class { function hidden(){ target(); } }; $peer?->ping(); target(); } }";
    let mut service = service(source).await;
    let outer = prepare(&mut service, source, "function out").await[0].clone();
    let outgoing = send(&mut service, outgoing_calls_request(4, outer.clone())).await;
    let outgoing = outgoing.as_array().expect("outer edges");
    assert_eq!(outgoing.len(), 2, "{outgoing:?}");
    let target = outgoing
        .iter()
        .find(|edge| edge["to"]["data"]["fqn"] == "target")
        .unwrap();
    let ranges = target["fromRanges"].as_array().unwrap();
    assert_eq!(ranges.len(), 2, "{ranges:?}");
    for offset in [
        source.find("/* 😀 */ target").unwrap() + "/* 😀 */ ".len(),
        source.rfind("target();").unwrap(),
    ] {
        let (line, column) = utf16_position_for_offset(source, offset);
        assert!(
            ranges.iter().any(
                |range| range["start"] == json!({"line":line,"character":column})
                    && range["end"] == json!({"line":line,"character":column+6})
            ),
            "{ranges:?}"
        );
    }
    let incoming = send(
        &mut service,
        incoming_calls_request(5, target["to"].clone()),
    )
    .await;
    assert_eq!(incoming.as_array().unwrap().len(), 1, "{incoming}");
    assert_eq!(incoming[0]["from"]["data"]["fqn"], "Owner::outer");
    assert_eq!(incoming[0]["fromRanges"], target["fromRanges"]);
    let ping = outgoing
        .iter()
        .find(|edge| edge["to"]["data"]["fqn"] == "Peer::ping")
        .unwrap();
    assert_eq!(ping["fromRanges"].as_array().unwrap().len(), 1);
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn p236_prepare_scope_edges_keep_constructor_arguments_and_outer_continuation() {
    let source="<?php\r\nfunction target() {}\r\nfunction outer() { /* 😀 */ $o = new class(target(), 17) { public $property; }; $f = function() { /* inner */ }; /* after */ $after = 1; }";
    let mut service = service(source).await;
    for needle in ["target(), 1", "}; /* aft", "$aft"] {
        let item = prepare(&mut service, source, needle).await;
        assert_eq!(item[0]["data"]["fqn"], "outer", "{needle}: {item}");
    }
    for needle in ["function() ", "/* inner */ ", "$prop"] {
        let item = prepare(&mut service, source, needle).await;
        assert!(item.is_null(), "{needle}: {item}");
    }
    let outer = prepare(&mut service, source, "function out").await[0].clone();
    let calls = send(&mut service, outgoing_calls_request(4, outer)).await;
    assert_eq!(calls.as_array().unwrap().len(), 1, "{calls}");
    assert_eq!(calls[0]["to"]["data"]["fqn"], "target");
    assert_eq!(calls[0]["fromRanges"].as_array().unwrap().len(), 1);
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn p236_prepare_incomplete_callable_syntax_does_not_guess_outer_ownership() {
    for fragment in [
        "$f = fn $inside;",
        "$f = function $inside;",
        "$f = fn($inside) /* missing arrow */ $inside;",
        "$f = function() use $inside;",
        "$f = function { $inside; };",
        "$f = function( { $inside; };",
        "$f = fn() { $inside; };",
    ] {
        let source = format!("<?php class Owner {{ function outer() {{ /* 😀 */ {fragment} }} }}");
        let mut parser = php_lsp_parser::parser::FileParser::new();
        parser.parse_full(&source);
        assert!(
            parser.tree().unwrap().root_node().has_error(),
            "fixture must exercise syntax recovery"
        );
        let mut service = service(&source).await;
        let value = prepare(&mut service, &source, "$ins").await;
        assert!(
            value.is_null(),
            "incomplete callable borrowed outer owner: {value}\n{source}\n{}",
            parser.tree().unwrap().root_node().to_sexp()
        );
        send(&mut service, shutdown_request(99)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p236_prepare_edit_close_reopen_event_orders_never_mix_callable_scopes() {
    tokio::time::timeout(Duration::from_secs(15),async {
        for prefix in ["$f=function(){ ","$f=static fn()=> "] {
            let plain=format!("<?php\r\nfunction outer($probe) {{ /* 😀 */ {}$probe; }}", " ".repeat(prefix.len()));
            let nested=format!("<?php\r\nfunction outer($probe) {{ /* 😀 */ {prefix}$probe; {} }}",if prefix.contains("function") {"};"}else{""});
            let offset=plain.rfind("$probe").unwrap()+3;
            assert_eq!(nested.rfind("$probe").unwrap()+3,offset);
            let (line,column)=utf16_position_for_offset(&plain,offset);
            let mut service=service(&plain).await;
            let baseline=send(&mut service,prepare_call_hierarchy_request(19,URI,line,column)).await;
            assert_eq!(baseline[0]["data"]["fqn"],"outer");
            for round in 0..12 {
                let request=prepare_call_hierarchy_request(20,URI,line,column);
                let edit=did_change_full_notification(URI,2+round*2,&nested);
                let prepared=service.ready().await.unwrap().call(request);
                let changed=service.ready().await.unwrap().call(edit);
                let result=if round%2==0 {
                    let (value,change)=tokio::join!(biased;prepared,changed);change.unwrap();value.unwrap()
                } else {
                    let (change,value)=tokio::join!(biased;changed,prepared);change.unwrap();value.unwrap()
                };
                let result=extract_result(result);
                assert!(result.is_null() || result==baseline,"mixed prepare: {result}");
                let latest=send(&mut service,prepare_call_hierarchy_request(21,URI,line,column)).await;
                assert!(latest.is_null(),"stale outer fallback after edit: {latest}");
                send(&mut service,did_close_notification(URI)).await;
                assert!(send(&mut service,prepare_call_hierarchy_request(22,URI,line,column)).await.is_null());
                send(&mut service,did_open_notification(URI,&plain)).await;
                let reopened=send(&mut service,prepare_call_hierarchy_request(23,URI,line,column)).await;
                assert_eq!(reopened,baseline);
                let (start_line,start_column)=utf16_position_after(&plain,"function ");
                assert_eq!(reopened[0]["selectionRange"],json!({"start":{"line":start_line,"character":start_column},"end":{"line":start_line,"character":start_column+5}}));
            }
            send(&mut service,shutdown_request(99)).await;
        }
    }).await.expect("bounded hierarchy lifecycle rounds");
}

#[tokio::test(flavor = "current_thread")]
async fn p236_recovery_rejection_does_not_disable_healthy_sibling_fallback() {
    let source = "<?php
function outer() { $f = fn $broken; /* 😀 */ $healthy = 1; }
/* outside */";
    let mut service = service(source).await;
    let prepared = prepare(&mut service, source, "$heal").await;
    assert_eq!(prepared[0]["data"]["fqn"], "outer");
    assert!(prepare(&mut service, source, "/* out").await.is_null());
    send(&mut service, shutdown_request(99)).await;
}
