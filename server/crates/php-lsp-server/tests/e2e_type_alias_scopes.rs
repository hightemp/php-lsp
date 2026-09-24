mod support;

use support::*;

fn source_position(source: &str, needle: &str, occurrence: usize) -> (u32, u32) {
    let offset = source
        .match_indices(needle)
        .nth(occurrence)
        .map(|(offset, _)| offset + needle.find('$').unwrap_or(0) + 2)
        .expect("hover marker should occur in source");
    let prefix = &source[..offset];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let column = prefix
        .rfind('\n')
        .map_or(prefix.len(), |start| prefix.len() - start - 1);
    (line, column as u32)
}

#[tokio::test(flavor = "current_thread")]
async fn repeated_namespace_sections_keep_file_phpdoc_aliases_separate_in_hover() {
    let source = r#"<?php
namespace App {
    /** @phpstan-type Shared array{first: int} */
    use Vendor\One;
    function first(): void {
        /** @var Shared $row */
        $row = [];
        echo $row;
    }
}

namespace App {
    /** @phpstan-type Shared array{second: string} */
    use Vendor\Two;
    function second(): void {
        /** @var Shared $row */
        $row = [];
        echo $row;
    }
}
"#;
    let uri = "file:///test/AliasSections.php";
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });
    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(
            1,
            None,
            Some(json!({ "stubExtensions": [] })),
        ))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, source))
        .await
        .unwrap();

    for (occurrence, expected, rejected) in [
        (0, "first: int", "second: string"),
        (1, "second: string", "first: int"),
    ] {
        let (line, column) = source_position(source, "echo $row", occurrence);
        let hover = extract_result(
            service
                .ready()
                .await
                .unwrap()
                .call(hover_request(2 + occurrence as i64, uri, line, column))
                .await
                .unwrap(),
        );
        let markdown = hover_markdown_value(&hover);
        assert!(
            markdown.contains(expected),
            "section {occurrence}: {markdown}"
        );
        assert!(
            !markdown.contains(rejected),
            "section {occurrence}: {markdown}"
        );
    }

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn organize_imports_keeps_alias_uses_owned_by_the_same_namespace() {
    let sources = [
        r#"<?php
/** @phpstan-type Local Imported */
namespace App {
use Vendor\Foo as Imported;
}
"#,
        r#"<?php
namespace App {
/** @phpstan-import-type X from Imported as Local */
use Vendor\Foo as Imported;
}
"#,
        r#"<?php
namespace App {
use Vendor\Foo as Imported;
/** @phpstan-type Local Imported */ class Holder {}
}
"#,
    ];
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });
    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request(1))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();

    for (case, source) in sources.into_iter().enumerate() {
        let uri = format!("file:///test/OwnAlias{case}.php");
        service
            .ready()
            .await
            .unwrap()
            .call(did_open_notification(&uri, source))
            .await
            .unwrap();
        let result = extract_result(
            service
                .ready()
                .await
                .unwrap()
                .call(organize_imports_request(10 + case as i64, &uri))
                .await
                .unwrap(),
        );
        let action = result
            .as_array()
            .into_iter()
            .flatten()
            .find(|item| item["title"] == "Organize imports")
            .unwrap_or_else(|| panic!("case {case} missing organize action: {result}"));
        let new_text = action["edit"]["changes"][&uri][0]["newText"]
            .as_str()
            .unwrap_or("");
        assert!(
            new_text.contains("use Vendor\\Foo as Imported;"),
            "case {case}: {action}"
        );
    }
    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn organize_imports_ignores_phpdoc_aliases_owned_by_another_namespace() {
    let source = r#"<?php
namespace First {
use Vendor\Foo as Imported;
}
/** @phpstan-type Local Imported */
namespace Second {
}
"#;
    let uri = "file:///test/OrganizeScopedAliases.php";
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });
    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request(1))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, source))
        .await
        .unwrap();
    let result = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(organize_imports_request(2, uri))
            .await
            .unwrap(),
    );
    let action = result
        .as_array()
        .into_iter()
        .flatten()
        .find(|item| item["title"] == "Organize imports")
        .unwrap_or_else(|| panic!("missing organize action: {result}"));
    assert_eq!(action["edit"]["changes"][uri][0]["newText"], "", "{action}");
    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}
