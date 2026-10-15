#![cfg(unix)]
mod support;
use php_lsp_types::uri::path_to_uri;
use std::path::PathBuf;
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
struct Fixture {
    root: PathBuf,
    uri: String,
    source: String,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
impl Fixture {
    async fn new(
        source: &str,
        analyzer: &str,
        messages: serde_json::Value,
        enabled: bool,
    ) -> (Self, LspService<PhpLspBackend>, serde_json::Value) {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("php-lsp-qfm-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("Subject 😀.php");
        fs::write(&path, source).unwrap();
        let uri = path_to_uri(&path).unwrap();
        let output = if analyzer == "phpstan" {
            json!({"files":{(path.to_str().unwrap()):{"messages":messages}}})
        } else {
            let mut messages = messages;
            for issue in messages.as_array_mut().unwrap() {
                issue["file_path"] = json!(path);
            }
            messages
        };
        let script = root.join("analyzer.sh");
        fs::write(&script, format!("cat <<'JSON'\n{output}\nJSON\n")).unwrap();
        let mut options = json!({"stubExtensions":[],"indexVendor":false,"diagnosticsMode":"basic-semantic","analyzerCodeActions":{"enabled":enabled}});
        options[format!("{analyzer}Enabled")] = json!(true);
        options[format!("{analyzer}Command")] =
            json!(format!("sh '{}' {{file}}", script.display()));
        options["phpstanMemoryLimit"] = json!("");
        let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(request) = socket.next().await {
                let _ = tx.send(request);
            }
        });
        send(
            &mut service,
            initialize_request_with_options(1, None, Some(options)),
        )
        .await;
        send(&mut service, did_open_notification(&uri, source)).await;
        send(
            &mut service,
            Request::build("textDocument/didSave")
                .params(json!({"textDocument":{"uri":uri}}))
                .finish(),
        )
        .await;
        let published = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let result = next_publish_diagnostics(&mut rx, &uri, Duration::from_secs(10)).await;
                if result["diagnostics"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|d| d["source"] == analyzer)
                {
                    break result;
                }
            }
        })
        .await
        .unwrap();
        (
            Self {
                root,
                uri,
                source: source.into(),
            },
            service,
            published,
        )
    }
    async fn action(
        &self,
        service: &mut LspService<PhpLspBackend>,
        diagnostic: &serde_json::Value,
        title: &str,
    ) -> serde_json::Value {
        let line = diagnostic["range"]["start"]["line"].as_u64().unwrap() as u32;
        let actions = send(
            service,
            code_action_request(20, &self.uri, line, 0, line, 0, json!([diagnostic])),
        )
        .await;
        actions
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["title"].as_str().is_some_and(|t| t.starts_with(title)))
            .cloned()
            .unwrap_or_else(|| panic!("missing {title}: {actions}\nDiagnostic: {diagnostic}"))
    }
    fn apply(&self, action: &serde_json::Value) -> String {
        let mut edits: Vec<tower_lsp::ls_types::TextEdit> =
            serde_json::from_value(action["edit"]["changes"][&self.uri].clone()).unwrap();
        edits.sort_by_key(|e| std::cmp::Reverse(e.range.start));
        let offset = |p: tower_lsp::ls_types::Position| {
            let line_start = self
                .source
                .split_inclusive('\n')
                .take(p.line as usize)
                .map(str::len)
                .sum::<usize>();
            let line = &self.source[line_start..];
            let mut units = 0;
            for (i, c) in line.char_indices() {
                if units == p.character {
                    return line_start + i;
                }
                units += c.len_utf16() as u32;
            }
            self.source.len()
        };
        let mut source = self.source.clone();
        for edit in edits {
            source.replace_range(
                offset(edit.range.start)..offset(edit.range.end),
                &edit.new_text,
            );
        }
        let mut parser = php_lsp_parser::parser::FileParser::new();
        parser.parse_full(&source);
        assert!(
            !parser.tree().unwrap().root_node().has_error(),
            "invalid edited PHP: {source}"
        );
        source
    }
}

#[tokio::test(flavor = "current_thread")]
async fn qfm_phpstan_pipeline_produces_and_applies_all_fix_families() {
    let source="<?php\r\nnamespace Vendor { class Thing {} function Correct(): void {} }\r\nnamespace App {\r\nfunction work(array $items): void {\r\n    throw new \\RuntimeException();\r\n    new Vendor\\Thing();\r\n    /* 😀 */ \\Vendor\\correct();\r\n}\r\n}\r\n";
    let messages = json!([
        {"line":5,"identifier":"missingType.checkedException","message":"Function App\\work() throws checked exception RuntimeException but it's missing from the PHPDoc @throws tag."},
        {"line":4,"identifier":"missingType.iterableValue","message":"Function App\\work() has parameter $items with no value type specified in iterable type array."},
        {"line":6,"identifier":"class.notFound","message":"Instantiated class App\\Vendor\\Thing not found."},
        {"line":7,"identifier":"function.nameCase","message":"Call to function Vendor\\Correct() with incorrect case: Vendor\\correct"}
    ]);
    let (fixture, mut service, published) = Fixture::new(source, "phpstan", messages, true).await;
    for (code, title, expected) in [
        (
            "missingType.checkedException",
            "Add @throws",
            "@throws \\RuntimeException",
        ),
        (
            "missingType.iterableValue",
            "Add PHPDoc iterable value type",
            "@param array<array-key, mixed> $items",
        ),
        (
            "class.notFound",
            "Replace class name with",
            "new \\Vendor\\Thing();",
        ),
        (
            "function.nameCase",
            "Replace with",
            "/* 😀 */ \\Vendor\\Correct();",
        ),
    ] {
        let diagnostic = published["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["code"] == code)
            .unwrap();
        let action = fixture.action(&mut service, diagnostic, title).await;
        let edited = fixture.apply(&action);
        assert!(edited.contains(expected), "{edited}");
        assert!(edited.contains("/* 😀 */"));
        assert!(
            !edited.replace("\r\n", "").contains('\n'),
            "line endings changed: {edited:?}"
        );
    }
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn qfm_psalm_pipeline_adds_throws_without_synthetic_metadata() {
    let source =
        "<?php\nnamespace App;\nfunction run(): void {\n    throw new \\RuntimeException();\n}\n";
    let messages = json!([{"line_from":4,"column_from":5,"column_to":35,"type":"MissingThrowsDocblock","message":"RuntimeException is thrown but not caught - please either catch or add a @throws annotation"}]);
    let (fixture, mut service, published) = Fixture::new(source, "psalm", messages, true).await;
    let diagnostic = published["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["source"] == "psalm")
        .unwrap();
    let action = fixture
        .action(&mut service, diagnostic, "Add @throws")
        .await;
    assert!(fixture
        .apply(&action)
        .contains("@throws \\RuntimeException"));
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn qfm_inline_doc_insertion_remains_inside_php_and_attached_to_the_callable() {
    let source = "<?php function run(): void { throw new \\RuntimeException(); }";
    let messages = json!([{"line_from":1,"column_from":29,"column_to":58,"type":"MissingThrowsDocblock","message":"RuntimeException is thrown but not caught - please either catch or add a @throws annotation"}]);
    let (fixture, mut service, published) = Fixture::new(source, "psalm", messages, true).await;
    let diagnostic = published["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["source"] == "psalm")
        .unwrap();
    let action = fixture
        .action(&mut service, diagnostic, "Add @throws")
        .await;
    let edited = fixture.apply(&action);
    assert!(
        edited.starts_with("<?php"),
        "PHPDoc became inline HTML: {edited}"
    );
    let mut parser = php_lsp_parser::parser::FileParser::new();
    parser.parse_full(&edited);
    let symbols = php_lsp_parser::symbols::extract_file_symbols(
        parser.tree().unwrap(),
        &edited,
        &fixture.uri,
    );
    assert!(symbols
        .symbols
        .iter()
        .find(|s| s.name == "run")
        .unwrap()
        .doc_comment
        .as_deref()
        .unwrap()
        .contains("@throws \\RuntimeException"));
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn qfm_old_published_metadata_is_rejected_after_edit_reopen_or_tampering() {
    for event in ["edit", "reopen", "tamper"] {
        let source = "<?php\nfunction run(): void {\nthrow new \\RuntimeException();\n}\n";
        let messages = json!([{"line":3,"identifier":"missingType.checkedException","message":"Function run() throws checked exception RuntimeException but it's missing from the PHPDoc @throws tag."}]);
        let (fixture, mut service, published) =
            Fixture::new(source, "phpstan", messages, true).await;
        let mut diagnostic = published["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["source"] == "phpstan")
            .unwrap()
            .clone();
        assert!(diagnostic["data"]["phpLsp"]["snapshot"].is_object());
        match event {
            "edit" => {
                send(
                    &mut service,
                    did_change_full_notification(&fixture.uri, 2, &format!("{source}\n")),
                )
                .await;
            }
            "reopen" => {
                send(&mut service, did_close_notification(&fixture.uri)).await;
                send(&mut service, did_open_notification(&fixture.uri, source)).await;
            }
            "tamper" => {
                diagnostic["data"]["phpLsp"]["analyzerFixes"][0]["exception"] =
                    json!("Oops */ die(); /*")
            }
            _ => unreachable!(),
        }
        let actions = send(
            &mut service,
            code_action_request(30, &fixture.uri, 2, 0, 2, 0, json!([diagnostic])),
        )
        .await;
        assert!(
            actions.as_array().unwrap().iter().all(|a| a["title"]
                .as_str()
                .is_none_or(|title| !title.starts_with("Add @throws"))),
            "{event}: {actions}"
        );
        send(&mut service, shutdown_request(99)).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn qfm_unknown_messages_richer_types_closures_and_ambiguous_names_are_not_guessed() {
    for (source,line,code,message) in [
        ("<?php\nfunction run(array $items):void {}\n",2,"missingType.iterableValue","Unfamiliar message mentioning $items array"),
        ("<?php\n/** @param array<string> $items */\nfunction run(array $items):void {}\n",3,"missingType.iterableValue","Function run() has parameter $items with no value type specified in iterable type array."),
        ("<?php\nfunction run():void {\n$fn=function(){throw new \\RuntimeException();};\n}\n",3,"missingType.checkedException","Function run() throws checked exception RuntimeException but it's missing from the PHPDoc @throws tag."),
        ("<?php\nnamespace V {class Item{}} namespace App {\nfunction run(){new V\\Item; new V\\Item;}\n}\n",3,"class.notFound","Instantiated class App\\V\\Item not found."),
    ] {
        let (fixture,mut service,published)=Fixture::new(source,"phpstan",json!([{"line":line,"identifier":code,"message":message}]),true).await;
        let diagnostic=published["diagnostics"].as_array().unwrap().iter().find(|d|d["source"]=="phpstan").unwrap();
        assert!(diagnostic.get("data").is_none_or(|d|d.is_null()),"unsafe metadata: {diagnostic}");
        send(&mut service,shutdown_request(99)).await;
        drop(fixture);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn qfm_metadata_and_actions_respect_the_disabled_setting() {
    let source = "<?php\nfunction run(array $items):void {}\n";
    let (fixture,mut service,published)=Fixture::new(source,"phpstan",json!([{"line":2,"identifier":"missingType.iterableValue","message":"Function run() has parameter $items with no value type specified in iterable type array."}]),false).await;
    let diagnostic = published["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["source"] == "phpstan")
        .unwrap();
    assert!(diagnostic.get("data").is_none_or(|d| d.is_null()));
    let actions = send(
        &mut service,
        code_action_request(30, &fixture.uri, 1, 0, 1, 0, json!([diagnostic])),
    )
    .await;
    assert!(actions.as_array().unwrap().is_empty(), "{actions}");
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn qfm_new_phpdoc_tags_preserve_complete_multiline_type_blocks() {
    for (source,line,code,message,title) in [
        ("<?php\n/** Keep summary.\n * @return array{\n * item: string\n * }\n */\nfunction run():array {\nthrow new \\RuntimeException();\n}\n",8,"missingType.checkedException","Function run() throws checked exception RuntimeException but it's missing from the PHPDoc @throws tag.","Add @throws"),
        ("<?php\n/** Keep summary.\n * @param array{\n * item: string\n * } $already Keep description.\n */\nfunction run(array $items, array $already):void {}\n",7,"missingType.iterableValue","Function run() has parameter $items with no value type specified in iterable type array.","Add PHPDoc iterable value type"),
    ] {
        let (fixture,mut service,published)=Fixture::new(source,"phpstan",json!([{"line":line,"identifier":code,"message":message}]),true).await;
        let diagnostic=published["diagnostics"].as_array().unwrap().iter().find(|d|d["source"]=="phpstan").unwrap();
        let action=fixture.action(&mut service,diagnostic,title).await;
        let edited=fixture.apply(&action);
        let doc=|text:&str| {let start=text.find("/**").unwrap();let end=text.find("*/").unwrap()+2;php_lsp_parser::phpdoc::parse_phpdoc(&text[start..end])};
        let before=doc(source);let after=doc(&edited);
        assert_eq!(before.return_type,after.return_type,"{edited}");
        for param in before.params {assert!(after.params.contains(&param),"lost original param: {edited}");}
        assert!(edited.contains("Keep summary."));
        send(&mut service,shutdown_request(99)).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn qfm_explicit_mixed_preserves_iterable_keys_and_implicit_nullability() {
    let mut failures = Vec::new();
    for (parameter, reported, expected) in [
        ("iterable $items", "iterable", "iterable<mixed, mixed>"),
        (
            "iterable $items = NULL",
            "iterable",
            "iterable<mixed, mixed>|null",
        ),
        (
            "array $items = null",
            "array",
            "array<array-key, mixed>|null",
        ),
        (
            "array $items = (NULL)",
            "array",
            "array<array-key, mixed>|null",
        ),
        ("?array $items", "array", "array<array-key, mixed>|null"),
    ] {
        let source = format!("<?php\nfunction run({parameter}):void {{}}\n");
        let message=format!("Function run() has parameter $items with no value type specified in iterable type {reported}.");
        let (fixture, mut service, published) = Fixture::new(
            &source,
            "phpstan",
            json!([{"line":2,"identifier":"missingType.iterableValue","message":message}]),
            true,
        )
        .await;
        let diagnostic = published["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["source"] == "phpstan")
            .unwrap();
        let action = fixture
            .action(&mut service, diagnostic, "Add PHPDoc iterable value type")
            .await;
        let edited = fixture.apply(&action);
        if !edited.contains(&format!("@param {expected} $items")) {
            failures.push(edited);
        }
        send(&mut service, shutdown_request(99)).await;
    }
    assert!(
        failures.is_empty(),
        "narrowed native contracts: {}",
        failures.join("\n")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn qfm_existing_multiline_param_is_updated_once_without_losing_its_description() {
    let source="<?php\n/** Keep summary.\n * @param array\n * $items Keep description.\n */\nfunction run(array $items):void {}\n";
    let messages = json!([{"line":6,"identifier":"missingType.iterableValue","message":"Function run() has parameter $items with no value type specified in iterable type array."}]);
    let (fixture, mut service, published) = Fixture::new(source, "phpstan", messages, true).await;
    let diagnostic = published["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["source"] == "phpstan")
        .unwrap();
    let action = fixture
        .action(&mut service, diagnostic, "Add PHPDoc iterable value type")
        .await;
    let edited = fixture.apply(&action);
    assert_eq!(
        edited.matches("@param").count(),
        1,
        "duplicated contract: {edited}"
    );
    assert!(edited.contains("array<array-key, mixed>"));
    assert!(edited.contains("Keep description."));
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn qfm_reference_parameter_tag_is_not_parsed_as_an_intersection() {
    for tag in [
        "@param array &$items description",
        "@param array\n * &$items description",
    ] {
        let source = format!("<?php\n/** {tag}\n */\nfunction run(array &$items):void {{}}\n");
        let line = source
            .lines()
            .position(|line| line.starts_with("function run"))
            .unwrap()
            + 1;
        let messages = json!([{"line":line,"identifier":"missingType.iterableValue","message":"Function run() has parameter $items with no value type specified in iterable type array."}]);
        let (fixture, mut service, published) =
            Fixture::new(&source, "phpstan", messages, true).await;
        let diagnostic = published["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["source"] == "phpstan")
            .unwrap();
        let action = fixture
            .action(&mut service, diagnostic, "Add PHPDoc iterable value type")
            .await;
        let edited = fixture.apply(&action);
        assert_eq!(edited.matches("@param").count(), 1, "{edited}");
        assert!(edited.contains("array<array-key, mixed>"));
        assert!(edited.contains("&$items description"));
        send(&mut service, shutdown_request(99)).await;
    }
}
