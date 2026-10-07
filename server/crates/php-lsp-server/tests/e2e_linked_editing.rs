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
                Some(json!({"stubExtensions":[], "diagnosticsMode":"off", "indexVendor":false})),
            ),
        )
        .await;
        Self {
            service,
            uri: path_to_uri(&std::env::temp_dir().join("php-lsp-linked-editing/Imports.php"))
                .unwrap(),
        }
    }

    async fn open(&mut self, source: &str) {
        send(&mut self.service, did_open_notification(&self.uri, source)).await;
    }
    async fn change(&mut self, version: i32, source: &str) {
        send(
            &mut self.service,
            did_change_full_notification(&self.uri, version, source),
        )
        .await;
    }

    async fn request(&mut self, source: &str, offset: usize) -> serde_json::Value {
        let (line, col) = utf16_position_for_offset(source, offset);
        send(
            &mut self.service,
            linked_editing_range_request(2, &self.uri, line, col),
        )
        .await
    }

    async fn pair(&mut self, source: &str, name: &str, offsets: &[usize]) {
        let expected: Vec<_> = offsets.iter().map(|offset| {
            let start = utf16_position_for_offset(source, *offset);
            let end = utf16_position_for_offset(source, *offset + name.len());
            json!({"start":{"line":start.0,"character":start.1}, "end":{"line":end.0,"character":end.1}})
        }).collect();
        assert_eq!(expected.len(), 2);
        for offset in offsets {
            let actual = self.request(source, offset + 1).await;
            assert_eq!(actual["ranges"], json!(expected), "{source}: {actual}");
            assert_eq!(actual["wordPattern"], "[A-Za-z_][A-Za-z0-9_]*");
        }
    }

    async fn none(&mut self, source: &str, offset: usize) {
        let actual = self.request(source, offset).await;
        assert!(actual.is_null(), "{source} at {offset}: {actual}");
    }

    async fn finish(mut self) {
        send(&mut self.service, shutdown_request(99)).await;
    }
}

fn positions(source: &str, name: &str) -> Vec<usize> {
    source
        .match_indices(name)
        .map(|(offset, _)| offset)
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn linked_editing_selects_terminal_and_alias_roles_with_utf16_crlf_and_comments() {
    let source =
        "<?php\nnamespace Thing {\n/* 😀 Ж */ use Thing\\Thing /* Thing decoy */ AS Thing;\n}\n";
    for source in [source.to_string(), source.replace('\n', "\r\n")] {
        let mut fixture = Fixture::new().await;
        fixture.open(&source).await;
        let offsets = positions(&source, "Thing");
        fixture
            .pair(&source, "Thing", &[offsets[2], offsets[4]])
            .await;
        for index in [0, 1, 3] {
            fixture.none(&source, offsets[index] + 1).await;
        }
        fixture.finish().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn linked_editing_group_and_comma_imports_do_not_merge_independent_targets() {
    let mut fixture = Fixture::new().await;
    for (index, source) in [
        "<?php use Vendor\\{First\\Thing as Thing, Second\\Thing as Local};",
        "<?php use First\\Thing as Thing, Second\\Thing as Local;",
        "<?php use Thing\\Thing\\{Thing as Thing, Other as Other};",
    ]
    .iter()
    .enumerate()
    {
        if index == 0 {
            fixture.open(source).await;
        } else {
            fixture.change(index as i32 + 1, source).await;
        }
        let offsets = positions(source, "Thing");
        if index < 2 {
            fixture.pair(source, "Thing", &offsets[..2]).await;
            fixture.none(source, offsets[2] + 1).await;
            fixture
                .none(source, source.find("Local").unwrap() + 1)
                .await;
        } else {
            fixture.pair(source, "Thing", &offsets[2..]).await;
            for offset in &offsets[..2] {
                fixture.none(source, offset + 1).await;
            }
        }
    }
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn linked_editing_rejects_ambiguous_bindings_and_preserves_independent_import_kinds() {
    let mut fixture = Fixture::new().await;
    for (index, source) in [
        "<?php use Vendor\\{Thing as Thing, Other as thing};",
        "<?php use function Vendor\\{Thing as Thing, Other as thing};",
        "<?php use const Vendor\\{Thing as Thing, Other as Thing};",
        "<?php use First\\Thing as Thing, Second\\Thing;",
    ]
    .iter()
    .enumerate()
    {
        if index == 0 {
            fixture.open(source).await;
        } else {
            fixture.change(index as i32 + 1, source).await;
        }
        for offset in positions(source, "Thing") {
            fixture.none(source, offset + 1).await;
        }
    }
    let source =
        "<?php use Vendor\\{Thing as Thing, function Thing as Thing, const Thing as Thing};";
    fixture.change(5, source).await;
    for pair in positions(source, "Thing").chunks_exact(2) {
        fixture.pair(source, "Thing", pair).await;
    }
    let source = "<?php use const Vendor\\{Thing as Thing, Other as thing};";
    fixture.change(6, source).await;
    fixture
        .pair(source, "Thing", &positions(source, "Thing"))
        .await;
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn linked_editing_never_links_namespace_segments_or_body_names_by_spelling() {
    let mut fixture = Fixture::new().await;
    let source = "<?php namespace Thing\\Thing { class Thing {} function Thing() {} new Thing(); }";
    fixture.open(source).await;
    for offset in positions(source, "Thing") {
        fixture.none(source, offset + 1).await;
    }
    let source = "<?php use Vendor\\Thing as Local; class Thing {}";
    fixture.change(2, source).await;
    for offset in positions(source, "Thing") {
        fixture.none(source, offset + 1).await;
    }
    for token in ["use", "Vendor", "as", "Local", ";"] {
        fixture.none(source, source.find(token).unwrap()).await;
    }
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn linked_editing_updates_pairs_after_unsaved_edits_and_isolates_documents_and_reopen() {
    let mut fixture = Fixture::new().await;
    let original = "<?php use Vendor\\Thing as Thing;";
    fixture.open(original).await;
    fixture
        .pair(original, "Thing", &positions(original, "Thing"))
        .await;
    let other = "<?php namespace Thing\\Thing { class Thing {} }";
    let other_uri =
        path_to_uri(&std::env::temp_dir().join("php-lsp-linked-editing/Other.php")).unwrap();
    send(
        &mut fixture.service,
        did_open_notification(&other_uri, other),
    )
    .await;
    fixture
        .pair(original, "Thing", &positions(original, "Thing"))
        .await;
    let renamed = "<?php /* 😀 */ use Vendor\\Renamed as Renamed;";
    fixture.change(2, renamed).await;
    fixture
        .pair(renamed, "Renamed", &positions(renamed, "Renamed"))
        .await;
    let unaliased = "<?php use Vendor\\Renamed;";
    fixture.change(3, unaliased).await;
    fixture
        .none(unaliased, unaliased.find("Renamed").unwrap() + 1)
        .await;
    send(&mut fixture.service, did_close_notification(&fixture.uri)).await;
    fixture
        .none(original, original.find("Thing").unwrap() + 1)
        .await;
    fixture.open(original).await;
    fixture
        .pair(original, "Thing", &positions(original, "Thing"))
        .await;
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn linked_editing_ranges_drive_incremental_edits_without_changing_import_prefixes() {
    let mut fixture = Fixture::new().await;
    let original = "<?php /* 😀 */ namespace Thing { use Thing\\Thing as Thing; }";
    fixture.open(original).await;
    let offsets = positions(original, "Thing");
    let response = fixture.request(original, offsets[2] + 1).await;
    let ranges = response["ranges"].as_array().expect("linked ranges");
    assert_eq!(ranges.len(), 2, "{response}");
    let mut source = original.to_string();
    let mut changes = Vec::new();
    for range in ranges.iter().rev() {
        assert_eq!(range["start"]["line"], 0);
        let start = php_lsp_parser::utf16::utf16_col_to_byte(
            &source,
            0,
            range["start"]["character"].as_u64().unwrap() as u32,
        ) as usize;
        let end = php_lsp_parser::utf16::utf16_col_to_byte(
            &source,
            0,
            range["end"]["character"].as_u64().unwrap() as u32,
        ) as usize;
        source.replace_range(start..end, "Renamed");
        changes.push(json!({"range":range,"text":"Renamed"}));
    }
    assert_eq!(
        source,
        "<?php /* 😀 */ namespace Thing { use Thing\\Renamed as Renamed; }"
    );
    send(
        &mut fixture.service,
        Request::build("textDocument/didChange")
            .params(json!({
                "textDocument":{"uri":fixture.uri,"version":2}, "contentChanges":changes
            }))
            .finish(),
    )
    .await;
    fixture
        .pair(&source, "Renamed", &positions(&source, "Renamed"))
        .await;
    for offset in positions(&source, "Thing") {
        fixture.none(&source, offset + 1).await;
    }
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn linked_editing_declines_malformed_imports_without_disabling_intact_imports() {
    let mut fixture = Fixture::new().await;
    for (index, source) in [
        "<?php use Vendor\\Thing as Thing",
        "<?php use Vendor\\{Thing as Thing, Other as };",
        "<?php use Vendor\\{Thing as Thing, Other;",
    ]
    .iter()
    .enumerate()
    {
        if index == 0 {
            fixture.open(source).await;
        } else {
            fixture.change(index as i32 + 1, source).await;
        }
        for offset in positions(source, "Thing") {
            fixture.none(source, offset + 1).await;
        }
    }
    let intact = "<?php use Vendor\\Thing as Thing; $broken = ;";
    fixture.change(4, intact).await;
    fixture
        .pair(intact, "Thing", &positions(intact, "Thing"))
        .await;
    fixture.finish().await;
}
