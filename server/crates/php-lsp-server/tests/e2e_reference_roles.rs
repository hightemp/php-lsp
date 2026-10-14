mod support;
use support::*;
const URI: &str = "file:///test/p238-roles.php";
async fn send(service: &mut LspService<PhpLspBackend>, request: Request) -> serde_json::Value {
    match service.ready().await.unwrap().call(request).await.unwrap() {
        Some(response) => {
            assert!(response.error().is_none(), "{response:?}");
            extract_result(Some(response))
        }
        None => serde_json::Value::Null,
    }
}
async fn check(source: &str, needle: &str) {
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
    let (line, column) = utf16_position_after(source, needle);
    let result = send(
        &mut service,
        document_highlight_request(2, URI, line, column),
    )
    .await;
    let highlights = result
        .as_array()
        .unwrap_or_else(|| panic!("no highlights: {result}\n{source}"));
    let mut expected = 0;
    for (marker, kind) in [("/*W*/", 3), ("/*R*/", 2)] {
        for (start, _) in source.match_indices(marker) {
            expected += 1;
            let start = start + marker.len();
            let text = &source[start..];
            let length = text
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
                .map(char::len_utf8)
                .sum::<usize>();
            let (sl, sc) = utf16_position_for_offset(source, start);
            let (el, ec) = utf16_position_for_offset(source, start + length);
            let range =
                json!({"start":{"line":sl,"character":sc},"end":{"line":el,"character":ec}});
            let highlight = highlights
                .iter()
                .find(|h| h["range"] == range)
                .unwrap_or_else(|| panic!("missing {marker} {range}: {result}\n{source}"));
            assert_eq!(
                highlight["kind"], kind,
                "{marker} {range}: {result}\n{source}"
            );
        }
    }
    assert_eq!(
        highlights.len(),
        expected,
        "unexpected binding: {result}\n{source}"
    );
    send(&mut service, shutdown_request(99)).await;
}
#[tokio::test(flavor = "current_thread")]
async fn p238_multiline_parameters_and_closure_arrow_bindings_are_writes() {
    for source in [
        "<?php\r\nfunction f(\r\n /* 😀 */ /*W*/$value\r\n) { /*R*/$value; }",
        "<?php $fn = fn(/*W*/$value) => /*R*/$value;",
        "<?php $fn = function(\n/*W*/$value\n){ /*R*/$value; };",
    ] {
        check(source, "/*R*/$val").await;
    }
}
#[tokio::test(flavor = "current_thread")]
async fn p238_foreach_catch_and_destructuring_targets_are_writes() {
    for source in [
        "<?php function f(){ foreach([[1]] as [/*W*/$value]) { /*R*/$value; } }",
        "<?php function f(){ foreach([1] as /*W*/$value) { /*R*/$value; } }",
        "<?php function f(){ try {} catch(\\Exception /*W*/$value) { /*R*/$value; } }",
        "<?php function f(){ [/*W*/$value] = [1]; /*R*/$value; }",
    ] {
        check(source, "/*R*/$val").await;
    }
}
#[tokio::test(flavor = "current_thread")]
async fn p238_multiline_assignments_updates_and_array_writes_follow_cst() {
    let source="<?php\r\nfunction f(){\r\n/*W*/$value\r\n= [];\r\n/*W*/$value /* comment */\r\n+= 1;\r\n++\r\n/*W*/$value;\r\n/*W*/$value[0]=1;\r\n/*R*/$value;\r\n}";
    check(source, "/*R*/$val").await;
}
#[tokio::test(flavor = "current_thread")]
async fn p238_comment_declaration_words_do_not_turn_reads_into_writes() {
    check("<?php function f(/*W*/$value){\n/* function fake( */ /*R*/$value;\n$object[/*R*/$value]=1;\n/*R*/$value->property=1;\n}","/*R*/$val").await;
}
#[tokio::test(flavor = "current_thread")]
async fn p238_property_declarations_and_multiline_writes_are_structural() {
    check("<?php class Box {\r\npublic\r\nint\r\n/*W*/$value;\r\nfunction f(){\r\n$this->/*W*/value\r\n= 1;\r\n$this->/*R*/value;\r\n}\r\n}","/*R*/val").await;
}
#[tokio::test(flavor = "current_thread")]
async fn p238_capture_is_read_and_builtin_output_is_write() {
    check("<?php function f(/*W*/$value){\npreg_match('/x/','x',/*W*/$value);\n$fn=function() use(/*R*/$value){ /*R*/$value; };\n}","/*R*/$val").await;
}

#[tokio::test(flavor = "current_thread")]
async fn p238_writable_bases_and_property_containers_keep_receiver_and_index_reads() {
    for source in [
        "<?php function f(/*W*/$value){ $a[/*R*/$value]=1; /*W*/$value[0]=1; /*R*/$value[0]->field=1; $alias = &/*R*/$value; unset(/*W*/$value); }",
        "<?php class Box {public /*W*/$value;function f($index){ $this->/*W*/value[$index]=1; unset($this->/*W*/value); $this->/*R*/value->field=1; }}",
        "<?php function f(/*W*/$value){ $other=/*R*/$value; call(/*R*/$value); /*W*/$value ??= 1; /*W*/$value++; }",
    ] {check(source,"/*R*/").await;}
}
#[tokio::test(flavor = "current_thread")]
async fn p238_custom_function_does_not_inherit_builtin_output_roles() {
    check("<?php namespace Local; function preg_match($a,$b,$c) {} function f(/*W*/$value){ preg_match('x','x',/*R*/$value); }","/*R*/$val").await;
}

#[tokio::test(flavor = "current_thread")]
async fn p238_builtin_output_properties_are_writes_but_ordinary_arguments_are_reads() {
    for source in [
        "<?php class Box {public /*W*/$value;function f(){ preg_match('/x/','x',$this->/*W*/value); preg_match('/x/','x',$this->/*W*/value[0]); ordinary($this->/*R*/value); }}",
        "<?php class Box {public static /*W*/$value;function f(){ preg_match('/x/','x',Box::/*W*/$value); ordinary(Box::/*R*/$value); }}",
        "<?php namespace Local; use function preg_match as match_it; class Box {public /*W*/$value;function f(){ match_it(matches: $this->/*W*/value, pattern: '/x/', subject: 'x'); \\preg_match('/x/','x',$this->/*W*/value); ordinary($this->/*R*/value); }}",
        "<?php namespace Local; function preg_match($a,$b,$c) {} class Box {public /*W*/$value;function f(){ preg_match('/x/','x',$this->/*R*/value); }}",
        "<?php namespace {function preg_match($a,$b,&$c){}} namespace Local {class Box {public /*W*/$value;function f(){ preg_match('/x/','x',$this->/*W*/value); ordinary($this->/*R*/value); }}}",
    ] { check(source,"/*R*/").await; }
}
