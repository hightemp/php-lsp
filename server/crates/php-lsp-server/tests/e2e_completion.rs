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
    /** @var array{foo: User, bar?: int, meta: array{city: string}} $row */
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
}
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
        let character = (prefix.len() - line_start) as u32;
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
        .call(hover_request(2, &supported_uri, 18, 8))
        .await
        .unwrap();
    let class_hover_result = extract_result(class_hover);
    let class_hover_text = hover_markdown_value(&class_hover_result);
    assert!(
        class_hover_text.contains("Class-level PHPDoc")
            && class_hover_text.contains("@property-read int $version")
            && class_hover_text.contains("@method User findById()"),
        "class hover should include PHPDoc summary and virtual members, got: {}",
        class_hover_text
    );

    let method_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(3, &supported_uri, 35, 22))
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
        .call(completion_request(4, &usage_uri, 11, 23))
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
        .call(completion_request(41, &usage_uri, 13, 14))
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
        .call(completion_request(42, static_usage_uri, 4, 19))
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
        resolved_method_doc.contains("@method User findById()"),
        "completionItem/resolve should document virtual method, got: {}",
        resolved_method_result
    );

    let virtual_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(7, &usage_uri, 11, 25))
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
        .call(definition_request(8, &usage_uri, 11, 25))
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
        Some(12),
        "virtual property definition should point at @property tag name, got: {}",
        property_definition_result
    );

    let method_definition = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(9, &usage_uri, 12, 24))
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
        Some(15),
        "virtual method definition should point at @method tag name, got: {}",
        method_definition_result
    );

    let prepare_rename = service
        .ready()
        .await
        .unwrap()
        .call(prepare_rename_request(10, &usage_uri, 11, 25))
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
        .call(rename_request(11, &usage_uri, 11, 25, "caption"))
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
