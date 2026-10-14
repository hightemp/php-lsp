mod support;
use support::*;

const URI: &str = "file:///test/p239.php";
async fn send(service: &mut LspService<PhpLspBackend>, request: Request) -> serde_json::Value {
    match service.ready().await.unwrap().call(request).await.unwrap() {
        Some(response) => {
            assert!(response.error().is_none(), "{response:?}");
            extract_result(Some(response))
        }
        None => serde_json::Value::Null,
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
    service
}
fn position(value: &serde_json::Value) -> (u32, u32) {
    (
        value["line"].as_u64().unwrap() as u32,
        value["character"].as_u64().unwrap() as u32,
    )
}
fn offset(source: &str, position: (u32, u32)) -> usize {
    let mut start = 0;
    for (line, text) in source.split('\n').enumerate() {
        if line == position.0 as usize {
            let text = text.strip_suffix('\r').unwrap_or(text);
            let mut units = 0;
            for (byte, ch) in text.char_indices() {
                if units == position.1 {
                    return start + byte;
                }
                units += ch.len_utf16() as u32;
            }
            assert_eq!(
                units, position.1,
                "invalid coordinate {position:?} in {source:?}"
            );
            return start + text.len();
        }
        start += text.len() + 1;
    }
    panic!("invalid line {position:?}");
}
fn check(source: &str, positions: &[(u32, u32)], result: &serde_json::Value) {
    let items = result
        .as_array()
        .unwrap_or_else(|| panic!("expected array: {result}"));
    assert_eq!(items.len(), positions.len(), "lost positions: {result}");
    for (item, point) in items.iter().zip(positions) {
        let mut current = Some(item);
        let mut child = None;
        while let Some(item) = current {
            let start = position(&item["range"]["start"]);
            let end = position(&item["range"]["end"]);
            offset(source, start);
            offset(source, end);
            assert!(
                start <= *point && *point <= end,
                "caret {point:?} outside {item}"
            );
            if let Some((a, b)) = child {
                assert!(
                    start <= a && b <= end && (start, end) != (a, b),
                    "invalid parent: {item}"
                );
            }
            child = Some((start, end));
            current = item.get("parent").filter(|parent| parent.is_object());
        }
    }
}
fn empty(point: (u32, u32)) -> serde_json::Value {
    json!({"range":{"start":{"line":point.0,"character":point.1},"end":{"line":point.0,"character":point.1}}})
}

#[tokio::test(flavor = "current_thread")]
async fn p239_selection_preserves_mixed_positions_order_duplicates_and_eof() {
    let mut service = service().await;
    let source = "<?php\n\nfunction f(){ $value = trim(' hi '); }\n\n";
    send(&mut service, did_open_notification(URI, source)).await;
    let call = utf16_position_after(source, "tr");
    let positions = vec![call, (1, 0), (4, 0), call, (0, 0)];
    let result = send(
        &mut service,
        selection_range_request(2, URI, positions.clone()),
    )
    .await;
    check(source, &positions, &result);
    assert_eq!(result[0], result[3]);
    for (i, point) in positions.iter().enumerate() {
        let single = send(
            &mut service,
            selection_range_request(10 + i as i64, URI, vec![*point]),
        )
        .await;
        assert_eq!(result[i], single[0]);
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn p239_selection_empty_source_empty_batch_and_whitespace_have_total_responses() {
    let mut service = service().await;
    for source in ["", "  ", "<?php\r\n\r\n"] {
        send(&mut service, did_open_notification(URI, source)).await;
        let result = send(&mut service, selection_range_request(2, URI, vec![])).await;
        assert_eq!(result, json!([]));
        let point = utf16_position_for_offset(source, source.len());
        let result = send(
            &mut service,
            selection_range_request(3, URI, vec![point, (0, 0), point]),
        )
        .await;
        check(source, &[point, (0, 0), point], &result);
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn p239_invalid_selection_line_rejects_the_entire_batch() {
    let mut service = service().await;
    let source = "<?php $value = 1;";
    send(&mut service, did_open_notification(URI, source)).await;
    for bad in [(1, 0), (99, 0), (u32::MAX, 0)] {
        let response = service
            .ready()
            .await
            .unwrap()
            .call(selection_range_request(2, URI, vec![(0, 8), bad, (0, 9)]))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            response.error().map(|e| e.code),
            Some(ErrorCode::InvalidParams),
            "{response:?}"
        );
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn p239_selection_clamps_columns_and_keeps_unicode_crlf_ranges_valid() {
    let mut service = service().await;
    let source = "<?php\r\n/* 😀 */ $value = '😀';\r\n";
    send(&mut service, did_open_notification(URI, source)).await;
    let eol = (
        1,
        source
            .split('\n')
            .nth(1)
            .unwrap()
            .trim_end_matches('\r')
            .encode_utf16()
            .count() as u32,
    );
    let emoji = utf16_position_after(source, "/* ");
    let result = send(
        &mut service,
        selection_range_request(2, URI, vec![(1, u32::MAX), (emoji.0, emoji.1 + 1), (2, 0)]),
    )
    .await;
    check(source, &[eol, emoji, (2, 0)], &result);
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn p239_template_selection_maps_tokens_and_keeps_unmapped_positions() {
    let mut service = service().await;
    for (uri, language, source, needle, expected) in [
        (
            "file:///test/p239.twig",
            "twig",
            "<p>😀 plain</p>\r\n{{ customer.name }}\r\n<footer>unmapped</footer>\r\n",
            "custo",
            "customer",
        ),
        (
            "file:///test/p239.blade.php",
            "blade",
            "<p>😀 plain</p>\r\n{{ $customer->name }}\r\n<footer>unmapped</footer>\r\n",
            "custo",
            "customer",
        ),
    ] {
        send(
            &mut service,
            did_open_notification_with_language(uri, language, source),
        )
        .await;
        let mapped = utf16_position_after(source, needle);
        let unmapped = (2, 12);
        let result = send(
            &mut service,
            selection_range_request(2, uri, vec![unmapped, mapped, (3, 0), mapped]),
        )
        .await;
        check(source, &[unmapped, mapped, (3, 0), mapped], &result);
        assert_eq!(result[0], empty(unmapped));
        assert_eq!(result[2], empty((3, 0)));
        assert_eq!(result[1], result[3]);
        let range = &result[1]["range"];
        assert_eq!(
            &source[offset(source, position(&range["start"]))
                ..offset(source, position(&range["end"]))],
            expected
        );
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn p239_selection_tracks_edits_close_reopen_and_syntax_recovery() {
    let mut service = service().await;
    for (version, source) in [
        (1, "<?php\n\n$value = call(\n"),
        (2, "<?php\r\n\r\n$other = 2;\r\n"),
    ] {
        let notification = if version == 1 {
            did_open_notification(URI, source)
        } else {
            did_change_full_notification(URI, version, source)
        };
        send(&mut service, notification).await;
        let positions = vec![
            (2, 3),
            (1, 0),
            utf16_position_for_offset(source, source.len()),
        ];
        let result = send(
            &mut service,
            selection_range_request(3, URI, positions.clone()),
        )
        .await;
        check(source, &positions, &result);
    }
    send(&mut service, did_close_notification(URI)).await;
    assert!(
        send(&mut service, selection_range_request(4, URI, vec![(0, 0)]))
            .await
            .is_null()
    );
    let source = "<?php\n\n$new = 3;\n";
    send(&mut service, did_open_notification(URI, source)).await;
    let result = send(
        &mut service,
        selection_range_request(5, URI, vec![(2, 3), (3, 0)]),
    )
    .await;
    check(source, &[(2, 3), (3, 0)], &result);
    assert_eq!(
        result[0]["range"],
        json!({"start":{"line":2,"character":1},"end":{"line":2,"character":4}})
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn p239_selection_bounds_recursive_response_depth() {
    let mut service = service().await;
    let source = format!(
        "<?php $value = {}trim('x'){};",
        "(".repeat(128),
        ")".repeat(128)
    );
    send(&mut service, did_open_notification(URI, &source)).await;
    let point = utf16_position_after(&source, "tr");
    let result = send(&mut service, selection_range_request(2, URI, vec![point])).await;
    check(&source, &[point], &result);
    let chain = selection_range_chain(&result[0]);
    assert!(
        chain.len() <= 64,
        "recursive response depth: {}",
        chain.len()
    );
    assert_eq!(chain[0].3 - chain[0].1, 4, "keep the closest identifier");
    send(&mut service, shutdown_request(99)).await;
}
