mod support;

use php_lsp_types::uri::path_to_uri;
use support::*;

fn incremental_change(
    uri: &str,
    version: i32,
    line: u32,
    start: u32,
    end: u32,
    text: &str,
) -> Request {
    Request::build("textDocument/didChange")
        .params(json!({
            "textDocument": {"uri": uri, "version": version},
            "contentChanges": [{
                "range": {
                    "start": {"line": line, "character": start},
                    "end": {"line": line, "character": end}
                },
                "text": text
            }]
        }))
        .finish()
}

async fn links(service: &mut LspService<PhpLspBackend>, id: i64, uri: &str) -> serde_json::Value {
    extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(document_link_request(id, uri))
            .await
            .unwrap(),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn oversized_and_mid_surrogate_incremental_edits_keep_document_links_aligned() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("php-lsp-incremental-position-{nanos}"));
    fs::create_dir_all(&root).unwrap();
    for name in [
        "one.php",
        "two.php",
        "tail.php",
        "three.php",
        "X😀.php",
        "😀X.php",
        "Y😀.php",
    ] {
        fs::write(root.join(name), "<?php\n").unwrap();
    }
    let root_uri = path_to_uri(&root).unwrap();
    let tail_uri = path_to_uri(&root.join("tail.php")).unwrap();
    let three_uri = path_to_uri(&root.join("three.php")).unwrap();
    let emoji_uri = path_to_uri(&root.join("X😀.php")).unwrap();
    let changed_emoji_uri = path_to_uri(&root.join("Y😀.php")).unwrap();
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move { socket.collect::<Vec<_>>().await });
    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(1, Some(&root_uri), None))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();

    for (index, ending) in ["\n", "\r\n"].into_iter().enumerate() {
        let source = format!("<?php{ending}require 'one.php';{ending}require 'two.php';{ending}");
        let uri = path_to_uri(&root.join(format!("main-{index}.php"))).unwrap();
        fs::write(root.join(format!("main-{index}.php")), &source).unwrap();
        service
            .ready()
            .await
            .unwrap()
            .call(did_open_notification(&uri, &source))
            .await
            .unwrap();
        service
            .ready()
            .await
            .unwrap()
            .call(incremental_change(
                &uri,
                2,
                1,
                999,
                999,
                " require 'tail.php';",
            ))
            .await
            .unwrap();
        let first = links(&mut service, 2 + index as i64 * 2, &uri).await;
        let tail = first
            .as_array()
            .and_then(|items| items.iter().find(|item| item["target"] == tail_uri));
        assert_eq!(
            tail.and_then(|item| item["range"]["start"]["line"].as_u64()),
            Some(1),
            "oversized edit crossed {ending:?} into the next line: {first}"
        );

        service
            .ready()
            .await
            .unwrap()
            .call(incremental_change(&uri, 3, 2, 9, 16, "three.php"))
            .await
            .unwrap();
        let second = links(&mut service, 3 + index as i64 * 2, &uri).await;
        assert!(
            second
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["target"] == three_uri)),
            "following edit lost its line after {ending:?}: {second}"
        );
    }

    let source = "<?php\ninclude '😀.php';\n";
    let uri = path_to_uri(&root.join("emoji.php")).unwrap();
    fs::write(root.join("emoji.php"), source).unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&uri, source))
        .await
        .unwrap();
    let (line, start) = utf16_position_at(source, "😀");
    service
        .ready()
        .await
        .unwrap()
        .call(incremental_change(&uri, 2, line, start + 1, start + 1, "X"))
        .await
        .unwrap();
    let first = links(&mut service, 6, &uri).await;
    assert!(
        first
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["target"] == emoji_uri)),
        "mid-surrogate edit should insert before emoji: {first}"
    );
    service
        .ready()
        .await
        .unwrap()
        .call(incremental_change(&uri, 3, line, start, start + 1, "Y"))
        .await
        .unwrap();
    let second = links(&mut service, 7, &uri).await;
    assert!(
        second
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["target"] == changed_emoji_uri)),
        "following edit should retain emoji UTF-16 alignment: {second}"
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
    fs::remove_dir_all(root).unwrap();
}
