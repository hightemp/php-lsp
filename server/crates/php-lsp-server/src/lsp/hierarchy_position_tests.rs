use super::*;

#[tokio::test(flavor = "current_thread")]
async fn line_model_hierarchy_batches_build_one_position_index_per_source() {
    use futures::StreamExt;
    use tower::{Service, ServiceExt};
    let (mut service, mut socket) = tower_lsp::LspService::new(PhpLspBackend::new);
    tokio::spawn(async move { while socket.next().await.is_some() {} });
    let request=tower_lsp::jsonrpc::Request::build("initialize").id(1).params(serde_json::json!({"capabilities":{},"initializationOptions":{"stubExtensions":[],"indexVendor":false,"diagnosticsMode":"off"}})).finish();
    service.ready().await.unwrap().call(request).await.unwrap();
    let uri = "file:///line-hierarchy-batch.php";
    let mut source = "<?php\rclass Base {}\rfunction target():void {}\r".to_string();
    for i in 0..100 {
        source.push_str(&format!(
            "class Child{i} extends Base {{}}\rfunction caller{i}():void {{target();}}\r"
        ));
    }
    let request=tower_lsp::jsonrpc::Request::build("textDocument/didOpen").params(serde_json::json!({"textDocument":{"uri":uri,"languageId":"php","version":1,"text":source}})).finish();
    service.ready().await.unwrap().call(request).await.unwrap();
    let backend = service.inner();
    let item = backend
        .lsp_prepare_type_hierarchy(
            serde_json::from_value(
                serde_json::json!({"textDocument":{"uri":uri},"position":{"line":1,"character":8}}),
            )
            .unwrap(),
        )
        .await
        .unwrap()
        .unwrap()
        .remove(0);
    POSITION_INDEX_BUILDS.with(|c| c.set(0));
    let children = backend
        .lsp_subtypes(TypeHierarchySubtypesParams {
            item,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(children.len(), 100);
    assert_eq!(
        POSITION_INDEX_BUILDS.with(|c| c.get()),
        1,
        "per-symbol source rescans in subtypes"
    );
    for child in children {
        let i: usize = child.name.strip_prefix("Child").unwrap().parse().unwrap();
        assert_eq!(
            child.selection_range.start,
            Position::new(3 + 2 * i as u32, 6)
        );
    }
    let item=backend.lsp_prepare_call_hierarchy(serde_json::from_value(serde_json::json!({"textDocument":{"uri":uri},"position":{"line":2,"character":10}})).unwrap()).await.unwrap().unwrap().remove(0);
    POSITION_INDEX_BUILDS.with(|c| c.set(0));
    let callers = backend
        .lsp_incoming_calls(CallHierarchyIncomingCallsParams {
            item,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(callers.len(), 100);
    assert_eq!(
        POSITION_INDEX_BUILDS.with(|c| c.get()),
        1,
        "per-symbol source rescans in incoming calls"
    );
    for caller in callers {
        let i: usize = caller
            .from
            .name
            .strip_prefix("caller")
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            caller.from.selection_range.start,
            Position::new(4 + 2 * i as u32, 9)
        );
    }
}
