mod support;

use support::*;

#[tokio::test(flavor = "current_thread")]
async fn test_code_action_add_use_for_unknown_class_and_function() {
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

    let vendor_code = r#"<?php
namespace Vendor;

class Bar {}

function helper(): void {}
"#;
    let vendor_uri = "file:///test/Vendor.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(vendor_uri, vendor_code))
        .await
        .unwrap();

    let app_code = r#"<?php
namespace App;

class Demo {
    public function run(): void {
        new Bar();
        helper();
    }
}
"#;
    let app_uri = "file:///test/AddUse.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(app_uri, app_code))
        .await
        .unwrap();

    let class_diag = json!([{
        "range": {
            "start": { "line": 5, "character": 12 },
            "end": { "line": 5, "character": 15 }
        },
        "severity": 2,
        "source": "php-lsp",
        "message": "Unknown class: App\\Bar"
    }]);
    let class_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request(2, app_uri, 5, 12, 5, 15, class_diag))
        .await
        .unwrap();
    let class_result = extract_result(class_resp);
    let class_actions = class_result.as_array().expect("code actions array");
    assert!(
        class_actions.iter().any(
            |action| action.get("title").and_then(|v| v.as_str()) == Some("Import Vendor\\Bar")
        ),
        "expected import action for Vendor\\Bar, got: {}",
        class_result
    );
    let class_edit_text = class_actions[0]["edit"]["changes"][app_uri][0]["newText"]
        .as_str()
        .unwrap_or("");
    assert!(
        class_edit_text.contains("use Vendor\\Bar;"),
        "expected use insertion edit, got: {}",
        class_result
    );

    let function_diag = json!([{
        "range": {
            "start": { "line": 6, "character": 8 },
            "end": { "line": 6, "character": 14 }
        },
        "severity": 2,
        "source": "php-lsp",
        "message": "Unknown function: App\\helper"
    }]);
    let function_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request(3, app_uri, 6, 8, 6, 14, function_diag))
        .await
        .unwrap();
    let function_result = extract_result(function_resp);
    let function_actions = function_result.as_array().expect("code actions array");
    assert!(
        function_actions.iter().any(|action| {
            action.get("title").and_then(|v| v.as_str()) == Some("Import Vendor\\helper")
        }),
        "expected import action for Vendor\\helper, got: {}",
        function_result
    );
    let function_edit_text = function_actions[0]["edit"]["changes"][app_uri][0]["newText"]
        .as_str()
        .unwrap_or("");
    assert!(
        function_edit_text.contains("use function Vendor\\helper;"),
        "expected use function insertion edit, got: {}",
        function_result
    );

    let conflict_code = r#"<?php
namespace App;

use Other\Bar;

class ConflictDemo {
    public function run(): void {
        new Bar();
    }
}
"#;
    let conflict_uri = "file:///test/AddUseConflict.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(conflict_uri, conflict_code))
        .await
        .unwrap();

    let conflict_diag = json!([{
        "range": {
            "start": { "line": 7, "character": 12 },
            "end": { "line": 7, "character": 15 }
        },
        "severity": 2,
        "source": "php-lsp",
        "message": "Unknown class: Other\\Bar"
    }]);
    let conflict_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request(
            4,
            conflict_uri,
            7,
            12,
            7,
            15,
            conflict_diag,
        ))
        .await
        .unwrap();
    let conflict_result = extract_result(conflict_resp);
    let conflict_actions = conflict_result.as_array().expect("code actions array");
    assert!(
        conflict_actions.iter().any(|action| {
            action.get("title").and_then(|v| v.as_str()) == Some("Import Vendor\\Bar as BarImport")
        }),
        "expected aliased import action for Vendor\\Bar, got: {}",
        conflict_result
    );
    let conflict_edits = conflict_actions[0]["edit"]["changes"][conflict_uri]
        .as_array()
        .expect("edits");
    assert!(
        conflict_edits
            .iter()
            .any(|edit| edit["newText"].as_str() == Some("use Vendor\\Bar as BarImport;\n")),
        "expected aliased use insertion, got: {}",
        conflict_result
    );
    assert!(
        conflict_edits
            .iter()
            .any(|edit| edit["newText"].as_str() == Some("BarImport")),
        "expected usage replacement with alias, got: {}",
        conflict_result
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
async fn test_code_action_implement_missing_interface_and_abstract_methods() {
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
            Some(json!({ "phpVersion": "8.2" })),
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

    let code = r#"<?php
namespace App;

interface Logger
{
    public function log(string $message): void;
}

interface Factory
{
    public static function make(?string &$name, int ...$ids): array;
}

interface CountableThing extends Logger
{
    public function count(): int;
}

interface RepositoryContract
{
    /**
     * Find entity by id.
     *
     * @template T of object
     * @param positive-int $id Entity id.
     * @return T|null Entity or null.
     * @throws \RuntimeException
     * @phpstan-return T|null
     */
    #[Audit('read')]
    public function find(int $id): ?object;
}

abstract class Base
{
    public function log(string $message): void
    {
    }

    /**
     * Build base value.
     *
     * @param int $value Base value.
     * @return non-empty-string|null
     */
    #[BaseContract]
    abstract protected function base(int $value = 0): ?string;
}

class Demo extends Base implements CountableThing, Factory, RepositoryContract
{
}

class Complete extends Base implements CountableThing, Factory, RepositoryContract
{
    protected function base(int $value = 0): ?string
    {
        return null;
    }

    public function count(): int
    {
        return 0;
    }

    public static function make(?string &$name, int ...$ids): array
    {
        return [];
    }

    public function find(int $id): ?object
    {
        return null;
    }
}
"#;
    let uri = "file:///test/ImplementMissingMethods.php";
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
        .call(code_action_request(2, uri, 49, 0, 49, 0, json!([])))
        .await
        .unwrap();
    let result = extract_result(resp);
    let actions = result.as_array().expect("code actions array");
    let action = actions
        .iter()
        .find(|action| {
            action.get("title").and_then(|value| value.as_str())
                == Some("Implement 4 missing methods")
        })
        .cloned()
        .unwrap_or_else(|| panic!("expected implement missing methods action, got: {}", result));
    assert!(
        action.get("edit").is_none(),
        "implement missing methods should resolve lazily, got: {}",
        action
    );
    assert!(
        action.get("data").is_some(),
        "implement missing methods should carry resolve data, got: {}",
        action
    );

    let resolve_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_resolve_request(3, action))
        .await
        .unwrap();
    let resolved = extract_result(resolve_resp);
    let new_text = resolved["edit"]["changes"][uri][0]["newText"]
        .as_str()
        .unwrap_or_else(|| panic!("expected generated method stubs, got: {}", resolved));

    assert!(
        new_text.contains("protected function base(int $value = 0): ?string"),
        "expected abstract parent method stub, got: {}",
        new_text
    );
    assert!(
        new_text.contains("Build base value.")
            && new_text.contains("@return non-empty-string|null")
            && new_text.contains("#[BaseContract]"),
        "expected abstract parent PHPDoc and attribute metadata, got: {}",
        new_text
    );
    assert!(
        new_text.contains("public function count(): int"),
        "expected interface method stub, got: {}",
        new_text
    );
    assert!(
        new_text.contains("Find entity by id.")
            && new_text.contains("@template T of object")
            && new_text.contains("@param positive-int $id Entity id.")
            && new_text.contains("@return T|null Entity or null.")
            && new_text.contains("@throws \\RuntimeException")
            && new_text.contains("@phpstan-return T|null")
            && new_text.contains("#[Audit('read')]")
            && new_text.contains("public function find(int $id): ?object"),
        "expected interface PHPDoc, analyzer metadata, attribute, and native-safe signature, got: {}",
        new_text
    );
    assert!(
        new_text.contains("public static function make(?string &$name, int ...$ids): array"),
        "expected static by-ref variadic interface method stub, got: {}",
        new_text
    );
    assert!(
        !new_text.contains("function log("),
        "concrete inherited method should not be duplicated, got: {}",
        new_text
    );
    assert!(
        new_text.contains("throw new \\BadMethodCallException('Not implemented yet.');"),
        "expected safe throwing body, got: {}",
        new_text
    );

    let complete_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request(4, uri, 53, 0, 53, 0, json!([])))
        .await
        .unwrap();
    let complete_result = extract_result(complete_resp);
    assert!(
        !complete_result
            .as_array()
            .expect("code actions array")
            .iter()
            .any(|action| action
                .get("title")
                .and_then(|value| value.as_str())
                .is_some_and(|title| title.starts_with("Implement "))),
        "complete implementation should not offer implement-missing action, got: {}",
        complete_result
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
async fn test_code_action_generate_constructor_getters_and_setters() {
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
            Some(json!({ "phpVersion": "8.2" })),
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

    let code = r#"<?php
namespace App;

class GenerateMembers
{
    private string $name;
    private readonly int $id;
    private ?bool $active = null;
    private static int $count = 0;
}
"#;
    let uri = "file:///test/GenerateMembers.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let constructor_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            2,
            uri,
            ((3, 6), (3, 21)),
            json!([]),
            vec!["refactor.rewrite"],
        ))
        .await
        .unwrap();
    let constructor_result = extract_result(constructor_resp);
    let constructor_action = constructor_result
        .as_array()
        .expect("code actions array")
        .iter()
        .find(|action| {
            action.get("title").and_then(|value| value.as_str()) == Some("Generate constructor")
        })
        .cloned()
        .unwrap_or_else(|| panic!("expected constructor action, got: {}", constructor_result));
    assert!(
        constructor_action.get("edit").is_none(),
        "constructor action should resolve lazily, got: {}",
        constructor_action
    );
    let constructor_resolve = service
        .ready()
        .await
        .unwrap()
        .call(code_action_resolve_request(3, constructor_action))
        .await
        .unwrap();
    let constructor_resolved = extract_result(constructor_resolve);
    let constructor_text = constructor_resolved["edit"]["changes"][uri][0]["newText"]
        .as_str()
        .unwrap_or_else(|| panic!("expected constructor edit, got: {}", constructor_resolved));
    assert!(
        constructor_text
            .contains("public function __construct(string $name, int $id, ?bool $active = null)"),
        "expected constructor params with nullable default, got: {}",
        constructor_text
    );
    assert!(
        constructor_text.contains("$this->name = $name;")
            && constructor_text.contains("$this->id = $id;")
            && constructor_text.contains("$this->active = $active;"),
        "expected instance assignments, got: {}",
        constructor_text
    );
    assert!(
        !constructor_text.contains("count"),
        "static property should not be included in constructor, got: {}",
        constructor_text
    );

    let active_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            4,
            uri,
            ((7, 18), (7, 25)),
            json!([]),
            vec!["refactor.rewrite"],
        ))
        .await
        .unwrap();
    let active_result = extract_result(active_resp);
    let active_actions = active_result.as_array().expect("code actions array");
    let getter_action = active_actions
        .iter()
        .find(|action| {
            action.get("title").and_then(|value| value.as_str())
                == Some("Generate getter `isActive`")
        })
        .cloned()
        .unwrap_or_else(|| panic!("expected bool getter action, got: {}", active_result));
    let setter_action = active_actions
        .iter()
        .find(|action| {
            action.get("title").and_then(|value| value.as_str())
                == Some("Generate setter `setActive`")
        })
        .cloned()
        .unwrap_or_else(|| panic!("expected setter action, got: {}", active_result));

    let getter_resolve = service
        .ready()
        .await
        .unwrap()
        .call(code_action_resolve_request(5, getter_action))
        .await
        .unwrap();
    let getter_resolved = extract_result(getter_resolve);
    let getter_text = getter_resolved["edit"]["changes"][uri][0]["newText"]
        .as_str()
        .unwrap_or_else(|| panic!("expected getter edit, got: {}", getter_resolved));
    assert!(
        getter_text.contains("public function isActive(): ?bool")
            && getter_text.contains("return $this->active;"),
        "expected bool getter, got: {}",
        getter_text
    );

    let setter_resolve = service
        .ready()
        .await
        .unwrap()
        .call(code_action_resolve_request(6, setter_action))
        .await
        .unwrap();
    let setter_resolved = extract_result(setter_resolve);
    let setter_text = setter_resolved["edit"]["changes"][uri][0]["newText"]
        .as_str()
        .unwrap_or_else(|| panic!("expected setter edit, got: {}", setter_resolved));
    assert!(
        setter_text.contains("public function setActive(?bool $active): void")
            && setter_text.contains("$this->active = $active;"),
        "expected setter, got: {}",
        setter_text
    );

    let readonly_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            7,
            uri,
            ((6, 25), (6, 28)),
            json!([]),
            vec!["refactor.rewrite"],
        ))
        .await
        .unwrap();
    let readonly_result = extract_result(readonly_resp);
    let readonly_actions = readonly_result.as_array().expect("code actions array");
    assert!(
        readonly_actions.iter().any(|action| {
            action.get("title").and_then(|value| value.as_str()) == Some("Generate getter `getId`")
        }),
        "expected readonly getter, got: {}",
        readonly_result
    );
    assert!(
        !readonly_actions.iter().any(|action| {
            action.get("title").and_then(|value| value.as_str()) == Some("Generate setter `setId`")
        }),
        "readonly property should not offer setter, got: {}",
        readonly_result
    );

    let static_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            8,
            uri,
            ((8, 23), (8, 29)),
            json!([]),
            vec!["refactor.rewrite"],
        ))
        .await
        .unwrap();
    let static_result = extract_result(static_resp);
    let static_getter_action = static_result
        .as_array()
        .expect("code actions array")
        .iter()
        .find(|action| {
            action.get("title").and_then(|value| value.as_str())
                == Some("Generate getter `getCount`")
        })
        .cloned()
        .unwrap_or_else(|| panic!("expected static getter action, got: {}", static_result));
    let static_getter_resolve = service
        .ready()
        .await
        .unwrap()
        .call(code_action_resolve_request(9, static_getter_action))
        .await
        .unwrap();
    let static_getter_resolved = extract_result(static_getter_resolve);
    let static_getter_text = static_getter_resolved["edit"]["changes"][uri][0]["newText"]
        .as_str()
        .unwrap_or_else(|| {
            panic!(
                "expected static getter edit, got: {}",
                static_getter_resolved
            )
        });
    assert!(
        static_getter_text.contains("public static function getCount(): int")
            && static_getter_text.contains("return self::$count;"),
        "expected static getter, got: {}",
        static_getter_text
    );

    let advanced_code = r#"<?php
namespace App;

class AdvancedMembers
{
    /** @var array<int, string> Items by id. */
    private array $items = [];

    /** @var positive-int Identifier. */
    private $id;

    /** @phpstan-var non-empty-list<class-string> Classes. */
    private static array $classes = [];
}
"#;
    let advanced_uri = "file:///test/AdvancedMembers.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(advanced_uri, advanced_code))
        .await
        .unwrap();

    let advanced_constructor_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            10,
            advanced_uri,
            ((3, 6), (3, 21)),
            json!([]),
            vec!["refactor.rewrite"],
        ))
        .await
        .unwrap();
    let advanced_constructor_result = extract_result(advanced_constructor_resp);
    let advanced_constructor_action = advanced_constructor_result
        .as_array()
        .expect("code actions array")
        .iter()
        .find(|action| {
            action.get("title").and_then(|value| value.as_str()) == Some("Generate constructor")
        })
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "expected advanced constructor action, got: {}",
                advanced_constructor_result
            )
        });
    let advanced_constructor_resolve = service
        .ready()
        .await
        .unwrap()
        .call(code_action_resolve_request(11, advanced_constructor_action))
        .await
        .unwrap();
    let advanced_constructor_resolved = extract_result(advanced_constructor_resolve);
    let advanced_constructor_text = advanced_constructor_resolved["edit"]["changes"][advanced_uri]
        [0]["newText"]
        .as_str()
        .unwrap_or_else(|| {
            panic!(
                "expected advanced constructor edit, got: {}",
                advanced_constructor_resolved
            )
        });
    assert!(
        advanced_constructor_text.contains("@param array<int, string> $items Items by id.")
            && advanced_constructor_text.contains("@param positive-int $id Identifier.")
            && advanced_constructor_text
                .contains("public function __construct(array $items, int $id)")
            && !advanced_constructor_text.contains("classes"),
        "expected constructor PHPDoc with refined types and native-safe params, got: {}",
        advanced_constructor_text
    );

    let items_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            12,
            advanced_uri,
            ((6, 20), (6, 26)),
            json!([]),
            vec!["refactor.rewrite"],
        ))
        .await
        .unwrap();
    let items_result = extract_result(items_resp);
    let items_actions = items_result.as_array().expect("code actions array");
    let items_getter_action = items_actions
        .iter()
        .find(|action| {
            action.get("title").and_then(|value| value.as_str())
                == Some("Generate getter `getItems`")
        })
        .cloned()
        .unwrap_or_else(|| panic!("expected items getter action, got: {}", items_result));
    let items_setter_action = items_actions
        .iter()
        .find(|action| {
            action.get("title").and_then(|value| value.as_str())
                == Some("Generate setter `setItems`")
        })
        .cloned()
        .unwrap_or_else(|| panic!("expected items setter action, got: {}", items_result));
    let items_getter_resolve = service
        .ready()
        .await
        .unwrap()
        .call(code_action_resolve_request(13, items_getter_action))
        .await
        .unwrap();
    let items_getter_resolved = extract_result(items_getter_resolve);
    let items_getter_text = items_getter_resolved["edit"]["changes"][advanced_uri][0]["newText"]
        .as_str()
        .unwrap_or_else(|| panic!("expected items getter edit, got: {}", items_getter_resolved));
    assert!(
        items_getter_text.contains("@return array<int, string> Items by id.")
            && items_getter_text.contains("public function getItems(): array"),
        "expected getter PHPDoc with refined return type, got: {}",
        items_getter_text
    );
    let items_setter_resolve = service
        .ready()
        .await
        .unwrap()
        .call(code_action_resolve_request(14, items_setter_action))
        .await
        .unwrap();
    let items_setter_resolved = extract_result(items_setter_resolve);
    let items_setter_text = items_setter_resolved["edit"]["changes"][advanced_uri][0]["newText"]
        .as_str()
        .unwrap_or_else(|| panic!("expected items setter edit, got: {}", items_setter_resolved));
    assert!(
        items_setter_text.contains("@param array<int, string> $items Items by id.")
            && items_setter_text.contains("public function setItems(array $items): void"),
        "expected setter PHPDoc with refined param type, got: {}",
        items_setter_text
    );

    let classes_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            15,
            advanced_uri,
            ((12, 30), (12, 37)),
            json!([]),
            vec!["refactor.rewrite"],
        ))
        .await
        .unwrap();
    let classes_result = extract_result(classes_resp);
    let classes_getter_action = classes_result
        .as_array()
        .expect("code actions array")
        .iter()
        .find(|action| {
            action.get("title").and_then(|value| value.as_str())
                == Some("Generate getter `getClasses`")
        })
        .cloned()
        .unwrap_or_else(|| panic!("expected classes getter action, got: {}", classes_result));
    let classes_getter_resolve = service
        .ready()
        .await
        .unwrap()
        .call(code_action_resolve_request(16, classes_getter_action))
        .await
        .unwrap();
    let classes_getter_resolved = extract_result(classes_getter_resolve);
    let classes_getter_text = classes_getter_resolved["edit"]["changes"][advanced_uri][0]
        ["newText"]
        .as_str()
        .unwrap_or_else(|| {
            panic!(
                "expected classes getter edit, got: {}",
                classes_getter_resolved
            )
        });
    assert!(
        classes_getter_text.contains("@return non-empty-list<class-string> Classes.")
            && classes_getter_text.contains("public static function getClasses(): array")
            && classes_getter_text.contains("return self::$classes;"),
        "expected static getter PHPDoc from analyzer var tag, got: {}",
        classes_getter_text
    );

    let existing_code = r#"<?php
namespace App;

class ExistingMembers
{
    private string $title;

    public function __construct()
    {
    }

    public function getTitle(): string
    {
        return $this->title;
    }

    public function setTitle(string $title): void
    {
        $this->title = $title;
    }
}
"#;
    let existing_uri = "file:///test/ExistingMembers.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(existing_uri, existing_code))
        .await
        .unwrap();

    let existing_class_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            17,
            existing_uri,
            ((3, 6), (3, 21)),
            json!([]),
            vec!["refactor.rewrite"],
        ))
        .await
        .unwrap();
    let existing_class_result = extract_result(existing_class_resp);
    assert!(
        !existing_class_result
            .as_array()
            .expect("code actions array")
            .iter()
            .any(
                |action| action.get("title").and_then(|value| value.as_str())
                    == Some("Generate constructor")
            ),
        "existing constructor should suppress constructor action, got: {}",
        existing_class_result
    );

    let existing_property_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            18,
            existing_uri,
            ((5, 19), (5, 25)),
            json!([]),
            vec!["refactor.rewrite"],
        ))
        .await
        .unwrap();
    let existing_property_result = extract_result(existing_property_resp);
    assert!(
        !existing_property_result
            .as_array()
            .expect("code actions array")
            .iter()
            .any(|action| {
                matches!(
                    action.get("title").and_then(|value| value.as_str()),
                    Some("Generate getter `getTitle`") | Some("Generate setter `setTitle`")
                )
            }),
        "existing accessors should suppress accessor actions, got: {}",
        existing_property_result
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
async fn test_code_action_change_visibility_and_promote_constructor_parameter() {
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
            Some(json!({ "phpVersion": "8.2" })),
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

    let visibility_code = r#"<?php
namespace App;

interface VisibilityContract
{
    public function required(): void;
}

abstract class VisibilityBase
{
    abstract protected function inherited(): void;
}

class VisibilityChild extends VisibilityBase implements VisibilityContract
{
    public function required(): void
    {
    }

    protected function inherited(): void
    {
    }
}

class VisibilityDemo
{
    private string $name;
    protected const FLAG = true;

    public function run(): void
    {
    }

    public function __construct(private int $id)
    {
    }
}
"#;
    let visibility_uri = "file:///test/VisibilityDemo.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(visibility_uri, visibility_code))
        .await
        .unwrap();

    let property_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            2,
            visibility_uri,
            ((26, 20), (26, 24)),
            json!([]),
            vec!["refactor.rewrite"],
        ))
        .await
        .unwrap();
    let property_result = extract_result(property_resp);
    let property_action = property_result
        .as_array()
        .expect("code actions array")
        .iter()
        .find(|action| {
            action.get("title").and_then(|value| value.as_str())
                == Some("Change visibility to protected")
        })
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "expected property visibility action, got: {}",
                property_result
            )
        });
    let property_resolve = service
        .ready()
        .await
        .unwrap()
        .call(code_action_resolve_request(3, property_action))
        .await
        .unwrap();
    let property_resolved = extract_result(property_resolve);
    assert_eq!(
        property_resolved["edit"]["changes"][visibility_uri][0]["newText"].as_str(),
        Some("protected")
    );

    let const_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            4,
            visibility_uri,
            ((27, 20), (27, 24)),
            json!([]),
            vec!["refactor.rewrite"],
        ))
        .await
        .unwrap();
    let const_result = extract_result(const_resp);
    assert!(
        const_result
            .as_array()
            .expect("code actions array")
            .iter()
            .any(
                |action| action.get("title").and_then(|value| value.as_str())
                    == Some("Change visibility to public")
            ),
        "expected class constant visibility action, got: {}",
        const_result
    );

    let method_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            5,
            visibility_uri,
            ((29, 20), (29, 23)),
            json!([]),
            vec!["refactor.rewrite"],
        ))
        .await
        .unwrap();
    let method_result = extract_result(method_resp);
    assert!(
        method_result
            .as_array()
            .expect("code actions array")
            .iter()
            .any(
                |action| action.get("title").and_then(|value| value.as_str())
                    == Some("Change visibility to private")
            ),
        "expected method visibility action, got: {}",
        method_result
    );

    let promoted_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            6,
            visibility_uri,
            ((33, 43), (33, 45)),
            json!([]),
            vec!["refactor.rewrite"],
        ))
        .await
        .unwrap();
    let promoted_result = extract_result(promoted_resp);
    let promoted_action = promoted_result
        .as_array()
        .expect("code actions array")
        .iter()
        .find(|action| {
            action.get("title").and_then(|value| value.as_str())
                == Some("Change visibility to public")
        })
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "expected promoted property visibility action, got: {}",
                promoted_result
            )
        });
    let promoted_resolve = service
        .ready()
        .await
        .unwrap()
        .call(code_action_resolve_request(7, promoted_action))
        .await
        .unwrap();
    let promoted_resolved = extract_result(promoted_resolve);
    assert_eq!(
        promoted_resolved["edit"]["changes"][visibility_uri][0]["newText"].as_str(),
        Some("public")
    );

    let interface_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            8,
            visibility_uri,
            ((5, 21), (5, 29)),
            json!([]),
            vec!["refactor.rewrite"],
        ))
        .await
        .unwrap();
    let interface_result = extract_result(interface_resp);
    assert!(
        !interface_result
            .as_array()
            .expect("code actions array")
            .iter()
            .any(|action| action
                .get("title")
                .and_then(|value| value.as_str())
                .is_some_and(|title| title.starts_with("Change visibility"))),
        "interface contract method should not offer visibility changes, got: {}",
        interface_result
    );

    let implemented_contract_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            9,
            visibility_uri,
            ((15, 20), (15, 28)),
            json!([]),
            vec!["refactor.rewrite"],
        ))
        .await
        .unwrap();
    let implemented_contract_result = extract_result(implemented_contract_resp);
    assert!(
        !implemented_contract_result
            .as_array()
            .expect("code actions array")
            .iter()
            .any(|action| {
                matches!(
                    action.get("title").and_then(|value| value.as_str()),
                    Some("Change visibility to protected") | Some("Change visibility to private")
                )
            }),
        "implementation of public interface method should not offer lowering, got: {}",
        implemented_contract_result
    );

    let protected_override_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            10,
            visibility_uri,
            ((19, 24), (19, 33)),
            json!([]),
            vec!["refactor.rewrite"],
        ))
        .await
        .unwrap();
    let protected_override_result = extract_result(protected_override_resp);
    let protected_override_actions = protected_override_result
        .as_array()
        .expect("code actions array");
    assert!(
        protected_override_actions.iter().any(|action| {
            action.get("title").and_then(|value| value.as_str())
                == Some("Change visibility to public")
        }) && !protected_override_actions.iter().any(|action| {
            action.get("title").and_then(|value| value.as_str())
                == Some("Change visibility to private")
        }),
        "protected override should allow widening but not lowering below abstract contract, got: {}",
        protected_override_result
    );

    let promote_code = r#"<?php
namespace App;

class PromoteDemo
{
    private readonly string $name;
    private int $age;

    public function __construct(string $name, int $age)
    {
        $this->name = $name;
        $this->age = $age;
    }
}
"#;
    let promote_uri = "file:///test/PromoteDemo.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(promote_uri, promote_code))
        .await
        .unwrap();

    let promote_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            11,
            promote_uri,
            ((8, 40), (8, 44)),
            json!([]),
            vec!["refactor.rewrite"],
        ))
        .await
        .unwrap();
    let promote_result = extract_result(promote_resp);
    let promote_action = promote_result
        .as_array()
        .expect("code actions array")
        .iter()
        .find(|action| {
            action.get("title").and_then(|value| value.as_str())
                == Some("Promote constructor parameter `$name`")
        })
        .cloned()
        .unwrap_or_else(|| panic!("expected promote action, got: {}", promote_result));
    assert!(
        promote_action.get("edit").is_none(),
        "promote action should resolve lazily, got: {}",
        promote_action
    );
    let promote_resolve = service
        .ready()
        .await
        .unwrap()
        .call(code_action_resolve_request(12, promote_action))
        .await
        .unwrap();
    let promote_resolved = extract_result(promote_resolve);
    let promote_edits = promote_resolved["edit"]["changes"][promote_uri]
        .as_array()
        .unwrap_or_else(|| panic!("expected promote edits, got: {}", promote_resolved));
    assert!(
        promote_edits
            .iter()
            .any(|edit| edit["newText"].as_str() == Some("private readonly string $name")),
        "expected promoted parameter replacement, got: {}",
        promote_resolved
    );
    assert!(
        promote_edits
            .iter()
            .filter(|edit| edit["newText"].as_str() == Some(""))
            .count()
            == 2,
        "expected property and assignment deletions, got: {}",
        promote_resolved
    );

    let metadata_promote_code = r#"<?php
namespace App;

class MetadataPromotion
{
    /**
     * Display name.
     *
     * @var non-empty-string
     */
    #[Sensitive]
    private string $label;

    public function __construct(
        string $label,
    ) {
        $this->label = $label;
    }
}
"#;
    let metadata_promote_uri = "file:///test/MetadataPromotion.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(
            metadata_promote_uri,
            metadata_promote_code,
        ))
        .await
        .unwrap();
    let metadata_promote_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            13,
            metadata_promote_uri,
            ((14, 16), (14, 22)),
            json!([]),
            vec!["refactor.rewrite"],
        ))
        .await
        .unwrap();
    let metadata_promote_result = extract_result(metadata_promote_resp);
    let metadata_promote_action = metadata_promote_result
        .as_array()
        .expect("code actions array")
        .iter()
        .find(|action| {
            action.get("title").and_then(|value| value.as_str())
                == Some("Promote constructor parameter `$label`")
        })
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "expected metadata promote action, got: {}",
                metadata_promote_result
            )
        });
    let metadata_promote_resolve = service
        .ready()
        .await
        .unwrap()
        .call(code_action_resolve_request(14, metadata_promote_action))
        .await
        .unwrap();
    let metadata_promote_resolved = extract_result(metadata_promote_resolve);
    let metadata_promote_edits = metadata_promote_resolved["edit"]["changes"][metadata_promote_uri]
        .as_array()
        .unwrap_or_else(|| {
            panic!(
                "expected metadata promote edits, got: {}",
                metadata_promote_resolved
            )
        });
    assert!(
        metadata_promote_edits.iter().any(|edit| {
            edit["newText"].as_str().is_some_and(|new_text| {
                new_text.contains("Display name.")
                    && new_text.contains("@var non-empty-string")
                    && new_text.contains("#[Sensitive]")
                    && new_text.contains("private string $label")
            })
        }),
        "expected promoted parameter replacement to carry PHPDoc and attribute metadata, got: {}",
        metadata_promote_resolved
    );
    assert!(
        metadata_promote_edits
            .iter()
            .filter(|edit| edit["newText"].as_str() == Some(""))
            .count()
            == 2,
        "expected metadata property block and assignment deletions, got: {}",
        metadata_promote_resolved
    );

    let complex_code = r#"<?php
namespace App;

class ComplexPromotion
{
    private string $title;

    public function __construct(string $title)
    {
        $this->title = trim($title);
    }
}
"#;
    let complex_uri = "file:///test/ComplexPromotion.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(complex_uri, complex_code))
        .await
        .unwrap();
    let complex_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            15,
            complex_uri,
            ((7, 40), (7, 45)),
            json!([]),
            vec!["refactor.rewrite"],
        ))
        .await
        .unwrap();
    let complex_result = extract_result(complex_resp);
    assert!(
        !complex_result
            .as_array()
            .expect("code actions array")
            .iter()
            .any(|action| action
                .get("title")
                .and_then(|value| value.as_str())
                .is_some_and(|title| title.starts_with("Promote constructor parameter"))),
        "complex assignment should suppress promote action, got: {}",
        complex_result
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
async fn test_code_action_extract_and_inline_refactors() {
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
            Some(json!({ "phpVersion": "8.2" })),
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

    let refactor_code = r#"<?php
namespace App;

class RefactorDemo
{
    public function compute(int $a, int $b): int
    {
        return ($a + $b) * 2;
    }

    public function constants(): string
    {
        return 'active';
    }

    public function inline(int $a, int $b): int
    {
        $total = $a + $b;
        return $total;
    }

    public function collision(int $a, int $b): int
    {
        $extracted = 1;
        return $a * $b;
    }

    public function inlineMany(int $a, int $b): int
    {
        $total = $a + $b;
        $left = $total;
        return $left + $total;
    }

    public function reassigned(int $a, int $b): int
    {
        $value = $a;
        $value = $b;
        return $value;
    }

    public function blocked(bool $flag): int
    {
        if ($flag) {
            $value = 1;
        }

        return $value;
    }
}

class ConstantCollisionDemo
{
    private const EXTRACTED = 1;

    public function value(): int
    {
        return 10;
    }
}

function outside(): string
{
    return 'outside';
}
"#;
    let uri = "file:///test/RefactorDemo.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, refactor_code))
        .await
        .unwrap();

    let extract_variable_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            2,
            uri,
            ((7, 16), (7, 23)),
            json!([]),
            vec!["refactor.extract"],
        ))
        .await
        .unwrap();
    let extract_variable_result = extract_result(extract_variable_resp);
    let extract_variable_action = extract_variable_result
        .as_array()
        .expect("code actions array")
        .iter()
        .find(|action| {
            action.get("title").and_then(|value| value.as_str())
                == Some("Extract variable `$extracted`")
        })
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "expected extract variable action, got: {}",
                extract_variable_result
            )
        });
    assert!(
        extract_variable_action.get("edit").is_none(),
        "extract variable should resolve lazily, got: {}",
        extract_variable_action
    );
    let extract_variable_resolve = service
        .ready()
        .await
        .unwrap()
        .call(code_action_resolve_request(
            3,
            extract_variable_action.clone(),
        ))
        .await
        .unwrap();
    let extract_variable_resolved = extract_result(extract_variable_resolve);
    let extract_variable_edits = extract_variable_resolved["edit"]["changes"][uri]
        .as_array()
        .unwrap_or_else(|| {
            panic!(
                "expected extract variable edits, got: {}",
                extract_variable_resolved
            )
        });
    assert!(
        extract_variable_edits
            .iter()
            .any(|edit| { edit["newText"].as_str() == Some("        $extracted = $a + $b;\n") })
            && extract_variable_edits
                .iter()
                .any(|edit| edit["newText"].as_str() == Some("$extracted")),
        "expected assignment insertion and expression replacement, got: {}",
        extract_variable_resolved
    );

    let extract_constant_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            4,
            uri,
            ((12, 15), (12, 23)),
            json!([]),
            vec!["refactor.extract"],
        ))
        .await
        .unwrap();
    let extract_constant_result = extract_result(extract_constant_resp);
    let extract_constant_action = extract_constant_result
        .as_array()
        .expect("code actions array")
        .iter()
        .find(|action| {
            action.get("title").and_then(|value| value.as_str())
                == Some("Extract constant `EXTRACTED`")
        })
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "expected extract constant action, got: {}",
                extract_constant_result
            )
        });
    let extract_constant_resolve = service
        .ready()
        .await
        .unwrap()
        .call(code_action_resolve_request(5, extract_constant_action))
        .await
        .unwrap();
    let extract_constant_resolved = extract_result(extract_constant_resolve);
    let extract_constant_edits = extract_constant_resolved["edit"]["changes"][uri]
        .as_array()
        .unwrap_or_else(|| {
            panic!(
                "expected extract constant edits, got: {}",
                extract_constant_resolved
            )
        });
    assert!(
        extract_constant_edits.iter().any(|edit| edit["newText"]
            .as_str()
            .is_some_and(|new_text| new_text.contains("private const EXTRACTED = 'active';")))
            && extract_constant_edits
                .iter()
                .any(|edit| edit["newText"].as_str() == Some("self::EXTRACTED")),
        "expected constant insertion and literal replacement, got: {}",
        extract_constant_resolved
    );

    let inline_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            6,
            uri,
            ((18, 15), (18, 21)),
            json!([]),
            vec!["refactor.inline"],
        ))
        .await
        .unwrap();
    let inline_result = extract_result(inline_resp);
    let inline_action = inline_result
        .as_array()
        .expect("code actions array")
        .iter()
        .find(|action| {
            action.get("title").and_then(|value| value.as_str()) == Some("Inline variable `$total`")
        })
        .cloned()
        .unwrap_or_else(|| panic!("expected inline variable action, got: {}", inline_result));
    let inline_resolve = service
        .ready()
        .await
        .unwrap()
        .call(code_action_resolve_request(7, inline_action))
        .await
        .unwrap();
    let inline_resolved = extract_result(inline_resolve);
    let inline_edits = inline_resolved["edit"]["changes"][uri]
        .as_array()
        .unwrap_or_else(|| panic!("expected inline edits, got: {}", inline_resolved));
    assert!(
        inline_edits
            .iter()
            .any(|edit| edit["newText"].as_str() == Some("($a + $b)"))
            && inline_edits
                .iter()
                .any(|edit| edit["newText"].as_str() == Some("")),
        "expected inline replacement and assignment deletion, got: {}",
        inline_resolved
    );

    let collision_extract_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            8,
            uri,
            ((24, 15), (24, 22)),
            json!([]),
            vec!["refactor.extract"],
        ))
        .await
        .unwrap();
    let collision_extract_result = extract_result(collision_extract_resp);
    let collision_extract_action = collision_extract_result
        .as_array()
        .expect("code actions array")
        .iter()
        .find(|action| {
            action.get("title").and_then(|value| value.as_str())
                == Some("Extract variable `$extracted2`")
        })
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "expected extract variable collision fallback, got: {}",
                collision_extract_result
            )
        });
    let collision_extract_resolve = service
        .ready()
        .await
        .unwrap()
        .call(code_action_resolve_request(9, collision_extract_action))
        .await
        .unwrap();
    let collision_extract_resolved = extract_result(collision_extract_resolve);
    let collision_extract_edits = collision_extract_resolved["edit"]["changes"][uri]
        .as_array()
        .unwrap_or_else(|| {
            panic!(
                "expected collision extract edits, got: {}",
                collision_extract_resolved
            )
        });
    assert!(
        collision_extract_edits
            .iter()
            .any(|edit| { edit["newText"].as_str() == Some("        $extracted2 = $a * $b;\n") })
            && collision_extract_edits
                .iter()
                .any(|edit| edit["newText"].as_str() == Some("$extracted2")),
        "expected collision-safe extracted variable edits, got: {}",
        collision_extract_resolved
    );

    let inline_many_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            10,
            uri,
            ((31, 23), (31, 29)),
            json!([]),
            vec!["refactor.inline"],
        ))
        .await
        .unwrap();
    let inline_many_result = extract_result(inline_many_resp);
    let inline_many_action = inline_many_result
        .as_array()
        .expect("code actions array")
        .iter()
        .find(|action| {
            action.get("title").and_then(|value| value.as_str()) == Some("Inline variable `$total`")
        })
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "expected multi-read inline variable action, got: {}",
                inline_many_result
            )
        });
    let inline_many_resolve = service
        .ready()
        .await
        .unwrap()
        .call(code_action_resolve_request(11, inline_many_action))
        .await
        .unwrap();
    let inline_many_resolved = extract_result(inline_many_resolve);
    let inline_many_edits = inline_many_resolved["edit"]["changes"][uri]
        .as_array()
        .unwrap_or_else(|| {
            panic!(
                "expected multi-read inline edits, got: {}",
                inline_many_resolved
            )
        });
    assert!(
        inline_many_edits
            .iter()
            .filter(|edit| edit["newText"].as_str() == Some("($a + $b)"))
            .count()
            == 2
            && inline_many_edits
                .iter()
                .any(|edit| edit["newText"].as_str() == Some("")),
        "expected all same-block reads to be inlined and assignment deleted, got: {}",
        inline_many_resolved
    );

    let reassigned_inline_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            12,
            uri,
            ((38, 15), (38, 21)),
            json!([]),
            vec!["refactor.inline"],
        ))
        .await
        .unwrap();
    let reassigned_inline_result = extract_result(reassigned_inline_resp);
    assert!(
        !reassigned_inline_result
            .as_array()
            .expect("code actions array")
            .iter()
            .any(|action| {
                action.get("title").and_then(|value| value.as_str())
                    == Some("Inline variable `$value`")
            }),
        "reassigned local variable should be suppressed, got: {}",
        reassigned_inline_result
    );

    let constant_collision_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            13,
            uri,
            ((57, 15), (57, 17)),
            json!([]),
            vec!["refactor.extract"],
        ))
        .await
        .unwrap();
    let constant_collision_result = extract_result(constant_collision_resp);
    let constant_collision_action = constant_collision_result
        .as_array()
        .expect("code actions array")
        .iter()
        .find(|action| {
            action.get("title").and_then(|value| value.as_str())
                == Some("Extract constant `EXTRACTED2`")
        })
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "expected extract constant collision fallback, got: {}",
                constant_collision_result
            )
        });
    let constant_collision_resolve = service
        .ready()
        .await
        .unwrap()
        .call(code_action_resolve_request(14, constant_collision_action))
        .await
        .unwrap();
    let constant_collision_resolved = extract_result(constant_collision_resolve);
    let constant_collision_edits = constant_collision_resolved["edit"]["changes"][uri]
        .as_array()
        .unwrap_or_else(|| {
            panic!(
                "expected constant collision edits, got: {}",
                constant_collision_resolved
            )
        });
    assert!(
        constant_collision_edits.iter().any(|edit| edit["newText"]
            .as_str()
            .is_some_and(|new_text| new_text.contains("private const EXTRACTED2 = 10;")))
            && constant_collision_edits
                .iter()
                .any(|edit| edit["newText"].as_str() == Some("self::EXTRACTED2")),
        "expected collision-safe extracted constant edits, got: {}",
        constant_collision_resolved
    );

    let nonliteral_constant_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            15,
            uri,
            ((7, 16), (7, 23)),
            json!([]),
            vec!["refactor.extract"],
        ))
        .await
        .unwrap();
    let nonliteral_constant_result = extract_result(nonliteral_constant_resp);
    assert!(
        !nonliteral_constant_result
            .as_array()
            .expect("code actions array")
            .iter()
            .any(|action| action
                .get("title")
                .and_then(|value| value.as_str())
                .is_some_and(|title| title.starts_with("Extract constant"))),
        "non-literal selection should not offer extract constant, got: {}",
        nonliteral_constant_result
    );

    let out_of_class_constant_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            16,
            uri,
            ((63, 11), (63, 20)),
            json!([]),
            vec!["refactor.extract"],
        ))
        .await
        .unwrap();
    let out_of_class_constant_result = extract_result(out_of_class_constant_resp);
    assert!(
        !out_of_class_constant_result
            .as_array()
            .expect("code actions array")
            .iter()
            .any(|action| action
                .get("title")
                .and_then(|value| value.as_str())
                .is_some_and(|title| title.starts_with("Extract constant"))),
        "out-of-class literal should not offer extract constant, got: {}",
        out_of_class_constant_result
    );

    let blocked_inline_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            17,
            uri,
            ((47, 15), (47, 21)),
            json!([]),
            vec!["refactor.inline"],
        ))
        .await
        .unwrap();
    let blocked_inline_result = extract_result(blocked_inline_resp);
    assert!(
        !blocked_inline_result
            .as_array()
            .expect("code actions array")
            .iter()
            .any(
                |action| action.get("title").and_then(|value| value.as_str())
                    == Some("Inline variable `$value`")
            ),
        "branch-crossing inline should be suppressed, got: {}",
        blocked_inline_result
    );

    service
        .ready()
        .await
        .unwrap()
        .call(did_change_full_notification(uri, 2, refactor_code))
        .await
        .unwrap();
    let stale_resolve = service
        .ready()
        .await
        .unwrap()
        .call(code_action_resolve_request(18, extract_variable_action))
        .await
        .unwrap();
    let stale_resolved = extract_result(stale_resolve);
    assert_eq!(
        stale_resolved["edit"]["changes"]
            .as_object()
            .map(|map| map.len()),
        Some(0),
        "stale extract action should resolve to no-op edit, got: {}",
        stale_resolved
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
async fn test_code_action_update_phpdoc_from_signature() {
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
            Some(json!({ "phpVersion": "8.2" })),
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

    let create_code = r#"<?php
namespace App;

function makeLabel(string $name, ?int &$count, bool ...$flags): string
{
    return $name;
}
"#;
    let create_uri = "file:///test/CreatePhpDoc.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(create_uri, create_code))
        .await
        .unwrap();

    let create_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            2,
            create_uri,
            ((3, 9), (3, 18)),
            json!([]),
            vec!["refactor.rewrite"],
        ))
        .await
        .unwrap();
    let create_result = extract_result(create_resp);
    let create_action = create_result
        .as_array()
        .expect("code actions array")
        .iter()
        .find(|action| {
            action.get("title").and_then(|value| value.as_str())
                == Some("Update PHPDoc from signature")
        })
        .cloned()
        .unwrap_or_else(|| panic!("expected create PHPDoc action, got: {}", create_result));
    assert!(
        create_action.get("edit").is_none(),
        "update PHPDoc action should resolve lazily, got: {}",
        create_action
    );
    let create_resolve = service
        .ready()
        .await
        .unwrap()
        .call(code_action_resolve_request(3, create_action))
        .await
        .unwrap();
    let create_resolved = extract_result(create_resolve);
    let create_text = create_resolved["edit"]["changes"][create_uri][0]["newText"]
        .as_str()
        .unwrap_or_else(|| panic!("expected create PHPDoc edit, got: {}", create_resolved));
    assert!(
        create_text.contains("@param string $name")
            && create_text.contains("@param ?int &$count")
            && create_text.contains("@param bool ...$flags")
            && create_text.contains("@return string"),
        "expected generated PHPDoc to mirror the signature, got: {}",
        create_text
    );

    let patch_code = r#"<?php
namespace App;

class DocDemo
{
    /**
     * Build a value.
     *
     * @template T
     * @param int $stale Drop me.
     * @param array<int, string> $items Items.
     * @return int
     * @throws \RuntimeException
     * @deprecated Use other.
     */
    public function build(string $name, array $items): void
    {
    }
}
"#;
    let patch_uri = "file:///test/PatchPhpDoc.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(patch_uri, patch_code))
        .await
        .unwrap();

    let patch_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            4,
            patch_uri,
            ((15, 20), (15, 25)),
            json!([]),
            vec!["refactor.rewrite"],
        ))
        .await
        .unwrap();
    let patch_result = extract_result(patch_resp);
    let patch_action = patch_result
        .as_array()
        .expect("code actions array")
        .iter()
        .find(|action| {
            action.get("title").and_then(|value| value.as_str())
                == Some("Update PHPDoc from signature")
        })
        .cloned()
        .unwrap_or_else(|| panic!("expected patch PHPDoc action, got: {}", patch_result));
    let patch_resolve = service
        .ready()
        .await
        .unwrap()
        .call(code_action_resolve_request(5, patch_action))
        .await
        .unwrap();
    let patch_resolved = extract_result(patch_resolve);
    let patch_text = patch_resolved["edit"]["changes"][patch_uri][0]["newText"]
        .as_str()
        .unwrap_or_else(|| panic!("expected patch PHPDoc edit, got: {}", patch_resolved));
    assert!(
        patch_text.contains("Build a value.")
            && patch_text.contains("@template T")
            && patch_text.contains("@param string $name")
            && patch_text.contains("@param array<int, string> $items Items.")
            && patch_text.contains("@throws \\RuntimeException")
            && patch_text.contains("@deprecated Use other."),
        "expected updated PHPDoc to preserve summary and unrelated tags, got: {}",
        patch_text
    );
    assert!(
        !patch_text.contains("$stale") && !patch_text.contains("@return"),
        "expected stale param and redundant void return to be removed, got: {}",
        patch_text
    );

    let supported_code = r#"<?php
namespace App;

class SupportedDoc
{
    /**
     * @phpstan-template T of object
     * @psalm-param list<string> $names Analyzer-specific param.
     * @param string $name Old name.
     * @return array<int, string> Labels.
     * @phpstan-return non-empty-array<int, string>
     */
    public function labels(string &$name): array
    {
        return [$name];
    }

    /**
     * @param string $title Title.
     */
    public function __construct(private readonly string $title, public ?int $age = null)
    {
    }
}
"#;
    let supported_uri = "file:///test/SupportedPhpDoc.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(supported_uri, supported_code))
        .await
        .unwrap();

    let rich_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            6,
            supported_uri,
            ((12, 20), (12, 26)),
            json!([]),
            vec!["refactor.rewrite"],
        ))
        .await
        .unwrap();
    let rich_result = extract_result(rich_resp);
    let rich_action = rich_result
        .as_array()
        .expect("code actions array")
        .iter()
        .find(|action| {
            action.get("title").and_then(|value| value.as_str())
                == Some("Update PHPDoc from signature")
        })
        .cloned()
        .unwrap_or_else(|| panic!("expected rich PHPDoc action, got: {}", rich_result));
    let rich_resolve = service
        .ready()
        .await
        .unwrap()
        .call(code_action_resolve_request(7, rich_action))
        .await
        .unwrap();
    let rich_resolved = extract_result(rich_resolve);
    let rich_text = rich_resolved["edit"]["changes"][supported_uri][0]["newText"]
        .as_str()
        .unwrap_or_else(|| panic!("expected rich PHPDoc edit, got: {}", rich_resolved));
    assert!(
        rich_text.contains("@phpstan-template T of object")
            && rich_text.contains("@psalm-param list<string> $names Analyzer-specific param.")
            && rich_text.contains("@param string &$name Old name.")
            && rich_text.contains("@return array<int, string> Labels.")
            && rich_text.contains("@phpstan-return non-empty-array<int, string>"),
        "expected PHPDoc sync to preserve analyzer tags, generic return precision, return description, and by-ref param token, got: {}",
        rich_text
    );

    let promoted_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            8,
            supported_uri,
            ((20, 40), (20, 45)),
            json!([]),
            vec!["refactor.rewrite"],
        ))
        .await
        .unwrap();
    let promoted_result = extract_result(promoted_resp);
    let promoted_action = promoted_result
        .as_array()
        .expect("code actions array")
        .iter()
        .find(|action| {
            action.get("title").and_then(|value| value.as_str())
                == Some("Update PHPDoc from signature")
        })
        .cloned()
        .unwrap_or_else(|| panic!("expected promoted PHPDoc action, got: {}", promoted_result));
    let promoted_resolve = service
        .ready()
        .await
        .unwrap()
        .call(code_action_resolve_request(9, promoted_action))
        .await
        .unwrap();
    let promoted_resolved = extract_result(promoted_resolve);
    let promoted_text = promoted_resolved["edit"]["changes"][supported_uri][0]["newText"]
        .as_str()
        .unwrap_or_else(|| panic!("expected promoted PHPDoc edit, got: {}", promoted_resolved));
    assert!(
        promoted_text.contains("@param string $title Title.")
            && promoted_text.contains("@param ?int $age"),
        "expected promoted constructor params in PHPDoc sync, got: {}",
        promoted_text
    );

    let redundant_code = r#"<?php
namespace App;

/** @return void */
function done(): void
{
}
"#;
    let redundant_uri = "file:///test/RedundantPhpDoc.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(redundant_uri, redundant_code))
        .await
        .unwrap();
    let redundant_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            10,
            redundant_uri,
            ((4, 9), (4, 13)),
            json!([]),
            vec!["refactor.rewrite"],
        ))
        .await
        .unwrap();
    let redundant_result = extract_result(redundant_resp);
    let redundant_action = redundant_result
        .as_array()
        .expect("code actions array")
        .iter()
        .find(|action| {
            action.get("title").and_then(|value| value.as_str())
                == Some("Update PHPDoc from signature")
        })
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "expected redundant PHPDoc action, got: {}",
                redundant_result
            )
        });
    let redundant_resolve = service
        .ready()
        .await
        .unwrap()
        .call(code_action_resolve_request(11, redundant_action))
        .await
        .unwrap();
    let redundant_resolved = extract_result(redundant_resolve);
    assert_eq!(
        redundant_resolved["edit"]["changes"][redundant_uri][0]["newText"].as_str(),
        Some(""),
        "expected redundant PHPDoc-only block to be removed, got: {}",
        redundant_resolved
    );

    let current_code = r#"<?php
namespace App;

/**
 * @param string $name
 * @return string
 */
function current(string $name): string
{
    return $name;
}
"#;
    let current_uri = "file:///test/CurrentPhpDoc.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(current_uri, current_code))
        .await
        .unwrap();
    let current_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request_with_only(
            12,
            current_uri,
            ((7, 9), (7, 16)),
            json!([]),
            vec!["refactor.rewrite"],
        ))
        .await
        .unwrap();
    let current_result = extract_result(current_resp);
    assert!(
        !current_result
            .as_array()
            .expect("code actions array")
            .iter()
            .any(
                |action| action.get("title").and_then(|value| value.as_str())
                    == Some("Update PHPDoc from signature")
            ),
        "up-to-date PHPDoc should not offer update action, got: {}",
        current_result
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
async fn test_code_action_organize_imports_sorts_groups_and_removes_unused() {
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

use Zed\Unused;
use function Vendor\zeta;
use Vendor\Foo;
use const Vendor\VALUE;
use Alpha\Bar;

class Demo {
    public function run(Foo $foo, Bar $bar): void {
        zeta();
        echo VALUE;
    }
}
"#;
    let uri = "file:///test/OrganizeImports.php";
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
        .call(organize_imports_request(2, uri))
        .await
        .unwrap();
    let result = extract_result(resp);
    let actions = result.as_array().expect("code actions array");
    let organize_action = actions
        .iter()
        .find(|action| action.get("title").and_then(|v| v.as_str()) == Some("Organize imports"))
        .unwrap_or_else(|| panic!("expected Organize imports action, got: {}", result));

    assert_eq!(
        organize_action.get("kind").and_then(|v| v.as_str()),
        Some("source.organizeImports")
    );

    let new_text = organize_action["edit"]["changes"][uri][0]["newText"]
        .as_str()
        .unwrap_or("");
    assert_eq!(
        new_text,
        "use Alpha\\Bar;\nuse Vendor\\Foo;\n\nuse function Vendor\\zeta;\n\nuse const Vendor\\VALUE;\n"
    );
    assert!(
        !new_text.contains("Zed\\Unused"),
        "unused import should be removed, got: {}",
        new_text
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
async fn test_code_action_unused_import_quickfixes() {
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

use App\Unused;
use App\AlsoUnused;
use App\Used;

class Demo {
    public function run(Used $used): void {}
}
"#;
    let uri = "file:///test/UnusedImportQuickfix.php";
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
        .call(code_action_request(2, uri, 3, 0, 3, 15, json!([])))
        .await
        .unwrap();
    let result = extract_result(resp);
    let actions = result.as_array().expect("code actions array");
    let remove_single = actions
        .iter()
        .find(|action| {
            action.get("title").and_then(|value| value.as_str()) == Some("Remove unused import")
        })
        .unwrap_or_else(|| panic!("expected Remove unused import action, got: {}", result));
    assert_eq!(
        remove_single["edit"]["changes"][uri][0]["newText"].as_str(),
        Some("")
    );

    let remove_all = actions
        .iter()
        .find(|action| {
            action.get("title").and_then(|value| value.as_str())
                == Some("Remove all unused imports")
        })
        .unwrap_or_else(|| panic!("expected Remove all unused imports action, got: {}", result));
    let remove_all_text = remove_all["edit"]["changes"][uri][0]["newText"]
        .as_str()
        .unwrap_or("");
    assert!(
        remove_all_text.contains("use App\\Used;")
            && !remove_all_text.contains("App\\Unused")
            && !remove_all_text.contains("App\\AlsoUnused"),
        "expected organize-imports-backed bulk unused import removal, got: {}",
        remove_all
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
async fn test_code_action_deprecated_replacement_from_diagnostic_data() {
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

    let code = "<?php\noldCall();\n";
    let uri = "file:///test/DeprecatedReplacement.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let deprecated_diag = json!([
        {
            "range": {
                "start": { "line": 1, "character": 0 },
                "end": { "line": 1, "character": 7 }
            },
            "severity": 2,
            "source": "php-lsp",
            "code": "php-lsp.deprecated",
            "message": "Deprecated call: oldCall",
            "data": {
                "phpLsp": {
                    "replacement": {
                        "title": "Replace deprecated call with `newCall`",
                        "newText": "newCall"
                    }
                }
            }
        }
    ]);

    let resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request(2, uri, 1, 0, 1, 7, deprecated_diag))
        .await
        .unwrap();
    let result = extract_result(resp);
    let action = result
        .as_array()
        .expect("code actions array")
        .iter()
        .find(|action| {
            action.get("title").and_then(|value| value.as_str())
                == Some("Replace deprecated call with `newCall`")
        })
        .unwrap_or_else(|| panic!("expected deprecated replacement action, got: {}", result));
    assert_eq!(
        action["edit"]["changes"][uri][0]["newText"].as_str(),
        Some("newCall")
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
async fn test_code_action_external_analyzer_fixes_are_opt_in() {
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

class Demo
{
    public function run(array $items): void
    {
        foreach ($items as $item) {}
        Legacy\Foo::run();
        risky();
    }
}
"#;
    let uri = "file:///test/AnalyzerQuickfixes.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let throws_diag = json!([
        {
            "range": {
                "start": { "line": 9, "character": 8 },
                "end": { "line": 9, "character": 13 }
            },
            "severity": 1,
            "source": "phpstan",
            "code": "missingType.checkedException",
            "message": "Method App\\Demo::run() throws RuntimeException but is missing @throws.",
            "data": {
                "phpLsp": {
                    "analyzerFixes": [
                        { "kind": "addThrows", "exception": "\\RuntimeException" }
                    ]
                }
            }
        }
    ]);

    let disabled_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request(
            2,
            uri,
            9,
            8,
            9,
            13,
            throws_diag.clone(),
        ))
        .await
        .unwrap();
    let disabled_result = extract_result(disabled_resp);
    assert!(
        disabled_result
            .as_array()
            .expect("code actions array")
            .iter()
            .all(|action| action
                .get("title")
                .and_then(|value| value.as_str())
                .is_none_or(
                    |title| !title.contains("PHPStan") && !title.starts_with("Add @throws")
                )),
        "analyzer quick fixes must be disabled by default, got: {}",
        disabled_result
    );

    service
        .ready()
        .await
        .unwrap()
        .call(did_change_configuration_notification(json!({
            "phpLsp": {
                "analyzerCodeActions": {
                    "enabled": true
                }
            }
        })))
        .await
        .unwrap();

    let throws_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request(3, uri, 9, 8, 9, 13, throws_diag))
        .await
        .unwrap();
    let throws_result = extract_result(throws_resp);
    let throws_actions = throws_result.as_array().expect("code actions array");
    assert!(
        throws_actions.iter().any(|action| {
            action.get("title").and_then(|value| value.as_str())
                == Some("Ignore PHPStan diagnostic locally")
                && action["edit"]["changes"][uri][0]["newText"]
                    .as_str()
                    .is_some_and(|text| text.contains("@phpstan-ignore-next-line"))
        }),
        "expected PHPStan ignore action, got: {}",
        throws_result
    );
    assert!(
        throws_actions.iter().any(|action| {
            action.get("title").and_then(|value| value.as_str())
                == Some("Add @throws \\RuntimeException")
                && action["edit"]["changes"][uri][0]["newText"]
                    .as_str()
                    .is_some_and(|text| text.contains("@throws \\RuntimeException"))
        }),
        "expected add @throws action, got: {}",
        throws_result
    );

    let iterable_diag = json!([
        {
            "range": {
                "start": { "line": 5, "character": 23 },
                "end": { "line": 5, "character": 29 }
            },
            "severity": 1,
            "source": "phpstan",
            "code": "missingType.iterableValue",
            "message": "Method App\\Demo::run() has parameter $items with no value type specified in iterable type array.",
            "data": {
                "phpLsp": {
                    "analyzerFixes": [
                        {
                            "kind": "addIterableValueType",
                            "variable": "items",
                            "typeText": "array<int, Item>"
                        }
                    ]
                }
            }
        }
    ]);
    let iterable_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request(4, uri, 5, 23, 5, 29, iterable_diag))
        .await
        .unwrap();
    let iterable_result = extract_result(iterable_resp);
    assert!(
        iterable_result
            .as_array()
            .expect("code actions array")
            .iter()
            .any(|action| {
                action.get("title").and_then(|value| value.as_str())
                    == Some("Add PHPDoc iterable value type for `$items`")
                    && action["edit"]["changes"][uri][0]["newText"]
                        .as_str()
                        .is_some_and(|text| text.contains("@param array<int, Item> $items"))
            }),
        "expected iterable value PHPDoc action, got: {}",
        iterable_result
    );

    let prefixed_class_diag = json!([
        {
            "range": {
                "start": { "line": 8, "character": 8 },
                "end": { "line": 8, "character": 18 }
            },
            "severity": 1,
            "source": "psalm",
            "code": "UndefinedClass",
            "message": "Class Legacy\\Foo was not found.",
            "data": {
                "phpLsp": {
                    "analyzerFixes": [
                        {
                            "kind": "replacePrefixedClassName",
                            "replacement": "\\App\\Legacy\\Foo"
                        }
                    ]
                }
            }
        }
    ]);
    let prefixed_class_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_request(
            5,
            uri,
            8,
            8,
            8,
            18,
            prefixed_class_diag,
        ))
        .await
        .unwrap();
    let prefixed_class_result = extract_result(prefixed_class_resp);
    let prefixed_class_actions = prefixed_class_result
        .as_array()
        .expect("code actions array");
    assert!(
        prefixed_class_actions.iter().any(|action| {
            action.get("title").and_then(|value| value.as_str())
                == Some("Ignore Psalm diagnostic locally")
                && action["edit"]["changes"][uri][0]["newText"]
                    .as_str()
                    .is_some_and(|text| text.contains("@psalm-suppress UndefinedClass"))
        }),
        "expected Psalm ignore action, got: {}",
        prefixed_class_result
    );
    assert!(
        prefixed_class_actions.iter().any(|action| {
            action.get("title").and_then(|value| value.as_str())
                == Some("Replace class name with `\\App\\Legacy\\Foo`")
                && action["edit"]["changes"][uri][0]["newText"].as_str()
                    == Some("\\App\\Legacy\\Foo")
        }),
        "expected prefixed class replacement action, got: {}",
        prefixed_class_result
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
async fn test_code_action_add_return_type_from_phpdoc() {
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
            Some(json!({ "phpVersion": "8.2" })),
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

    let code = r#"<?php
namespace App;

/**
 * @return string|null
 */
function label($value) {
    return $value;
}

class Demo {
    /**
     * @return static
     */
    public function fluent() {
        return $this;
    }

    /** @return int */
    public function already(): int {
        return 1;
    }

    /** @return string */
    public function __construct() {}
}
"#;
    let uri = "file:///test/AddReturnType.php";
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
        .call(add_return_type_request(2, uri, ((0, 0), (26, 0))))
        .await
        .unwrap();
    let result = extract_result(resp);
    let actions = result.as_array().expect("code actions array");

    let function_action = actions
        .iter()
        .find(|action| {
            action.get("title").and_then(|v| v.as_str()) == Some("Add return type `string|null`")
        })
        .unwrap_or_else(|| panic!("expected string|null return type action, got: {}", result));
    assert_eq!(
        function_action.get("kind").and_then(|v| v.as_str()),
        Some("refactor.rewrite")
    );
    assert!(
        function_action.get("edit").is_none(),
        "add return type action should be resolved lazily, got: {}",
        function_action
    );
    assert!(
        function_action.get("data").is_some(),
        "add return type action should carry resolve data, got: {}",
        function_action
    );
    let function_resolve_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_resolve_request(3, function_action.clone()))
        .await
        .unwrap();
    let function_resolved = extract_result(function_resolve_resp);
    assert_eq!(
        function_resolved["edit"]["changes"][uri][0]["newText"].as_str(),
        Some(": string|null")
    );

    let method_action = actions
        .iter()
        .find(|action| {
            action.get("title").and_then(|v| v.as_str()) == Some("Add return type `static`")
        })
        .unwrap_or_else(|| panic!("expected static return type action, got: {}", result));
    assert_eq!(method_action.get("edit").and_then(|v| v.as_object()), None);
    let method_resolve_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_resolve_request(4, method_action.clone()))
        .await
        .unwrap();
    let method_resolved = extract_result(method_resolve_resp);
    assert_eq!(
        method_resolved["edit"]["changes"][uri][0]["newText"].as_str(),
        Some(": static")
    );
    assert!(
        !actions.iter().any(|action| {
            action
                .get("title")
                .and_then(|v| v.as_str())
                .is_some_and(|title| title.contains("int"))
        }),
        "should not offer action for declarations that already have native return type: {}",
        result
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
async fn test_code_action_resolve_add_return_type_returns_noop_for_stale_version() {
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
            Some(json!({ "phpVersion": "8.2" })),
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

    let code = r#"<?php
/**
 * @return string|null
 */
function label($value) {
    return $value;
}
"#;
    let uri = "file:///test/StaleAddReturnType.php";
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
        .call(add_return_type_request(2, uri, ((0, 0), (8, 0))))
        .await
        .unwrap();
    let result = extract_result(resp);
    let action = result
        .as_array()
        .expect("code actions array")
        .first()
        .cloned()
        .unwrap_or_else(|| panic!("expected add return type action, got: {}", result));

    let changed_code = r#"<?php
/**
 * @return string|null
 */
function label($value): string|null {
    return $value;
}
"#;
    service
        .ready()
        .await
        .unwrap()
        .call(did_change_full_notification(uri, 2, changed_code))
        .await
        .unwrap();

    let resolve_resp = service
        .ready()
        .await
        .unwrap()
        .call(code_action_resolve_request(3, action))
        .await
        .unwrap();
    let resolved = extract_result(resolve_resp);
    let changes = resolved["edit"]["changes"]
        .as_object()
        .expect("empty changes object");
    assert!(
        changes.is_empty(),
        "stale add return type action should resolve to a no-op edit, got: {}",
        resolved
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
async fn test_code_action_add_return_type_respects_php_version() {
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
            Some(json!({ "phpVersion": "7.4" })),
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

    let code = r#"<?php
/**
 * @return string|null
 */
function label($value) {
    return $value;
}
"#;
    let uri = "file:///test/AddReturnTypePhp74.php";
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
        .call(add_return_type_request(2, uri, ((0, 0), (8, 0))))
        .await
        .unwrap();
    let result = extract_result(resp);
    let actions = result.as_array().expect("code actions array");
    assert!(
        actions.is_empty(),
        "PHP 7.4 should not offer PHP 8 union return type action, got: {}",
        result
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}
