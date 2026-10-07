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
    notifications: UnboundedReceiver<Request>,
    uri: String,
}

impl Fixture {
    async fn new(root: Option<&std::path::Path>) -> Self {
        let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
        let (tx, notifications) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(notification) = socket.next().await {
                let _ = tx.send(notification);
            }
        });
        let root_uri = root.map(|root| path_to_uri(root).unwrap());
        send(&mut service, initialize_request_with_options(1, root_uri.as_deref(),
            Some(json!({"stubExtensions":[], "indexVendor":false, "diagnosticsMode":"off", "cache":{"enabled":false}})))).await;
        let path = root
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::env::temp_dir().join("php-lsp-document-symbols"));
        Self {
            service,
            notifications,
            uri: path_to_uri(&path.join("Sections.php")).unwrap(),
        }
    }

    async fn open(&mut self, source: &str) {
        // Fixtures must exercise valid PHP, rather than accidentally relying on error recovery.
        let mut parser = php_lsp_parser::parser::FileParser::new();
        parser.parse_full(source);
        assert!(!parser.tree().unwrap().root_node().has_error(), "{source}");
        send(&mut self.service, did_open_notification(&self.uri, source)).await;
    }

    async fn symbols(&mut self) -> serde_json::Value {
        send(&mut self.service, document_symbol_request(2, &self.uri)).await
    }

    async fn change(&mut self, version: i32, source: &str) {
        send(
            &mut self.service,
            did_change_full_notification(&self.uri, version, source),
        )
        .await;
    }

    async fn finish(mut self) {
        send(&mut self.service, shutdown_request(99)).await;
    }
}

fn shape(symbols: &serde_json::Value) -> serde_json::Value {
    json!(symbols
        .as_array()
        .unwrap()
        .iter()
        .map(|symbol| {
            json!([
                symbol["name"],
                symbol["kind"],
                shape(&symbol.get("children").cloned().unwrap_or(json!([])))
            ])
        })
        .collect::<Vec<_>>())
}

fn leaf(name: &str, kind: u64) -> serde_json::Value {
    json!([name, kind, []])
}
fn branch(name: &str, kind: u64, children: Vec<serde_json::Value>) -> serde_json::Value {
    json!([name, kind, children])
}

fn position(value: &serde_json::Value) -> (u32, u32) {
    (
        value["line"].as_u64().unwrap() as u32,
        value["character"].as_u64().unwrap() as u32,
    )
}

fn source_offset(source: &str, point: (u32, u32)) -> usize {
    let mut start = 0;
    for (line, text) in source.split('\n').enumerate() {
        if line as u32 == point.0 {
            let text = text.strip_suffix('\r').unwrap_or(text);
            let mut units = 0;
            for (offset, ch) in text.char_indices() {
                if units == point.1 {
                    return start + offset;
                }
                units += ch.len_utf16() as u32;
            }
            assert_eq!(units, point.1, "invalid UTF-16 position {point:?}");
            return start + text.len();
        }
        start += text.len() + 1;
    }
    panic!("invalid line {point:?}");
}

fn selection_text<'a>(source: &'a str, symbol: &serde_json::Value) -> &'a str {
    let selection = &symbol["selectionRange"];
    &source[source_offset(source, position(&selection["start"]))
        ..source_offset(source, position(&selection["end"]))]
}

fn assert_ranges(source: &str, symbols: &serde_json::Value, parent: Option<&serde_json::Value>) {
    for symbol in symbols.as_array().unwrap() {
        let range = &symbol["range"];
        let start = position(&range["start"]);
        let end = position(&range["end"]);
        assert!(start <= end, "{symbol}");
        source_offset(source, start);
        source_offset(source, end);
        let selection = &symbol["selectionRange"];
        assert!(
            start <= position(&selection["start"]) && position(&selection["end"]) <= end,
            "{symbol}"
        );
        assert!(!selection_text(source, symbol).is_empty(), "{symbol}");
        if symbol["kind"] == 3 {
            assert_eq!(
                selection_text(source, symbol),
                symbol["name"].as_str().unwrap(),
                "{symbol}"
            );
        }
        if let Some(parent) = parent {
            assert!(
                position(&parent["range"]["start"]) <= start
                    && end <= position(&parent["range"]["end"]),
                "child outside parent: {parent} -> {symbol}"
            );
        }
        if let Some(children) = symbol.get("children") {
            assert_ranges(source, children, Some(symbol));
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn document_symbols_keep_unbracketed_namespace_declarations_and_members_separate() {
    let source = "<?php\nnamespace Alpha;\nclass Service { public int $first; public function run(int $a): void {} }\nfunction build(): int { return 1; }\nconst FLAG = 1;\nnamespace Beta;\nclass Service { public int $second; public function run(string $b): string {} }\nfunction build(): string { return ''; }\nconst FLAG = 2;\n";
    let mut fixture = Fixture::new(None).await;
    fixture.open(source).await;
    let actual = fixture.symbols().await;
    assert_eq!(
        shape(&actual),
        json!([
            branch(
                "Alpha",
                3,
                vec![
                    branch("Service", 5, vec![leaf("first", 7), leaf("run", 6)]),
                    leaf("build", 12),
                    leaf("FLAG", 14)
                ]
            ),
            branch(
                "Beta",
                3,
                vec![
                    branch("Service", 5, vec![leaf("second", 7), leaf("run", 6)]),
                    leaf("build", 12),
                    leaf("FLAG", 14)
                ]
            )
        ])
    );
    assert_eq!(
        position(&actual[0]["range"]["end"]),
        utf16_position_at(source, "namespace Beta")
    );
    assert_eq!(
        position(&actual[1]["range"]["end"]),
        utf16_position_for_offset(source, source.len())
    );
    assert_eq!(
        actual[0]["children"][0]["children"][1]["detail"],
        "(int $a): void"
    );
    assert_eq!(
        actual[1]["children"][0]["children"][1]["detail"],
        "(string $b): string"
    );
    assert_ranges(source, &actual, None);
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn document_symbols_group_all_declaration_kinds_in_bracketed_namespaces() {
    let source = "<?php\nnamespace App\\Model {\n/** @deprecated */ class Model { public const ID = 1; }\ninterface Contract { public function run(): void; }\ntrait Shared { public int $value; }\nenum State { case READY; }\n}\nnamespace App\\Tools { function helper(): void {} const ONE = 1, TWO = 2; }\n";
    let mut fixture = Fixture::new(None).await;
    fixture.open(source).await;
    let actual = fixture.symbols().await;
    assert_eq!(
        shape(&actual),
        json!([
            branch(
                "App\\Model",
                3,
                vec![
                    branch("Model", 5, vec![leaf("ID", 14)]),
                    branch("Contract", 11, vec![leaf("run", 6)]),
                    branch("Shared", 11, vec![leaf("value", 7)]),
                    // Preserve the parser's existing implicit enum name property.
                    branch("State", 10, vec![leaf("READY", 22), leaf("name", 7)])
                ]
            ),
            branch(
                "App\\Tools",
                3,
                vec![leaf("helper", 12), leaf("ONE", 14), leaf("TWO", 14)]
            )
        ])
    );
    assert_eq!(actual[0]["children"][0]["tags"], json!([1]));
    assert_eq!(
        position(&actual[0]["range"]["end"]),
        utf16_position_after(source, "\n}")
    );
    assert_ranges(source, &actual, None);
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn document_symbols_preserve_repeated_namespace_sections_in_source_order() {
    for source in [
        "<?php namespace Same; class First {} namespace Other; class Middle {} namespace Same; class Last {}",
        "<?php namespace Same { class First {} } namespace Other { class Middle {} } namespace Same { class Last {} }",
    ] {
        let mut fixture = Fixture::new(None).await;
        fixture.open(source).await;
        let actual = fixture.symbols().await;
        assert_eq!(shape(&actual), json!([branch("Same", 3, vec![leaf("First", 5)]), branch("Other", 3, vec![leaf("Middle", 5)]), branch("Same", 3, vec![leaf("Last", 5)])]));
        assert!(position(&actual[0]["range"]["end"]) <= position(&actual[1]["range"]["start"]));
        assert!(position(&actual[1]["range"]["end"]) <= position(&actual[2]["range"]["start"]));
        assert_ranges(source, &actual, None);
        fixture.finish().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn document_symbols_leave_global_sections_at_root_between_named_namespaces() {
    let source = "<?php namespace { class Before {} } namespace App { class Scoped {} } namespace { function between() {} const ROOT = 1; } namespace Other { class Last {} } namespace { class After {} }";
    let mut fixture = Fixture::new(None).await;
    fixture.open(source).await;
    let actual = fixture.symbols().await;
    assert_eq!(
        shape(&actual),
        json!([
            leaf("Before", 5),
            branch("App", 3, vec![leaf("Scoped", 5)]),
            leaf("between", 12),
            leaf("ROOT", 14),
            branch("Other", 3, vec![leaf("Last", 5)]),
            leaf("After", 5)
        ])
    );
    assert_ranges(source, &actual, None);
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn document_symbols_keep_empty_named_sections_without_inventing_global_wrappers() {
    let mut fixture = Fixture::new(None).await;
    fixture
        .open("<?php namespace Empty {} namespace Full { class One {} } namespace Empty {}")
        .await;
    assert_eq!(
        shape(&fixture.symbols().await),
        json!([
            branch("Empty", 3, vec![]),
            branch("Full", 3, vec![leaf("One", 5)]),
            branch("Empty", 3, vec![])
        ])
    );
    for (index, source) in [
        "<?php namespace {}",
        "<?php // empty\n",
        "<?php namespace { class Plain {} }",
    ]
    .iter()
    .enumerate()
    {
        fixture.change(index as i32 + 2, source).await;
        let result = fixture.symbols().await;
        if index < 2 {
            assert!(result.is_null(), "{result}");
        } else {
            assert_eq!(shape(&result), json!([leaf("Plain", 5)]));
        }
    }
    let empty_semicolon = "<?php namespace Only;";
    fixture.change(5, empty_semicolon).await;
    let result = fixture.symbols().await;
    assert_eq!(shape(&result), json!([branch("Only", 3, vec![])]));
    assert_ranges(empty_semicolon, &result, None);
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn document_symbols_do_not_attach_members_to_other_declarations_with_the_same_fqn() {
    // Duplicate declarations occur in editor buffers; lexical ownership still applies.
    let source = "<?php namespace Same { class Editing { public function first() {} } } namespace Same { class Editing { public function second() {} } }";
    let mut fixture = Fixture::new(None).await;
    fixture.open(source).await;
    let actual = fixture.symbols().await;
    assert_eq!(
        shape(&actual),
        json!([
            branch(
                "Same",
                3,
                vec![branch("Editing", 5, vec![leaf("first", 6)])]
            ),
            branch(
                "Same",
                3,
                vec![branch("Editing", 5, vec![leaf("second", 6)])]
            )
        ])
    );
    assert_ranges(source, &actual, None);
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn document_symbols_keep_phpdoc_virtual_members_and_contain_their_ranges() {
    let source = "<?php\nnamespace App {\n/** 😀\n * @method void virtualRun(int $count)\n * @property string $virtualValue\n */\n#[\\AllowDynamicProperties]\nclass Demo { public function nativeRun(): void {} }\n}\nnamespace Other { class Plain {} }\n";
    for source in [source.to_string(), source.replace('\n', "\r\n")] {
        let mut fixture = Fixture::new(None).await;
        fixture.open(&source).await;
        let actual = fixture.symbols().await;
        assert_eq!(
            shape(&actual),
            json!([
                branch(
                    "App",
                    3,
                    vec![branch(
                        "Demo",
                        5,
                        vec![
                            leaf("nativeRun", 6),
                            leaf("virtualValue", 7),
                            leaf("virtualRun", 6)
                        ]
                    )]
                ),
                branch("Other", 3, vec![leaf("Plain", 5)])
            ])
        );
        let class = &actual[0]["children"][0];
        assert_eq!(
            position(&class["range"]["start"]),
            utf16_position_at(&source, "/** 😀")
        );
        assert_eq!(selection_text(&source, class), "Demo");
        assert_eq!(
            selection_text(&source, &class["children"][1]),
            "virtualValue"
        );
        assert_eq!(selection_text(&source, &class["children"][2]), "virtualRun");
        assert_eq!(class["children"][2]["detail"], "(int $count): void");
        assert_ranges(&source, &actual, None);
        fixture.finish().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn document_symbols_keep_phpdoc_members_with_their_duplicate_declaration_owner() {
    let source = "<?php namespace Same {\n/** @method void firstVirtual() */\nclass Editing { public function firstNative() {} }\n} namespace Same {\n/** @method void secondVirtual() */\nclass Editing { public function secondNative() {} }\n}";
    let mut fixture = Fixture::new(None).await;
    fixture.open(source).await;
    let actual = fixture.symbols().await;
    assert_eq!(
        shape(&actual),
        json!([
            branch(
                "Same",
                3,
                vec![branch(
                    "Editing",
                    5,
                    vec![leaf("firstNative", 6), leaf("firstVirtual", 6)]
                )]
            ),
            branch(
                "Same",
                3,
                vec![branch(
                    "Editing",
                    5,
                    vec![leaf("secondNative", 6), leaf("secondVirtual", 6)]
                )]
            )
        ])
    );
    assert_eq!(
        position(&actual[0]["children"][0]["range"]["start"]),
        utf16_position_at(source, "/** @method void firstVirtual")
    );
    assert_eq!(
        position(&actual[1]["children"][0]["range"]["start"]),
        utf16_position_at(source, "/** @method void secondVirtual")
    );
    assert_ranges(source, &actual, None);
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn document_symbols_namespace_ranges_use_utf16_and_ignore_header_comment_decoys() {
    let source = "<?php\n/* 😀 */ namespace /* Alpha, {; */ Alpha { /* 😀 */ class One {} } /* 😀 */ namespace /* Бета\\Ж */\n// Бета\\Ж decoy\n# Бета\\Ж decoy\nБета\\Ж { class Two {} }\n";
    for source in [source.to_string(), source.replace('\n', "\r\n")] {
        let mut fixture = Fixture::new(None).await;
        fixture.open(&source).await;
        let actual = fixture.symbols().await;
        assert_eq!(
            shape(&actual),
            json!([
                branch("Alpha", 3, vec![leaf("One", 5)]),
                branch("Бета\\Ж", 3, vec![leaf("Two", 5)])
            ])
        );
        assert_eq!(
            position(&actual[0]["range"]["start"]),
            utf16_position_at(&source, "namespace /* Alpha")
        );
        assert_eq!(
            position(&actual[0]["selectionRange"]["start"]),
            utf16_position_at(&source, "Alpha {")
        );
        assert_eq!(
            position(&actual[1]["selectionRange"]["start"]),
            utf16_position_at(&source, "Бета\\Ж {")
        );
        assert_ranges(&source, &actual, None);
        fixture.finish().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn document_symbols_replace_namespace_ownership_after_unsaved_changes_and_reopen() {
    let mut fixture = Fixture::new(None).await;
    fixture
        .open("<?php namespace Old { class Moved {} } namespace Removed { class Gone {} }")
        .await;
    assert_eq!(
        shape(&fixture.symbols().await),
        json!([
            branch("Old", 3, vec![leaf("Moved", 5)]),
            branch("Removed", 3, vec![leaf("Gone", 5)])
        ])
    );
    fixture
        .change(2, "<?php namespace New; class Moved {}")
        .await;
    assert_eq!(
        shape(&fixture.symbols().await),
        json!([branch("New", 3, vec![leaf("Moved", 5)])])
    );
    fixture.change(3, "<?php class Moved {}").await;
    assert_eq!(shape(&fixture.symbols().await), json!([leaf("Moved", 5)]));
    send(&mut fixture.service, did_close_notification(&fixture.uri)).await;
    fixture
        .open("<?php namespace Reopened { class Fresh {} }")
        .await;
    assert_eq!(
        shape(&fixture.symbols().await),
        json!([branch("Reopened", 3, vec![leaf("Fresh", 5)])])
    );
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn document_symbols_use_the_same_namespace_hierarchy_for_indexed_closed_and_open_files() {
    let root = std::env::temp_dir().join(format!(
        "php-lsp-document-symbols-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    let source = "<?php namespace /* decoy */ Disk { class Stored {} } namespace { function globalHelper() {} } namespace Tail { class Last {} }\n";
    fs::write(root.join("Sections.php"), source).unwrap();
    let mut fixture = Fixture::new(Some(&root)).await;
    send(&mut fixture.service, initialized_notification()).await;
    wait_for_indexing_phase(&mut fixture.notifications, "ready", Duration::from_secs(15)).await;
    let closed = fixture.symbols().await;
    assert_eq!(
        shape(&closed),
        json!([
            branch("Disk", 3, vec![leaf("Stored", 5)]),
            leaf("globalHelper", 12),
            branch("Tail", 3, vec![leaf("Last", 5)])
        ])
    );
    assert_ranges(source, &closed, None);
    fixture.open(source).await;
    assert_eq!(fixture.symbols().await, closed);
    fixture
        .change(2, "<?php namespace Unsaved { class Replacement {} }")
        .await;
    assert_eq!(
        shape(&fixture.symbols().await),
        json!([branch("Unsaved", 3, vec![leaf("Replacement", 5)])])
    );
    send(&mut fixture.service, did_close_notification(&fixture.uri)).await;
    assert_eq!(
        fixture.symbols().await,
        closed,
        "close restores disk namespace sections"
    );
    fixture.finish().await;
    fs::remove_dir_all(root).unwrap();
}
