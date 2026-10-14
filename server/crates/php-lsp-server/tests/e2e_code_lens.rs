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

#[tokio::test(flavor = "current_thread")]
async fn p238_lenses_follow_unsaved_calls_reopen_utf16_and_root_isolation() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move { while socket.next().await.is_some() {} });
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("php-lsp-p238-roots-{nonce}"));
    let a = root.join("a");
    let b = root.join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    let uri = |path: &std::path::Path| php_lsp_types::uri::path_to_uri(path).unwrap();
    let target = uri(&a.join("Types.php"));
    let caller = uri(&a.join("Caller.php"));
    let foreign = uri(&b.join("Other.php"));
    let template = uri(&a.join("view.blade.php"));
    send(
        &mut service,
        initialize_request_with_workspace_folders_and_options(
            1,
            vec![("a", &uri(&a)), ("b", &uri(&b))],
            Some(json!({"stubExtensions":[],"indexVendor":false,"diagnosticsMode":"off"})),
        ),
    )
    .await;
    send(
        &mut service,
        did_open_notification(
            &target,
            "<?php\r\n/* 😀 */ class Box {static function run(){}}",
        ),
    )
    .await;
    send(
        &mut service,
        did_open_notification(&foreign, "<?php Box::run();Box::run();"),
    )
    .await;
    send(
        &mut service,
        did_open_notification_with_language(&template, "blade", "@php Box::run(); @endphp"),
    )
    .await;
    for (i, source) in [
        "<?php /* 😀 */ Box::run();",
        "<?php /* removed */",
        "<?php /* 😀 */ Box::run();",
    ]
    .iter()
    .enumerate()
    {
        let change = if i == 0 || i == 2 {
            did_open_notification(&caller, source)
        } else {
            did_change_full_notification(&caller, 2, source)
        };
        send(&mut service, change).await;
        let result = send(&mut service, code_lens_request(10 + i as i64, &target)).await;
        let lenses = result.as_array().unwrap_or_else(|| panic!("{result}"));
        assert_eq!(lenses.len(), 2);
        for lens in lenses {
            let count = if i == 1 { 0 } else { 1 };
            assert_eq!(lens["data"]["references"], count, "{result}");
            assert_eq!(lens["command"]["arguments"][0], target);
            assert_eq!(lens["command"]["arguments"][1], lens["range"]["start"]);
            let locations = lens["command"]["arguments"][2].as_array().unwrap();
            assert_eq!(locations.len(), count as usize);
            if count == 1 {
                assert_eq!(locations[0]["uri"], caller);
                let method = lens["data"]["fqn"] == "Box::run";
                let column = if method { 20 } else { 15 };
                assert_eq!(
                    locations[0]["range"]["start"]["character"], column,
                    "{lens}"
                );
            }
        }
        let class = lenses.iter().find(|l| l["data"]["fqn"] == "Box").unwrap();
        assert_eq!(class["range"]["start"]["character"], 15);
        if i == 1 {
            send(&mut service, did_close_notification(&caller)).await;
        }
    }
    send(&mut service, shutdown_request(99)).await;
    fs::remove_dir_all(root).unwrap();
}
