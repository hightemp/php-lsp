mod support;

use support::*;

type TestService = LspService<PhpLspBackend>;

async fn send(service: &mut TestService, request: Request) -> serde_json::Value {
    let response = service.ready().await.unwrap().call(request).await.unwrap();
    response
        .map(|response| {
            assert!(
                response.error().is_none(),
                "LSP request failed: {response:?}"
            );
            extract_result(Some(response))
        })
        .unwrap_or(serde_json::Value::Null)
}

async fn setup(source: &str, php_version: &str) -> (TestService, String) {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move { socket.collect::<Vec<_>>().await });
    send(
        &mut service,
        initialize_request_with_options(
            1,
            None,
            Some(json!({
                "phpVersion": php_version, "stubExtensions": [], "composerEnabled": false
            })),
        ),
    )
    .await;
    let uri =
        php_lsp_types::uri::path_to_uri(std::path::Path::new("/test/Constructor.php")).unwrap();
    send(&mut service, did_open_notification(&uri, source)).await;
    (service, uri)
}

async fn constructor_action(
    service: &mut TestService,
    uri: &str,
    source: &str,
) -> Option<serde_json::Value> {
    let position = utf16_position_after(source, "class Child");
    let result = send(
        service,
        code_action_request_with_only(
            2,
            uri,
            (position, position),
            json!([]),
            vec!["refactor.rewrite"],
        ),
    )
    .await;
    result
        .as_array()
        .expect("code actions")
        .iter()
        .find(|action| action["title"] == "Generate constructor")
        .cloned()
}

fn apply_constructor_edit(source: &str, resolved: &serde_json::Value, uri: &str) -> String {
    let edits = resolved["edit"]["changes"][uri]
        .as_array()
        .expect("constructor edits");
    assert_eq!(edits.len(), 1);
    let edit = &edits[0];
    assert_eq!(edit["range"]["start"], edit["range"]["end"]);
    let position = &edit["range"]["start"];
    let line = position["line"].as_u64().unwrap() as usize;
    let character = position["character"].as_u64().unwrap() as usize;
    let start = source
        .split_inclusive('\n')
        .take(line)
        .map(str::len)
        .sum::<usize>();
    let mut width = 0;
    let mut offset = start;
    for ch in source[start..].chars() {
        if width == character {
            break;
        }
        width += ch.len_utf16();
        offset += ch.len_utf8();
    }
    assert_eq!(width, character);
    let mut result = source.to_string();
    result.insert_str(offset, edit["newText"].as_str().unwrap());
    let mut parser = php_lsp_parser::parser::FileParser::new();
    parser.parse_full(&result);
    assert!(
        !parser.tree().unwrap().root_node().has_error(),
        "invalid generated PHP: {result}"
    );
    result
}

fn assert_php_output_if_available(source: &str, expected: &str) {
    let output = match std::process::Command::new("php")
        .args(["-n", "-r", source.strip_prefix("<?php").unwrap()])
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => panic!("PHP execution failed: {error}"),
    };
    assert!(
        output.status.success(),
        "PHP failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), expected);
}

#[tokio::test(flavor = "current_thread")]
async fn test_generate_constructor_forwards_effective_parent_parameters() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move { socket.collect::<Vec<_>>().await });
    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request(1))
        .await
        .unwrap();
    let uri =
        php_lsp_types::uri::path_to_uri(std::path::Path::new("/test/InheritedConstructor.php"))
            .unwrap();
    let source = "<?php\nclass Base { public function __construct(int $id, string $mode = 'safe', string ...$tags) {} }\nclass Middle extends Base {}\nclass Child extends Middle { private string $name; }\n";
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
            .call(code_action_request_with_only(
                2,
                &uri,
                ((3, 6), (3, 11)),
                json!([]),
                vec!["refactor.rewrite"],
            ))
            .await
            .unwrap(),
    );
    let action = result
        .as_array()
        .unwrap()
        .iter()
        .find(|action| action["title"] == "Generate constructor")
        .unwrap_or_else(|| panic!("missing constructor action: {result}"))
        .clone();
    let resolved = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(code_action_resolve_request(3, action))
            .await
            .unwrap(),
    );
    let text = resolved["edit"]["changes"][&uri][0]["newText"]
        .as_str()
        .unwrap_or_else(|| panic!("missing constructor edit: {resolved}"));
    assert!(
        text.contains("parent::__construct($id, $mode, ...$tags);"),
        "parent initialization must be preserved: {text}"
    );
    assert!(text.contains("int $id, string $name, string $mode = 'safe', string ...$tags"));
    assert!(text.contains("$this->name = $name;"));
}

#[tokio::test(flavor = "current_thread")]
async fn test_generate_constructor_runtime_initialization_and_reference_forwarding() {
    let source = r#"<?php
class Base {
    public array $state;
    public function __construct(int &$id, string $mode = 'safe', string &...$tags) {
        $id++;
        foreach ($tags as &$tag) { $tag .= '!'; }
        $this->state = [$id, $mode, $tags];
    }
}
class Middle extends Base {}
/* 😀 */ class Child extends Middle { private string $name; }
$id = 7; $tag = 'tag';
$child = new Child($id, 'child', 'custom', $tag);
$default = new Child($id, 'default');
echo json_encode([$child->state, $default->state, $id, $tag]);
"#;
    let (mut service, uri) = setup(source, "8.2").await;
    let action = constructor_action(&mut service, &uri, source)
        .await
        .expect("constructor");
    let resolved = send(&mut service, code_action_resolve_request(3, action)).await;
    let generated = apply_constructor_edit(source, &resolved, &uri);
    assert!(generated.contains("int &$id, string $name, string $mode = 'safe', string &...$tags"));
    assert_php_output_if_available(
        &generated,
        r#"[[8,"custom",["tag!"]],[9,"safe",[]],9,"tag!"]"#,
    );
    send(
        &mut service,
        did_change_full_notification(&uri, 2, &generated),
    )
    .await;
    assert!(
        constructor_action(&mut service, &uri, &generated)
            .await
            .is_none(),
        "must not generate a second constructor"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_generate_constructor_native_types_and_declaring_namespace() {
    let parent = r#"<?php
namespace Contracts;
class Dependency {}
class Root {}
namespace Library;
use Contracts\Dependency as Alias;
class Base extends \Contracts\Root {
    /** @param string $dependency @param int $raw */
    public function __construct(public Alias $dependency, self $peer, parent $root, $raw, ?Alias $maybe = null) {}
}
"#;
    let child = "<?php\nnamespace App;\nuse Library\\Base;\nclass Child extends Base { private string $name; }\n";
    let (mut service, uri) = setup(child, "8.2").await;
    let parent_uri =
        php_lsp_types::uri::path_to_uri(std::path::Path::new("/test/Base.php")).unwrap();
    send(&mut service, did_open_notification(&parent_uri, parent)).await;
    let action = constructor_action(&mut service, &uri, child)
        .await
        .expect("constructor");
    let resolved = send(&mut service, code_action_resolve_request(3, action)).await;
    let generated = apply_constructor_edit(child, &resolved, &uri);
    assert!(generated.contains("\\Contracts\\Dependency $dependency, \\Library\\Base $peer, \\Contracts\\Root $root, $raw, string $name, ?\\Contracts\\Dependency $maybe = null"), "native types must use their declaring scope, without promotion or PHPDoc narrowing: {generated}");
    assert!(generated.contains("parent::__construct($dependency, $peer, $root, $raw, $maybe);"));
}

#[tokio::test(flavor = "current_thread")]
async fn test_generate_constructor_parent_selection_and_safe_defaults() {
    for (ancestors, expected_signature, expected_call) in [
        ("class Base {}", "public function __construct(string $name)", ""),
        ("class Base { public function __construct() {} }", "public function __construct(string $name)", "parent::__construct();"),
        ("class Grand { public function __construct(int $outer) {} } class Base extends Grand { public function __construct(string $inner) {} }", "public function __construct(string $inner, string $name)", "parent::__construct($inner);"),
        ("class Base { protected function __construct(int $id) {} }", "protected function __construct(int $id, string $name)", "parent::__construct($id);"),
        ("class Base { public function __construct(array $options = ['a' => 1], int $limit = -1, bool $flag = true) {} }", "public function __construct(string $name, array $options = ['a' => 1], int $limit = -1, bool $flag = true)", "parent::__construct($options, $limit, $flag);"),
        ("class Base { public function __construct(\\Countable&\\Iterator $items) {} }", "public function __construct(\\Countable&\\Iterator $items, string $name)", "parent::__construct($items);"),
    ] {
        let source = format!("<?php\n{ancestors}\nclass Child extends Base {{ private string $name; }}\n");
        let (mut service, uri) = setup(&source, "8.2").await;
        let action = constructor_action(&mut service, &uri, &source).await.unwrap_or_else(|| panic!("missing action: {source}"));
        let resolved = send(&mut service, code_action_resolve_request(3, action)).await;
        let generated = apply_constructor_edit(&source, &resolved, &uri);
        assert!(generated.contains(expected_signature), "{generated}");
        if expected_call.is_empty() { assert!(!generated.contains("parent::__construct(")); }
        else { assert!(generated.contains(expected_call), "{generated}"); }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn test_generate_constructor_unsafe_hierarchies_fail_closed_in_proposal_and_resolve() {
    for (ancestors, inheritance, version) in [
        ("", "extends Unknown", "8.2"),
        ("class Base extends Missing {}", "extends Base", "8.2"),
        ("class Base extends Child {}", "extends Base", "8.2"),
        (
            "class Base { final public function __construct(int $id) {} }",
            "extends Base",
            "8.2",
        ),
        (
            "class Base { private function __construct() {} }",
            "extends Base",
            "8.2",
        ),
        (
            "abstract class Base { abstract public function __construct(int $id); }",
            "extends Base",
            "8.2",
        ),
        (
            "abstract class Grand { abstract public function __construct(int $id); } class Base extends Grand { public function __construct(int $id) {} }",
            "extends Base",
            "8.2",
        ),
        (
            "class Base { public function __construct(string $name) {} }",
            "extends Base",
            "8.2",
        ),
        (
            "trait T { public function __construct() {} } class Base { use T; }",
            "extends Base",
            "8.2",
        ),
        ("class Base { use Missing; }", "extends Base", "8.2"),
        (
            "interface I { public function __construct(int $id); }",
            "implements I",
            "8.2",
        ),
        (
            "class Base { public function __construct(int $id = self::VALUE) {} }",
            "extends Base",
            "8.2",
        ),
        (
            "class Base { public function __construct(string $file = __FILE__) {} }",
            "extends Base",
            "8.2",
        ),
        (
            "class Base { public function __construct($value = new Thing()) {} }",
            "extends Base",
            "8.2",
        ),
        (
            "class Base { public function __construct($id = DEFAULT_ID) {} }",
            "extends Base",
            "8.2",
        ),
        (
            "class Base { public function __construct(#[A] int $id) {} }",
            "extends Base",
            "8.2",
        ),
        (
            "class Base { public function __construct(int $id = 1, int $required) {} }",
            "extends Base",
            "8.2",
        ),
        (
            "class Base { public function Base(int $id) {} }",
            "extends Base",
            "7.4",
        ),
        (
            "class Base { public function __construct(mixed $id) {} }",
            "extends Base",
            "7.4",
        ),
    ] {
        let source =
            format!("<?php\n{ancestors}\nclass Child {inheritance} {{ private string $name; }}\n");
        let (mut service, uri) = setup(&source, version).await;
        assert!(
            constructor_action(&mut service, &uri, &source)
                .await
                .is_none(),
            "unsafe action offered: {source}"
        );
        let forged = json!({"title": "Generate constructor", "data": {
            "actionKind": "generateConstructor", "uri": uri, "documentVersion": 1,
            "range": {"start": {"line": 2, "character": 6}, "end": {"line": 2, "character": 11}},
            "extra": {"kind": "generateConstructor", "class_fqn": "Child"}
        }});
        let resolved = send(&mut service, code_action_resolve_request(3, forged)).await;
        assert_eq!(
            resolved["edit"]["changes"],
            json!({}),
            "unsafe resolve: {source}: {resolved}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn test_generate_constructor_rejects_shared_readonly_property_initialization() {
    for source in [
        "<?php class Base { public readonly string $name; public function __construct() { $this->name = 'base'; } } class Child extends Base { public readonly string $name; }",
        "<?php readonly class Base { public string $name; public function __construct() { $this->name = 'base'; } } readonly class Child extends Base { public string $name; }",
        "<?php class Grand { protected readonly string $name; public function __construct() { $this->name = 'base'; } } class Base extends Grand {} class Child extends Base { protected readonly string $name; }",
    ] {
        let (mut service, uri) = setup(source, "8.2").await;
        assert!(constructor_action(&mut service, &uri, source).await.is_none(), "double readonly initialization offered: {source}");
        let forged = json!({"title": "Generate constructor", "data": {
            "actionKind": "generateConstructor", "uri": uri, "documentVersion": 1,
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
            "extra": {"kind": "generateConstructor", "class_fqn": "Child"}
        }});
        let resolved = send(&mut service, code_action_resolve_request(3, forged)).await;
        assert_eq!(resolved["edit"]["changes"], json!({}));
    }
    let source = "<?php class Base { private readonly string $name; public function __construct() { $this->name = 'base'; } } class Child extends Base { private readonly string $name; } new Child('child'); echo 'ok';";
    let (mut service, uri) = setup(source, "8.2").await;
    let action = constructor_action(&mut service, &uri, source)
        .await
        .expect("private parent storage is independent");
    let resolved = send(&mut service, code_action_resolve_request(3, action)).await;
    assert_php_output_if_available(&apply_constructor_edit(source, &resolved, &uri), "ok");
}

#[tokio::test(flavor = "current_thread")]
async fn test_generate_constructor_rejects_parent_argument_observers() {
    for (imports, observer) in [
        ("", "func_num_args()"),
        ("", "\\func_num_args()"),
        ("use function func_num_args as countArgs;", "countArgs()"),
        ("", "count(func_get_args())"),
        ("use function func_get_args as getArgs;", "count(getArgs())"),
        ("", "func_get_arg(0)"),
        ("", "(function () { return func_num_args(); })()"),
    ] {
        let source = format!("<?php\n{imports}\nclass Base {{ public string $state; public function __construct($value = null) {{ if ({observer} === 0) $this->state = 'ready'; }} }}\nclass Child extends Base {{ private string $name = 'child'; }}\n");
        let (mut service, uri) = setup(&source, "8.2").await;
        assert!(
            constructor_action(&mut service, &uri, &source)
                .await
                .is_none(),
            "argument observer action offered: {source}"
        );
        let forged = json!({"title": "Generate constructor", "data": {
            "actionKind": "generateConstructor", "uri": uri, "documentVersion": 1,
            "range": {"start": {"line": 3, "character": 6}, "end": {"line": 3, "character": 11}},
            "extra": {"kind": "generateConstructor", "class_fqn": "Child"}
        }});
        let resolved = send(&mut service, code_action_resolve_request(3, forged)).await;
        assert_eq!(resolved["edit"]["changes"], json!({}));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn test_generate_constructor_resolve_revalidates_changed_parent_and_child() {
    let child = "<?php\nclass Child extends Base { private string $name; }\n";
    let (mut service, uri) = setup(child, "8.2").await;
    let parent_uri =
        php_lsp_types::uri::path_to_uri(std::path::Path::new("/test/Base.php")).unwrap();
    send(
        &mut service,
        did_open_notification(
            &parent_uri,
            "<?php class Base { public function __construct(int $id) {} }",
        ),
    )
    .await;
    let action = constructor_action(&mut service, &uri, child)
        .await
        .expect("constructor");
    send(
        &mut service,
        did_change_full_notification(
            &parent_uri,
            2,
            "<?php class Base { final public function __construct(int $id) {} }",
        ),
    )
    .await;
    let resolved = send(&mut service, code_action_resolve_request(3, action.clone())).await;
    assert_eq!(resolved["edit"]["changes"], json!({}));
    send(
        &mut service,
        did_change_full_notification(
            &parent_uri,
            3,
            "<?php class Base { public function __construct(string $token) {} }",
        ),
    )
    .await;
    let resolved = send(&mut service, code_action_resolve_request(4, action.clone())).await;
    let generated = apply_constructor_edit(child, &resolved, &uri);
    assert!(generated.contains("parent::__construct($token);"));
    assert!(!generated.contains("$id"));
    send(&mut service, did_change_full_notification(&uri, 2, child)).await;
    let stale = send(&mut service, code_action_resolve_request(5, action)).await;
    assert_eq!(stale["edit"]["changes"], json!({}));
}
