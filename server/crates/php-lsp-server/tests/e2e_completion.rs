mod support;

use support::*;

#[tokio::test(flavor = "current_thread")]
async fn test_completion() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    // Initialize
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

    // Open file with a class and try completion after "$"
    let code = r#"<?php
$name = "test";
$count = 42;
echo $
"#;
    let uri = "file:///test/completion.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    // Request completion after "$" on line 3
    let resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(2, uri, 3, 6))
        .await
        .unwrap();

    let result = extract_result(resp);
    // Should return completion items (could be an array or CompletionList)
    assert!(
        !result.is_null(),
        "completion should return results for variable context"
    );

    // Shutdown
    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_variable_completion_is_scoped_to_the_cursor_callable() {
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

    let code_with_markers = r#"<?php
function first(string $firstOnly, string ...$firstRest): void {
    $firstLocal = 1;
    $fi/*first*/;
}

function second(int $secondOnly, string $closureOuter): void {
    $secondLocal = 2;
    $se/*second*/;
    $closure = function (bool $closureOnly): void {
        $cl/*closure*/;
    };
}
"#;
    let markers = ["/*first*/", "/*second*/", "/*closure*/"];
    let marker_position = |marker: &str| -> (u32, u32) {
        let marker_offset = code_with_markers.find(marker).expect("completion marker");
        let mut prefix = code_with_markers[..marker_offset].to_string();
        for known_marker in markers {
            prefix = prefix.replace(known_marker, "");
        }
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
        let line_start = prefix.rfind('\n').map(|idx| idx + 1).unwrap_or(0);
        let character = prefix[line_start..].encode_utf16().count() as u32;
        (line, character)
    };
    let mut code = code_with_markers.to_string();
    for marker in markers {
        code = code.replace(marker, "");
    }
    let uri = "file:///test/scoped-variable-completion.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, &code))
        .await
        .unwrap();

    for (request_id, marker, expected, rejected) in [
        (
            2,
            "/*first*/",
            &["$firstOnly", "$firstRest", "$firstLocal"][..],
            &["$secondOnly", "$secondLocal"][..],
        ),
        (
            3,
            "/*second*/",
            &["$secondOnly", "$secondLocal"][..],
            &["$firstOnly", "$firstLocal"][..],
        ),
        (
            4,
            "/*closure*/",
            &["$closureOnly"][..],
            &["$closureOuter"][..],
        ),
    ] {
        let (line, character) = marker_position(marker);
        let response = service
            .ready()
            .await
            .unwrap()
            .call(completion_request(request_id, uri, line, character))
            .await
            .unwrap();
        let result = extract_result(response);
        let labels = completion_items_from_result(&result)
            .into_iter()
            .filter_map(|item| {
                item.get("label")
                    .and_then(|label| label.as_str())
                    .map(str::to_string)
            })
            .collect::<Vec<_>>();

        for label in expected {
            assert!(
                labels.iter().any(|candidate| candidate == label),
                "completion at {marker} should contain {label}, got: {labels:?}"
            );
        }
        for label in rejected {
            assert!(
                !labels.iter().any(|candidate| candidate == label),
                "completion at {marker} must not leak {label}, got: {labels:?}"
            );
        }
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
async fn test_completion_context_uses_utf16_lsp_position_after_non_ascii_text() {
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

    for (idx, (name, text)) in [
        ("cyrillic-emoji", "Привет 😀"),
        ("chinese", "中文测试"),
        ("tibetan", "བོད་ཡིག"),
        ("american-flag", "Ready 🇺🇸"),
        ("zwj-family", "Family 👨‍👩‍👧‍👦"),
        ("skin-tone", "Approve 👍🏽"),
        ("variation-combining", "Heart ❤️ and cafe\u{0301}"),
    ]
    .into_iter()
    .enumerate()
    {
        let code_with_marker = format!(
            "<?php\nclass Target {{ public function complete(): void {{}} }}\nfunction run(Target $target): void {{\n    $label = '{text}'; $target->/*caret*/\n}}\n"
        );
        let marker = "/*caret*/";
        let marker_offset = code_with_marker
            .find(marker)
            .expect("test code should contain marker");
        let code = code_with_marker.replace(marker, "");
        let prefix = &code[..marker_offset];
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
        let line_start = prefix.rfind('\n').map(|idx| idx + 1).unwrap_or(0);
        let character = prefix[line_start..].encode_utf16().count() as u32;
        let uri = format!("file:///test/utf16-completion-context-{name}.php");

        service
            .ready()
            .await
            .unwrap()
            .call(did_open_notification(&uri, &code))
            .await
            .unwrap();

        let resp = service
            .ready()
            .await
            .unwrap()
            .call(completion_request(2 + idx as i64, &uri, line, character))
            .await
            .unwrap();
        let result = extract_result(resp);
        let labels: Vec<String> = completion_items_from_result(&result)
            .iter()
            .filter_map(|item| item.get("label").and_then(|label| label.as_str()))
            .map(str::to_string)
            .collect();
        assert!(
            labels.contains(&"complete".to_string()),
            "completion should use the UTF-16 LSP position after {name} text, got: {labels:?}; result: {result}"
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
async fn test_framework_string_key_completion_and_definition() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-framework-string-keys-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp_root);
    fs::create_dir_all(tmp_root.join("config")).unwrap();
    fs::create_dir_all(tmp_root.join("resources/views/users")).unwrap();
    fs::create_dir_all(tmp_root.join("app")).unwrap();
    fs::write(
        tmp_root.join("config/app.php"),
        "<?php\nreturn ['name' => 'Demo'];\n",
    )
    .unwrap();
    fs::write(
        tmp_root.join("resources/views/users/show.blade.php"),
        "<h1>User</h1>\n",
    )
    .unwrap();

    let code_with_markers = r#"<?php
function run(): void {
    config('app./*config*/');
    view('users.show/*viewdef*/');
}
"#;
    let markers = ["/*config*/", "/*viewdef*/"];
    let marker_position = |marker: &str| -> (u32, u32) {
        let marker_offset = code_with_markers
            .find(marker)
            .expect("test code should contain marker");
        let mut prefix = code_with_markers[..marker_offset].to_string();
        for known_marker in markers {
            prefix = prefix.replace(known_marker, "");
        }
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
        let line_start = prefix.rfind('\n').map(|idx| idx + 1).unwrap_or(0);
        let character = (prefix.len() - line_start) as u32;
        (line, character)
    };
    let (config_line, config_character) = marker_position("/*config*/");
    let (view_line, view_character) = marker_position("/*viewdef*/");
    let mut code = code_with_markers.to_string();
    for marker in markers {
        code = code.replace(marker, "");
    }

    let app_path = tmp_root.join("app/StringKeys.php");
    fs::write(&app_path, &code).unwrap();
    let root_uri = format!("file://{}", tmp_root.to_string_lossy());
    let app_uri = format!("file://{}", app_path.to_string_lossy());
    let view_uri = format!(
        "file://{}",
        tmp_root
            .join("resources/views/users/show.blade.php")
            .to_string_lossy()
    );

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
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&app_uri, &code))
        .await
        .unwrap();

    let completion_resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(
            2,
            &app_uri,
            config_line,
            config_character,
        ))
        .await
        .unwrap();
    let completion_result = extract_result(completion_resp);
    let completion_items = completion_items_from_result(&completion_result);
    let app_name = completion_items
        .iter()
        .find(|item| item.get("label").and_then(|label| label.as_str()) == Some("app.name"))
        .expect("config key completion should include app.name");
    assert_eq!(
        app_name.get("insertText").and_then(|value| value.as_str()),
        Some("name")
    );

    let definition_resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(3, &app_uri, view_line, view_character))
        .await
        .unwrap();
    let definition_result = extract_result(definition_resp);
    assert_eq!(
        definition_result.get("uri").and_then(|uri| uri.as_str()),
        Some(view_uri.as_str()),
        "view key definition should jump to the template file"
    );

    let _ = fs::remove_dir_all(&tmp_root);
    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_completion_static_class_labels_inside_chained_call() {
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

    let code_with_marker = r#"<?php
namespace Symfony\Component\Validator\Constraints;

abstract class Constraint
{
    public const DEFAULT_GROUP = 'Default';
    public const CLASS_CONSTRAINT = 'class';
    public const PROPERTY_CONSTRAINT = 'property';

    public static function getErrorName(string $errorCode): string
    {
        return $errorCode;
    }

    public function validatedBy(): string
    {
        return static::class.'Validator';
    }
}

class Blank extends Constraint
{
    public const NOT_BLANK_ERROR = '183ad2de-533d-4796-a439-6d3c3852b549';
    public string $message = 'This value should be blank.';
}

class ViolationBuilder
{
    public function setCode(string $code): self
    {
        return $this;
    }
}

class Context
{
    public function buildViolation(string $message): ViolationBuilder
    {
        return new ViolationBuilder();
    }
}

class BlankValidator
{
    private Context $context;

    public function validate(Constraint $constraint): void
    {
        $this->context
            ->buildViolation($constraint->message)
            ->setCode(Blank::/*caret*/);
    }
}
"#;
    let marker = "/*caret*/";
    let offset = code_with_marker
        .find(marker)
        .expect("test code should contain caret marker");
    let code = code_with_marker.replace(marker, "");
    let prefix = &code[..offset];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let line_start = prefix.rfind('\n').map(|idx| idx + 1).unwrap_or(0);
    let character = (prefix.len() - line_start) as u32;
    let uri = "file:///test/blank-validator-completion.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, &code))
        .await
        .unwrap();

    let resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(2, uri, line, character))
        .await
        .unwrap();
    let result = extract_result(resp);
    let items = completion_items_from_result(&result);
    let labels: Vec<&str> = items
        .iter()
        .filter_map(|item| item.get("label").and_then(|label| label.as_str()))
        .collect();

    for expected in [
        "class",
        "NOT_BLANK_ERROR",
        "DEFAULT_GROUP",
        "CLASS_CONSTRAINT",
        "PROPERTY_CONSTRAINT",
        "getErrorName",
    ] {
        assert!(
            labels.contains(&expected),
            "expected static completion to include `{expected}`, got: {labels:?}"
        );
    }
    assert!(
        !labels.contains(&"validatedBy"),
        "instance method should stay hidden for ClassName:: completion"
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_shape_aware_completion_and_definition() {
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

    let code_with_markers = r#"<?php
namespace App;

class User {
    public function name(): string { return ''; }
}

function run(): void {
    /** @var array{foo: User, bar?: int, 'quoted-key': string, '中文键': string, 'བོད': string, meta: array{city: string}} $row */
    $row = [];
    /** @var list<User> $users */
    $users = [];
    /** @var object{title: string, owner?: User} $shape */
    $shape = (object)[];

    $row['/*array*/'];
    $row[/*bracket*/];
    $row['meta']['/*nested*/'];
    $users['/*list*/'];
    $shape->/*object*/;
    $row['foo/*phpdocdef*/']->name();
    $literal = ['literal' => 1, 'nested' => ['leaf' => true]];
    $literal['/*literal*/'];
    $literal['nested']['leaf/*literaldef*/'];
    $row['/*quoted*/'];
    $row['quoted-key/*quoteddef*/'];
    $row['中/*unicode*/'];
    $row['བོད/*tibetan*/'];
    /** @var RowAlias $aliasRow */
    $aliasRow = [];
    $aliasRow['/*alias*/'];
}

/**
 * @phpstan-type RowAlias array{
 *   'alias-key': User,
 *   nested: array{
 *     leaf: string,
 *   },
 * }
 */
"#;
    let markers = [
        "/*array*/",
        "/*bracket*/",
        "/*nested*/",
        "/*list*/",
        "/*object*/",
        "/*phpdocdef*/",
        "/*literal*/",
        "/*literaldef*/",
        "/*quoted*/",
        "/*quoteddef*/",
        "/*unicode*/",
        "/*tibetan*/",
        "/*alias*/",
    ];
    let marker_position = |marker: &str| -> (u32, u32) {
        let marker_offset = code_with_markers
            .find(marker)
            .expect("test code should contain marker");
        let mut prefix = code_with_markers[..marker_offset].to_string();
        for known_marker in markers {
            prefix = prefix.replace(known_marker, "");
        }
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
        let line_start = prefix.rfind('\n').map(|idx| idx + 1).unwrap_or(0);
        let character = prefix[line_start..].encode_utf16().count() as u32;
        (line, character)
    };
    let mut code = code_with_markers.to_string();
    for marker in markers {
        code = code.replace(marker, "");
    }
    let uri = "file:///test/shape-aware-completion.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, &code))
        .await
        .unwrap();

    let (array_line, array_character) = marker_position("/*array*/");
    let array_completion = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(2, uri, array_line, array_character))
        .await
        .unwrap();
    let array_result = extract_result(array_completion);
    let array_labels: Vec<String> = completion_items_from_result(&array_result)
        .iter()
        .filter_map(|item| item.get("label").and_then(|label| label.as_str()))
        .map(str::to_string)
        .collect();
    assert!(
        array_labels.contains(&"foo".to_string()) && array_labels.contains(&"bar".to_string()),
        "array shape completion should include foo/bar, got: {:?}; result: {}",
        array_labels,
        array_result
    );

    let (bracket_line, bracket_character) = marker_position("/*bracket*/");
    let bracket_completion = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(21, uri, bracket_line, bracket_character))
        .await
        .unwrap();
    let bracket_result = extract_result(bracket_completion);
    let bracket_items = completion_items_from_result(&bracket_result);
    let bracket_foo = bracket_items
        .iter()
        .find(|item| item.get("label").and_then(|label| label.as_str()) == Some("foo"))
        .unwrap_or_else(|| panic!("expected foo after open bracket, got: {bracket_items:?}"));
    assert_eq!(
        bracket_foo
            .get("insertText")
            .and_then(|value| value.as_str()),
        Some("'foo'"),
        "completion after '[' should insert a quoted array-shape key"
    );

    let (nested_line, nested_character) = marker_position("/*nested*/");
    let nested_completion = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(3, uri, nested_line, nested_character))
        .await
        .unwrap();
    let nested_result = extract_result(nested_completion);
    let nested_labels: Vec<String> = completion_items_from_result(&nested_result)
        .iter()
        .filter_map(|item| item.get("label").and_then(|label| label.as_str()))
        .map(str::to_string)
        .collect();
    assert!(
        nested_labels.contains(&"city".to_string()),
        "nested array shape completion should include city, got: {:?}; result: {}",
        nested_labels,
        nested_result
    );

    let (list_line, list_character) = marker_position("/*list*/");
    let list_completion = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(4, uri, list_line, list_character))
        .await
        .unwrap();
    let list_result = extract_result(list_completion);
    let list_labels: Vec<String> = completion_items_from_result(&list_result)
        .iter()
        .filter_map(|item| item.get("label").and_then(|label| label.as_str()))
        .map(str::to_string)
        .collect();
    assert!(
        list_labels.is_empty(),
        "list<T> should not produce shape key completion, got: {:?}; result: {}",
        list_labels,
        list_result
    );

    let (object_line, object_character) = marker_position("/*object*/");
    let object_completion = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(5, uri, object_line, object_character))
        .await
        .unwrap();
    let object_result = extract_result(object_completion);
    let object_labels: Vec<String> = completion_items_from_result(&object_result)
        .iter()
        .filter_map(|item| item.get("label").and_then(|label| label.as_str()))
        .map(str::to_string)
        .collect();
    assert!(
        object_labels.contains(&"title".to_string())
            && object_labels.contains(&"owner".to_string()),
        "object shape completion should include title/owner, got: {:?}; result: {}",
        object_labels,
        object_result
    );

    let (literal_line, literal_character) = marker_position("/*literal*/");
    let literal_completion = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(6, uri, literal_line, literal_character))
        .await
        .unwrap();
    let literal_result = extract_result(literal_completion);
    let literal_labels: Vec<String> = completion_items_from_result(&literal_result)
        .iter()
        .filter_map(|item| item.get("label").and_then(|label| label.as_str()))
        .map(str::to_string)
        .collect();
    assert!(
        literal_labels.contains(&"literal".to_string())
            && literal_labels.contains(&"nested".to_string()),
        "literal array shape completion should include literal/nested, got: {:?}; result: {}",
        literal_labels,
        literal_result
    );

    let (phpdoc_def_line, phpdoc_def_character) = marker_position("/*phpdocdef*/");
    let phpdoc_definition = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(
            7,
            uri,
            phpdoc_def_line,
            phpdoc_def_character,
        ))
        .await
        .unwrap();
    let phpdoc_definition_result = extract_result(phpdoc_definition);
    assert_eq!(
        phpdoc_definition_result["range"]["start"]["line"].as_u64(),
        Some(8),
        "PHPDoc shape key definition should point to @var shape, got: {}",
        phpdoc_definition_result
    );

    let (literal_def_line, literal_def_character) = marker_position("/*literaldef*/");
    let literal_definition = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(
            8,
            uri,
            literal_def_line,
            literal_def_character,
        ))
        .await
        .unwrap();
    let literal_definition_result = extract_result(literal_definition);
    assert_eq!(
        literal_definition_result["range"]["start"]["line"].as_u64(),
        Some(21),
        "literal shape key definition should point to array key declaration, got: {}",
        literal_definition_result
    );

    let (quoted_line, quoted_character) = marker_position("/*quoted*/");
    let quoted_completion = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(9, uri, quoted_line, quoted_character))
        .await
        .unwrap();
    let quoted_result = extract_result(quoted_completion);
    let quoted_items = completion_items_from_result(&quoted_result);
    let quoted_item = quoted_items
        .iter()
        .find(|item| item.get("label").and_then(|label| label.as_str()) == Some("quoted-key"))
        .unwrap_or_else(|| {
            panic!("expected quoted-key completion inside quotes, got: {quoted_items:?}")
        });
    assert_eq!(
        quoted_item
            .get("insertText")
            .and_then(|value| value.as_str()),
        Some("quoted-key"),
        "completion inside existing quotes should not duplicate quotes"
    );

    let (unicode_line, unicode_character) = marker_position("/*unicode*/");
    let unicode_completion = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(12, uri, unicode_line, unicode_character))
        .await
        .unwrap();
    let unicode_result = extract_result(unicode_completion);
    let unicode_labels: Vec<String> = completion_items_from_result(&unicode_result)
        .iter()
        .filter_map(|item| item.get("label").and_then(|label| label.as_str()))
        .map(str::to_string)
        .collect();
    assert!(
        unicode_labels.contains(&"中文键".to_string()),
        "quoted array-shape completion should handle Chinese key prefixes, got: {:?}; result: {}",
        unicode_labels,
        unicode_result
    );

    let (tibetan_line, tibetan_character) = marker_position("/*tibetan*/");
    let tibetan_completion = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(13, uri, tibetan_line, tibetan_character))
        .await
        .unwrap();
    let tibetan_result = extract_result(tibetan_completion);
    let tibetan_labels: Vec<String> = completion_items_from_result(&tibetan_result)
        .iter()
        .filter_map(|item| item.get("label").and_then(|label| label.as_str()))
        .map(str::to_string)
        .collect();
    assert!(
        tibetan_labels.contains(&"བོད".to_string()),
        "quoted array-shape completion should handle Tibetan key prefixes, got: {:?}; result: {}",
        tibetan_labels,
        tibetan_result
    );

    let (quoted_def_line, quoted_def_character) = marker_position("/*quoteddef*/");
    let quoted_definition = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(
            10,
            uri,
            quoted_def_line,
            quoted_def_character,
        ))
        .await
        .unwrap();
    let quoted_definition_result = extract_result(quoted_definition);
    assert_eq!(
        quoted_definition_result["range"]["start"]["line"].as_u64(),
        Some(8),
        "quoted PHPDoc shape key definition should point to the quoted key, got: {}",
        quoted_definition_result
    );

    let (alias_line, alias_character) = marker_position("/*alias*/");
    let alias_completion = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(14, uri, alias_line, alias_character))
        .await
        .unwrap();
    let alias_result = extract_result(alias_completion);
    let alias_labels: Vec<String> = completion_items_from_result(&alias_result)
        .iter()
        .filter_map(|item| item.get("label").and_then(|label| label.as_str()))
        .map(str::to_string)
        .collect();
    assert!(
        alias_labels.contains(&"alias-key".to_string())
            && alias_labels.contains(&"nested".to_string()),
        "multi-line file-level shape alias should expand for completion, got: {:?}; result: {}",
        alias_labels,
        alias_result
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_completion_member_access_from_inline_phpdoc_var() {
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

    let code = r#"<?php
namespace App;

class Baz {
    public function test(): void {}
}

function makeBaz() {}

function run(): void {
    /** @var Baz $baz2 */
    $baz2 = makeBaz();
    $baz2->
}
"#;
    let uri = "file:///test/phpdoc-completion.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    // Completion at the end of "$baz2->"
    let resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(2, uri, 12, 11))
        .await
        .unwrap();

    let result = extract_result(resp);
    assert!(
        !result.is_null(),
        "completion should return member items from inline @var type"
    );

    let labels: Vec<String> = if let Some(arr) = result.as_array() {
        arr.iter()
            .filter_map(|item| item.get("label").and_then(|v| v.as_str()))
            .map(|s| s.to_string())
            .collect()
    } else {
        result
            .get("items")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("label").and_then(|v| v.as_str()))
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default()
    };

    assert!(
        labels.iter().any(|label| label == "test"),
        "expected member completion to include `test`, got: {:?}",
        labels
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_completion_foreach_collection_value_after_did_change_incomplete_member_access() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
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

    let valid_code = r#"<?php
namespace Doctrine\Common\Collections {
    interface Collection {}
}

namespace App\Entity {
    use Doctrine\Common\Collections\Collection;

    class ReversePortingNumber {
        public function getPhoneNumber(): string { return ''; }
        public function setCurrentNumberStatus(NumberStatus $status): static { return $status instanceof NumberStatus ? $this : $this; }
    }

    class NumberStatus {}

    class ReverseRequest {
        /**
         * @return Collection<int, ReversePortingNumber>
         */
        public function getReversePortingNumbers(): Collection {}
    }
}

namespace App\Soap\Inbound\Handler {
    use App\Entity\NumberStatus;
    use App\Entity\ReverseRequest;

    final class CompleteHandler {
        public function updateReverseRequestForComplete(ReverseRequest $reverseRequest, NumberStatus $numberStatus): void {
            foreach ($reverseRequest->getReversePortingNumbers() as $portingNumber) {
                $portingNumber->setCurrentNumberStatus($numberStatus);
            }
        }
    }
}
"#;
    let changed_code = valid_code.replace(
        "                $portingNumber->setCurrentNumberStatus($numberStatus);",
        "                $portingNumber->",
    );
    let uri = "file:///test/foreach-collection-didchange-completion.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, valid_code))
        .await
        .unwrap();
    let initial_diagnostics =
        next_publish_diagnostics(&mut notifications, uri, Duration::from_secs(2)).await;
    assert_eq!(
        initial_diagnostics
            .get("diagnostics")
            .and_then(|diagnostics| diagnostics.as_array())
            .map(Vec::len),
        Some(0),
        "valid fixture should not publish diagnostics: {initial_diagnostics}"
    );

    service
        .ready()
        .await
        .unwrap()
        .call(did_change_full_notification(uri, 2, &changed_code))
        .await
        .unwrap();

    let completion_position = utf16_position_after(&changed_code, "$portingNumber->");
    let resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(
            2,
            uri,
            completion_position.0,
            completion_position.1,
        ))
        .await
        .unwrap();
    let result = extract_result(resp);
    let labels: Vec<String> = completion_items_from_result(&result)
        .iter()
        .filter_map(|item| item.get("label").and_then(|label| label.as_str()))
        .map(str::to_string)
        .collect();

    assert!(
        labels.iter().any(|label| label == "getPhoneNumber"),
        "expected foreach value completion to include entity methods after didChange, got: {labels:?}; result: {result}"
    );
    assert!(
        labels
            .iter()
            .any(|label| label == "setCurrentNumberStatus"),
        "expected foreach value completion to include all entity methods after didChange, got: {labels:?}; result: {result}"
    );

    let changed_diagnostics =
        next_publish_diagnostics(&mut notifications, uri, Duration::from_secs(2)).await;
    let diagnostic_messages = published_diagnostic_messages(&changed_diagnostics);
    assert!(
        diagnostic_messages
            .iter()
            .any(|message| message.contains("Syntax error") || message.contains("Missing")),
        "dangling member access should still be reported as tree-sitter syntax diagnostics, got: {diagnostic_messages:?}"
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_completion_foreach_collection_value_from_indexed_qualified_phpdoc_return() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-indexed-qualified-collection-completion-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp_root);
    fs::create_dir_all(tmp_root.join("src/Controller")).unwrap();
    fs::create_dir_all(tmp_root.join("src/Entity")).unwrap();
    fs::create_dir_all(tmp_root.join("src/Repository")).unwrap();
    fs::create_dir_all(tmp_root.join("src/Support")).unwrap();

    let file_uri = |path: &std::path::Path| php_lsp_types::uri::path_to_uri(path).unwrap();
    let root_uri = file_uri(&tmp_root);
    let collection_path = tmp_root.join("src/Support/Collection.php");
    let entity_path = tmp_root.join("src/Entity/User.php");
    let repository_path = tmp_root.join("src/Repository/UserRepository.php");
    let controller_path = tmp_root.join("src/Controller/UserController.php");
    let controller_uri = file_uri(&controller_path);

    fs::write(
        &collection_path,
        r#"<?php
namespace App\Support;

interface Collection {}
"#,
    )
    .unwrap();
    fs::write(
        &entity_path,
        r#"<?php
namespace App\Entity;

final class User
{
    public function getName(): string { return ''; }
    public function getEmail(): string { return ''; }
}
"#,
    )
    .unwrap();
    fs::write(
        &repository_path,
        r#"<?php
namespace App;

use App\Entity as Model;
use App\Support\Collection;

final class UserRepository
{
    /** @return Collection<int, Entity\User> */
    public function namespaceUsers(): Collection {}

    /** @return Collection<int, Model\User> */
    public function aliasUsers(): Collection {}
}
"#,
    )
    .unwrap();

    let marker_absolute = "/*absolute*/";
    let marker_alias = "/*alias*/";
    let controller_with_markers = format!(
        r#"<?php
namespace App\Controller;

use App\UserRepository;

final class UserController
{{
    public function show(UserRepository $repository): void
    {{
        foreach ($repository->namespaceUsers() as $namespaceUser) {{
            $namespaceUser->{marker_absolute}getName();
        }}
        foreach ($repository->aliasUsers() as $aliasUser) {{
            $aliasUser->{marker_alias}getName();
        }}
    }}
}}
"#
    );
    let controller_code = controller_with_markers
        .replace(marker_absolute, "")
        .replace(marker_alias, "");
    let absolute_position = utf16_position_after(&controller_code, "$namespaceUser->");
    let alias_position = utf16_position_after(&controller_code, "$aliasUser->");
    fs::write(&controller_path, &controller_code).unwrap();

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
    wait_for_indexing_phase(&mut notifications, "ready", Duration::from_secs(5)).await;

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&controller_uri, &controller_code))
        .await
        .unwrap();

    for (request_id, position, label) in [
        (2, absolute_position, "namespace-relative generic return"),
        (3, alias_position, "alias-qualified generic return"),
    ] {
        let response = service
            .ready()
            .await
            .unwrap()
            .call(completion_request(
                request_id,
                &controller_uri,
                position.0,
                position.1,
            ))
            .await
            .unwrap();
        let result = extract_result(response);
        let labels: Vec<String> = completion_items_from_result(&result)
            .iter()
            .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
            .map(str::to_string)
            .collect();
        assert!(
            labels.iter().any(|candidate| candidate == "getName")
                && labels.iter().any(|candidate| candidate == "getEmail"),
            "expected completion from indexed {label} to resolve User methods, got: {labels:?}; result: {result}"
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
async fn test_completion_member_access_from_this_property_chain() {
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

    let code = r#"<?php
namespace App;

class Browser {
    public string $requestHeaders;
    public function request(): void {}
}

class Controller {
    private Browser $client;
    public function test(): void {
        $this->client->reques
    }
}
"#;
    let uri = "file:///test/property-chain-completion.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(2, uri, 11, 29))
        .await
        .unwrap();

    let result = extract_result(resp);
    let items = completion_items_from_result(&result);
    let labels: Vec<_> = items
        .iter()
        .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
        .collect();

    assert_eq!(
        labels.first().copied(),
        Some("request"),
        "expected method completion to sort first, got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"requestHeaders"),
        "expected property completion from chained type, got: {:?}",
        labels
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_completion_member_access_from_scoped_static_call_chain() {
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

    let code_with_markers = r#"<?php
namespace App;

class SelfProduct {
    public function selfOnly(): void {}
}

class StaticProduct {
    public function staticOnly(): void {}
}

class ParentProduct {
    public function parentOnly(): void {}
}

class BaseFactory {
    public static function makeParent(): ParentProduct { return new ParentProduct(); }
}

class ChildFactory extends BaseFactory {
    public static function makeSelf(): SelfProduct { return new SelfProduct(); }
    public static function makeNullableSelf(): ?SelfProduct { return new SelfProduct(); }
    public static function makeStatic(): StaticProduct { return new StaticProduct(); }

    public function run(): void {
        self::makeSelf()->/*self*/;
        self::makeNullableSelf()->/*nullable*/;
        static::makeStatic()->/*static*/;
        parent::makeParent()->/*parent*/;
    }
}
"#;
    let markers = ["/*self*/", "/*nullable*/", "/*static*/", "/*parent*/"];
    let marker_position = |marker: &str| -> (u32, u32) {
        let marker_offset = code_with_markers
            .find(marker)
            .expect("test code should contain marker");
        let mut prefix = code_with_markers[..marker_offset].to_string();
        for known_marker in markers {
            prefix = prefix.replace(known_marker, "");
        }
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
        let line_start = prefix.rfind('\n').map(|idx| idx + 1).unwrap_or(0);
        let character = prefix[line_start..].encode_utf16().count() as u32;
        (line, character)
    };
    let mut code = code_with_markers.to_string();
    for marker in markers {
        code = code.replace(marker, "");
    }
    let uri = "file:///test/scoped-static-call-chain-completion.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, &code))
        .await
        .unwrap();

    for (request_id, marker, expected_label) in [
        (2, "/*self*/", "selfOnly"),
        (3, "/*nullable*/", "selfOnly"),
        (4, "/*static*/", "staticOnly"),
        (5, "/*parent*/", "parentOnly"),
    ] {
        let (line, character) = marker_position(marker);
        let resp = service
            .ready()
            .await
            .unwrap()
            .call(completion_request(request_id, uri, line, character))
            .await
            .unwrap();
        let result = extract_result(resp);
        let labels: Vec<String> = completion_items_from_result(&result)
            .iter()
            .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
            .map(str::to_string)
            .collect();

        assert!(
            labels.iter().any(|label| label == expected_label),
            "expected scoped static-call chain completion to include {expected_label}, got: {:?}",
            labels
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
async fn test_completion_and_definition_nullable_variable_from_method_return_assignment() {
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

    let code = r#"<?php
namespace App;

class Request {
    public function hasSession(): bool { return true; }
    public function getSession(): Session { return new Session(); }
}

class Session {
    public function get(string $key): string { return ''; }
    public function all(): array { return []; }
}

class Controller {
    public function search(Request $request): void {
        $session = null;
        if ($request->hasSession()) {
            $session = $request->getSession();
        }

        $session?->get('token');
    }
}
"#;
    let uri = "file:///test/nullable-method-return-completion.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let completion = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(2, uri, 20, 22))
        .await
        .unwrap();
    let completion_result = extract_result(completion);
    let labels: Vec<String> = completion_items_from_result(&completion_result)
        .iter()
        .filter_map(|item| {
            item.get("label")
                .and_then(|label| label.as_str())
                .map(str::to_string)
        })
        .collect();
    assert!(
        labels.iter().any(|label| label == "get"),
        "expected nullable variable completion to include get, got: {:?}",
        labels
    );

    let definition = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(3, uri, 20, 19))
        .await
        .unwrap();
    let definition_result = extract_result(definition);
    assert_eq!(
        definition_result
            .get("uri")
            .and_then(|value| value.as_str()),
        Some(uri),
        "definition should point to same test file, got: {}",
        definition_result
    );
    assert_eq!(
        definition_result["range"]["start"]["line"].as_u64(),
        Some(9),
        "definition should point to Session::get, got: {}",
        definition_result
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_completion_member_access_from_nested_fully_qualified_new_stub_type() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-reflection-completion-{}",
        std::process::id()
    ));
    fs::create_dir_all(&tmp_root).unwrap();
    let root_uri = format!("file://{}", tmp_root.to_string_lossy());
    let stubs_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../data/stubs")
        .canonicalize()
        .unwrap();

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(
            1,
            Some(&root_uri),
            Some(json!({
                "stubsPath": stubs_path.to_string_lossy().to_string()
            })),
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
    wait_for_indexing_phase(&mut notifications, "stubsLoaded", Duration::from_secs(5)).await;

    let code_with_marker = r#"<?php
namespace App;

function validate(object $object, mixed $method): void
{
    if ($method instanceof \Closure) {
        $method($object);
    } elseif (\is_array($method)) {
        $method($object);
    } elseif (null !== $object) {
        if (!method_exists($object, $method)) {
            throw new \RuntimeException();
        }

        $reflMethod = new \ReflectionMethod($object, $method);

        if ($reflMethod->isStatic()) {
        }

        $required = (new \ReflectionClass($object))->getConstructor()?->getNumber/*caret*/;
    }
}
"#;
    let marker = "/*caret*/";
    let marker_offset = code_with_marker
        .find(marker)
        .expect("test code should contain caret marker");
    let code = code_with_marker.replace(marker, "");
    let marker_prefix = &code[..marker_offset];
    let marker_line = marker_prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let marker_line_start = marker_prefix.rfind('\n').map(|idx| idx + 1).unwrap_or(0);
    let marker_character = (marker_prefix.len() - marker_line_start) as u32;
    let uri = "file:///test/ReflectionCompletion.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, &code))
        .await
        .unwrap();

    let resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(2, uri, 16, 29))
        .await
        .unwrap();
    let result = extract_result(resp);
    let labels: Vec<String> = completion_items_from_result(&result)
        .iter()
        .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect();

    assert!(
        labels.iter().any(|label| label == "isStatic"),
        "expected ReflectionMethod completion to include isStatic, got: {:?}",
        labels
    );
    assert!(
        labels.iter().any(|label| label == "invoke"),
        "expected ReflectionMethod completion to include invoke, got: {:?}",
        labels
    );

    let resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(3, uri, marker_line, marker_character))
        .await
        .unwrap();
    let result = extract_result(resp);
    let labels: Vec<String> = completion_items_from_result(&result)
        .iter()
        .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect();

    assert!(
        labels
            .iter()
            .any(|label| label == "getNumberOfRequiredParameters"),
        "expected nullable new-expression chain completion to include getNumberOfRequiredParameters, got: {:?}",
        labels
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_completion_static_stub_class_lists_constants_first() {
    let stubs_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/stubs");
    if !stubs_path.join("zip/zip.php").is_file() {
        eprintln!(
            "Skipping ZipArchive static completion test: zip stubs not initialized at {}",
            stubs_path.display()
        );
        return;
    }
    let stubs_path = stubs_path.canonicalize().unwrap();

    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-zip-static-completion-{}",
        std::process::id()
    ));
    fs::create_dir_all(&tmp_root).unwrap();
    let root_uri = php_lsp_types::uri::path_to_uri(&tmp_root).unwrap();

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(
            1,
            Some(&root_uri),
            Some(json!({
                "stubsPath": stubs_path.to_string_lossy().to_string()
            })),
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
    wait_for_indexing_phase(&mut notifications, "stubsLoaded", Duration::from_secs(10)).await;

    let code = "<?php\nfunction writeZip(): void\n{\n    \\ZipArchive::\n}\n";
    let uri =
        php_lsp_types::uri::path_to_uri(&tmp_root.join("ZipArchiveStaticCompletion.php")).unwrap();
    let (line, character) = utf16_position_after(code, "\\ZipArchive::");
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&uri, code))
        .await
        .unwrap();

    let resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(2, &uri, line, character))
        .await
        .unwrap();
    let result = extract_result(resp);
    let labels: Vec<String> = completion_items_from_result(&result)
        .iter()
        .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect();

    let create_pos = labels
        .iter()
        .position(|label| label == "CREATE")
        .unwrap_or_else(|| panic!("expected ZipArchive::CREATE completion, got: {labels:?}"));
    let overwrite_pos = labels
        .iter()
        .position(|label| label == "OVERWRITE")
        .unwrap_or_else(|| panic!("expected ZipArchive::OVERWRITE completion, got: {labels:?}"));
    let class_pos = labels
        .iter()
        .position(|label| label == "class")
        .unwrap_or_else(|| panic!("expected ZipArchive::class completion, got: {labels:?}"));

    assert!(
        create_pos < class_pos && overwrite_pos < class_pos,
        "ZipArchive class constants should sort before ::class, got: {labels:?}"
    );
    assert!(
        !labels
            .iter()
            .any(|label| label == "open" || label == "close"),
        "instance methods should not appear in ZipArchive static completion, got: {labels:?}"
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
    let _ = fs::remove_dir_all(tmp_root);
}

#[tokio::test(flavor = "current_thread")]
async fn test_completion_member_access_from_parenthesized_new_expression() {
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

    let code_with_marker = r#"<?php
namespace App;

class Uri
{
    public function __construct(private mixed $client) {}
    public function setHost(string $host): self { return $this; }
    public function setPort(int $port): self { return $this; }
}

class UriFactory
{
    public function __construct(private mixed $client) {}

    public function create(): void
    {
        (new Uri($this->client))->set/*caret*/;
    }
}
"#;
    let marker = "/*caret*/";
    let marker_offset = code_with_marker
        .find(marker)
        .expect("test code should contain caret marker");
    let code = code_with_marker.replace(marker, "");
    let marker_prefix = &code[..marker_offset];
    let marker_line = marker_prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let marker_line_start = marker_prefix.rfind('\n').map(|idx| idx + 1).unwrap_or(0);
    let marker_character = (marker_prefix.len() - marker_line_start) as u32;
    let uri = "file:///test/NewExpressionCompletion.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, &code))
        .await
        .unwrap();

    let resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(2, uri, marker_line, marker_character))
        .await
        .unwrap();
    let result = extract_result(resp);
    let labels: Vec<String> = completion_items_from_result(&result)
        .iter()
        .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect();

    assert!(
        labels.iter().any(|label| label == "setHost"),
        "expected new-expression completion to include setHost, got: {:?}",
        labels
    );
    assert!(
        labels.iter().any(|label| label == "setPort"),
        "expected new-expression completion to include setPort, got: {:?}",
        labels
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_completion_polish_snippets_and_auto_imports() {
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

    let vendor_uri = "file:///test/VendorService.php";
    let vendor_code = r#"<?php
namespace Vendor;

class Service {}
"#;
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(vendor_uri, vendor_code))
        .await
        .unwrap();

    let app_uri = "file:///test/CompletionPolish.php";
    let app_code = r#"<?php
namespace App;

class Demo {
    public function run(): void {
        Ser
    }
}
"#;
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(app_uri, app_code))
        .await
        .unwrap();

    let auto_import_resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(2, app_uri, 5, 11))
        .await
        .unwrap();
    let auto_import_result = extract_result(auto_import_resp);
    let auto_import_items = completion_items_from_result(&auto_import_result);
    let service_item = auto_import_items
        .iter()
        .find(|item| item.get("label").and_then(|value| value.as_str()) == Some("Service"))
        .unwrap_or_else(|| panic!("expected Service completion, got: {auto_import_items:?}"));
    assert!(
        service_item.get("sortText").is_some(),
        "completion item should include stable sortText"
    );
    assert!(
        service_item.get("filterText").is_some(),
        "completion item should include filterText"
    );
    let edits = service_item
        .get("additionalTextEdits")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        edits.len(),
        1,
        "Service completion should add one import edit"
    );
    assert_eq!(
        edits[0].get("newText").and_then(|value| value.as_str()),
        Some("use Vendor\\Service;\n"),
        "auto-import edit should insert the selected class import"
    );
    assert_eq!(
        edits[0]["range"]["start"]["line"].as_u64(),
        Some(2),
        "auto-import should be inserted after namespace declaration"
    );

    let use_uri = "file:///test/UseCompletion.php";
    let use_code = "<?php\nnamespace App;\nuse Ven;\nclass Demo {}\n";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(use_uri, use_code))
        .await
        .unwrap();
    let use_resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(21, use_uri, 2, 7))
        .await
        .unwrap();
    let use_result = extract_result(use_resp);
    let use_items = completion_items_from_result(&use_result);
    let use_service_item = use_items
        .iter()
        .find(|item| item.get("label").and_then(|value| value.as_str()) == Some("Service"))
        .unwrap_or_else(|| panic!("expected Service use completion, got: {use_items:?}"));
    assert_eq!(
        use_service_item
            .get("insertText")
            .and_then(|value| value.as_str()),
        Some("Vendor\\Service"),
        "use statement completion should insert the full FQN"
    );

    let snippet_uri = "file:///test/CompletionSnippet.php";
    let snippet_code = "<?php\ncla";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(snippet_uri, snippet_code))
        .await
        .unwrap();
    let snippet_resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(3, snippet_uri, 1, 3))
        .await
        .unwrap();
    let snippet_result = extract_result(snippet_resp);
    let snippet_items = completion_items_from_result(&snippet_result);
    let class_item = snippet_items
        .iter()
        .find(|item| item.get("label").and_then(|value| value.as_str()) == Some("class"))
        .unwrap_or_else(|| panic!("expected class snippet completion, got: {snippet_items:?}"));
    assert_eq!(
        class_item.get("kind").and_then(|value| value.as_u64()),
        Some(15),
        "class completion should be a snippet item"
    );
    assert_eq!(
        class_item
            .get("insertTextFormat")
            .and_then(|value| value.as_u64()),
        Some(2),
        "class completion should use snippet insert text format"
    );
    assert!(
        class_item
            .get("insertText")
            .and_then(|value| value.as_str())
            .is_some_and(|text| text.contains("${1:Name}")),
        "class snippet should include a name placeholder"
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_completion_auto_imports_use_cursor_namespace_scope() {
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

    let vendor_uri = "file:///test/CursorScopedVendorTypes.php";
    let vendor_code = r#"<?php
namespace Vendor;

class Service {}
class Client {}
"#;
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(vendor_uri, vendor_code))
        .await
        .unwrap();

    let unbracketed_uri = "file:///test/CursorScopedUnbracketed.php";
    let unbracketed_code = r#"<?php
namespace First;
use Other\Service as Service;

function localFirst(): void {}

class FirstConsumer
{
    public function run(): void
    {
        localF
    }
}

namespace Second;

class SecondConsumer
{
    public function run(): void
    {
        Ser
    }
}
"#;
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(unbracketed_uri, unbracketed_code))
        .await
        .unwrap();

    let (local_line, local_character) = utf16_position_after(unbracketed_code, "        localF");
    let local_response = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(
            2,
            unbracketed_uri,
            local_line,
            local_character,
        ))
        .await
        .unwrap();
    let local_result = extract_result(local_response);
    let local_items = completion_items_from_result(&local_result);
    let local_item = local_items
        .iter()
        .find(|item| item.get("label").and_then(|value| value.as_str()) == Some("localFirst"))
        .unwrap_or_else(|| panic!("expected localFirst completion, got: {local_items:?}"));
    assert!(
        local_item
            .get("additionalTextEdits")
            .and_then(|value| value.as_array())
            .is_none_or(Vec::is_empty),
        "a current-namespace function must not receive an auto-import edit: {local_item:?}"
    );

    let (service_line, service_character) = utf16_position_after(unbracketed_code, "        Ser");
    let service_response = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(
            3,
            unbracketed_uri,
            service_line,
            service_character,
        ))
        .await
        .unwrap();
    let service_result = extract_result(service_response);
    let service_items = completion_items_from_result(&service_result);
    let service_item = service_items
        .iter()
        .find(|item| item.get("label").and_then(|value| value.as_str()) == Some("Service"))
        .unwrap_or_else(|| panic!("expected Vendor\\Service completion, got: {service_items:?}"));
    let service_edits = service_item
        .get("additionalTextEdits")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(service_edits.len(), 1, "expected one scoped import edit");
    assert_eq!(
        service_edits[0]
            .get("newText")
            .and_then(|value| value.as_str()),
        Some("use Vendor\\Service;\n")
    );
    let second_namespace_offset = unbracketed_code
        .find("namespace Second;")
        .expect("second unbracketed namespace");
    let second_namespace_line = unbracketed_code[..second_namespace_offset]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count() as u64;
    assert_eq!(
        service_edits[0]["range"]["start"]["line"].as_u64(),
        Some(second_namespace_line + 1),
        "unbracketed import must be inserted inside the cursor namespace"
    );

    let bracketed_uri = "file:///test/CursorScopedBracketed.php";
    let bracketed_code = r#"<?php
namespace Alpha {
    use Other\Client as Client;
}

namespace Beta {

    class Consumer
    {
        public function run(): void
        {
            Cli
        }
    }
}
"#;
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(bracketed_uri, bracketed_code))
        .await
        .unwrap();
    let (client_line, client_character) = utf16_position_after(bracketed_code, "            Cli");
    let client_response = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(
            4,
            bracketed_uri,
            client_line,
            client_character,
        ))
        .await
        .unwrap();
    let client_result = extract_result(client_response);
    let client_items = completion_items_from_result(&client_result);
    let client_item = client_items
        .iter()
        .find(|item| item.get("label").and_then(|value| value.as_str()) == Some("Client"))
        .unwrap_or_else(|| panic!("expected Vendor\\Client completion, got: {client_items:?}"));
    let client_edits = client_item
        .get("additionalTextEdits")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(client_edits.len(), 1, "expected one scoped import edit");
    assert_eq!(
        client_edits[0]
            .get("newText")
            .and_then(|value| value.as_str()),
        Some("use Vendor\\Client;\n")
    );
    let beta_namespace_offset = bracketed_code
        .find("namespace Beta {")
        .expect("second bracketed namespace");
    let beta_namespace_line = bracketed_code[..beta_namespace_offset]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count() as u64;
    assert_eq!(
        client_edits[0]["range"]["start"]["line"].as_u64(),
        Some(beta_namespace_line + 1),
        "bracketed import must be inserted inside the cursor namespace"
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_completion_auto_import_case_rules_are_kind_aware() {
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

    let vendor_uri = "file:///test/CaseAwareAutoImportVendor.php";
    let vendor_code = r#"<?php
namespace Vendor;

class Service {}
function helper(): void {}
function utility(): void {}
const PACKAGE_VERSION = '1';
"#;
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(vendor_uri, vendor_code))
        .await
        .unwrap();

    let existing_uri = "file:///test/CaseAwareExistingImports.php";
    let existing_code = r#"<?php
namespace App;

use vEnDoR\sErViCe;
use function VENDOR\HELPER;
use const vendor\PACKAGE_VERSION;

function existingImports(): void
{
    new Ser
    hel
    PAC
}
"#;
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(existing_uri, existing_code))
        .await
        .unwrap();

    for (request_id, needle, label) in [
        (31, "    new Ser", "Service"),
        (32, "    hel", "helper"),
        (33, "    PAC", "PACKAGE_VERSION"),
    ] {
        let (line, character) = utf16_position_after(existing_code, needle);
        let response = service
            .ready()
            .await
            .unwrap()
            .call(completion_request(
                request_id,
                existing_uri,
                line,
                character,
            ))
            .await
            .unwrap();
        let result = extract_result(response);
        let items = completion_items_from_result(&result);
        let item = items
            .iter()
            .find(|item| item.get("label").and_then(|value| value.as_str()) == Some(label))
            .unwrap_or_else(|| panic!("expected {label} completion, got: {items:?}"));
        assert!(
            item.get("additionalTextEdits")
                .and_then(|value| value.as_array())
                .is_none_or(Vec::is_empty),
            "existing {label} import with PHP-equivalent casing must not be duplicated: {item:?}"
        );
    }

    let collision_uri = "file:///test/CaseAwareImportAliases.php";
    let collision_code = r#"<?php
namespace App;

use Other\Occupied as service;
use function Other\occupied_function as HELPER;
use const vEnDoR\package_version;

function importAliases(): void
{
    new Ser
    hel
    PAC
}
"#;
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(collision_uri, collision_code))
        .await
        .unwrap();

    for (request_id, needle, label) in [(34, "    new Ser", "Service"), (35, "    hel", "helper")] {
        let (line, character) = utf16_position_after(collision_code, needle);
        let response = service
            .ready()
            .await
            .unwrap()
            .call(completion_request(
                request_id,
                collision_uri,
                line,
                character,
            ))
            .await
            .unwrap();
        let result = extract_result(response);
        let items = completion_items_from_result(&result);
        let item = items
            .iter()
            .find(|item| item.get("label").and_then(|value| value.as_str()) == Some(label))
            .unwrap_or_else(|| panic!("expected {label} completion, got: {items:?}"));
        assert!(
            item.get("additionalTextEdits")
                .and_then(|value| value.as_array())
                .is_none_or(Vec::is_empty),
            "{label} alias collision must be case-insensitive: {item:?}"
        );
    }

    let (constant_line, constant_character) = utf16_position_after(collision_code, "    PAC");
    let constant_response = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(
            36,
            collision_uri,
            constant_line,
            constant_character,
        ))
        .await
        .unwrap();
    let constant_result = extract_result(constant_response);
    let constant_items = completion_items_from_result(&constant_result);
    let constant_item = constant_items
        .iter()
        .find(|item| item.get("label").and_then(|value| value.as_str()) == Some("PACKAGE_VERSION"))
        .unwrap_or_else(|| panic!("expected PACKAGE_VERSION completion, got: {constant_items:?}"));
    let constant_edits = constant_item
        .get("additionalTextEdits")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        constant_edits.len(),
        1,
        "constant identifier casing must remain significant: {constant_item:?}"
    );
    assert_eq!(
        constant_edits[0]
            .get("newText")
            .and_then(|value| value.as_str()),
        Some("use const Vendor\\PACKAGE_VERSION;\n")
    );

    let scoped_uri = "file:///test/CaseAwareScopedDeclarations.php";
    let scoped_code = r#"<?php
namespace First;

class Service {}
function HELPER(): void {}

namespace Second;

function UTILITY(): void {}

function scopedDeclarations(): void
{
    new Ser
    hel
    uti
}
"#;
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(scoped_uri, scoped_code))
        .await
        .unwrap();

    for (request_id, needle, label, detail, expected_import) in [
        (
            37,
            "    new Ser",
            "Service",
            "Vendor\\Service",
            "use Vendor\\Service;\n",
        ),
        (
            38,
            "    hel",
            "helper",
            "Vendor\\helper",
            "use function Vendor\\helper;\n",
        ),
    ] {
        let (line, character) = utf16_position_after(scoped_code, needle);
        let response = service
            .ready()
            .await
            .unwrap()
            .call(completion_request(request_id, scoped_uri, line, character))
            .await
            .unwrap();
        let result = extract_result(response);
        let items = completion_items_from_result(&result);
        let item = items
            .iter()
            .find(|item| {
                item.get("label").and_then(|value| value.as_str()) == Some(label)
                    && item.get("detail").and_then(|value| value.as_str()) == Some(detail)
            })
            .unwrap_or_else(|| panic!("expected {detail} completion, got: {items:?}"));
        let edits = item
            .get("additionalTextEdits")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            edits.len(),
            1,
            "a declaration from another namespace scope must not suppress {detail}: {item:?}"
        );
        assert_eq!(
            edits[0].get("newText").and_then(|value| value.as_str()),
            Some(expected_import)
        );
    }

    let (utility_line, utility_character) = utf16_position_after(scoped_code, "    uti");
    let utility_response = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(
            39,
            scoped_uri,
            utility_line,
            utility_character,
        ))
        .await
        .unwrap();
    let utility_result = extract_result(utility_response);
    let utility_items = completion_items_from_result(&utility_result);
    let utility_item = utility_items
        .iter()
        .find(|item| {
            item.get("label").and_then(|value| value.as_str()) == Some("utility")
                && item.get("detail").and_then(|value| value.as_str()) == Some("Vendor\\utility")
        })
        .unwrap_or_else(|| panic!("expected Vendor\\utility completion, got: {utility_items:?}"));
    assert!(
        utility_item
            .get("additionalTextEdits")
            .and_then(|value| value.as_array())
            .is_none_or(Vec::is_empty),
        "a local function collision in the current namespace must be case-insensitive: {utility_item:?}"
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_completion_auto_imports_same_fqn_by_completion_kind() {
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

    let vendor_uri = "file:///test/SameFqnAutoImportVendor.php";
    let vendor_code = r#"<?php
namespace Collision;

/** Same-FQN class documentation. */
class Shared {}

/**
 * Same-FQN function documentation.
 * @param int $count Number of items.
 * @return string
 */
function Shared(int $count): string { return (string) $count; }
const Shared = 1;
"#;
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(vendor_uri, vendor_code))
        .await
        .unwrap();

    let app_uri = "file:///test/SameFqnAutoImportConsumer.php";
    let app_code = r#"<?php
namespace App;

function run(): void
{
    Sha
}
"#;
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(app_uri, app_code))
        .await
        .unwrap();

    let (line, character) = utf16_position_after(app_code, "    Sha");
    let response = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(41, app_uri, line, character))
        .await
        .unwrap();
    let result = extract_result(response);
    let items = completion_items_from_result(&result);

    for (kind, expected_import) in [
        (7, "use Collision\\Shared;\n"),
        (3, "use function Collision\\Shared;\n"),
        (21, "use const Collision\\Shared;\n"),
    ] {
        let item = items
            .iter()
            .find(|item| {
                item.get("label").and_then(|value| value.as_str()) == Some("Shared")
                    && item.get("detail").and_then(|value| value.as_str())
                        == Some("Collision\\Shared")
                    && item.get("kind").and_then(|value| value.as_u64()) == Some(kind)
            })
            .unwrap_or_else(|| {
                panic!("expected Collision\\Shared completion kind {kind}, got: {items:?}")
            });
        let edits = item
            .get("additionalTextEdits")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            edits.len(),
            1,
            "same-FQN completion kind {kind} should have one matching import: {item:?}"
        );
        assert_eq!(
            edits[0].get("newText").and_then(|value| value.as_str()),
            Some(expected_import),
            "same-FQN completion must resolve its own symbol kind"
        );
    }

    let function_item = items
        .iter()
        .find(|item| {
            item.get("label").and_then(|value| value.as_str()) == Some("Shared")
                && item.get("detail").and_then(|value| value.as_str()) == Some("Collision\\Shared")
                && item.get("kind").and_then(|value| value.as_u64()) == Some(3)
        })
        .cloned()
        .expect("same-FQN function completion item");
    let resolved_response = service
        .ready()
        .await
        .unwrap()
        .call(completion_resolve_request(42, function_item))
        .await
        .unwrap();
    let resolved = extract_result(resolved_response);
    assert_eq!(
        resolved.get("detail").and_then(|value| value.as_str()),
        Some("(int $count): string"),
        "completionItem/resolve must use the function signature, not the same-FQN class: {resolved}"
    );
    assert!(
        resolved
            .get("documentation")
            .and_then(|value| value.get("value"))
            .and_then(|value| value.as_str())
            .is_some_and(|value| value.contains("Same-FQN function documentation")),
        "completionItem/resolve must use the function PHPDoc: {resolved}"
    );

    let eof_uri = "file:///test/AutoImportAtFinalNamespaceEof.php";
    let eof_code =
        "<?php\nnamespace First;\nuse Other\\Thing as Shared;\n\nnamespace Second;\nnew Sha";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(eof_uri, eof_code))
        .await
        .unwrap();
    let (eof_line, eof_character) = utf16_position_after(eof_code, "new Sha");
    let eof_response = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(43, eof_uri, eof_line, eof_character))
        .await
        .unwrap();
    let eof_result = extract_result(eof_response);
    let eof_items = completion_items_from_result(&eof_result);
    let eof_item = eof_items
        .iter()
        .find(|item| {
            item.get("label").and_then(|value| value.as_str()) == Some("Shared")
                && item.get("detail").and_then(|value| value.as_str()) == Some("Collision\\Shared")
                && item.get("kind").and_then(|value| value.as_u64()) == Some(7)
        })
        .unwrap_or_else(|| panic!("expected Collision\\Shared at final EOF, got: {eof_items:?}"));
    let eof_edits = eof_item
        .get("additionalTextEdits")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        eof_edits.len(),
        1,
        "an import alias from an earlier namespace must not leak at final EOF: {eof_item:?}"
    );
    assert_eq!(
        eof_edits[0].get("newText").and_then(|value| value.as_str()),
        Some("use Collision\\Shared;\n\n")
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_signature_help_for_function_method_and_constructor() {
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

    let code = r#"<?php
namespace App;

/**
 * Build a greeting.
 * @param string $name Person name.
 * @param int $count Repeat count.
 * @return string
 */
function greet(string $name, int $count = 1): string { return $name; }

class Greeter {
    /**
     * @param string $prefix Prefix text.
     */
    public function __construct(string $prefix) {}

    /**
     * @param string $name Person name.
     * @param int $count Repeat count.
     */
    public function say(string $name, int $count): string { return $name; }
}

function run(): void {
    greet("Ada", 2);
    $greeter = new Greeter("Hi");
    $greeter->say("Ada", 2);
}
"#;
    let uri = "file:///test/signature-help.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let function_resp = service
        .ready()
        .await
        .unwrap()
        .call(signature_help_request(2, uri, 25, 18))
        .await
        .unwrap();
    let function_result = extract_result(function_resp);
    assert!(
        function_result["signatures"][0]["label"]
            .as_str()
            .unwrap_or("")
            .contains("App\\greet(string $name, int $count = 1): string"),
        "expected function signature, got: {}",
        function_result
    );
    assert_eq!(
        function_result["activeParameter"].as_u64(),
        Some(1),
        "second function argument should be active"
    );
    assert!(
        function_result["signatures"][0]["parameters"][0]["documentation"]["value"]
            .as_str()
            .unwrap_or("")
            .contains("Person name."),
        "expected @param documentation, got: {}",
        function_result
    );

    let ctor_resp = service
        .ready()
        .await
        .unwrap()
        .call(signature_help_request(3, uri, 26, 30))
        .await
        .unwrap();
    let ctor_result = extract_result(ctor_resp);
    assert!(
        ctor_result["signatures"][0]["label"]
            .as_str()
            .unwrap_or("")
            .contains("App\\Greeter::__construct(string $prefix)"),
        "expected constructor signature, got: {}",
        ctor_result
    );

    let method_resp = service
        .ready()
        .await
        .unwrap()
        .call(signature_help_request(4, uri, 27, 26))
        .await
        .unwrap();
    let method_result = extract_result(method_resp);
    assert!(
        method_result["signatures"][0]["label"]
            .as_str()
            .unwrap_or("")
            .contains("App\\Greeter::say(string $name, int $count): string"),
        "expected method signature, got: {}",
        method_result
    );
    assert_eq!(
        method_result["activeParameter"].as_u64(),
        Some(1),
        "second method argument should be active"
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_hover_and_completion_respond_while_workspace_indexing_runs() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-indexing-responsiveness-{}-{}",
        std::process::id(),
        nanos
    ));
    let src_dir = tmp_root.join("src");
    fs::create_dir_all(&src_dir).unwrap();
    for file_index in 0..240 {
        let mut code = format!("<?php\nnamespace Stress\\Generated{};\n", file_index);
        for class_index in 0..12 {
            code.push_str(&format!(
                "class Generated{}_{class_index} {{ public function method{class_index}(): void {{}} }}\n",
                file_index
            ));
        }
        fs::write(src_dir.join(format!("Generated{file_index}.php")), code).unwrap();
    }

    let root_uri = format!("file://{}", tmp_root.to_string_lossy());
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

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
    wait_for_indexing_phase(&mut notifications, "indexing", Duration::from_secs(2)).await;

    let uri = "file:///test/IndexingResponsiveness.php";
    let code = "<?php\nnamespace App\\Stress;\nclass RealtimeService { public function ping(): void {} }\nfunction run(RealtimeService $service): void {\n    $service->\n}\n";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(2, uri, 3, 18));
    let completion = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(3, uri, 4, 14));
    let (hover_response, completion_response) =
        tokio::time::timeout(Duration::from_secs(2), async {
            futures::join!(hover, completion)
        })
        .await
        .expect("hover and completion should respond while indexing runs");

    let hover_result = extract_result(hover_response.unwrap());
    assert!(
        hover_markdown_value(&hover_result).contains("RealtimeService"),
        "hover should resolve open-file class during indexing, got: {}",
        hover_result
    );
    let completion_result = extract_result(completion_response.unwrap());
    let labels: Vec<_> = completion_items_from_result(&completion_result)
        .into_iter()
        .filter_map(|item| {
            item.get("label")
                .and_then(|label| label.as_str())
                .map(str::to_string)
        })
        .collect();
    assert!(
        labels.iter().any(|label| label == "ping"),
        "completion should include open-file member during indexing, got: {:?}",
        labels
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
    let _ = fs::remove_dir_all(&tmp_root);
}

#[tokio::test(flavor = "current_thread")]
async fn test_phpdoc_fixture_hover_completion_definition_and_diagnostics() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    let fixture_root = lsp_cases_fixture_root();
    let root_uri = format!("file://{}", fixture_root.display());

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

    let supported_path = fixture_root.join("src/PhpDoc/SupportedTags.php");
    let supported_uri = format!("file://{}", supported_path.display());
    let supported_content = fs::read_to_string(&supported_path).unwrap();
    let supported_class_name = utf16_position_after(&supported_content, "class ");
    let supported_build_method = utf16_position_at(&supported_content, "build(string");
    let property_tag = utf16_position_at(&supported_content, "@property string $label");
    let method_tag = utf16_position_at(&supported_content, "@method User findById");
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&supported_uri, &supported_content))
        .await
        .unwrap();

    let usage_path = fixture_root.join("src/PhpDoc/VirtualMembers.php");
    let usage_uri = format!("file://{}", usage_path.display());
    let usage_content = fs::read_to_string(&usage_path).unwrap();
    let label_completion = utf16_position_after(&usage_content, "$subject->");
    let chained_completion_position = utf16_position_after(&usage_content, "$subject->owner->");
    let dirty_arrow_offset = usage_content
        .find("$subject->dirty")
        .expect("fixture should contain dirty assignment")
        + "$subject->".len();
    let dirty_completion = utf16_position_for_offset(&usage_content, dirty_arrow_offset);
    let label_usage = utf16_position_at(&usage_content, "label;");
    let method_usage = utf16_position_at(&usage_content, "findById");
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&usage_uri, &usage_content))
        .await
        .unwrap();

    let class_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(
            2,
            &supported_uri,
            supported_class_name.0,
            supported_class_name.1,
        ))
        .await
        .unwrap();
    let class_hover_result = extract_result(class_hover);
    let class_hover_text = hover_markdown_value(&class_hover_result);
    assert!(
        class_hover_text.contains("Class-level PHPDoc")
            && class_hover_text.contains("@property-read int $version")
            && class_hover_text.contains("@method User findById(int $id)"),
        "class hover should include PHPDoc summary and virtual members, got: {}",
        class_hover_text
    );

    let method_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(
            3,
            &supported_uri,
            supported_build_method.0,
            supported_build_method.1,
        ))
        .await
        .unwrap();
    let method_hover_result = extract_result(method_hover);
    let method_hover_text = hover_markdown_value(&method_hover_result);
    assert!(
        method_hover_text.contains("**Throws:**")
            && method_hover_text.contains("\\InvalidArgumentException")
            && method_hover_text.contains("Deprecated")
            && method_hover_text.contains("Use buildFromPayload() instead"),
        "method hover should include @throws and @deprecated, got: {}",
        method_hover_text
    );

    let completion = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(
            4,
            &usage_uri,
            label_completion.0,
            label_completion.1,
        ))
        .await
        .unwrap();
    let completion_result = extract_result(completion);
    let items = completion_items_from_result(&completion_result);
    let label_item = items
        .iter()
        .find(|item| item.get("label").and_then(|value| value.as_str()) == Some("label"))
        .cloned()
        .expect("completion should include @property $label");
    let find_by_id_item = items
        .iter()
        .find(|item| item.get("label").and_then(|value| value.as_str()) == Some("findById"))
        .cloned()
        .expect("completion should include @method findById");
    assert!(
        items.iter().any(|item| {
            item.get("label").and_then(|value| value.as_str()) == Some("version")
                && item.get("detail").and_then(|value| value.as_str()) == Some("@property-read int")
        }),
        "completion should include @property-read detail, got: {}",
        completion_result
    );

    let chained_completion = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(
            43,
            &usage_uri,
            chained_completion_position.0,
            chained_completion_position.1,
        ))
        .await
        .unwrap();
    let chained_completion_result = extract_result(chained_completion);
    let chained_items = completion_items_from_result(&chained_completion_result);
    assert!(
        chained_items
            .iter()
            .any(|item| { item.get("label").and_then(|value| value.as_str()) == Some("getName") }),
        "chained completion should infer @property User $owner, got: {}",
        chained_completion_result
    );
    assert!(
        !items
            .iter()
            .any(|item| item.get("label").and_then(|value| value.as_str()) == Some("dirty")),
        "read completion should not include @property-write, got: {}",
        completion_result
    );

    let write_completion = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(
            41,
            &usage_uri,
            dirty_completion.0,
            dirty_completion.1,
        ))
        .await
        .unwrap();
    let write_completion_result = extract_result(write_completion);
    let write_items = completion_items_from_result(&write_completion_result);
    assert!(
        write_items.iter().any(|item| {
            item.get("label").and_then(|value| value.as_str()) == Some("dirty")
                && item.get("detail").and_then(|value| value.as_str())
                    == Some("@property-write bool")
        }),
        "write completion should include @property-write detail, got: {}",
        write_completion_result
    );
    assert!(
        !write_items
            .iter()
            .any(|item| item.get("label").and_then(|value| value.as_str()) == Some("version")),
        "write completion should not include @property-read, got: {}",
        write_completion_result
    );

    let static_usage_uri = "file:///test/PhpDocStaticVirtualMembers.php";
    let static_usage_content =
        "<?php\nnamespace App\\PhpDoc;\nfunction makeSupported(): void\n{\n    SupportedTags::\n}\n";
    let static_completion_position = utf16_position_after(static_usage_content, "SupportedTags::");
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(
            static_usage_uri,
            static_usage_content,
        ))
        .await
        .unwrap();
    let static_completion = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(
            42,
            static_usage_uri,
            static_completion_position.0,
            static_completion_position.1,
        ))
        .await
        .unwrap();
    let static_completion_result = extract_result(static_completion);
    let static_items = completion_items_from_result(&static_completion_result);
    assert!(
        static_items
            .iter()
            .any(|item| item.get("label").and_then(|value| value.as_str()) == Some("make")),
        "static completion should include static @method, got: {}",
        static_completion_result
    );
    assert!(
        !static_items
            .iter()
            .any(|item| item.get("label").and_then(|value| value.as_str()) == Some("findById")),
        "static completion should not include instance @method, got: {}",
        static_completion_result
    );

    let resolved_label = service
        .ready()
        .await
        .unwrap()
        .call(completion_resolve_request(5, label_item))
        .await
        .unwrap();
    let resolved_label_result = extract_result(resolved_label);
    let resolved_label_doc = documentation_markdown_value(&resolved_label_result);
    assert!(
        resolved_label_doc.contains("@property string $label")
            && resolved_label_doc.contains("Human-readable label"),
        "completionItem/resolve should document virtual property, got: {}",
        resolved_label_result
    );

    let resolved_method = service
        .ready()
        .await
        .unwrap()
        .call(completion_resolve_request(6, find_by_id_item))
        .await
        .unwrap();
    let resolved_method_result = extract_result(resolved_method);
    let resolved_method_doc = documentation_markdown_value(&resolved_method_result);
    assert!(
        resolved_method_doc.contains("@method User findById(int $id)"),
        "completionItem/resolve should document virtual method, got: {}",
        resolved_method_result
    );

    let virtual_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(7, &usage_uri, label_usage.0, label_usage.1))
        .await
        .unwrap();
    let virtual_hover_result = extract_result(virtual_hover);
    let virtual_hover_text = hover_markdown_value(&virtual_hover_result);
    assert!(
        virtual_hover_text.contains("@property string $label")
            && virtual_hover_text.contains("Human-readable label"),
        "hover on virtual property should use class PHPDoc tag, got: {}",
        virtual_hover_text
    );

    let property_definition = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(
            8,
            &usage_uri,
            label_usage.0,
            label_usage.1,
        ))
        .await
        .unwrap();
    let property_definition_result = extract_result(property_definition);
    assert_eq!(
        property_definition_result
            .get("uri")
            .and_then(|value| value.as_str()),
        Some(supported_uri.as_str()),
        "virtual property definition should point to SupportedTags.php, got: {}",
        property_definition_result
    );
    assert_eq!(
        property_definition_result["range"]["start"]["line"].as_u64(),
        Some(property_tag.0 as u64),
        "virtual property definition should point at @property tag name, got: {}",
        property_definition_result
    );

    let method_definition = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(
            9,
            &usage_uri,
            method_usage.0,
            method_usage.1,
        ))
        .await
        .unwrap();
    let method_definition_result = extract_result(method_definition);
    assert_eq!(
        method_definition_result
            .get("uri")
            .and_then(|value| value.as_str()),
        Some(supported_uri.as_str()),
        "virtual method definition should point to SupportedTags.php, got: {}",
        method_definition_result
    );
    assert_eq!(
        method_definition_result["range"]["start"]["line"].as_u64(),
        Some(method_tag.0 as u64),
        "virtual method definition should point at @method tag name, got: {}",
        method_definition_result
    );

    let prepare_rename = service
        .ready()
        .await
        .unwrap()
        .call(prepare_rename_request(
            10,
            &usage_uri,
            label_usage.0,
            label_usage.1,
        ))
        .await
        .unwrap();
    assert!(
        extract_result(prepare_rename).is_null(),
        "prepareRename should reject PHPDoc virtual members"
    );

    let rename = service
        .ready()
        .await
        .unwrap()
        .call(rename_request(
            11,
            &usage_uri,
            label_usage.0,
            label_usage.1,
            "caption",
        ))
        .await
        .unwrap();
    let rename_error = extract_error_message(rename).unwrap_or_default();
    assert!(
        rename_error.contains("Cannot rename PHPDoc virtual members"),
        "rename should explicitly reject PHPDoc virtual members, got: {}",
        rename_error
    );

    let edge_path = fixture_root.join("src/PhpDoc/EdgeCases.php");
    let edge_uri = format!("file://{}", edge_path.display());
    let edge_content = fs::read_to_string(&edge_path).unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&edge_uri, &edge_content))
        .await
        .unwrap();
    let edge_diagnostics =
        next_publish_diagnostics(&mut notifications, &edge_uri, Duration::from_secs(2)).await;
    assert!(
        edge_diagnostics
            .get("diagnostics")
            .and_then(|value| value.as_array())
            .is_some(),
        "PHPDoc edge-case fixture should publish diagnostics without crashing, got: {}",
        edge_diagnostics
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}
